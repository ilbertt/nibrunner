use protocol::DesiredInstance;

use crate::config::HostConfig;

/// Why this host cannot put the instance within reach, or nothing.
///
/// A host answers for what it was configured to answer for, and a document may ask for more than
/// that. Saying so is the whole of the check: an app left running behind a front door that was
/// never opened is reported healthy and reached by nobody, which is the one failure a level-
/// triggered daemon cannot converge its way out of.
pub fn ingress_refusal(desired: &DesiredInstance, config: &HostConfig) -> Option<String> {
    if let Err(invalid) = desired.config.validate_ports() {
        return Some(invalid.to_string());
    }

    let serves_http = config.proxy.http.is_some() || config.proxy.https.is_some();
    if !desired.hostnames.is_empty() && !serves_http {
        return Some(format!(
            "it answers for {} but this host runs no proxy, so nothing would reach it",
            desired.hostnames.len()
        ));
    }

    let streams = desired.config.ports.len();
    if streams > 0 && config.ingress.is_none() {
        return Some(format!(
            "it asks for {streams} port beside its HTTP one and this host names no [ingress] to bind them on"
        ));
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{HttpListener, IngressConfig, ProxyConfig};
    use crate::test_support::*;
    use protocol::{GuestPort, InstancePort, PortIngress, PortName};

    fn ssh_port() -> InstancePort {
        InstancePort {
            name: PortName::parse("ssh").unwrap(),
            guest_port: GuestPort::new(22).unwrap(),
            ingress: PortIngress::Tcp,
        }
    }

    fn config_with(proxy: ProxyConfig, ingress: Option<IngressConfig>) -> HostConfig {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let mut config = HostConfig::under(directory.path());
        config.proxy = proxy;
        config.ingress = ingress;
        std::mem::forget(directory);
        config
    }

    fn serving_http() -> ProxyConfig {
        ProxyConfig {
            http: Some(HttpListener { port: 8080 }),
            https: None,
        }
    }

    fn binding() -> Option<IngressConfig> {
        Some(IngressConfig {
            listen_address: std::net::Ipv4Addr::LOCALHOST.into(),
        })
    }

    fn answering_for_a_name() -> protocol::DesiredInstance {
        desired_instance(|instance| instance.hostnames = vec![app_hostname()])
    }

    #[test]
    fn an_app_with_a_hostname_on_a_host_that_runs_no_proxy_is_refused_by_name() {
        let refusal = ingress_refusal(
            &answering_for_a_name(),
            &config_with(ProxyConfig::default(), None),
        )
        .expect("nothing would have reached it");
        assert!(refusal.contains("no proxy"), "{refusal}");

        assert_eq!(
            ingress_refusal(
                &desired_instance(|instance| instance.hostnames = vec![]),
                &config_with(ProxyConfig::default(), None),
            ),
            None,
            "an app that answers for no name asks the proxy for nothing"
        );
        assert_eq!(
            ingress_refusal(&answering_for_a_name(), &config_with(serving_http(), None)),
            None
        );
    }

    #[test]
    fn a_stream_port_on_a_host_that_names_no_ingress_is_refused_by_name() {
        let wanting = desired_instance(|instance| instance.config.ports = vec![ssh_port()]);
        let refusal = ingress_refusal(&wanting, &config_with(serving_http(), None))
            .expect("nothing would have bound it");
        assert!(refusal.contains("[ingress]"), "{refusal}");
        assert_eq!(
            ingress_refusal(&wanting, &config_with(serving_http(), binding())),
            None
        );
    }

    #[test]
    fn a_document_naming_more_ports_than_an_app_may_have_is_refused_before_either_check() {
        let mut second = ssh_port();
        second.name = PortName::parse("other").unwrap();
        second.guest_port = GuestPort::new(9000).unwrap();
        let greedy = desired_instance(|instance| instance.config.ports = vec![ssh_port(), second]);

        let refusal = ingress_refusal(&greedy, &config_with(serving_http(), binding()))
            .expect("three ports is one more than an app may answer on");
        assert!(refusal.contains("at most"), "{refusal}");
    }

    #[test]
    fn a_port_named_or_numbered_twice_is_refused() {
        let duplicate_name =
            desired_instance(|instance| instance.config.ports = vec![ssh_port(), ssh_port()]);
        assert!(ingress_refusal(&duplicate_name, &config_with(serving_http(), binding())).is_some());

        let shadowing_http = desired_instance(|instance| {
            instance.config.ports = vec![InstancePort {
                name: PortName::parse("again").unwrap(),
                guest_port: GuestPort::new(instance.config.http_port.get()).unwrap(),
                ingress: PortIngress::Tcp,
            }];
        });
        let refusal = ingress_refusal(&shadowing_http, &config_with(serving_http(), binding()))
            .expect("one guest port cannot answer two ways");
        assert!(refusal.contains("claimed twice"), "{refusal}");
    }
}
