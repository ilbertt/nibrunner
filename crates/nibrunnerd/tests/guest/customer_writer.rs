//! The isolated S3 fixture is provided by .github/scripts/test-customer-writer.sh.
use std::path::Path;
use std::time::Duration;

use nibrunnerd::config::VolumeBackend;
use nibrunnerd::test_support::{customer_writer_units, zerofs_settings};
use protocol::{DesiredInstanceState, InstanceState, S3VolumeStorage};

fn run(program: &str, args: &[&str]) -> String {
    let output = std::process::Command::new(program).args(args).output().unwrap();
    assert!(
        output.status.success(),
        "{program}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

async fn until(path: &Path, mount: bool) {
    tokio::time::timeout(Duration::from_secs(90), async {
        while !if mount {
            std::process::Command::new("mountpoint")
                .arg("-q")
                .arg(path)
                .status()
                .unwrap()
                .success()
        } else {
            path.exists()
        } {
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{} did not become ready", path.display()));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_guest_disk_is_recovered_from_only_the_customer_bucket_and_password() {
    let Some(root) = std::env::var_os("NIBRUNNER_CUSTOMER_FIXTURE").filter(|path| !path.is_empty()) else {
        return;
    };
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_test_writer()
        .try_init();
    let root = Path::new(&root);
    let path = root.to_str().unwrap();
    let binary = root.join("zerofs");
    nibrunnerd::install_crypto_provider();
    nibrunnerd::install::zerofs::fetch(&binary).await.unwrap();
    nibrunnerd::install::service_user::ensure("zerofs").unwrap();
    let endpoint = std::fs::read_to_string(root.join("endpoint")).unwrap();
    let shared_mount = root.join("shared-mount");
    std::fs::create_dir(&shared_mount).unwrap();
    let shared_config = root.join("shared.toml");
    std::fs::write(
        &shared_config,
        format!(
            r#"
[storage]
url = "s3://shared-bucket/host-1"
encryption_password = "shared-fixture-password"
[aws]
endpoint = "{endpoint}"
region = "us-east-1"
access_key_id = "fixture-access"
secret_access_key = "fixture-secret"
virtual_hosted_style_request = "false"
[cache]
dir = "{path}/shared-cache"
disk_size_gb = 1.0
memory_size_gb = 1.0
[servers.ninep]
unix_socket = "{path}/shared-9p.sock"
[servers.nbd]
unix_socket = "{path}/shared-nbd.sock"
[servers.rpc]
unix_socket = "{path}/shared-rpc.sock"
[telemetry]
enabled = false
"#
        ),
    )
    .unwrap();
    run(
        "systemd-run",
        &[
            "--unit=nibrunner-fixture-writer",
            "--collect",
            &format!("--setenv=SSL_CERT_FILE={path}/ca.crt"),
            binary.to_str().unwrap(),
            "run",
            "-c",
            shared_config.to_str().unwrap(),
        ],
    );
    until(&root.join("shared-9p.sock"), false).await;
    run(
        "systemd-run",
        &[
            "--unit=nibrunner-fixture-mount",
            "--collect",
            binary.to_str().unwrap(),
            "mount",
            "--writeback",
            "false",
            &format!("unix:{path}/shared-9p.sock"),
            shared_mount.to_str().unwrap(),
        ],
    );
    until(&shared_mount, true).await;
    let mut host = crate::host_with(|config| {
        let settings = zerofs_settings(|settings| {
            settings.binary = binary.clone();
            settings.mount_path = shared_mount.clone();
            settings.config_file = shared_config.clone();
            settings.nbd_socket_path = root.join("shared-nbd.sock");
            settings.storage_url = "s3://shared-bucket/host-1".into();
        });
        for (name, contents) in customer_writer_units(config, &settings) {
            let file = Path::new("/run/systemd/system").join(name);
            std::fs::write(file, contents).unwrap();
        }
        let dropin = Path::new("/run/systemd/system/nibrunner-customer-zerofs@.service.d");
        std::fs::create_dir_all(dropin).unwrap();
        let fixture = format!("[Service]\nIPAddressAllow=127.0.0.1/32\nEnvironment=SSL_CERT_FILE={path}/ca.crt\nStandardError=journal\n");
        std::fs::write(dropin.join("fixture.conf"), fixture).unwrap();
        run("systemctl", &["daemon-reload"]);
        config.volumes = VolumeBackend::Zerofs(Box::new(settings));
    }).await.unwrap();
    let mut app = host.tenant(1).running();
    let storage: S3VolumeStorage = serde_json::from_value(serde_json::json!({
        "endpoint": endpoint,
        "bucket": "customer-bucket",
        "prefix": "my-app",
        "region": "us-east-1",
        "encryptionPassword": "customer-password",
        "auth": { "kind": "access-key", "accessKeyId": "fixture-access", "secretAccessKey": "fixture-secret" }
    }))
    .unwrap();
    app.volume.storage = Some(storage);
    host.deploy(std::slice::from_ref(&app)).await;
    host.until_state(&app.app_id, InstanceState::Running).await;
    assert_eq!(
        host.get(&app, "/write?path=ownership&body=independent-recovery")
            .await
            .unwrap()
            .status,
        200
    );
    app.instance.desired_state = DesiredInstanceState::Stopped;
    host.deploy(std::slice::from_ref(&app)).await;
    host.until_state(&app.app_id, InstanceState::Stopped).await;
    host.pause_controllers().await;
    host.host.volumes.flush(&app.app_id).await.unwrap();
    host.host
        .volumes
        .detach(&app.volume_id, &app.app_id)
        .await
        .unwrap();
    run(
        "systemctl",
        &[
            "stop",
            "nibrunner-customer-mount@app-1.service",
            "nibrunner-customer-zerofs@app-1.service",
        ],
    );
    host.stop().await;
    assert!(!shared_mount.join(".nbd/vol-1").exists());

    let config = root.join("recovery.toml");
    std::fs::write(
        &config,
        format!(
            r#"
[storage]
url = "s3://customer-bucket/my-app"
encryption_password = "customer-password"
[aws]
endpoint = "{endpoint}"
region = "us-east-1"
access_key_id = "fixture-access"
secret_access_key = "fixture-secret"
virtual_hosted_style_request = "false"
[cache]
dir = "{path}/new-cache"
disk_size_gb = 1.0
memory_size_gb = 0.25
[servers.nbd]
unix_socket = "{path}/recovery.sock"
[telemetry]
enabled = false
"#
        ),
    )
    .unwrap();
    run(
        "systemd-run",
        &[
            "--unit=nibrunner-fixture-recovery",
            "--collect",
            &format!("--setenv=SSL_CERT_FILE={path}/ca.crt"),
            binary.to_str().unwrap(),
            "run",
            "--read-only",
            "-c",
            config.to_str().unwrap(),
        ],
    );
    until(&root.join("recovery.sock"), false).await;
    run(
        "nbd-client",
        &[
            "-unix",
            root.join("recovery.sock").to_str().unwrap(),
            "/dev/nbd8",
            "-N",
            "vol-1",
        ],
    );
    let marker = run("debugfs", &["-R", "cat /upper/app/data/ownership", "/dev/nbd8"]);
    run("nbd-client", &["-d", "/dev/nbd8"]);
    assert_eq!(marker.trim(), "independent-recovery");
}
