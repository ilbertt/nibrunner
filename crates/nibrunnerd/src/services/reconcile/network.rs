use std::sync::Arc;

use nft_render::{FirewallState, ForwardedInstance};
use protocol::InstanceState;

use crate::adapters::proxy::RouteTable;
use crate::host::Host;
use crate::services::report::routes::renderable_routes;

pub async fn forwarded_instances(host: &Host) -> Vec<ForwardedInstance> {
    let mut forwarded = Vec::new();
    for record in host.state.records().await {
        if record.state != InstanceState::Running {
            continue;
        }
        let Some(slot) = host.slot_of(&record.app_id).await else {
            continue;
        };
        forwarded.push(ForwardedInstance {
            app_id: record.app_id.clone(),
            host_port: slot.host_port,
            http_port: record.http_port,
            extra_public_port: record.wants_extra_public_port().then_some(slot.extra_public_port),
            host_ipv4: slot.host_ipv4,
            guest_ipv4: slot.guest_ipv4,
        });
    }
    forwarded
}

pub async fn apply_network(host: &Host) {
    let state = FirewallState {
        instances: forwarded_instances(host).await,
        control_plane_cidrs_v4: host.config.control_plane_cidrs_v4.clone(),
        control_plane_cidrs_v6: host.config.control_plane_cidrs_v6.clone(),
    };
    match host.firewall.apply(&state).await {
        Ok(()) => host.state.modify(|snapshot| snapshot.isolated = true).await,
        Err(error) => {
            host.state.modify(|snapshot| snapshot.isolated = false).await;
            tracing::error!(error = %error.message(), "firewall apply failed");
        }
    }
}

pub async fn apply_activators(host: &Arc<Host>) {
    let slots: Vec<_> = host
        .slots()
        .await
        .into_iter()
        .map(|slot| (slot.app_id, slot.host_port))
        .collect();
    host.activator.serve(&slots).await;
}

pub async fn apply_routes(host: &Host) {
    let table = RouteTable::from_targets(&renderable_routes(&host.state.records().await));
    host.router.apply(table).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::*;
    use protocol::INSTANCE_STATES;

    async fn forwards_for(records: Vec<crate::services::report::InstanceRecord>) -> Vec<ForwardedInstance> {
        let host = crate::test_support::test_host().await;
        for record in records {
            host.slot_for(&record.app_id).await.unwrap();
            host.state.put_record(record).await;
        }
        forwarded_instances(&host).await
    }

    #[tokio::test]
    async fn the_forward_is_what_decides_whether_a_port_reaches_the_guest() {
        for state in INSTANCE_STATES
            .iter()
            .filter(|state| **state != InstanceState::Running)
        {
            let forwarded = forwards_for(vec![instance_record(|record| record.state = *state)]).await;
            assert!(forwarded.is_empty(), "{state:?} should not be forwarded");
        }
        let running = forwards_for(vec![instance_record(|_| {})]).await;
        assert_eq!(running.len(), 1);
        assert_eq!(running[0].app_id, app_id());
        assert_eq!(running[0].guest_ipv4.as_str(), "10.201.0.2");
        assert_eq!(running[0].http_port, protocol::DEFAULT_HTTP_PORT);
    }

    #[tokio::test]
    async fn a_host_holding_two_apps_and_running_one_forwards_only_that_one() {
        let forwarded = forwards_for(vec![
            instance_record(|record| record.state = InstanceState::Stopped),
            instance_record(|record| record.app_id = protocol::AppId::parse("app-2").unwrap()),
        ])
        .await;
        assert_eq!(forwarded.len(), 1);
    }

    #[tokio::test]
    async fn an_app_is_forwarded_the_port_it_asked_for_and_no_other() {
        let asked = forwards_for(vec![instance_record(|record| {
            record.has_extra_public_port = Some(true)
        })])
        .await;
        assert_eq!(asked[0].extra_public_port.map(|port| port.get()), Some(22_000));
        let did_not = forwards_for(vec![instance_record(|_| {})]).await;
        assert_eq!(did_not[0].extra_public_port, None);
        let older = forwards_for(vec![instance_record(|record| {
            record.has_extra_public_port = None
        })])
        .await;
        assert_eq!(older[0].extra_public_port, None);
    }

    #[tokio::test]
    async fn an_app_with_no_slot_is_not_forwarded_however_healthy_its_record_looks() {
        let host = test_host().await;
        host.state.put_record(instance_record(|_| {})).await;
        assert!(forwarded_instances(&host).await.is_empty());
    }

    #[tokio::test]
    async fn a_ruleset_that_loaded_is_what_lets_this_host_start_anything() {
        let host = test_host().await;
        apply_network(&host).await;
        assert!(host.state.snapshot().await.isolated);
    }

    #[tokio::test]
    async fn one_that_would_not_load_leaves_the_host_saying_it_is_not_isolated() {
        let mut host = test_host().await;
        let (commands, _log) = crate::test_support::mocks::commands_answering(|_| {
            Err(crate::ports::CommandError::Unstartable {
                executable: "nft".to_string(),
                reason: "it is not installed".to_string(),
            })
        });
        Arc::get_mut(&mut host.host)
            .expect("nothing else holds this host yet")
            .firewall = Arc::new(crate::adapters::net::firewall::HostFirewall::new(commands));
        host.state.modify(|snapshot| snapshot.isolated = true).await;

        apply_network(&host).await;

        assert!(!host.state.snapshot().await.isolated);
    }

    #[tokio::test]
    async fn the_apps_this_host_holds_slots_for_are_the_ones_it_answers_the_door_for() {
        let _serial = ONE_HOST_AT_A_TIME.lock().await;
        let host = test_host().await;
        host.slot_for(&app_id()).await.unwrap();

        apply_activators(host.arc()).await;
        assert_eq!(host.activator.listening_for().await, vec![app_id()]);

        host.allocator.lock().await.release(&app_id());
        apply_activators(host.arc()).await;
        assert!(host.activator.listening_for().await.is_empty());
    }

    #[tokio::test]
    async fn the_routes_a_pass_publishes_are_the_hostnames_of_the_records_it_holds() {
        let host = test_host().await;
        host.state.put_record(instance_record(|_| {})).await;

        apply_routes(&host).await;
        assert_eq!(
            host.router
                .routes()
                .await
                .port_for(app_hostname().hostname.as_str()),
            Some(instance_record(|_| {}).host_port)
        );

        host.state.drop_record(&app_id()).await;
        apply_routes(&host).await;
        assert!(host.router.routes().await.is_empty());
    }
}
