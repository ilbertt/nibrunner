//! What the host's networking is a function of: which apps are up, and which slots it holds.

use std::sync::Arc;

use nft_render::{FirewallState, ForwardedInstance};
use protocol::InstanceState;

use crate::host::Host;
use crate::proxy::RouteTable;
use crate::report::routes::renderable_routes;

/// Only a tenant that has answered is forwarded: a booted-but-dead VM must never take traffic.
/// The rule is the switch on the loopback port an app is reached by — with it the request is
/// rewritten to the guest before local delivery, and without it the daemon answers it instead.
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

/// A failed apply leaves whatever was already in the kernel, because `nft -f` replaces the table
/// in one transaction. Tearing down running tenants over a transient failure would be the bigger
/// outage — refusing to add new ones is the part that has to hold.
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

/// A listener on every port this host holds a slot for, so one with no forward still answers.
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

    async fn forwards_for(records: Vec<crate::report::InstanceRecord>) -> Vec<ForwardedInstance> {
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

    /// Absent rather than present-and-ignored: what renders the rules reads the field's presence,
    /// so an app that did not ask has to be indistinguishable from one that could not have.
    #[tokio::test]
    async fn an_app_is_forwarded_the_port_it_asked_for_and_no_other() {
        let asked = forwards_for(vec![instance_record(|record| {
            record.has_extra_public_port = Some(true)
        })])
        .await;
        assert_eq!(asked[0].extra_public_port.map(|port| port.get()), Some(22_000));
        let did_not = forwards_for(vec![instance_record(|_| {})]).await;
        assert_eq!(did_not[0].extra_public_port, None);
        // A record written before an app could ask for one says nothing, and reads as the no it meant.
        let older = forwards_for(vec![instance_record(|record| {
            record.has_extra_public_port = None
        })])
        .await;
        assert_eq!(older[0].extra_public_port, None);
    }
}
