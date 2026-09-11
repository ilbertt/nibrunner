use super::*;

fn instance_json() -> serde_json::Value {
    serde_json::json!({
        "appId": "app-1",
        "deploymentId": "dep-1",
        "volumeId": "vol-1",
        "desiredState": "on-request",
        "idleTimeoutMs": 300000,
        "layers": [
            {
                "kind": "filesystem",
                "digest": "b".repeat(64),
                "sizeBytes": 31457280,
                "objectKey": "layers/debian-apphost"
            },
            {
                "kind": "executable",
                "destinationPath": "/app/server",
                "digest": "a".repeat(64),
                "sizeBytes": 27,
                "objectKey": "artifacts/9f1c2f0e-0d4e-4a1b-9c3a-1f8b6d2e7a45"
            }
        ],
        "config": {
            "httpPort": 3000,
            "ports": [{ "name": "ssh", "guestPort": 22 }],
            "command": {
                "program": "/app/server",
                "args": ["serve"],
                "workingDirectory": "/app",
                "environment": { "DSN": "postgres://u:p@h/db", "PORT_HINT": "${NIBRUN_HTTP_PORT}" }
            },
            "resources": { "vcpuCount": 1, "memoryMib": 256 },
            "healthCheck": { "intervalMs": 5000, "timeoutMs": 2000, "gracePeriodMs": 30000, "healthyThreshold": 1, "unhealthyThreshold": 3 },
            "restartPolicy": { "maxRestarts": 5, "initialBackoffMs": 500, "maxBackoffMs": 30000, "backoffFactor": 2, "resetAfterMs": 60000 }
        },
        "hostnames": [{ "hostname": "app-1.apps.example.com", "kind": "platform" }],
        "somethingNewer": true
    })
}

fn desired_json() -> serde_json::Value {
    serde_json::json!({
        "hostId": "host-1",
        "volumes": [{ "volumeId": "vol-1", "appId": "app-1", "sizeBytes": 4096, "desiredState": "present" }],
        "instances": [instance_json()],
        "checkpoints": [],
        "exports": []
    })
}

#[test]
fn a_desired_state_round_trips_with_its_wire_names() {
    let parsed: HostDesiredState = serde_json::from_value(desired_json()).expect("parses");
    assert_eq!(parsed.instances[0].desired_state, DesiredInstanceState::OnRequest);
    assert_eq!(
        parsed.instances[0].idle_timeout_ms.map(|t| t.get()),
        Some(300_000)
    );
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
    let environment: TenantEnvironment = [("KEY".to_string(), TenantValue::parse("hunter2").unwrap())]
        .into_iter()
        .collect();
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
    assert_eq!(
        Timestamp::from_epoch_ms(1_785_751_200_000).as_str(),
        "2026-08-03T10:00:00.000Z"
    );
    assert_eq!(
        Timestamp::parse("2026-08-03T10:00:00.000Z").unwrap().epoch_ms(),
        1_785_751_200_000
    );
    assert!(Ipv4Address::parse("10.201.0.2").is_ok());
    assert!(Ipv4Address::parse("10.201.0.256").is_err());
    assert!(Ipv4Address::parse("01.2.3.4").is_err());
    assert!(Hostname::parse("app-1.apps.example.com").is_ok());
    assert!(Hostname::parse("localhost").is_err());
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
        layer_digests: Vec::new(),
        restart_count: 0,
        started_at: None,
        last_healthy_at: None,
        last_exit_code: Some(0),
        compute: None,
        meters: UsageMeters::default(),
        message: None,
    };
    let written = serde_json::to_value(&instance).unwrap();
    assert_eq!(written["lastExitCode"], 0);
    assert!(written.get("startedAt").is_none());
    assert!(written.get("message").is_none());
    assert_eq!(written["hostPort"], 21000);
    // An app that has used nothing has used nothing, which is a figure and not an absence.
    assert_eq!(written["meters"]["runningMs"], 0);
    assert_eq!(written["meters"]["rxBytes"], 0);
}

#[test]
fn a_report_written_before_anything_was_metered_still_reads_back() {
    let older = serde_json::json!({
        "appId": "app-1",
        "deploymentId": "dep-1",
        "state": "running",
        "restartCount": 0
    });
    let read: ReportedInstance = serde_json::from_value(older).unwrap();
    assert_eq!(read.meters, UsageMeters::default());
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
    let query: FilesystemQueryResponse =
        serde_json::from_str(r#"{"result":"query","query":{"queryId":"q1","appId":"app-1","path":"/"}}"#)
            .unwrap();
    assert!(matches!(query, FilesystemQueryResponse::Query { .. }));
}

fn instance_with(edit: impl FnOnce(&mut serde_json::Value)) -> serde_json::Value {
    let mut document = instance_json();
    document
        .as_object_mut()
        .expect("an object")
        .remove("idleTimeoutMs");
    edit(&mut document);
    document
}

fn read(document: serde_json::Value) -> Result<DesiredInstance, serde_json::Error> {
    serde_json::from_value(document)
}

#[test]
fn an_activation_policy_round_trips_with_its_wire_names() {
    let instance = read(instance_with(|document| {
        document["activation"] = serde_json::json!({
            "sleepWhen": { "kind": "traffic-idle", "timeoutMs": 900000 },
            "readyWhen": { "kind": "boot-completed" }
        });
    }))
    .expect("parses");

    let policy = instance.activation();
    assert_eq!(
        policy.sleep_when,
        SleepPolicy::TrafficIdle {
            timeout_ms: IdleTimeoutMs::try_from(900_000).unwrap()
        }
    );
    assert_eq!(policy.ready_when, ReadinessPolicy::BootCompleted);

    let written = serde_json::to_value(&instance).expect("serialises");
    assert_eq!(written["activation"]["sleepWhen"]["kind"], "traffic-idle");
    assert_eq!(written["activation"]["sleepWhen"]["timeoutMs"], 900_000);
    assert_eq!(written["activation"]["readyWhen"]["kind"], "boot-completed");
    assert!(written.get("idleTimeoutMs").is_none());
}

#[test]
fn a_readiness_left_out_is_the_one_every_instance_written_before_it_had() {
    let instance = read(instance_with(|document| {
        document["activation"] = serde_json::json!({ "sleepWhen": { "kind": "never" } });
    }))
    .expect("parses");
    assert_eq!(instance.activation().ready_when, ReadinessPolicy::PortAnswers);
    assert_eq!(instance.activation().sleep_when, SleepPolicy::Never);
}

#[test]
fn a_document_that_names_no_policy_means_what_it_meant_before_there_were_any() {
    let on_request = read(instance_with(|document| {
        document["desiredState"] = serde_json::json!("on-request");
    }))
    .expect("parses");
    assert_eq!(
        on_request.activation().sleep_when,
        SleepPolicy::TrafficIdle {
            timeout_ms: DEFAULT_IDLE_TIMEOUT
        }
    );

    let timed = read(instance_with(|document| {
        document["desiredState"] = serde_json::json!("on-request");
        document["idleTimeoutMs"] = serde_json::json!(900_000);
    }))
    .expect("parses");
    assert_eq!(
        timed.activation().sleep_when,
        SleepPolicy::TrafficIdle {
            timeout_ms: IdleTimeoutMs::try_from(900_000).unwrap()
        }
    );

    for state in ["running", "stopped"] {
        let kept_up = read(instance_with(|document| {
            document["desiredState"] = serde_json::json!(state);
        }))
        .expect("parses");
        assert_eq!(kept_up.activation().sleep_when, SleepPolicy::Never, "{state}");
    }
}

#[test]
fn a_document_that_says_when_an_instance_sleeps_twice_is_refused() {
    let refused = read(instance_with(|document| {
        document["idleTimeoutMs"] = serde_json::json!(900_000);
        document["activation"] = serde_json::json!({
            "sleepWhen": { "kind": "traffic-idle", "timeoutMs": 900000 }
        });
    }))
    .expect_err("both spellings are refused");
    assert!(refused.to_string().contains("name one"), "{refused}");
}

#[test]
fn only_an_on_request_instance_may_name_a_sleep_policy_that_fires() {
    for state in ["running", "stopped"] {
        let refused = read(instance_with(|document| {
            document["desiredState"] = serde_json::json!(state);
            document["activation"] = serde_json::json!({
                "sleepWhen": { "kind": "max-lifetime", "ttlMs": 3600000 }
            });
        }))
        .expect_err("nothing would wake it again");
        assert!(refused.to_string().contains("on-request"), "{state}: {refused}");

        assert!(
            read(instance_with(|document| {
                document["desiredState"] = serde_json::json!(state);
                document["activation"] = serde_json::json!({ "sleepWhen": { "kind": "never" } });
            }))
            .is_ok(),
            "{state} may still say it never sleeps"
        );
    }
}

#[test]
fn a_policy_this_host_does_not_offer_is_refused_by_name_rather_than_ignored() {
    assert!(read(instance_with(|document| {
        document["activation"] = serde_json::json!({ "sleepWhen": { "kind": "no-sessions" } });
    }))
    .is_err());

    assert!(read(instance_with(|document| {
        document["activation"] = serde_json::json!({
            "sleepWhen": { "kind": "never" },
            "readyWhen": { "kind": "agent-ready" }
        });
    }))
    .is_err());

    assert!(
        read(instance_with(|document| {
            document["activation"] = serde_json::json!({});
        }))
        .is_err(),
        "a policy that names no sleepWhen says nothing"
    );
}

#[test]
fn a_lifetime_outside_what_a_host_will_hold_is_refused() {
    assert!(MaxLifetimeMs::try_from(MIN_MAX_LIFETIME_MS).is_ok());
    assert!(MaxLifetimeMs::try_from(MAX_MAX_LIFETIME_MS).is_ok());
    assert!(MaxLifetimeMs::try_from(MIN_MAX_LIFETIME_MS - 1).is_err());
    assert!(MaxLifetimeMs::try_from(MAX_MAX_LIFETIME_MS + 1).is_err());

    let refused = read(instance_with(|document| {
        document["desiredState"] = serde_json::json!("on-request");
        document["activation"] = serde_json::json!({
            "sleepWhen": { "kind": "max-lifetime", "ttlMs": 1000 }
        });
    }))
    .expect_err("a second is not a lifetime a host will hold");
    assert!(refused.to_string().contains("ttlMs"), "{refused}");
}

mod schema {
    use super::*;

    fn validator(schema: schemars::Schema) -> jsonschema::Validator {
        let schema = schema.to_value();
        jsonschema::meta::validate(&schema).expect("a schema the draft accepts");
        jsonschema::validator_for(&schema).expect("a schema that compiles")
    }

    fn with(mut document: serde_json::Value, pointer: &str, value: serde_json::Value) -> serde_json::Value {
        let (parent, key) = pointer.rsplit_once('/').expect(pointer);
        match document.pointer_mut(parent).expect(parent) {
            serde_json::Value::Object(object) => object.insert(key.to_string(), value),
            serde_json::Value::Array(items) => items
                .get_mut(key.parse::<usize>().expect(key))
                .map(|slot| std::mem::replace(slot, value)),
            _ => panic!("{parent} holds neither an object nor an array"),
        };
        document
    }

    fn without(mut document: serde_json::Value, pointer: &str) -> serde_json::Value {
        let (parent, key) = pointer.rsplit_once('/').expect(pointer);
        document
            .pointer_mut(parent)
            .and_then(serde_json::Value::as_object_mut)
            .expect(parent)
            .remove(key);
        document
    }

    fn reported_json() -> serde_json::Value {
        let now = Timestamp::parse("2026-08-03T10:00:00.000Z").unwrap();
        let capacity = HostCapacity {
            vcpu_count: 8,
            memory_mib: 32_768,
            cache_bytes: 1 << 40,
        };
        let state = HostReportedState {
            host_id: HostId::parse("host-1").unwrap(),
            reported_at: now.clone(),
            state: HostState::Ready,
            capacity,
            allocatable: capacity,
            versions: HostVersions {
                agent: "0.1.0".into(),
                guest_image: "2026.8.3-1".into(),
                zerofs: "0.5.0".into(),
                firecracker: "1.12.0".into(),
            },
            volumes: vec![ReportedVolume {
                volume_id: VolumeId::parse("vol-1").unwrap(),
                app_id: AppId::parse("app-1").unwrap(),
                state: VolumeState::Ready,
                size_bytes: 4096,
                storage_prefix: Some(ObjectKey::parse("volumes/vol-1").unwrap()),
                device_path: Some("/dev/nbd0".into()),
                usage: Some(FilesystemUsage {
                    total_bytes: 4096,
                    used_bytes: 1024,
                    measured_at: now.clone(),
                }),
                message: Some(StateMessage::new("mounted")),
            }],
            instances: vec![ReportedInstance {
                app_id: AppId::parse("app-1").unwrap(),
                deployment_id: DeploymentId::parse("dep-1").unwrap(),
                state: InstanceState::Running,
                host_port: HostPort::new(21000).ok(),
                guest_ipv4: Some(Ipv4Address::parse("10.201.0.2").unwrap()),
                layer_digests: vec![
                    Sha256Digest::parse("b".repeat(64)).unwrap(),
                    Sha256Digest::parse("a".repeat(64)).unwrap(),
                ],
                restart_count: 1,
                started_at: Some(now.clone()),
                last_healthy_at: Some(now.clone()),
                last_exit_code: Some(0),
                compute: Some(ComputeUsage {
                    memory_total_bytes: 268_435_456,
                    memory_used_bytes: 1_048_576,
                    cpu_share: Some(0.25),
                    measured_at: now.clone(),
                }),
                meters: UsageMeters {
                    running_ms: 60_000,
                    idle_ms: 30_000,
                    cpu_ms: 12_000,
                    rx_bytes: 4096,
                    tx_bytes: 8192,
                    disk_provisioned_mib_seconds: 368_640,
                    disk_used_mib_seconds: 90,
                },
                message: Some(StateMessage::new("healthy")),
            }],
            checkpoints: vec![ReportedCheckpoint {
                checkpoint_id: CheckpointId::parse("ckpt-1").unwrap(),
                volume_id: VolumeId::parse("vol-1").unwrap(),
                state: CheckpointState::Ready,
                reference: Some(StateMessage::new("snap-1")),
                ready_at: Some(now.clone()),
                message: None,
            }],
            exports: vec![ReportedExport {
                export_id: ExportId::parse("exp-1").unwrap(),
                checkpoint_id: Some(CheckpointId::parse("ckpt-1").unwrap()),
                state: ExportState::Ready,
                size_bytes: Some(2048),
                ready_at: Some(now),
                message: None,
            }],
        };
        serde_json::to_value(state).unwrap()
    }

    fn activated(policy: serde_json::Value) -> serde_json::Value {
        with(
            without(desired_json(), "/instances/0/idleTimeoutMs"),
            "/instances/0/activation",
            policy,
        )
    }

    #[test]
    fn the_desired_state_schema_accepts_what_the_parser_accepts() {
        let validator = validator(crate::schema::desired_state());
        let accepted = [
            desired_json(),
            activated(serde_json::json!({
                "sleepWhen": { "kind": "max-lifetime", "ttlMs": 3600000 },
                "readyWhen": { "kind": "boot-completed" }
            })),
            activated(serde_json::json!({ "sleepWhen": { "kind": "traffic-idle", "timeoutMs": 900000 } })),
            with(
                activated(serde_json::json!({ "sleepWhen": { "kind": "never" } })),
                "/instances/0/desiredState",
                serde_json::json!("running"),
            ),
        ];
        for document in accepted {
            serde_json::from_value::<HostDesiredState>(document.clone()).unwrap();
            let errors: Vec<String> = validator.iter_errors(&document).map(|e| e.to_string()).collect();
            assert!(errors.is_empty(), "{errors:#?}");
        }
    }

    #[test]
    fn the_desired_state_schema_refuses_what_the_parser_refuses() {
        let validator = validator(crate::schema::desired_state());
        let sixty_five = serde_json::Value::Array(vec![serde_json::json!("x"); MAX_ARGUMENTS + 1]);
        let nine_layers =
            serde_json::Value::Array(vec![instance_json()["layers"][0].clone(); MAX_LAYERS + 1]);
        let broken = [
            with(desired_json(), "/hostId", serde_json::json!("has.a.dot")),
            with(
                desired_json(),
                "/instances/0/appId",
                serde_json::json!("x".repeat(64)),
            ),
            with(
                desired_json(),
                "/instances/0/desiredState",
                serde_json::json!("asleep"),
            ),
            with(
                desired_json(),
                "/instances/0/idleTimeoutMs",
                serde_json::json!(10),
            ),
            with(
                desired_json(),
                "/instances/0/layers/0/digest",
                serde_json::json!("A".repeat(64)),
            ),
            with(
                desired_json(),
                "/instances/0/layers/0/objectKey",
                serde_json::json!(""),
            ),
            with(
                desired_json(),
                "/instances/0/layers/0/kind",
                serde_json::json!("binary"),
            ),
            without(desired_json(), "/instances/0/layers/0/kind"),
            without(desired_json(), "/instances/0/layers/1/digest"),
            without(desired_json(), "/instances/0/layers/1/destinationPath"),
            with(
                desired_json(),
                "/instances/0/layers/1/destinationPath",
                serde_json::json!("app/server"),
            ),
            with(
                desired_json(),
                "/instances/0/layers/1/destinationPath",
                serde_json::json!("/"),
            ),
            with(
                desired_json(),
                "/instances/0/layers/1/destinationPath",
                serde_json::json!("/sbin/init"),
            ),
            with(desired_json(), "/instances/0/layers", serde_json::json!([])),
            with(desired_json(), "/instances/0/layers", nine_layers),
            with(
                desired_json(),
                "/instances/0/config/httpPort",
                serde_json::json!(0),
            ),
            with(
                desired_json(),
                "/instances/0/config/httpPort",
                serde_json::json!(70_000),
            ),
            with(
                desired_json(),
                "/instances/0/config/httpPort",
                serde_json::json!("3000"),
            ),
            with(desired_json(), "/instances/0/config/command/args", sixty_five),
            without(desired_json(), "/instances/0/config/command/program"),
            with(
                desired_json(),
                "/instances/0/config/command/program",
                serde_json::json!("app/server"),
            ),
            without(desired_json(), "/instances/0/config/command/workingDirectory"),
            with(
                desired_json(),
                "/instances/0/config/command/workingDirectory",
                serde_json::json!("/app/"),
            ),
            with(
                desired_json(),
                "/instances/0/config/command/environment",
                serde_json::json!({ "1A": "x" }),
            ),
            with(
                desired_json(),
                "/instances/0/config/command/environment",
                serde_json::json!({ "__proto__": "x" }),
            ),
            with(
                desired_json(),
                "/instances/0/config/ports/0/name",
                serde_json::json!("SSH"),
            ),
            with(
                desired_json(),
                "/instances/0/config/ports/0/guestPort",
                serde_json::json!(0),
            ),
            with(
                desired_json(),
                "/instances/0/hostnames/0/hostname",
                serde_json::json!("localhost"),
            ),
            with(
                desired_json(),
                "/instances/0/hostnames/0/kind",
                serde_json::json!("vanity"),
            ),
            with(desired_json(), "/volumes/0/sizeBytes", serde_json::json!(-1)),
            with(
                desired_json(),
                "/volumes/0/desiredState",
                serde_json::json!("gone"),
            ),
            without(desired_json(), "/instances/0/layers"),
            without(desired_json(), "/volumes/0/appId"),
            with(
                desired_json(),
                "/instances/0/activation",
                serde_json::json!({ "sleepWhen": { "kind": "traffic-idle", "timeoutMs": 900000 } }),
            ),
            with(
                activated(serde_json::json!({ "sleepWhen": { "kind": "max-lifetime", "ttlMs": 3600000 } })),
                "/instances/0/desiredState",
                serde_json::json!("running"),
            ),
            activated(serde_json::json!({ "sleepWhen": { "kind": "no-sessions" } })),
            activated(serde_json::json!({ "sleepWhen": { "kind": "max-lifetime", "ttlMs": 1000 } })),
            activated(
                serde_json::json!({ "sleepWhen": { "kind": "never" }, "readyWhen": { "kind": "prayer" } }),
            ),
        ];
        for document in broken {
            assert!(
                serde_json::from_value::<HostDesiredState>(document.clone()).is_err(),
                "the parser took {document}"
            );
            assert!(!validator.is_valid(&document), "the schema took {document}");
        }
    }

    // What the parser knows and JSON Schema has no words for: a value may only name the runtime
    // values the guest offers. A document the schema passes can still be refused for this.
    #[test]
    fn the_desired_state_schema_cannot_see_which_runtime_values_the_guest_offers() {
        let validator = validator(crate::schema::desired_state());
        let document = with(
            desired_json(),
            "/instances/0/config/command/environment",
            serde_json::json!({ "URL": "${NIBRUN_NOPE}" }),
        );
        assert!(serde_json::from_value::<HostDesiredState>(document.clone()).is_err());
        assert!(validator.is_valid(&document));
    }

    #[test]
    fn the_reported_state_schema_accepts_what_the_daemon_writes() {
        let validator = validator(crate::schema::reported_state());
        let document = reported_json();
        let errors: Vec<String> = validator.iter_errors(&document).map(|e| e.to_string()).collect();
        assert!(errors.is_empty(), "{errors:#?}");
    }

    #[test]
    fn the_reported_state_schema_refuses_what_the_parser_refuses() {
        let validator = validator(crate::schema::reported_state());
        let broken = [
            with(
                reported_json(),
                "/reportedAt",
                serde_json::json!("2026-08-03T10:00:00"),
            ),
            with(reported_json(), "/state", serde_json::json!("asleep")),
            with(reported_json(), "/instances/0/state", serde_json::json!("asleep")),
            with(
                reported_json(),
                "/instances/0/guestIpv4",
                serde_json::json!("01.2.3.4"),
            ),
            with(reported_json(), "/instances/0/hostPort", serde_json::json!(0)),
            with(
                reported_json(),
                "/instances/0/restartCount",
                serde_json::json!(-1),
            ),
            with(
                reported_json(),
                "/instances/0/meters/cpuMs",
                serde_json::json!(-1),
            ),
            with(reported_json(), "/volumes/0/state", serde_json::json!("lost")),
            without(reported_json(), "/capacity"),
        ];
        for document in broken {
            assert!(
                serde_json::from_value::<HostReportedState>(document.clone()).is_err(),
                "the parser took {document}"
            );
            assert!(!validator.is_valid(&document), "the schema took {document}");
        }
    }

    #[test]
    fn every_published_schema_is_named_by_its_id() {
        for (filename, schema) in crate::schema::all() {
            let id = schema.get("$id").and_then(serde_json::Value::as_str).unwrap();
            assert_eq!(id, format!("{}{filename}", crate::schema::SCHEMA_ID_BASE));
        }
    }
}
