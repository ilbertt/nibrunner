//! The planner. Ported function for function from `apps/agent/src/lib/reconcile/plan.ts`.

use std::collections::{BTreeMap, BTreeSet};

use protocol::{
    AppId, CheckpointId, DeploymentId, DesiredCheckpoint, DesiredExport, DesiredInstance,
    DesiredInstanceState, DesiredPresence, DesiredVolume, ExportId, HostDesiredState, ObjectKey,
    VolumeId,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservedInstance {
    pub app_id: AppId,
    /// Absent for a microVM this daemon has no record of — a host that lost its state file.
    pub volume_id: Option<VolumeId>,
    pub deployment_id: Option<DeploymentId>,
    pub present: bool,
    pub running: bool,
    /// Stopped without being asked to: left alone, because rebooting would hide a broken deploy.
    pub exited: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservedVolume {
    pub volume_id: VolumeId,
    pub app_id: AppId,
    pub attached: bool,
    pub size_bytes: u64,
    pub storage_prefix: ObjectKey,
    pub device_path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservedCheckpoint {
    pub checkpoint_id: CheckpointId,
    pub volume_id: VolumeId,
}

/// Remembered rather than observed: the host holds write-only credentials on the export bucket,
/// so re-writing on doubt would re-upload a tenant's whole dataset on every daemon restart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservedExport {
    pub export_id: ExportId,
    pub written: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ObservedState {
    pub instances: Vec<ObservedInstance>,
    pub volumes: Vec<ObservedVolume>,
    pub checkpoints: Vec<ObservedCheckpoint>,
    pub exports: Vec<ObservedExport>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstanceStopReason {
    DesiredStopped,
    NotDesired,
    Superseded,
    Idle,
}

impl InstanceStopReason {
    pub fn as_str(self) -> &'static str {
        match self {
            InstanceStopReason::DesiredStopped => "desired-stopped",
            InstanceStopReason::NotDesired => "not-desired",
            InstanceStopReason::Superseded => "superseded",
            InstanceStopReason::Idle => "idle",
        }
    }
}

/// `Sleep` is not `Stop`: it says this app should be here, holding its port and its hostnames,
/// with no microVM until something asks for one. What it produces is the record and the slot a
/// request needs to find, which is why an app nobody has ever visited still has to be planned.
#[derive(Debug, Clone, PartialEq)]
pub enum InstancePlan {
    Start { desired: DesiredInstance },
    Replace { desired: DesiredInstance },
    Sleep { desired: DesiredInstance },
    Stop { app_id: AppId, reason: InstanceStopReason },
    Forget { app_id: AppId },
    None { app_id: AppId },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VolumePlan {
    Provision { desired: DesiredVolume },
    Teardown { desired: DesiredVolume },
    Blocked { desired: DesiredVolume, blocked_by: Vec<AppId> },
    None { volume_id: VolumeId },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckpointPlan {
    Create { desired: DesiredCheckpoint },
    Delete { desired: DesiredCheckpoint },
    None { checkpoint_id: CheckpointId },
}

/// `Forget`, not `Delete`: expiry is the bucket's lifecycle rule, which is what makes it
/// unforgettable.
#[derive(Debug, Clone, PartialEq)]
pub enum ExportPlan {
    Write { desired: DesiredExport },
    Forget { export_id: ExportId },
    None { export_id: ExportId },
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct ReconcilePlan {
    pub instances: Vec<InstancePlan>,
    pub volumes: Vec<VolumePlan>,
    pub checkpoints: Vec<CheckpointPlan>,
    pub exports: Vec<ExportPlan>,
}

impl ReconcilePlan {
    /// A blocked teardown converges on a later pass, and nothing else would run one: only desired
    /// state moving triggers a reconcile, and deferred work does not move it.
    pub fn has_deferred_work(&self) -> bool {
        self.volumes.iter().any(|action| matches!(action, VolumePlan::Blocked { .. }))
    }
}

/// The asymmetry is the point. `instances` is authoritative, so a microVM the control plane does
/// not mention is stopped. `volumes` is not: removal is only ever an explicit `absent`, because a
/// truncated response must not be able to destroy a tenant's filesystem.
pub fn plan_reconcile(desired: &HostDesiredState, observed: &ObservedState) -> ReconcilePlan {
    ReconcilePlan {
        instances: plan_instances(desired, observed),
        volumes: plan_volumes(desired, observed),
        checkpoints: plan_checkpoints(desired, observed),
        exports: plan_exports(desired, observed),
    }
}

fn plan_instance(wanted: &DesiredInstance, current: Option<&ObservedInstance>) -> InstancePlan {
    if wanted.desired_state == DesiredInstanceState::Stopped {
        return if current.is_some_and(|instance| instance.running) {
            InstancePlan::Stop { app_id: wanted.app_id.clone(), reason: InstanceStopReason::DesiredStopped }
        } else {
            InstancePlan::None { app_id: wanted.app_id.clone() }
        };
    }
    // An app that runs on request is reconciled like any other up to the last line: a release this
    // host has not served yet comes up rather than waiting to be asked for, because a deploy is
    // the one moment an owner is watching and the only one where the health check runs at all —
    // an app first booted by a visitor reports a broken binary to them. Sleep is what a release
    // that has already had its microVM does between requests.
    if wanted.desired_state == DesiredInstanceState::OnRequest {
        let Some(current) = current.filter(|instance| instance.present) else {
            return InstancePlan::Start { desired: wanted.clone() };
        };
        if current.deployment_id.as_ref() != Some(&wanted.deployment_id) {
            return InstancePlan::Replace { desired: wanted.clone() };
        }
        return if current.running {
            InstancePlan::None { app_id: wanted.app_id.clone() }
        } else {
            InstancePlan::Sleep { desired: wanted.clone() }
        };
    }
    let Some(current) = current.filter(|instance| instance.present) else {
        return InstancePlan::Start { desired: wanted.clone() };
    };
    if current.deployment_id.as_ref() != Some(&wanted.deployment_id) {
        return InstancePlan::Replace { desired: wanted.clone() };
    }
    if current.running {
        return InstancePlan::None { app_id: wanted.app_id.clone() };
    }
    if current.exited {
        InstancePlan::None { app_id: wanted.app_id.clone() }
    } else {
        InstancePlan::Start { desired: wanted.clone() }
    }
}

fn plan_instances(desired: &HostDesiredState, observed: &ObservedState) -> Vec<InstancePlan> {
    let observed_by_id: BTreeMap<&AppId, &ObservedInstance> =
        observed.instances.iter().map(|instance| (&instance.app_id, instance)).collect();
    let desired_ids: BTreeSet<&AppId> = desired.instances.iter().map(|instance| &instance.app_id).collect();
    let mut plans: Vec<InstancePlan> = desired
        .instances
        .iter()
        .map(|wanted| plan_instance(wanted, observed_by_id.get(&wanted.app_id).copied()))
        .collect();
    plans.extend(
        observed
            .instances
            .iter()
            .filter(|current| !desired_ids.contains(&current.app_id))
            .map(|current| {
                if current.running {
                    InstancePlan::Stop { app_id: current.app_id.clone(), reason: InstanceStopReason::NotDesired }
                } else {
                    InstancePlan::Forget { app_id: current.app_id.clone() }
                }
            }),
    );
    plans
}

fn plan_volumes(desired: &HostDesiredState, observed: &ObservedState) -> Vec<VolumePlan> {
    let observed_by_id: BTreeMap<&VolumeId, &ObservedVolume> =
        observed.volumes.iter().map(|volume| (&volume.volume_id, volume)).collect();
    let mut used_by: BTreeMap<VolumeId, Vec<AppId>> = BTreeMap::new();
    for instance in &observed.instances {
        if !instance.present {
            continue;
        }
        let Some(volume_id) = &instance.volume_id else {
            continue;
        };
        used_by.entry(volume_id.clone()).or_default().push(instance.app_id.clone());
    }
    for instance in &desired.instances {
        used_by.entry(instance.volume_id.clone()).or_default();
    }

    desired
        .volumes
        .iter()
        .map(|wanted| {
            let current = observed_by_id.get(&wanted.volume_id).copied();
            if wanted.desired_state == DesiredPresence::Absent {
                let holders = used_by.get(&wanted.volume_id).cloned().unwrap_or_default();
                if !holders.is_empty() {
                    return VolumePlan::Blocked { desired: wanted.clone(), blocked_by: holders };
                }
                return match current {
                    Some(_) => VolumePlan::Teardown { desired: wanted.clone() },
                    None => VolumePlan::None { volume_id: wanted.volume_id.clone() },
                };
            }
            match current {
                Some(volume) if volume.attached && volume.size_bytes >= wanted.size_bytes => {
                    VolumePlan::None { volume_id: wanted.volume_id.clone() }
                }
                _ => VolumePlan::Provision { desired: wanted.clone() },
            }
        })
        .collect()
}

/// Not a diff against reality: the host cannot see the bucket it writes to, so a bundle already
/// written is left alone even if the object has since expired underneath it.
///
/// Authoritative, like `instances` and unlike `volumes`: an export this end remembers and desired
/// state does not mention is dropped. Safe to be authoritative about because forgetting costs a
/// re-upload at worst, where forgetting a volume would cost the tenant's data.
fn plan_exports(desired: &HostDesiredState, observed: &ObservedState) -> Vec<ExportPlan> {
    let written_ids: BTreeSet<&ExportId> =
        observed.exports.iter().filter(|current| current.written).map(|current| &current.export_id).collect();
    let desired_ids: BTreeSet<&ExportId> = desired.exports.iter().map(|wanted| &wanted.export_id).collect();
    let mut plans: Vec<ExportPlan> = desired
        .exports
        .iter()
        .map(|wanted| {
            if wanted.desired_state == DesiredPresence::Absent {
                return ExportPlan::Forget { export_id: wanted.export_id.clone() };
            }
            if written_ids.contains(&wanted.export_id) {
                ExportPlan::None { export_id: wanted.export_id.clone() }
            } else {
                ExportPlan::Write { desired: wanted.clone() }
            }
        })
        .collect();
    plans.extend(
        observed
            .exports
            .iter()
            .filter(|current| !desired_ids.contains(&current.export_id))
            .map(|current| ExportPlan::Forget { export_id: current.export_id.clone() }),
    );
    plans
}

fn plan_checkpoints(desired: &HostDesiredState, observed: &ObservedState) -> Vec<CheckpointPlan> {
    let observed_ids: BTreeSet<&CheckpointId> =
        observed.checkpoints.iter().map(|checkpoint| &checkpoint.checkpoint_id).collect();
    desired
        .checkpoints
        .iter()
        .map(|wanted| {
            let exists = observed_ids.contains(&wanted.checkpoint_id);
            match (wanted.desired_state, exists) {
                (DesiredPresence::Present, true) => CheckpointPlan::None { checkpoint_id: wanted.checkpoint_id.clone() },
                (DesiredPresence::Present, false) => CheckpointPlan::Create { desired: wanted.clone() },
                (DesiredPresence::Absent, true) => CheckpointPlan::Delete { desired: wanted.clone() },
                (DesiredPresence::Absent, false) => CheckpointPlan::None { checkpoint_id: wanted.checkpoint_id.clone() },
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::*;

    fn plan(desired: HostDesiredState, observed: ObservedState) -> ReconcilePlan {
        plan_reconcile(&desired, &observed)
    }

    mod instances_are_authoritative {
        use super::*;

        #[test]
        fn a_desired_instance_nothing_is_running_is_started() {
            let result = plan(desired_state(|state| state.instances = vec![desired_instance(|_| {})]), ObservedState::default());
            assert_eq!(result.instances, vec![InstancePlan::Start { desired: desired_instance(|_| {}) }]);
        }

        #[test]
        fn a_running_instance_the_control_plane_does_not_mention_is_stopped() {
            let result = plan(desired_state(|_| {}), observed_state(|state| state.instances = vec![observed_instance(|_| {})]));
            assert_eq!(
                result.instances,
                vec![InstancePlan::Stop { app_id: app_id(), reason: InstanceStopReason::NotDesired }]
            );
        }

        #[test]
        fn a_stopped_instance_the_control_plane_does_not_mention_is_forgotten() {
            let result = plan(
                desired_state(|_| {}),
                observed_state(|state| {
                    state.instances = vec![observed_instance(|instance| {
                        instance.volume_id = None;
                        instance.deployment_id = None;
                        instance.running = false;
                        instance.exited = true;
                    })]
                }),
            );
            assert_eq!(result.instances, vec![InstancePlan::Forget { app_id: app_id() }]);
        }

        #[test]
        fn a_deployment_change_replaces_rather_than_restarts() {
            let result = plan(
                desired_state(|state| state.instances = vec![desired_instance(|_| {})]),
                observed_state(|state| {
                    state.instances = vec![observed_instance(|instance| {
                        instance.deployment_id = Some(DeploymentId::parse("dep-0").unwrap())
                    })]
                }),
            );
            assert!(matches!(result.instances[0], InstancePlan::Replace { .. }));
        }

        #[test]
        fn a_microvm_with_no_record_is_treated_as_a_mismatch_not_as_converged() {
            let result = plan(
                desired_state(|state| state.instances = vec![desired_instance(|_| {})]),
                observed_state(|state| {
                    state.instances = vec![observed_instance(|instance| {
                        instance.volume_id = None;
                        instance.deployment_id = None;
                    })]
                }),
            );
            assert!(matches!(result.instances[0], InstancePlan::Replace { .. }));
        }

        #[test]
        fn a_vm_that_exited_on_its_own_is_not_booted_again() {
            let result = plan(
                desired_state(|state| state.instances = vec![desired_instance(|_| {})]),
                observed_state(|state| {
                    state.instances = vec![observed_instance(|instance| {
                        instance.running = false;
                        instance.exited = true;
                    })]
                }),
            );
            assert_eq!(result.instances, vec![InstancePlan::None { app_id: app_id() }]);
        }

        /// The shape a reboot produces: the daemon's records survive on disk, so the instance is
        /// still present and still wanted, but nothing has run since the host came up. Reading
        /// that as an exit leaves the host serving nothing until somebody logs in.
        #[test]
        fn a_vm_that_has_not_run_since_the_host_booted_is_started() {
            let result = plan(
                desired_state(|state| state.instances = vec![desired_instance(|_| {})]),
                observed_state(|state| {
                    state.instances = vec![observed_instance(|instance| {
                        instance.running = false;
                        instance.exited = false;
                    })]
                }),
            );
            assert_eq!(result.instances, vec![InstancePlan::Start { desired: desired_instance(|_| {}) }]);
        }

        #[test]
        fn desired_state_stopped_stops_a_running_instance_and_leaves_a_stopped_one_alone() {
            let stopped = || desired_instance(|instance| instance.desired_state = DesiredInstanceState::Stopped);
            let running = plan(
                desired_state(|state| state.instances = vec![stopped()]),
                observed_state(|state| {
                    state.instances = vec![observed_instance(|instance| {
                        instance.volume_id = None;
                        instance.deployment_id = None;
                    })]
                }),
            );
            assert_eq!(
                running.instances[0],
                InstancePlan::Stop { app_id: app_id(), reason: InstanceStopReason::DesiredStopped }
            );
            let already = plan(desired_state(|state| state.instances = vec![stopped()]), ObservedState::default());
            assert_eq!(already.instances, vec![InstancePlan::None { app_id: app_id() }]);
        }
    }

    /// A release comes up once so that somebody watches it come up, and sleeps from then on.
    mod an_app_that_runs_on_request {
        use super::*;

        fn on_request() -> DesiredInstance {
            desired_instance(|instance| instance.desired_state = DesiredInstanceState::OnRequest)
        }

        /// Where the health check happens: a release nothing has ever run is one nothing has ever
        /// checked, and leaving the first boot to a visitor reports a broken binary to them.
        #[test]
        fn one_this_host_has_never_served_is_started_rather_than_put_to_sleep() {
            let result = plan(desired_state(|state| state.instances = vec![on_request()]), ObservedState::default());
            assert_eq!(result.instances, vec![InstancePlan::Start { desired: on_request() }]);
        }

        #[test]
        fn one_already_sleeping_stays_that_way_however_many_times_this_runs() {
            let result = plan(
                desired_state(|state| state.instances = vec![on_request()]),
                observed_state(|state| {
                    state.instances = vec![observed_instance(|instance| {
                        instance.running = false;
                        instance.exited = false;
                    })]
                }),
            );
            assert_eq!(result.instances, vec![InstancePlan::Sleep { desired: on_request() }]);
        }

        /// Replaced rather than started: a start keeps the record it found, so the release the
        /// host reports would stay the one this deploy supersedes.
        #[test]
        fn a_deploy_onto_one_that_is_asleep_replaces_it_rather_than_leaving_it_asleep() {
            let result = plan(
                desired_state(|state| state.instances = vec![on_request()]),
                observed_state(|state| {
                    state.instances = vec![observed_instance(|instance| {
                        instance.running = false;
                        instance.exited = false;
                        instance.deployment_id = Some(DeploymentId::parse("dep-0").unwrap());
                    })]
                }),
            );
            assert_eq!(result.instances, vec![InstancePlan::Replace { desired: on_request() }]);
        }

        #[test]
        fn one_that_is_up_and_serving_the_release_it_should_be_is_left_alone() {
            let result = plan(
                desired_state(|state| state.instances = vec![on_request()]),
                observed_state(|state| state.instances = vec![observed_instance(|_| {})]),
            );
            assert_eq!(result.instances, vec![InstancePlan::None { app_id: app_id() }]);
        }

        #[test]
        fn one_that_is_up_on_an_older_release_is_replaced() {
            let result = plan(
                desired_state(|state| state.instances = vec![on_request()]),
                observed_state(|state| {
                    state.instances = vec![observed_instance(|instance| {
                        instance.deployment_id = Some(DeploymentId::parse("dep-0").unwrap())
                    })]
                }),
            );
            assert_eq!(result.instances, vec![InstancePlan::Replace { desired: on_request() }]);
        }
    }

    mod volumes_are_not_authoritative {
        use super::*;

        fn absent() -> DesiredVolume {
            desired_volume(|volume| volume.desired_state = DesiredPresence::Absent)
        }

        #[test]
        fn a_volume_missing_from_desired_state_is_left_completely_alone() {
            let result = plan(desired_state(|_| {}), observed_state(|state| state.volumes = vec![observed_volume(|_| {})]));
            assert_eq!(result.volumes, vec![]);
        }

        #[test]
        fn removal_requires_an_explicit_absent() {
            let result = plan(
                desired_state(|state| state.volumes = vec![absent()]),
                observed_state(|state| state.volumes = vec![observed_volume(|_| {})]),
            );
            assert!(matches!(result.volumes[0], VolumePlan::Teardown { .. }));
        }

        #[test]
        fn a_volume_still_held_by_an_instance_is_blocked_rather_than_destroyed() {
            let result = plan(
                desired_state(|state| state.volumes = vec![absent()]),
                observed_state(|state| {
                    state.volumes = vec![observed_volume(|_| {})];
                    state.instances = vec![observed_instance(|instance| {
                        instance.running = false;
                        instance.exited = true;
                    })];
                }),
            );
            assert_eq!(result.volumes[0], VolumePlan::Blocked { desired: absent(), blocked_by: vec![app_id()] });
            // Deleting an app takes two passes — stop the instance, then tear the volume down —
            // and only a generation change runs a pass. Without this the second one never comes.
            assert!(result.has_deferred_work());
        }

        #[test]
        fn a_plan_that_finished_everything_leaves_nothing_to_re_run_for() {
            let result = plan(
                desired_state(|state| state.volumes = vec![desired_volume(|_| {})]),
                observed_state(|state| state.volumes = vec![observed_volume(|_| {})]),
            );
            assert!(matches!(result.volumes[0], VolumePlan::None { .. }));
            assert!(!result.has_deferred_work());
        }

        #[test]
        fn an_unattached_volume_is_provisioned_and_a_grown_one_re_provisioned() {
            let missing = plan(desired_state(|state| state.volumes = vec![desired_volume(|_| {})]), ObservedState::default());
            assert!(matches!(missing.volumes[0], VolumePlan::Provision { .. }));
            let small = plan(
                desired_state(|state| {
                    state.volumes = vec![desired_volume(|volume| volume.size_bytes = VOLUME_SIZE_BYTES * 2)]
                }),
                observed_state(|state| state.volumes = vec![observed_volume(|_| {})]),
            );
            assert!(matches!(small.volumes[0], VolumePlan::Provision { .. }));
            let detached = plan(
                desired_state(|state| state.volumes = vec![desired_volume(|_| {})]),
                observed_state(|state| state.volumes = vec![observed_volume(|volume| volume.attached = false)]),
            );
            assert!(matches!(detached.volumes[0], VolumePlan::Provision { .. }));
        }
    }

    #[test]
    fn checkpoints_create_only_what_is_missing_and_delete_only_what_exists() {
        let held = observed_state(|state| {
            state.checkpoints = vec![ObservedCheckpoint { checkpoint_id: checkpoint_id(), volume_id: volume_id() }]
        });
        let create = plan(
            desired_state(|state| state.checkpoints = vec![desired_checkpoint(|_| {})]),
            ObservedState::default(),
        );
        assert!(matches!(create.checkpoints[0], CheckpointPlan::Create { .. }));
        let already = plan(
            desired_state(|state| state.checkpoints = vec![desired_checkpoint(|_| {})]),
            held.clone(),
        );
        assert!(matches!(already.checkpoints[0], CheckpointPlan::None { .. }));
        let remove = plan(
            desired_state(|state| {
                state.checkpoints =
                    vec![desired_checkpoint(|checkpoint| checkpoint.desired_state = DesiredPresence::Absent)]
            }),
            held,
        );
        assert!(matches!(remove.checkpoints[0], CheckpointPlan::Delete { .. }));
    }

    mod exports {
        use super::*;

        #[test]
        fn a_bundle_this_host_has_not_written_is_written_and_one_already_written_never_twice() {
            let fresh = plan(desired_state(|state| state.exports = vec![desired_export(|_| {})]), ObservedState::default());
            assert_eq!(fresh.exports, vec![ExportPlan::Write { desired: desired_export(|_| {}) }]);
            let written = plan(
                desired_state(|state| state.exports = vec![desired_export(|_| {})]),
                observed_state(|state| state.exports = vec![ObservedExport { export_id: export_id(), written: true }]),
            );
            assert!(matches!(written.exports[0], ExportPlan::None { .. }));
        }

        /// A failed export is remembered as a record but not as a bundle, so the next reconcile
        /// is what retries it.
        #[test]
        fn a_bundle_that_failed_is_retried() {
            let result = plan(
                desired_state(|state| state.exports = vec![desired_export(|_| {})]),
                observed_state(|state| state.exports = vec![ObservedExport { export_id: export_id(), written: false }]),
            );
            assert!(matches!(result.exports[0], ExportPlan::Write { .. }));
        }

        #[test]
        fn absent_forgets_the_record_rather_than_deleting_an_object_it_cannot_reach() {
            let result = plan(
                desired_state(|state| {
                    state.exports = vec![desired_export(|export| export.desired_state = DesiredPresence::Absent)]
                }),
                observed_state(|state| state.exports = vec![ObservedExport { export_id: export_id(), written: true }]),
            );
            assert_eq!(result.exports, vec![ExportPlan::Forget { export_id: export_id() }]);
        }

        /// `absent` only reaches a record the control plane still has. One it never had, or has
        /// lost, is a record nothing would ever withdraw.
        #[test]
        fn a_record_desired_state_does_not_mention_at_all_is_forgotten_too() {
            let result = plan(
                desired_state(|_| {}),
                observed_state(|state| state.exports = vec![ObservedExport { export_id: export_id(), written: true }]),
            );
            assert_eq!(result.exports, vec![ExportPlan::Forget { export_id: export_id() }]);
        }
    }
}
