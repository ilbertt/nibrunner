use crate::domain::reconcile::idle::{apply_sleep, record_activity};
use crate::host::Host;

#[cfg_attr(any(test, feature = "testing"), mockall::automock)]
#[async_trait::async_trait]
pub trait IdleService: Send + Sync {
    async fn record_activity(&self);
    async fn apply_sleep(&self);
}

pub struct HostIdle {
    host: std::sync::Arc<Host>,
}

impl HostIdle {
    pub fn new(host: std::sync::Arc<Host>) -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self { host })
    }
}

#[async_trait::async_trait]
impl IdleService for HostIdle {
    async fn record_activity(&self) {
        record_activity(&self.host).await;
    }

    async fn apply_sleep(&self) {
        apply_sleep(&self.host).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::reconcile::idle::DEFAULT_IDLE_TIMEOUT_MS;
    use crate::ports::VmCall;
    use crate::test_support::*;
    use protocol::InstanceState;
    use protocol::{AppId, DesiredInstanceState, IdleTimeoutMs};
    fn quiet_record() -> crate::domain::report::InstanceRecord {
        instance_record(|record| {
            record.on_request = true;
            record.desired_running = true;
            record.state = InstanceState::Running;
        })
    }
    fn other() -> AppId {
        AppId::parse("app-2").unwrap()
    }
    const EARLIER: i64 = 1_000;
    async fn on_request_host(idle_timeout_ms: Option<IdleTimeoutMs>) -> TestHost {
        let host = test_host().await;
        host.cache.lock().await.accept(desired_state(|state| {
            state.instances = vec![desired_instance(|instance| {
                instance.desired_state = DesiredInstanceState::OnRequest;
                instance.idle_timeout_ms = idle_timeout_ms;
            })]
        }));
        host.slot_for(&app_id()).await.unwrap();
        host.state.put_record(quiet_record()).await;
        host
    }
    async fn last_reached(host: &TestHost, ms_ago: i64) {
        let moment = crate::clock::now_ms() - ms_ago;
        host.state
            .modify(|snapshot| {
                snapshot.last_active_at_ms.insert(app_id(), moment);
            })
            .await;
    }
    #[tokio::test]
    async fn the_service_measures_the_host_it_was_built_on() {
        let host = test_host().await;
        host.slot_for(&app_id()).await.unwrap();
        host.state
            .modify(|snapshot| {
                snapshot.last_active_at_ms.insert(other(), EARLIER);
            })
            .await;

        HostIdle::new(host.arc().clone()).record_activity().await;

        assert!(!host
            .state
            .snapshot()
            .await
            .last_active_at_ms
            .contains_key(&other()));
    }
    #[tokio::test]
    async fn the_service_lets_the_quiet_apps_go_on_the_host_it_was_built_on() {
        let host = on_request_host(None).await;
        last_reached(&host, DEFAULT_IDLE_TIMEOUT_MS as i64 + 1).await;

        HostIdle::new(host.arc().clone()).apply_sleep().await;

        assert_eq!(host.vms.calls(), vec![VmCall::Sleep]);
    }
}
