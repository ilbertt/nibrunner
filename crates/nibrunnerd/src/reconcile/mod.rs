//! Pull, converge, report. Nothing here is driven by a command: desired state describes a world,
//! this compares it with what the host is observed to be doing, and the difference is the work.

pub mod idle;
pub mod instances;
pub mod network;
pub mod plan;
pub mod volumes;

pub use plan::*;

use std::collections::BTreeSet;
use std::sync::Arc;

use protocol::{DesiredInstanceState, HostDesiredState};

use crate::host::Host;
use crate::vm::UNKNOWN_VM;

/// The union of what the host is running and what this daemon has notes on: a microVM with no
/// note is an orphan to stop, and a note with no microVM is an instance that is gone.
pub async fn observe(host: &Host, desired: &HostDesiredState) -> ObservedState {
    let snapshot = host.state.snapshot().await;
    let mut app_ids: BTreeSet<protocol::AppId> = host.vms.adopted_app_ids().await.into_iter().collect();
    app_ids.extend(snapshot.records.keys().cloned());
    let app_ids: Vec<_> = app_ids.into_iter().collect();
    let statuses = host.vms.statuses(&app_ids).await;

    ObservedState {
        instances: app_ids
            .iter()
            .map(|app_id| {
                let status = statuses.get(app_id).copied().unwrap_or(UNKNOWN_VM);
                let record = snapshot.records.get(app_id);
                ObservedInstance {
                    app_id: app_id.clone(),
                    volume_id: record.map(|record| record.volume_id.clone()),
                    deployment_id: record.map(|record| record.deployment_id.clone()),
                    present: status.loaded || record.is_some(),
                    running: status.active,
                    // `started_this_boot` keeps a reboot out of this: the record survives on disk,
                    // so without it every instance would look exited after a reboot and be left
                    // alone.
                    exited: !status.active
                        && status.started_this_boot
                        && record.is_some_and(|record| record.started_at.is_some() && !record.stop_requested),
                }
            })
            .collect(),
        volumes: volumes::observe_volumes(host, &volumes::volume_owners(desired, &snapshot.records)).await,
        checkpoints: Vec::new(),
        exports: Vec::new(),
    }
}

/// Hostnames, the health check and whether the app should be up all change without the artifact
/// changing, so they are folded in before anything reads the record.
async fn sync_desired(host: &Host, desired: &HostDesiredState) {
    for wanted in &desired.instances {
        host.state
            .update_record(&wanted.app_id, |record| {
                record.hostnames = wanted.hostnames.clone();
                record.health_check = wanted.config.health_check.clone();
                record.resources = wanted.config.resources;
                record.desired_running = wanted.desired_state != DesiredInstanceState::Stopped;
                record.on_request = wanted.desired_state == DesiredInstanceState::OnRequest;
                record.http_port = wanted.config.http_port;
                record.has_extra_public_port = Some(wanted.config.has_extra_public_port);
            })
            .await;
    }
}

async fn apply_stops(host: &Host, plan: &ReconcilePlan) {
    for action in &plan.instances {
        match action {
            InstancePlan::Stop { app_id, reason } => {
                instances::stop_instance(host, app_id, reason.as_str()).await;
            }
            InstancePlan::Replace { desired } => {
                instances::stop_instance(host, &desired.app_id, InstanceStopReason::Superseded.as_str())
                    .await;
                let _ = host.vms.discard(&desired.app_id).await;
                host.state.drop_record(&desired.app_id).await;
            }
            InstancePlan::Forget { app_id } => {
                let _ = host.vms.discard(app_id).await;
                host.state.drop_record(app_id).await;
            }
            _ => {}
        }
    }
}

/// The apps this host answers for without running anything. Nothing boots here — that is what
/// makes them cheap — but the slot and the record are what a request has to arrive to, so they
/// are made before the activators bind and before routing is rendered.
async fn apply_sleeps(host: &Host, plan: &ReconcilePlan) {
    for action in &plan.instances {
        if let InstancePlan::Sleep { desired } = action {
            instances::sleep_instance(host, desired).await;
        }
    }
}

/// A tenant only ever boots onto a host whose isolation ruleset is in the kernel: the guest is an
/// arbitrary binary one routing decision away from the control plane and its neighbours.
async fn apply_starts(host: &Host, plan: &ReconcilePlan) {
    let starts: Vec<_> = plan
        .instances
        .iter()
        .filter_map(|action| match action {
            InstancePlan::Start { desired } | InstancePlan::Replace { desired } => Some(desired),
            _ => None,
        })
        .collect();
    if starts.is_empty() {
        return;
    }
    if !host.state.snapshot().await.isolated {
        tracing::error!(
            refused = starts.len(),
            "instance starts refused: isolation ruleset not applied"
        );
        return;
    }
    for desired in starts {
        instances::start_instance(host, desired).await;
    }
}

/// One pass. The order is the contract: what a request has to find is made before anything binds
/// to it, the ruleset is in the kernel before anything boots, and the forwards are rendered again
/// after the starts that produced them.
pub async fn reconcile(host: &Arc<Host>, desired: &HostDesiredState) {
    let observed = observe(host, desired).await;
    let plan = plan_reconcile(desired, &observed);
    host.state
        .modify(|snapshot| snapshot.deferred_work = plan.has_deferred_work())
        .await;
    sync_desired(host, desired).await;

    // The fetch does not need the host to have stopped anything, so it runs beside the stops
    // rather than after them.
    let prefetch = instances::prefetch_artifacts(host, &plan);
    let stops = apply_stops(host, &plan);
    tokio::join!(prefetch, stops);

    volumes::apply_volumes(host, &plan, &observed, desired).await;
    // Before the activators below, because the slot one binds on is allocated here.
    apply_sleeps(host, &plan).await;
    // Before the forwards below are withdrawn from a tenant that has just stopped, so the port it
    // was reached on is answered rather than closed.
    network::apply_activators(host).await;
    // Before anything boots: nothing persists the ruleset across a reboot, so a host that started
    // its VMs first would serve tenants through a kernel with no table of ours.
    network::apply_network(host).await;
    apply_starts(host, &plan).await;
    volumes::apply_teardowns(host, &plan).await;
    // Again, because a start is where an app that is not `on-request` first gets a slot: without
    // this its loopback port stays nobody's until the next pass, and a request arriving in
    // between is refused by the kernel rather than answered by this daemon saying why.
    network::apply_activators(host).await;
    // And again, because the instances started above are the ones whose forwards this renders.
    network::apply_network(host).await;
    network::apply_routes(host).await;
    host.persist().await;

    host.state.modify(|snapshot| snapshot.converged = true).await;
    host.state.signal_report();
}

/// What the status tick does: probe what is due, put the result in the kernel and in the routes,
/// and write down what changed.
pub async fn refresh(host: &Arc<Host>) {
    instances::refresh_states(host).await;
    network::apply_network(host).await;
    network::apply_routes(host).await;
    host.persist().await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::{VmCall, VmError};
    use crate::test_support::*;
    use crate::vm::VmStatus;
    use protocol::InstanceState;

    fn running_vm() -> VmStatus {
        VmStatus {
            loaded: true,
            active: true,
            failed: false,
            started_this_boot: true,
            exit_code: None,
        }
    }

    fn stopped_vm() -> VmStatus {
        VmStatus {
            loaded: true,
            active: false,
            failed: false,
            started_this_boot: true,
            exit_code: Some(0),
        }
    }

    /// One app with somewhere to write, which is the least a host can be asked to run.
    fn running_app() -> protocol::HostDesiredState {
        desired_state(|state| {
            state.volumes = vec![desired_volume(|_| {})];
            state.instances = vec![desired_instance(|instance| {
                instance.hostnames = vec![app_hostname()]
            })];
        })
    }

    /// A wake is a restore, and a cold boot is only what is left when there is nothing to restore.
    /// Which one ran is the difference between a visitor waiting thirty milliseconds and a second.
    #[tokio::test]
    async fn an_app_is_woken_by_putting_back_the_microvm_it_had() {
        let host = test_host().await;
        host.vms.set_status(stopped_vm());
        host.state
            .put_record(instance_record(|record| {
                record.on_request = true;
                record.state = InstanceState::Idle;
            }))
            .await;

        let on_request =
            desired_instance(|instance| instance.desired_state = DesiredInstanceState::OnRequest);
        let outcome = instances::resume_instance(&host, &on_request).await.unwrap();
        assert_eq!(outcome, crate::services::WakeOutcome::Restored);
        assert_eq!(host.vms.calls(), vec![VmCall::Wake]);
        assert!(host.state.record(&app_id()).await.unwrap().started_at.is_some());
    }

    /// The one path that may cold-boot: a first sleep, a redeploy, a host that rebooted, a guest
    /// image that moved. Every other failure leaves the app down and says why.
    #[tokio::test]
    async fn a_snapshot_nothing_can_load_is_a_cold_boot_instead() {
        let host = test_host().await;
        host.volumes.provision(&desired_volume(|_| {})).await.unwrap();
        host.vms.set_status(stopped_vm());
        host.vms.refuse_wake(VmError::SnapshotUnusable {
            reason: "the host has rebooted".into(),
        });
        host.state.modify(|snapshot| snapshot.isolated = true).await;
        host.state
            .put_record(instance_record(|record| {
                record.on_request = true;
                record.state = InstanceState::Idle;
            }))
            .await;

        let on_request =
            desired_instance(|instance| instance.desired_state = DesiredInstanceState::OnRequest);
        let outcome = instances::resume_instance(&host, &on_request).await.unwrap();
        assert_eq!(outcome, crate::services::WakeOutcome::ColdBoot);
        assert_eq!(host.vms.calls(), vec![VmCall::Wake, VmCall::Boot]);
    }

    /// An app woken every morning for a year is not an app that crashed three hundred times.
    #[tokio::test]
    async fn a_restore_is_not_a_restart_so_it_costs_the_app_nothing() {
        let host = test_host().await;
        host.vms.set_status(stopped_vm());
        host.state
            .put_record(instance_record(|record| {
                record.on_request = true;
                record.state = InstanceState::Idle;
                record.restart_count = 4;
            }))
            .await;

        let on_request =
            desired_instance(|instance| instance.desired_state = DesiredInstanceState::OnRequest);
        instances::resume_instance(&host, &on_request).await.unwrap();
        let record = host.state.record(&app_id()).await.unwrap();
        assert_eq!(record.restart_count, 4);
        assert_eq!(record.start_attempts.attempts, 0);
    }

    /// Firecracker takes a snapshot load only from a process that has configured nothing, and
    /// starting one for a microVM already up is what would have hidden that.
    #[tokio::test]
    async fn a_microvm_that_is_already_up_is_left_alone_rather_than_restored_onto() {
        let host = test_host().await;
        host.vms.set_status(running_vm());
        host.state
            .put_record(instance_record(|record| record.on_request = true))
            .await;
        let on_request =
            desired_instance(|instance| instance.desired_state = DesiredInstanceState::OnRequest);
        let outcome = instances::resume_instance(&host, &on_request).await.unwrap();
        assert_eq!(outcome, crate::services::WakeOutcome::AlreadyRunning);
        assert!(host.vms.calls().is_empty());
    }

    #[tokio::test]
    async fn an_app_that_has_gone_quiet_is_put_down_where_it_can_be_picked_up() {
        let host = test_host().await;
        host.slot_for(&app_id()).await.unwrap();
        host.state
            .put_record(instance_record(|record| record.on_request = true))
            .await;

        instances::suspend_instance(&host, &app_id(), "idle").await;

        assert_eq!(host.vms.calls(), vec![VmCall::Sleep]);
        let record = host.state.record(&app_id()).await.unwrap();
        assert_eq!(record.state, InstanceState::Idle);
        // What tells the health loop a microVM that is gone is asleep rather than crashed.
        assert!(record.stop_requested);
        assert!(!host.state.snapshot().await.snapshotting.contains(&app_id()));
    }

    /// A refusal is an outcome, not a failure: the microVM is left running, so the app goes on
    /// serving and the next measurement tick asks again.
    #[tokio::test]
    async fn one_that_may_not_be_snapshotted_is_left_up_rather_than_called_broken() {
        let host = test_host().await;
        host.slot_for(&app_id()).await.unwrap();
        host.vms.refuse_sleep(VmError::SleepRefused {
            reason: "it has already been asked to stop".into(),
        });
        host.state
            .put_record(instance_record(|record| record.on_request = true))
            .await;

        instances::suspend_instance(&host, &app_id(), "idle").await;

        assert_eq!(
            host.state.record(&app_id()).await.unwrap().state,
            InstanceState::Running
        );
        // The mark is cleared however the sleep ended, or the next real crash would read as one.
        assert!(!host.state.snapshot().await.snapshotting.contains(&app_id()));
    }

    /// No slot is no tap, no address and no device for a restore to land on. Stopping still
    /// reclaims the memory, which is what the sleep was for.
    #[tokio::test]
    async fn one_with_no_slot_to_come_back_to_is_stopped() {
        let host = test_host().await;
        host.state
            .put_record(instance_record(|record| record.on_request = true))
            .await;
        instances::suspend_instance(&host, &app_id(), "idle").await;
        assert_eq!(host.vms.calls(), vec![VmCall::Stop]);
    }

    /// A pass that lands while a snapshot is being taken: the capture has taken the VMM down and
    /// `stop_requested` is not written until it returns, so reading that as a crash would drop the
    /// app out of desired state and take its hostnames off the proxy with it.
    #[tokio::test]
    async fn a_pass_that_lands_mid_capture_reads_the_microvm_as_asleep_rather_than_crashed() {
        let host = test_host().await;
        host.vms.set_status(stopped_vm());
        host.state
            .put_record(instance_record(|record| {
                record.on_request = true;
                record.started_at = Some(observed_at());
            }))
            .await;
        host.state.mark_snapshotting(&app_id(), true).await;

        instances::refresh_states(host.arc()).await;

        let record = host.state.record(&app_id()).await.unwrap();
        assert_eq!(record.state, InstanceState::Idle);
        assert!(record.message.is_none());
    }

    #[tokio::test]
    async fn and_fails_it_once_the_snapshot_is_no_longer_in_flight() {
        let host = test_host().await;
        host.vms.set_status(stopped_vm());
        host.vms
            .set_verdict("the tenant used its 5 restarts without staying up; shutting the guest down");
        host.state
            .put_record(instance_record(|record| {
                record.on_request = true;
                record.started_at = Some(observed_at());
            }))
            .await;

        instances::refresh_states(host.arc()).await;

        let record = host.state.record(&app_id()).await.unwrap();
        assert_eq!(record.state, InstanceState::Failed);
        // The guest's own account wins over the exit code, which is 0 whenever it powered itself
        // off deliberately.
        assert!(record.message.unwrap().as_str().contains("used its 5 restarts"));
    }

    /// Instances are authoritative, so a microVM this host holds and desired state does not
    /// mention is stopped and forgotten — and a volume is only ever removed by an explicit absent.
    #[tokio::test]
    async fn one_pass_converges_a_host_onto_a_document_it_has_never_seen() {
        let _serial = ONE_HOST_AT_A_TIME.lock().await;
        let host = test_host().await;

        reconcile(host.arc(), &running_app()).await;

        // The volume was provisioned, the slot allocated, the record written and the app booted.
        assert!(host.slot_of(&app_id()).await.is_some());
        assert_eq!(host.vms.calls(), vec![VmCall::Boot]);
        let record = host.state.record(&app_id()).await.unwrap();
        assert_eq!(record.state, InstanceState::Starting);
        assert_eq!(record.deployment_id, deployment_id());
        // The ruleset went in before the boot, and the route is rendered for the hostname it holds.
        assert!(host.state.snapshot().await.isolated);
        assert_eq!(
            host.router
                .routes()
                .await
                .port_for(app_hostname().hostname.as_str()),
            Some(record.host_port)
        );
        // And it is written down, so a restart adopts rather than re-derives from nothing.
        assert!(host.config.instances_file().exists());
        assert!(host.config.slots_file().exists());
    }

    /// Two passes, and deliberately: the first stops what is running, and only a microVM that is
    /// down is one there is nothing left to stop before forgetting it.
    /// The port an app is reached on has to answer from the pass that created it. A slot that
    /// exists with nothing listening on it is a request meeting a refused connection, which an
    /// edge in front of this host cannot tell from a host that is not there.
    #[tokio::test]
    async fn the_port_an_app_is_reached_on_answers_from_the_pass_that_allocated_it() {
        let _serial = ONE_HOST_AT_A_TIME.lock().await;
        let host = test_host().await;
        reconcile(host.arc(), &running_app()).await;
        assert_eq!(host.activator.listening_for().await, vec![app_id()]);
    }

    #[tokio::test]
    async fn an_instance_desired_state_stops_naming_is_stopped_then_forgotten() {
        let _serial = ONE_HOST_AT_A_TIME.lock().await;
        let host = test_host().await;
        reconcile(host.arc(), &running_app()).await;
        host.vms.set_status(running_vm());

        reconcile(host.arc(), &desired_state(|_| {})).await;
        assert!(host.vms.calls().contains(&VmCall::Stop));
        assert_eq!(
            host.state.record(&app_id()).await.unwrap().state,
            InstanceState::Stopped
        );

        host.vms.set_status(stopped_vm());
        reconcile(host.arc(), &desired_state(|_| {})).await;
        assert!(host.state.record(&app_id()).await.is_none());
        assert!(host.vms.calls().contains(&VmCall::Discard));
    }

    /// An instance whose filesystem this host does not hold is one nothing may boot: a guest
    /// pointed at a device that is not there would come up with no data at all.
    #[tokio::test]
    async fn an_instance_whose_volume_is_not_here_does_not_boot() {
        let _serial = ONE_HOST_AT_A_TIME.lock().await;
        let host = test_host().await;
        // The instance is named and the volume is not, which is what a host that never provisioned
        // it looks like.
        reconcile(
            host.arc(),
            &desired_state(|state| state.instances = vec![desired_instance(|_| {})]),
        )
        .await;
        assert!(host.vms.calls().is_empty());
        let record = host.state.record(&app_id()).await.unwrap();
        assert_eq!(record.state, InstanceState::Failed);
        assert!(record.message.unwrap().as_str().contains("does not serve"));
    }

    #[tokio::test]
    async fn a_deploy_replaces_the_release_rather_than_restarting_it() {
        let _serial = ONE_HOST_AT_A_TIME.lock().await;
        let host = test_host().await;
        reconcile(host.arc(), &running_app()).await;
        host.vms.set_status(running_vm());

        let newer = desired_state(|state| {
            state.volumes = vec![desired_volume(|_| {})];
            state.instances = vec![desired_instance(|instance| {
                instance.deployment_id = protocol::DeploymentId::parse("dep-2").unwrap();
            })]
        });
        reconcile(host.arc(), &newer).await;

        assert_eq!(
            host.state.record(&app_id()).await.unwrap().deployment_id.as_str(),
            "dep-2"
        );
        // The outgoing microVM was stopped and discarded before the new one was booted.
        let calls = host.vms.calls();
        let stopped = calls.iter().position(|call| *call == VmCall::Stop).unwrap();
        let booted = calls.iter().rposition(|call| *call == VmCall::Boot).unwrap();
        assert!(stopped < booted);
    }
}
