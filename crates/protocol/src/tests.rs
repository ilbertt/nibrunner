use super::*;

fn instance_json() -> serde_json::Value {
    serde_json::json!({
        "appId": "app-1",
        "deploymentId": "dep-1",
        "volumeId": "vol-1",
        "desiredState": "on-request",
        "idleTimeoutMs": 300000,
        "artifact": {
            "digest": "a".repeat(64),
            "sizeBytes": 27,
            "objectKey": "artifacts/9f1c2f0e-0d4e-4a1b-9c3a-1f8b6d2e7a45",
            "filename": "pocketbase"
        },
        "config": {
            "httpPort": 3000,
            "hasExtraPublicPort": false,
            "args": ["serve"],
            "environment": { "DSN": "postgres://u:p@h/db", "PORT_HINT": "${NIBRUN_HTTP_PORT}" },
            "resources": { "vcpuCount": 1, "memoryMib": 256 },
            "healthCheck": { "intervalMs": 5000, "timeoutMs": 2000, "gracePeriodMs": 30000, "healthyThreshold": 1, "unhealthyThreshold": 3 },
            "restartPolicy": { "maxRestarts": 5, "initialBackoffMs": 500, "maxBackoffMs": 30000, "backoffFactor": 2, "resetAfterMs": 60000 }
        },
        "hostnames": [{ "hostname": "app-1.apps.example.com", "kind": "platform" }],
        "somethingNewer": true
    })
}

#[test]
fn a_desired_state_round_trips_with_its_wire_names() {
    let document = serde_json::json!({
        "hostId": "host-1",
        "volumes": [{ "volumeId": "vol-1", "appId": "app-1", "sizeBytes": 4096, "desiredState": "present" }],
        "instances": [instance_json()],
        "checkpoints": [],
        "exports": []
    });
    let parsed: HostDesiredState = serde_json::from_value(document).expect("parses");
    assert_eq!(parsed.instances[0].desired_state, DesiredInstanceState::OnRequest);
    assert_eq!(parsed.instances[0].idle_timeout_ms.map(|t| t.get()), Some(300_000));
    let written = serde_json::to_value(&parsed).expect("serialises");
    assert_eq!(written["instances"][0]["config"]["httpPort"], 3000);
    assert_eq!(written["instances"][0]["desiredState"], "on-request");
    assert!(written["instances"][0].get("somethingNewer").is_none());
}

#[test]
fn unknown_fields_are_tolerated_and_mistyped_ones_are_not() {
    let mut document = instance_json();
    document["config"]["httpPort"] = serde_json::json!("3000");
    assert!(serde_json::from_value::<DesiredInstance>(document).is_err());
}

#[test]
fn a_secret_never_prints_itself() {
    let secret = SecretString::parse("hunter2").unwrap();
    assert_eq!(format!("{secret:?}"), REDACTED);
    let environment: TenantEnvironment =
        [("KEY".to_string(), TenantValue::parse("hunter2").unwrap())].into_iter().collect();
    assert!(!format!("{environment:?}").contains("hunter2"));
}

#[test]
fn a_tenant_value_may_name_only_offered_runtime_values() {
    assert!(TenantValue::parse("$HOME and $$ and a bcrypt $2b$10$abc").is_ok());
    assert!(TenantValue::parse("http://x:${NIBRUN_HTTP_PORT}/").is_ok());
    assert!(TenantValue::parse("$NIBRUN_HTTP_PORT").is_ok());
    assert!(TenantValue::parse("$NIBRUN_HTTP_PORTS").is_err());
    assert!(TenantValue::parse("${NIBRUN_NOPE}").is_err());
    assert!(TenantValue::parse("${NIBRUN_HTTP_PORT").is_err());
    assert!(names_extra_public_port_values("${NIBRUN_PUBLIC_IPV4}:${NIBRUN_EXTRA_PUBLIC_PORT}"));
    assert!(!names_extra_public_port_values("${NIBRUN_HTTP_PORT}"));
}

#[test]
fn environment_names_follow_the_shell_rule_minus_one() {
    assert!(is_environment_name("_A1"));
    assert!(!is_environment_name("1A"));
    assert!(!is_environment_name("__proto__"));
    assert!(!is_environment_name("A-B"));
}

#[test]
fn identifiers_timestamps_and_addresses_are_checked() {
    assert!(AppId::parse("0198f3aa-1c2d-7e4b-9f11-a0b1c2d3e4f5").is_ok());
    assert!(AppId::parse("has.a.dot").is_err());
    assert!(AppId::parse("x".repeat(64)).is_err());
    assert!(Timestamp::parse("2026-08-03T10:00:00.000Z").is_ok());
    assert!(Timestamp::parse("2026-08-03T10:00:00+02:00").is_ok());
    assert!(Timestamp::parse("2026-08-03T10:00:00").is_err());
    assert_eq!(Timestamp::from_epoch_ms(1_785_751_200_000).as_str(), "2026-08-03T10:00:00.000Z");
    assert_eq!(Timestamp::parse("2026-08-03T10:00:00.000Z").unwrap().epoch_ms(), 1_785_751_200_000);
    assert!(Ipv4Address::parse("10.201.0.2").is_ok());
    assert!(Ipv4Address::parse("10.201.0.256").is_err());
    assert!(Ipv4Address::parse("01.2.3.4").is_err());
    assert!(Hostname::parse("app-1.apps.example.com").is_ok());
    assert!(Hostname::parse("localhost").is_err());
    assert!(Filename::parse("pocketbase").is_ok());
    assert!(Filename::parse("../x").is_err());
    assert!(Filename::parse("-flag").is_err());
    assert!(Sha256Digest::parse("A".repeat(64)).is_err());
    assert!(HttpPort::try_from(0u32).is_err());
    assert!(HttpPort::try_from(70000u32).is_err());
    assert!(GuestPath::parse("/a/b").is_ok());
    assert!(GuestPath::parse("/a/../b").is_err());
    assert!(GuestPath::parse("/it's").is_err());
    assert!(GuestPath::parse("/a/").is_err());
}

#[test]
fn a_report_omits_what_it_does_not_know() {
    let instance = ReportedInstance {
        app_id: AppId::parse("app-1").unwrap(),
        deployment_id: DeploymentId::parse("dep-1").unwrap(),
        state: InstanceState::Running,
        host_port: HostPort::new(21000).ok(),
        guest_ipv4: None,
        public_ipv4: None,
        extra_public_port: None,
        artifact_digest: None,
        restart_count: 0,
        started_at: None,
        last_healthy_at: None,
        last_exit_code: Some(0),
        compute: None,
        message: None,
    };
    let written = serde_json::to_value(&instance).unwrap();
    assert_eq!(written["lastExitCode"], 0);
    assert!(written.get("startedAt").is_none());
    assert!(written.get("message").is_none());
    assert_eq!(written["hostPort"], 21000);
}

#[test]
fn state_messages_are_cut_to_the_wire_ceiling() {
    let message = StateMessage::new("x".repeat(600));
    assert_eq!(message.as_str().len(), MAX_STATE_MESSAGE_LENGTH);
}

#[test]
fn a_filesystem_query_response_is_a_tagged_union() {
    let none: FilesystemQueryResponse = serde_json::from_str(r#"{"result":"none"}"#).unwrap();
    assert_eq!(none, FilesystemQueryResponse::None);
    let query: FilesystemQueryResponse = serde_json::from_str(
        r#"{"result":"query","query":{"queryId":"q1","appId":"app-1","path":"/"}}"#,
    )
    .unwrap();
    assert!(matches!(query, FilesystemQueryResponse::Query { .. }));
}
