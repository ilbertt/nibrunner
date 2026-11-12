#![allow(clippy::unwrap_used, clippy::panic, clippy::expect_used)]

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use nibrunnerd::adapters::cron_worker::CronWorkerClient;
use nibrunnerd::config::HostConfig;
use nibrunnerd::test_support::cron_worker::{self, HoldingGuest};
use protocol::{TenantLogBody, TenantLogEvent};
use tokio::process::Command;

async fn start_worker(config: &HostConfig) -> (tokio::process::Child, PathBuf) {
    use tokio::io::AsyncWriteExt;
    let address = cron_worker::address(config);
    let mut worker = Command::new(env!("CARGO_BIN_EXE_nibrunnerd"))
        .arg("cron-worker")
        .arg("--socket")
        .arg(&address)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    {
        let mut input = worker.stdin.take().unwrap();
        input.write_all(config.to_toml().as_bytes()).await.unwrap();
        input.shutdown().await.unwrap();
    }
    while tokio::net::UnixStream::connect(&address).await.is_err() {
        assert!(
            worker.try_wait().unwrap().is_none(),
            "the standalone worker exited before serving"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    (worker, address)
}

#[tokio::test]
#[ignore = "subprocess fixture; the restart test invokes this with --ignored"]
async fn a_client_process_waits_for_its_scheduled_command() {
    let config = HostConfig::load().unwrap();
    CronWorkerClient::connect(&cron_worker::address(&config))
        .await
        .unwrap()
        .execute(&config.runtime_dir.join("guest.vsock"), cron_worker::run())
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_running_execution_keeps_its_guest_channel_and_logs_after_its_client_is_killed() {
    tokio::time::timeout(Duration::from_secs(90), async {
        let directory = tempfile::tempdir_in("/tmp").unwrap();
        let config = HostConfig::under(directory.path());
        let config_file = directory.path().join("config.toml");
        std::fs::write(&config_file, config.to_toml()).unwrap();
        let run = cron_worker::run();
        let mut guest = HoldingGuest::new(&config.runtime_dir.join("guest.vsock")).await;
        let (mut worker, address) = start_worker(&config).await;
        let mut client = Command::new(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "a_client_process_waits_for_its_scheduled_command",
                "--nocapture",
            ])
            .env("NIBRUNNER_CONFIG", &config_file)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        guest.started().await;
        assert_eq!(cron_worker::active(&address).await, 1);
        client.kill().await.unwrap();
        assert_eq!(cron_worker::active(&address).await, 1);
        assert!(worker.try_wait().unwrap().is_none());
        guest.finish().await;
        while cron_worker::active(&address).await != 0 {
            tokio::task::yield_now().await;
        }
        let events: Vec<TenantLogEvent> =
            std::fs::read_to_string(config.logs_dir().join(format!("{}.jsonl", run.key.app_id)))
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
        let output: Vec<_> = events
            .iter()
            .filter_map(|event| match &event.body {
                TenantLogBody::CronData { text, run_id, .. } => Some((text.as_str(), run_id)),
                _ => None,
            })
            .collect();
        assert_eq!(
            output.iter().map(|(text, _)| *text).collect::<Vec<_>>(),
            vec!["before\n", "after\n"]
        );
        assert_eq!(output[0].1, output[1].1);
        worker.kill().await.unwrap();
    })
    .await
    .expect("the worker preserves an execution across client process death");
}

#[tokio::test(flavor = "multi_thread")]
async fn stopping_one_app_cancels_its_run_while_another_apps_run_keeps_its_channel() {
    tokio::time::timeout(Duration::from_secs(90), async {
        let directory = tempfile::tempdir_in("/tmp").unwrap();
        let config = HostConfig::under(directory.path());
        let first_path = config.runtime_dir.join("first.vsock");
        let second_path = config.runtime_dir.join("second.vsock");
        let mut first_guest = HoldingGuest::new(&first_path).await;
        let mut second_guest = HoldingGuest::new(&second_path).await;
        let (mut worker, address) = start_worker(&config).await;
        let first_client = CronWorkerClient::connect(&address).await.unwrap();
        let first = tokio::spawn(async move { first_client.execute(&first_path, cron_worker::run()).await });
        let second_client = CronWorkerClient::connect(&address).await.unwrap();
        let mut second_run = cron_worker::run();
        second_run.key.app_id = protocol::AppId::parse("app-2").unwrap();
        let other_app = second_run.key.app_id.clone();
        let second = tokio::spawn(async move { second_client.execute(&second_path, second_run).await });
        first_guest.started().await;
        second_guest.started().await;
        let cancelled = first_guest.expect_cancellation();
        CronWorkerClient::connect(&address)
            .await
            .unwrap()
            .stop(&cron_worker::run().key.app_id)
            .await
            .unwrap();
        cancelled.await.unwrap();
        assert!(first.await.unwrap().is_err());
        assert_eq!(
            CronWorkerClient::connect(&address)
                .await
                .unwrap()
                .active(&other_app)
                .await
                .unwrap(),
            1
        );
        second_guest.finish().await;
        assert_eq!(second.await.unwrap().unwrap().code, 0);
        worker.kill().await.unwrap();
    })
    .await
    .expect("stopping one app leaves other apps' jobs running");
}

#[tokio::test]
#[ignore = "subprocess fixture; the daemon restart test invokes this with --ignored"]
async fn a_daemon_process_waits_for_its_detached_scheduled_command() {
    use nibrunnerd::adapters::cron_worker::process::DetachedCronExecution;
    use nibrunnerd::ports::CronExecution;
    let config = HostConfig::load().unwrap();
    let execution = DetachedCronExecution::new(&config, PathBuf::from(env!("CARGO_BIN_EXE_nibrunnerd")));
    execution
        .run(
            &config.runtime_dir.join("guest.vsock"),
            cron_worker::run(),
            Box::pin(std::future::pending()),
        )
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_binary_upgrade_keeps_the_old_run_alive_and_launches_new_work_without_replaying_it() {
    use nibrunnerd::adapters::cron_worker::process::DetachedCronExecution;
    use nibrunnerd::ports::CronExecution;
    tokio::time::timeout(Duration::from_secs(90), async {
        let directory = tempfile::tempdir_in("/tmp").unwrap();
        let config = HostConfig::under(directory.path());
        let config_file = directory.path().join("config.toml");
        std::fs::write(&config_file, config.to_toml()).unwrap();
        let mut guest = HoldingGuest::new(&config.runtime_dir.join("guest.vsock")).await;
        let diagnostics = std::fs::File::create(directory.path().join("daemon.stderr")).unwrap();
        let mut daemon = Command::new(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "a_daemon_process_waits_for_its_detached_scheduled_command",
                "--nocapture",
            ])
            .env("NIBRUNNER_CONFIG", &config_file)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(diagnostics))
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        guest.started().await;
        daemon.kill().await.unwrap();
        {
            use std::io::Write;
            let cache = config.state_dir.join("cron-worker");
            let mut replacement = tempfile::NamedTempFile::new_in(&cache).unwrap();
            replacement.write_all(b"replaced executable image").unwrap();
            replacement.persist(cache.join("nibrunnerd")).unwrap();
        }
        let successor = std::sync::Arc::new(DetachedCronExecution::new(
            &config,
            PathBuf::from(env!("CARGO_BIN_EXE_nibrunnerd")),
        ));
        let app = cron_worker::run().key.app_id;
        assert!(successor.busy(&app).await.unwrap());
        let second_path = config.runtime_dir.join("next.vsock");
        let mut next_guest = HoldingGuest::new(&second_path).await;
        let execution = successor.clone();
        let next = tokio::spawn(async move {
            execution
                .run(&second_path, cron_worker::run(), Box::pin(std::future::pending()))
                .await
        });
        next_guest.started().await;
        assert!(successor.busy(&app).await.unwrap());
        guest.finish().await;
        next_guest.finish().await;
        assert_eq!(next.await.unwrap().unwrap().code, 0);
        while successor.busy(&app).await.unwrap() {
            tokio::task::yield_now().await;
        }
        let text = std::fs::read_to_string(config.logs_dir().join(format!("{app}.jsonl"))).unwrap();
        let events: Vec<TenantLogEvent> = text
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let mut runs = std::collections::BTreeMap::new();
        for event in &events {
            if let TenantLogBody::CronData { text, run_id, .. } = &event.body {
                runs.entry(run_id).or_insert_with(Vec::new).push(text.as_str());
            }
        }
        assert_eq!(runs.len(), 2);
        assert!(runs.values().all(|output| output == &vec!["before\n", "after\n"]));
        while std::fs::read_dir(config.runtime_dir.join("cron"))
            .unwrap()
            .any(|entry| {
                entry
                    .unwrap()
                    .path()
                    .extension()
                    .is_some_and(|extension| extension == "sock")
            })
        {
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    })
    .await
    .expect("a fresh daemon observes the surviving run and its worker exits after it finishes");
}
