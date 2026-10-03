use serde::Serialize;

#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct Distribution {
    count: usize,
    min_us: u64,
    p50_us: u64,
    p95_us: u64,
    p99_us: u64,
    max_us: u64,
}

fn distribution(samples: impl IntoIterator<Item = u64>) -> Distribution {
    let mut ordered: Vec<_> = samples.into_iter().collect();
    assert!(!ordered.is_empty(), "a distribution needs measurements");
    ordered.sort_unstable();
    let percentile = |percent: usize| ordered[(ordered.len() * percent).div_ceil(100) - 1];
    Distribution {
        count: ordered.len(),
        min_us: ordered[0],
        p50_us: percentile(50),
        p95_us: percentile(95),
        p99_us: percentile(99),
        max_us: ordered[ordered.len() - 1],
    }
}

#[cfg(target_os = "linux")]
mod machine {
    use std::io::Write;
    use std::time::Instant;

    use protocol::InstanceState;
    use serde::Serialize;

    use super::{distribution, Distribution};

    const DEFAULT_ROUNDS: u32 = 20;
    const IDLE_TIMEOUT_MS: u64 = 60_000;

    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct Measurement {
        round: u32,
        app_id: String,
        caller: u32,
        response_us: u64,
    }

    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct Wake {
        round: u32,
        app_id: String,
        first_response_us: u64,
    }

    #[derive(Clone, Copy, Serialize)]
    #[serde(rename_all = "camelCase")]
    struct Scenario {
        name: &'static str,
        apps: u32,
        callers_per_app: u32,
    }

    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct ScenarioReport {
        scenario: Scenario,
        rounds: u32,
        responses: Distribution,
        first_responses: Distribution,
        measurements: Vec<Measurement>,
        wakes: Vec<Wake>,
    }

    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct Report {
        version: &'static str,
        kernel: String,
        cpu: String,
        logical_cpus: usize,
        memory: String,
        scenarios: Vec<ScenarioReport>,
    }

    fn rounds() -> u32 {
        match std::env::var("NIBRUNNER_BENCHMARK_ROUNDS") {
            Ok(value) => value
                .parse::<u32>()
                .ok()
                .filter(|rounds| *rounds > 0)
                .expect("NIBRUNNER_BENCHMARK_ROUNDS must be a positive integer"),
            Err(std::env::VarError::NotPresent) => DEFAULT_ROUNDS,
            Err(error) => panic!("NIBRUNNER_BENCHMARK_ROUNDS could not be read: {error}"),
        }
    }

    async fn measure(scenario: Scenario, rounds: u32) -> ScenarioReport {
        let host = crate::host()
            .await
            .expect("wake benchmarks require NIBRUNNER_INTEGRATION=1");
        let apps: Vec<_> = (1..=scenario.apps)
            .map(|number| host.tenant(number).on_request(IDLE_TIMEOUT_MS))
            .collect();
        host.deploy(&apps).await;
        for app in &apps {
            host.until_state(&app.app_id, InstanceState::Running).await;
            let answer = host.get(app, "/remember").await.expect("the guest answers");
            assert_eq!(answer.status, 200, "{answer:?}");
            assert_eq!(answer.body, "1");
        }

        let mut measurements = Vec::new();
        let mut wakes = Vec::new();
        // Round zero warms the restore path and is excluded from the report.
        for round in 0..=rounds {
            for app in &apps {
                host.let_sleep(app).await;
            }
            let before: Vec<_> = apps
                .iter()
                .map(|app| host.host.metrics.sleep_wake.of(&app.app_id))
                .collect();
            let barrier = tokio::sync::Barrier::new((scenario.apps * scenario.callers_per_app) as usize);
            let batch_started = std::sync::OnceLock::new();
            let requests = apps.iter().flat_map(|app| {
                let host = &host;
                let barrier = &barrier;
                let batch_started = &batch_started;
                (0..scenario.callers_per_app).map(move |caller| async move {
                    barrier.wait().await;
                    let started = batch_started.get_or_init(Instant::now);
                    let answer = host
                        .get(app, "/remember")
                        .await
                        .expect("every request is answered");
                    let response_us =
                        u64::try_from(started.elapsed().as_micros()).expect("a request duration");
                    assert_eq!(answer.status, 200, "{answer:?}");
                    let remembered = answer
                        .body
                        .parse::<u64>()
                        .expect("the guest remembers its counter");
                    (
                        Measurement {
                            round,
                            app_id: app.app_id.to_string(),
                            caller,
                            response_us,
                        },
                        remembered,
                    )
                })
            });
            let answers = futures::future::join_all(requests).await;
            for (app, before) in apps.iter().zip(before) {
                let app_id = app.app_id.to_string();
                let app_answers: Vec<_> = answers
                    .iter()
                    .filter(|(measurement, _)| measurement.app_id == app_id)
                    .collect();
                let mut remembered: Vec<_> = app_answers.iter().map(|(_, counter)| *counter).collect();
                remembered.sort_unstable();
                let first = 2 + u64::from(round) * u64::from(scenario.callers_per_app);
                assert_eq!(
                    remembered,
                    (first..first + u64::from(scenario.callers_per_app)).collect::<Vec<_>>(),
                    "{} lost or duplicated its in-memory state",
                    app.app_id
                );
                let after = host.host.metrics.sleep_wake.of(&app.app_id);
                assert_eq!(
                    after.wakes[0],
                    before.wakes[0] + 1,
                    "one restore per app and round"
                );
                assert_eq!(after.wakes[1], before.wakes[1], "the wake never boots cold");
                if round > 0 {
                    wakes.push(Wake {
                        round,
                        app_id,
                        first_response_us: app_answers
                            .iter()
                            .map(|(measurement, _)| measurement.response_us)
                            .min()
                            .expect("each app had a caller"),
                    });
                }
            }
            if round > 0 {
                measurements.extend(answers.into_iter().map(|(measurement, _)| measurement));
            }
        }
        host.stop().await;
        ScenarioReport {
            scenario,
            rounds,
            responses: distribution(measurements.iter().map(|measurement| measurement.response_us)),
            first_responses: distribution(wakes.iter().map(|wake| wake.first_response_us)),
            measurements,
            wakes,
        }
    }

    fn proc_field(path: &str, field: &str) -> String {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|content| {
                content.lines().find_map(|line| {
                    let (key, value) = line.split_once(':')?;
                    (key.trim() == field).then(|| value.trim().to_string())
                })
            })
            .unwrap_or_else(|| "unavailable".to_string())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_benchmark_round_records_every_caller_and_one_restore_per_app() {
        if !std::env::var("NIBRUNNER_INTEGRATION").is_ok_and(|value| value == "1") {
            return;
        }
        let report = measure(
            Scenario {
                name: "smoke",
                apps: 2,
                callers_per_app: 2,
            },
            1,
        )
        .await;
        assert_eq!(report.responses.count, 4);
        assert_eq!(report.first_responses.count, 2);
        assert_eq!(report.measurements.len(), 4);
        assert_eq!(report.wakes.len(), 2);
        assert!(report
            .measurements
            .iter()
            .all(|measurement| measurement.round == 1));
        let json = serde_json::to_value(&report).expect("the report is JSON");
        assert_eq!(json["responses"]["count"], 4);
        assert_eq!(json["firstResponses"]["count"], 2);
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "measures real microVM wakes on an otherwise idle Linux test host"]
    async fn wake_latency_is_reported_for_single_and_concurrent_requests() {
        let scenarios = [
            Scenario {
                name: "single",
                apps: 1,
                callers_per_app: 1,
            },
            Scenario {
                name: "coalesced",
                apps: 1,
                callers_per_app: 32,
            },
            Scenario {
                name: "concurrent",
                apps: 4,
                callers_per_app: 8,
            },
        ];
        let rounds = rounds();
        let mut reports = Vec::new();
        for scenario in scenarios {
            reports.push(measure(scenario, rounds).await);
        }
        let report = Report {
            version: env!("CARGO_PKG_VERSION"),
            kernel: std::fs::read_to_string("/proc/sys/kernel/osrelease")
                .expect("the Linux kernel release")
                .trim()
                .to_string(),
            cpu: proc_field("/proc/cpuinfo", "model name"),
            logical_cpus: std::thread::available_parallelism().expect("the CPU count").get(),
            memory: proc_field("/proc/meminfo", "MemTotal"),
            scenarios: reports,
        };
        let json = serde_json::to_string_pretty(&report).expect("the benchmark report is JSON");
        if let Some(path) = std::env::var_os("NIBRUNNER_BENCHMARK_OUTPUT") {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)
                .expect("a new benchmark report file");
            writeln!(file, "{json}").expect("the benchmark report is written");
        }
        println!("{json}");
    }
}

#[cfg(not(target_os = "linux"))]
#[test]
#[ignore = "requires Linux, root and /dev/kvm"]
fn wake_latency_is_reported_for_single_and_concurrent_requests() {
    panic!("wake benchmarks require Linux, root, /dev/kvm and NIBRUNNER_INTEGRATION=1");
}

#[test]
fn percentiles_use_the_nearest_rank_in_the_sorted_measurements() {
    let report = distribution((1..=100).rev());
    assert_eq!(
        report,
        Distribution {
            count: 100,
            min_us: 1,
            p50_us: 50,
            p95_us: 95,
            p99_us: 99,
            max_us: 100,
        }
    );
}

#[test]
fn a_single_measurement_is_every_percentile() {
    let report = distribution([42]);
    assert_eq!(
        (
            report.min_us,
            report.p50_us,
            report.p95_us,
            report.p99_us,
            report.max_us
        ),
        (42, 42, 42, 42, 42)
    );
}

#[test]
fn a_small_sample_rounds_percentile_ranks_up() {
    let report = distribution([30, 10, 20]);
    assert_eq!((report.p50_us, report.p95_us, report.p99_us), (20, 30, 30));
}
