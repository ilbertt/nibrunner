use protocol::{AppHostname, AppId, HostPort};

use crate::services::report::InstanceRecord;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteTarget {
    pub app_id: AppId,
    pub hostnames: Vec<AppHostname>,
    pub host_port: HostPort,
}

pub fn renderable_routes(records: &[InstanceRecord]) -> Vec<RouteTarget> {
    records
        .iter()
        .filter(|record| !record.hostnames.is_empty())
        .map(|record| RouteTarget {
            app_id: record.app_id.clone(),
            hostnames: record.hostnames.clone(),
            host_port: record.host_port,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{app_hostname, instance_record};
    use protocol::{InstanceState, INSTANCE_STATES};

    #[test]
    fn a_host_answers_for_the_apps_it_holds_not_the_ones_that_happen_to_be_up() {
        for state in INSTANCE_STATES
            .iter()
            .filter(|state| **state != InstanceState::Running)
        {
            assert_eq!(
                renderable_routes(&[instance_record(|record| record.state = *state)]).len(),
                1
            );
        }
        assert!(renderable_routes(&[instance_record(|record| record.hostnames = vec![])]).is_empty());
        let running = renderable_routes(&[instance_record(|_| {})]);
        let stopped = renderable_routes(&[instance_record(|record| record.state = InstanceState::Stopped)]);
        assert_eq!(running[0].host_port, stopped[0].host_port);
        assert_eq!(running[0].hostnames, vec![app_hostname()]);
    }
}
