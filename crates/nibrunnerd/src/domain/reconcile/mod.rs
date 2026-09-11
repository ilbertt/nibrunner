pub mod checkpoints;
pub mod exports;
pub mod idle;
pub mod ingress;
pub mod instances;
pub mod network;
pub mod plan;
pub mod volumes;

pub use plan::*;

use std::collections::BTreeSet;
use std::sync::Arc;

use protocol::{DesiredInstanceState, HostDesiredState};

use crate::adapters::vm::UNKNOWN_VM;
use crate::domain::metrics::converge;
use crate::host::Host;

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
                    exited: !status.active
                        && status.started_this_boot
                        && record.is_some_and(|record| record.started_at.is_some() && !record.stop_requested),
                }
            })
            .collect(),
        volumes: volumes::observe_volumes(host, &volumes::volume_owners(desired, &snapshot.records)).await,
        checkpoints: checkpoints::observe_checkpoints(host, desired).await,
        exports: exports::observe_exports(host, desired).await,
    }
}

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

async fn apply_sleeps(host: &Host, plan: &ReconcilePlan) {
    for action in &plan.instances {
        if let InstancePlan::Sleep { desired } = action {
            instances::sleep_instance(host, desired).await;
        }
    }
}

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

pub async fn reconcile(host: &Arc<Host>, desired: &HostDesiredState) {
    let observed = observe(host, desired).await;
    let plan = plan_reconcile(desired, &observed);
    host.state
        .modify(|snapshot| snapshot.deferred_work = plan.has_deferred_work())
        .await;
    sync_desired(host, desired).await;

    let prefetch = instances::prefetch_layers(host, &plan);
    let stops = apply_stops(host, &plan);
    tokio::join!(prefetch, stops);

    volumes::apply_volumes(host, &plan, &observed, desired).await;
    apply_sleeps(host, &plan).await;
    network::apply_activators(host).await;
    network::apply_network(host).await;
    apply_starts(host, &plan).await;
    volumes::apply_teardowns(host, &plan).await;
    checkpoints::apply_checkpoints(host, &plan).await;
    exports::apply_exports(host, &plan).await;
    network::apply_activators(host).await;
    network::apply_network(host).await;
    network::apply_routes(host).await;
    converge::observe(host, crate::clock::now_ms()).await;
    host.persist().await;

    host.state.modify(|snapshot| snapshot.converged = true).await;
    host.state.signal_report();
}

pub async fn refresh(host: &Arc<Host>) {
    instances::refresh_states(host).await;
    converge::observe(host, crate::clock::now_ms()).await;
    network::apply_network(host).await;
    network::apply_routes(host).await;
    host.persist().await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::vm::VmStatus;
    use crate::ports::{VmCall, VmError};
    use crate::test_support::*;
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

    fn failed_vm() -> VmStatus {
        VmStatus {
            loaded: true,
            active: false,
            failed: true,
            started_this_boot: false,
            exit_code: None,
        }
    }

    fn running_app() -> protocol::HostDesiredState {
        desired_state(|state| {
            state.volumes = vec![desired_volume(|_| {})];
            state.instances = vec![desired_instance(|instance| {
                instance.hostnames = vec![app_hostname()]
            })];
        })
    }

    fn starting(desired: protocol::DesiredInstance) -> ReconcilePlan {
        ReconcilePlan {
            instances: vec![InstancePlan::Start { desired }],
            ..Default::default()
        }
    }

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
        assert_eq!(outcome, crate::ports::WakeOutcome::Restored);
        assert_eq!(host.vms.calls(), vec![VmCall::Wake]);
        assert!(host.state.record(&app_id()).await.unwrap().started_at.is_some());
    }

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
        assert_eq!(outcome, crate::ports::WakeOutcome::ColdBoot);
        assert_eq!(host.vms.calls(), vec![VmCall::Wake, VmCall::Boot]);
    }

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
        assert_eq!(outcome, crate::ports::WakeOutcome::AlreadyRunning);
        assert!(host.vms.calls().is_empty());
    }

    #[tokio::test]
    async fn an_app_that_has_gone_quiet_is_put_down_where_it_can_be_picked_up() {
        let host = test_host().await;
        host.slot_for(&app_id()).await.unwrap();
        host.state
            .put_record(instance_record(|record| record.on_request = true))
            .await;

        instances::suspend_instance(&host, &app_id(), crate::domain::activation::SleepReason::Quiet).await;

        assert_eq!(host.vms.calls(), vec![VmCall::Sleep]);
        let record = host.state.record(&app_id()).await.unwrap();
        assert_eq!(record.state, InstanceState::Idle);
        assert!(record.stop_requested);
        assert!(!host.state.snapshot().await.snapshotting.contains(&app_id()));
        let page = metrics_page(&host).await;
        assert!(page.contains("nibrunner_sleep_outcomes_total{reason=\"idle\",outcome=\"slept\"} 1\n"));
        assert!(page.contains("nibrunner_sleep_phase_seconds_count{phase=\"flush\",reason=\"idle\"} 1\n"));
        assert!(page.contains("nibrunner_sleep_phase_seconds_count{phase=\"total\",reason=\"idle\"} 1\n"));
        assert!(page.contains("nibrunner_instance_sleeps_total{app=\"app-1\",outcome=\"slept\"} 1\n"));
        assert!(
            page.contains("nibrunner_instance_last_sleep_seconds{app=\"app-1\"} 0."),
            "{page}"
        );
    }

    async fn metrics_page(host: &TestHost) -> String {
        crate::domain::metrics::tests::page(
            &crate::domain::metrics::tests::report(),
            &host.metrics,
            &host.state.snapshot().await,
            0,
        )
    }

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

        instances::suspend_instance(&host, &app_id(), crate::domain::activation::SleepReason::Quiet).await;

        assert_eq!(
            host.state.record(&app_id()).await.unwrap().state,
            InstanceState::Running
        );
        assert!(!host.state.snapshot().await.snapshotting.contains(&app_id()));
        let page = metrics_page(&host).await;
        assert!(page.contains("nibrunner_sleep_outcomes_total{reason=\"idle\",outcome=\"refused\"} 1\n"));
        assert!(
            page.contains("nibrunner_sleep_phase_seconds_count{phase=\"total\",reason=\"idle\"} 0\n"),
            "a sleep that did not happen took no time"
        );
    }

    #[tokio::test]
    async fn one_with_no_slot_to_come_back_to_is_stopped() {
        let host = test_host().await;
        host.state
            .put_record(instance_record(|record| record.on_request = true))
            .await;
        instances::suspend_instance(&host, &app_id(), crate::domain::activation::SleepReason::Quiet).await;
        assert_eq!(host.vms.calls(), vec![VmCall::Stop]);
    }

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
        assert!(record.message.unwrap().as_str().contains("used its 5 restarts"));
    }

    #[tokio::test]
    async fn one_pass_converges_a_host_onto_a_document_it_has_never_seen() {
        let _serial = ONE_HOST_AT_A_TIME.lock().await;
        let host = test_host().await;

        reconcile(host.arc(), &running_app()).await;

        assert!(host.slot_of(&app_id()).await.is_some());
        assert_eq!(host.vms.calls(), vec![VmCall::Boot]);
        let record = host.state.record(&app_id()).await.unwrap();
        assert_eq!(record.state, InstanceState::Starting);
        assert_eq!(record.deployment_id, deployment_id());
        assert!(host.state.snapshot().await.isolated);
        assert_eq!(
            host.router
                .routes()
                .await
                .port_for(app_hostname().hostname.as_str()),
            Some(record.host_port)
        );
        assert_eq!(host.repositories.instances.all().await.unwrap().len(), 1);
        assert_eq!(host.repositories.slots.all().await.unwrap().len(), 1);
    }

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

    #[tokio::test]
    async fn an_instance_whose_volume_is_not_here_does_not_boot() {
        let _serial = ONE_HOST_AT_A_TIME.lock().await;
        let host = test_host().await;
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
        let calls = host.vms.calls();
        let stopped = calls.iter().position(|call| *call == VmCall::Stop).unwrap();
        let booted = calls.iter().rposition(|call| *call == VmCall::Boot).unwrap();
        assert!(stopped < booted);
    }

    #[tokio::test]
    async fn a_microvm_this_host_adopted_without_a_record_is_still_observed() {
        let host = test_host().await;
        host.vms.set_adopted(vec![app_id()]);
        host.vms.set_status(running_vm());

        let observed = observe(&host, &desired_state(|_| {})).await;

        assert_eq!(observed.instances.len(), 1);
        assert_eq!(observed.instances[0].app_id, app_id());
        assert_eq!(observed.instances[0].volume_id, None);
        assert_eq!(observed.instances[0].deployment_id, None);
        assert!(observed.instances[0].present);
        assert!(observed.instances[0].running);
        assert!(!observed.instances[0].exited);
    }

    #[tokio::test]
    async fn a_record_with_nothing_loaded_is_still_present_so_the_plan_can_act_on_it() {
        let host = test_host().await;
        host.state.put_record(instance_record(|_| {})).await;

        let observed = observe(&host, &desired_state(|_| {})).await;

        assert_eq!(observed.instances.len(), 1);
        assert!(observed.instances[0].present);
        assert!(!observed.instances[0].running);
        assert_eq!(observed.instances[0].volume_id, Some(volume_id()));
        assert_eq!(observed.instances[0].deployment_id, Some(deployment_id()));
    }

    #[tokio::test]
    async fn a_guest_that_was_asked_to_stop_is_not_read_as_one_that_exited_on_its_own() {
        let host = test_host().await;
        host.vms.set_status(stopped_vm());
        host.state
            .put_record(instance_record(|record| {
                record.started_at = Some(observed_at());
                record.stop_requested = true;
            }))
            .await;
        assert!(!observe(&host, &desired_state(|_| {})).await.instances[0].exited);

        host.state
            .update_record(&app_id(), |record| record.stop_requested = false)
            .await;
        assert!(observe(&host, &desired_state(|_| {})).await.instances[0].exited);
    }

    #[tokio::test]
    async fn one_that_was_never_started_has_not_exited_either() {
        let host = test_host().await;
        host.vms.set_status(stopped_vm());
        host.state
            .put_record(instance_record(|record| record.started_at = None))
            .await;
        assert!(!observe(&host, &desired_state(|_| {})).await.instances[0].exited);
    }

    #[tokio::test]
    async fn the_document_is_what_an_existing_record_says_it_wants() {
        let host = test_host().await;
        host.state
            .put_record(instance_record(|record| {
                record.hostnames = vec![];
                record.on_request = false;
            }))
            .await;
        let desired = desired_state(|state| {
            state.instances = vec![desired_instance(|instance| {
                instance.desired_state = DesiredInstanceState::OnRequest;
                instance.hostnames = vec![app_hostname()];
            })]
        });

        sync_desired(&host, &desired).await;

        let record = host.state.record(&app_id()).await.unwrap();
        assert!(record.on_request);
        assert!(record.desired_running);
        assert_eq!(record.hostnames, vec![app_hostname()]);
    }

    #[tokio::test]
    async fn a_document_that_stops_an_app_says_so_on_the_record_without_stopping_anything() {
        let host = test_host().await;
        host.state.put_record(instance_record(|_| {})).await;
        let desired = desired_state(|state| {
            state.instances = vec![desired_instance(|instance| {
                instance.desired_state = DesiredInstanceState::Stopped
            })]
        });

        sync_desired(&host, &desired).await;

        let record = host.state.record(&app_id()).await.unwrap();
        assert!(!record.desired_running);
        assert!(!record.on_request);
        assert!(host.vms.calls().is_empty());
    }

    #[tokio::test]
    async fn a_document_naming_an_app_this_host_has_no_record_for_writes_nothing_down() {
        let host = test_host().await;
        sync_desired(&host, &running_app()).await;
        assert!(host.state.record(&app_id()).await.is_none());
    }

    #[tokio::test]
    async fn a_host_whose_isolation_ruleset_did_not_apply_starts_nothing() {
        let host = test_host().await;
        host.volumes.provision(&desired_volume(|_| {})).await.unwrap();

        apply_starts(&host, &starting(desired_instance(|_| {}))).await;

        assert!(host.vms.calls().is_empty());
        assert!(host.state.record(&app_id()).await.is_none());
    }

    #[tokio::test]
    async fn one_that_did_apply_it_starts_what_the_plan_names() {
        let host = test_host().await;
        host.volumes.provision(&desired_volume(|_| {})).await.unwrap();
        host.state.modify(|snapshot| snapshot.isolated = true).await;

        apply_starts(&host, &starting(desired_instance(|_| {}))).await;

        assert_eq!(host.vms.calls(), vec![VmCall::Boot]);
    }

    #[tokio::test]
    async fn a_plan_with_nothing_to_start_asks_nothing_of_the_ruleset() {
        let host = test_host().await;
        apply_starts(&host, &ReconcilePlan::default()).await;
        assert!(host.vms.calls().is_empty());
    }

    #[tokio::test]
    async fn an_instance_the_plan_forgets_is_discarded_and_its_record_dropped() {
        let host = test_host().await;
        host.state.put_record(instance_record(|_| {})).await;
        let plan = ReconcilePlan {
            instances: vec![InstancePlan::Forget { app_id: app_id() }],
            ..Default::default()
        };

        apply_stops(&host, &plan).await;

        assert_eq!(host.vms.calls(), vec![VmCall::Discard]);
        assert!(host.state.record(&app_id()).await.is_none());
    }

    #[tokio::test]
    async fn a_replacement_takes_the_old_microvm_down_before_the_record_is_dropped() {
        let host = test_host().await;
        host.state.put_record(instance_record(|_| {})).await;
        let plan = ReconcilePlan {
            instances: vec![InstancePlan::Replace {
                desired: desired_instance(|_| {}),
            }],
            ..Default::default()
        };

        apply_stops(&host, &plan).await;

        assert_eq!(host.vms.calls(), vec![VmCall::Stop, VmCall::Discard]);
        assert!(host.state.record(&app_id()).await.is_none());
    }

    #[tokio::test]
    async fn a_plan_that_leaves_an_instance_alone_touches_no_microvm() {
        let host = test_host().await;
        host.state.put_record(instance_record(|_| {})).await;
        let plan = ReconcilePlan {
            instances: vec![
                InstancePlan::None { app_id: app_id() },
                InstancePlan::Sleep {
                    desired: desired_instance(|_| {}),
                },
                InstancePlan::Start {
                    desired: desired_instance(|_| {}),
                },
            ],
            ..Default::default()
        };

        apply_stops(&host, &plan).await;

        assert!(host.vms.calls().is_empty());
        assert!(host.state.record(&app_id()).await.is_some());
    }

    #[tokio::test]
    async fn a_plan_that_puts_an_app_to_sleep_leaves_a_record_waiting_to_be_asked_for() {
        let host = test_host().await;
        let plan = ReconcilePlan {
            instances: vec![InstancePlan::Sleep {
                desired: desired_instance(|instance| {
                    instance.desired_state = DesiredInstanceState::OnRequest
                }),
            }],
            ..Default::default()
        };

        apply_sleeps(&host, &plan).await;

        let record = host.state.record(&app_id()).await.unwrap();
        assert_eq!(record.state, InstanceState::Idle);
        assert!(record.on_request);
        assert!(host.vms.calls().is_empty());
    }

    #[tokio::test]
    async fn a_second_pass_over_a_converged_host_boots_nothing_a_second_time() {
        let _serial = ONE_HOST_AT_A_TIME.lock().await;
        let host = test_host().await;
        reconcile(host.arc(), &running_app()).await;
        host.vms.set_status(running_vm());

        reconcile(host.arc(), &running_app()).await;

        assert_eq!(host.vms.calls(), vec![VmCall::Boot]);
        let snapshot = host.state.snapshot().await;
        assert!(snapshot.converged);
        assert!(!snapshot.deferred_work);
    }

    #[tokio::test]
    async fn a_removal_this_pass_could_not_finish_is_left_for_the_next_one() {
        let _serial = ONE_HOST_AT_A_TIME.lock().await;
        let host = test_host().await;
        host.state.put_record(instance_record(|_| {})).await;
        host.vms.set_status(running_vm());

        reconcile(
            host.arc(),
            &desired_state(|state| {
                state.volumes = vec![desired_volume(|volume| {
                    volume.desired_state = protocol::DesiredPresence::Absent
                })]
            }),
        )
        .await;

        let snapshot = host.state.snapshot().await;
        assert!(snapshot.deferred_work);
        assert!(snapshot.converged);
    }

    #[tokio::test]
    async fn a_refresh_publishes_the_routes_for_the_apps_this_host_holds() {
        let host = test_host().await;
        host.slot_for(&app_id()).await.unwrap();
        host.state.put_record(instance_record(|_| {})).await;

        refresh(host.arc()).await;

        let record = host.state.record(&app_id()).await.unwrap();
        assert_eq!(
            host.router
                .routes()
                .await
                .port_for(app_hostname().hostname.as_str()),
            Some(record.host_port)
        );
        assert_eq!(host.repositories.instances.all().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_microvm_the_host_never_started_is_not_asked_what_its_guest_said() {
        let host = test_host().await;
        host.vms.set_verdict("the tenant used its 5 restarts");
        host.vms.set_status(failed_vm());
        host.state.put_record(instance_record(|_| {})).await;

        refresh(host.arc()).await;

        let record = host.state.record(&app_id()).await.unwrap();
        assert_eq!(record.state, InstanceState::Failed);
        assert_eq!(
            record.message.unwrap().as_str(),
            "the microVM stopped without being asked to"
        );
    }
}
