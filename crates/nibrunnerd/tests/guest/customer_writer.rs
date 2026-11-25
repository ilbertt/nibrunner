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
    let expected_db = seed_database(&host, &mut app, root).await;
    let neighbour = host.tenant(2).running();
    host.deploy(&[app.clone(), neighbour.clone()]).await;
    host.until_state(&app.app_id, InstanceState::Running).await;
    host.until_state(&neighbour.app_id, InstanceState::Running).await;
    assert_eq!(
        host.get(&app, "/write?path=before&body=from-shared")
            .await
            .unwrap()
            .status,
        200
    );
    assert_eq!(
        host.get(&neighbour, "/write?path=neighbour&body=still-shared")
            .await
            .unwrap()
            .status,
        200
    );
    let owner_checkpoint = protocol::CheckpointId::parse("owner-snapshot").unwrap();
    let neighbour_checkpoint = protocol::CheckpointId::parse("neighbour-snapshot").unwrap();
    let mut desired = host.document(&[app.clone(), neighbour.clone()]);
    let checkpoint = |id: &protocol::CheckpointId, volume: &protocol::VolumeId| protocol::DesiredCheckpoint {
        checkpoint_id: id.clone(),
        volume_id: volume.clone(),
        desired_state: protocol::DesiredPresence::Present,
    };
    desired.checkpoints = vec![
        checkpoint(&owner_checkpoint, &app.volume_id),
        checkpoint(&neighbour_checkpoint, &neighbour.volume_id),
    ];
    host.write(&desired).await;
    host.until("both shared checkpoints ready", |report| {
        report
            .checkpoints
            .iter()
            .filter(|c| c.state == protocol::CheckpointState::Ready)
            .count()
            == 2
    })
    .await;
    rejected_credentials_preserve_the_shared_guest(&host, &app, &mut desired, &storage, &expected_db).await;
    app.volume.storage = Some(storage.clone());
    desired.volumes[0] = app.volume.clone();
    conflicting_image_restores_the_source(&host, &app, &mut desired).await;
    app.instance.deployment_id = protocol::DeploymentId::parse("dep-1-after-conflict").unwrap();
    desired.instances[0] = app.instance.clone();
    host.write(&desired).await;
    host.until("the dedicated disk to replace the shared disk", |_| {
        !shared_mount.join(".nbd/vol-1").exists()
            && Path::new("/run/nibrunner-customer-mounts/app-1/.nbd/vol-1").is_file()
    })
    .await;
    host.until_state(&app.app_id, InstanceState::Running).await;
    assert_eq!(
        host.get(&app, "/read?path=before").await.unwrap().body,
        "from-shared"
    );
    assert_eq!(
        host.get(&neighbour, "/read?path=neighbour").await.unwrap().body,
        "still-shared"
    );
    host.until(
        "shared history is reported while the other tenant retains its checkpoint",
        |report| {
            report.volumes.iter().any(|v| {
                v.volume_id == app.volume_id
                    && v.message
                        .as_ref()
                        .is_some_and(|m| m.as_str().contains("historical shared checkpoints"))
            })
        },
    )
    .await;
    let checkpoints = host.host.volumes.observe_checkpoints().await;
    assert!(!checkpoints.contains(&owner_checkpoint));
    assert!(checkpoints.contains(&neighbour_checkpoint));
    desired.checkpoints[1].desired_state = protocol::DesiredPresence::Absent;
    host.write(&desired).await;
    let journal = host
        .host
        .config
        .state_dir
        .join("customer-volumes/app-1/migration.json");
    host.until(
        "the detached source and shared checkpoint history to be released",
        |_| !journal.exists(),
    )
    .await;
    let copy_id = protocol::VolumeId::parse("copy-fixture").unwrap();
    let device = Path::new(".nbd").join(copy_id.as_str());
    let customer_mount = Path::new("/run/nibrunner-customer-mounts/app-1");
    let bytes = vec![0x57; 4096];
    std::fs::create_dir_all(shared_mount.join(".nbd")).unwrap();
    std::fs::write(shared_mount.join(&device), &bytes).unwrap();
    nibrunnerd::adapters::volumes::customer::transfer::copy_image(
        &shared_mount,
        customer_mount,
        &copy_id,
        uuid::Uuid::new_v4(),
    )
    .unwrap();
    assert_eq!(std::fs::read(customer_mount.join(&device)).unwrap(), bytes);
    assert_eq!(
        host.get(&app, "/write?path=ownership&body=independent-recovery")
            .await
            .unwrap()
            .status,
        200
    );
    assert_eq!(
        host.get(&app, "/read?path=ownership").await.unwrap().body,
        "independent-recovery"
    );
    migrate_sleeping_guest_after_writer_outage(&host, &app, neighbour, storage, &shared_mount).await;
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
    let image = root.join("recovered.ext4");
    let mut disk = std::fs::File::open("/dev/nbd8").unwrap();
    let mut copy = std::fs::File::create_new(&image).unwrap();
    assert_eq!(
        std::io::copy(&mut disk, &mut copy).unwrap(),
        app.volume.size_bytes
    );
    drop(disk);
    drop(copy);
    run("nbd-client", &["-d", "/dev/nbd8"]);
    // Stopping a VM is a power cut. Replay its committed ext4 journal on the customer's
    // disposable copy while the independently opened S3 image remains read-only.
    let replay = std::process::Command::new("e2fsck")
        .args(["-p", "-E", "journal_only"])
        .arg(&image)
        .output()
        .unwrap();
    assert!(
        matches!(replay.status.code(), Some(0 | 1)),
        "{}",
        String::from_utf8_lossy(&replay.stderr)
    );
    let read = std::process::Command::new("debugfs")
        .args(["-R", "cat /upper/app/data/ownership"])
        .arg(&image)
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8(read.stdout).unwrap().trim(),
        "independent-recovery",
        "{}",
        String::from_utf8_lossy(&read.stderr)
    );
}

async fn migrate_sleeping_guest_after_writer_outage(
    host: &nibrunnerd::test_support::machine::RunningHost,
    app: &nibrunnerd::test_support::machine::Tenant,
    neighbour: nibrunnerd::test_support::machine::Tenant,
    mut storage: S3VolumeStorage,
    shared_mount: &Path,
) {
    let mut sleeping = neighbour.on_request(60_000);
    host.deploy(&[app.clone(), sleeping.clone()]).await;
    host.let_sleep(&sleeping).await;
    host.until_state(&sleeping.app_id, InstanceState::Idle).await;
    let unit = "nibrunner-customer-mount@app-2.service";
    run("systemctl", &["mask", "--runtime", unit]);
    storage.prefix = protocol::ObjectKey::parse("second-app").unwrap();
    sleeping.volume.storage = Some(storage);
    sleeping.instance.desired_state = DesiredInstanceState::Running;
    sleeping.instance.idle_timeout_ms = None;
    host.deploy(&[app.clone(), sleeping.clone()]).await;
    host.until("the masked writer to refuse a migration attempt", |report| {
        report.volumes.iter().any(|v| {
            v.volume_id == sleeping.volume_id
                && v.message
                    .as_ref()
                    .is_some_and(|m| m.as_str().contains("serving the shared volume"))
        })
    })
    .await;
    let source = shared_mount.join(".nbd/vol-2");
    assert!(source.is_file());
    assert!(!Path::new("/run/nibrunner-customer-mounts/app-2/.nbd/vol-2").exists());
    assert_eq!(
        host.get(app, "/read?path=ownership").await.unwrap().body,
        "independent-recovery"
    );
    assert_eq!(
        host.get(&sleeping, "/read?path=neighbour").await.unwrap().body,
        "still-shared"
    );
    let destination = sleeping.volume.storage.take().unwrap();
    host.deploy(&[app.clone(), sleeping.clone()]).await;
    let placement = host
        .host
        .config
        .state_dir
        .join("customer-volumes")
        .join(sleeping.app_id.as_str());
    host.until("a withdrawn rejection to release its writer reservation", |_| {
        !placement.exists()
    })
    .await;
    assert_eq!(
        host.get(&sleeping, "/read?path=neighbour").await.unwrap().body,
        "still-shared"
    );
    sleeping.volume.storage = Some(destination);
    run("systemctl", &["unmask", "--runtime", unit]);
    sleeping.instance.deployment_id = protocol::DeploymentId::parse("dep-2-retry").unwrap();
    host.deploy(&[app.clone(), sleeping.clone()]).await;
    host.until("sleeping disk to migrate on retry", |_| !source.exists())
        .await;
    host.until_state(&sleeping.app_id, InstanceState::Running).await;
    assert_eq!(
        host.get(&sleeping, "/read?path=neighbour").await.unwrap().body,
        "still-shared"
    );
    sleeping.instance.desired_state = DesiredInstanceState::Stopped;
    host.deploy(&[app.clone(), sleeping.clone()]).await;
    host.until_state(&sleeping.app_id, InstanceState::Stopped).await;
    let mut retire = host.document(std::slice::from_ref(app));
    sleeping.volume.desired_state = protocol::DesiredPresence::Absent;
    retire.volumes.push(sleeping.volume);
    host.write(&retire).await;
    host.until("the fixture's second writer to retire", |report| {
        report
            .volumes
            .iter()
            .any(|v| v.volume_id == sleeping.volume_id && v.state == protocol::VolumeState::Deleted)
    })
    .await;
}

async fn seed_database(
    host: &nibrunnerd::test_support::machine::RunningHost,
    app: &mut nibrunnerd::test_support::machine::Tenant,
    root: &Path,
) -> String {
    use sha2::{Digest, Sha256};
    use sqlx::Connection;
    let path = root.join("fixture.sqlite");
    let options = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(&path)
        .create_if_missing(true);
    let mut db = sqlx::SqliteConnection::connect_with(&options).await.unwrap();
    sqlx::query("CREATE TABLE fixture (value TEXT NOT NULL)")
        .execute(&mut db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO fixture VALUES ('shared before migration')")
        .execute(&mut db)
        .await
        .unwrap();
    let row: String = sqlx::query_scalar("SELECT value FROM fixture")
        .fetch_one(&mut db)
        .await
        .unwrap();
    assert_eq!(row, "shared before migration");
    db.close().await.unwrap();
    let bytes = std::fs::read(path).unwrap();
    let mut archive = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_gnu();
    header.set_mode(0o600);
    header.set_size(bytes.len() as u64);
    archive
        .append_data(&mut header, "fixture.sqlite", bytes.as_slice())
        .unwrap();
    let tar = archive.into_inner().unwrap();
    let contents = nibrunnerd::test_support::initial_contents(&tar, "/app/data");
    let object = Path::new(&host.host.config.artifact_store_url).join(contents.object.object_key.as_str());
    std::fs::create_dir_all(object.parent().unwrap()).unwrap();
    std::fs::write(object, tar).unwrap();
    app.volume.initial_contents = Some(contents);
    format!("{:x}", Sha256::digest(bytes))
}

async fn rejected_credentials_preserve_the_shared_guest(
    host: &nibrunnerd::test_support::machine::RunningHost,
    app: &nibrunnerd::test_support::machine::Tenant,
    desired: &mut protocol::HostDesiredState,
    storage: &S3VolumeStorage,
    expected_db: &str,
) {
    let mut wrong = storage.clone();
    let protocol::S3VolumeAuth::AccessKey {
        secret_access_key, ..
    } = &mut wrong.auth
    else {
        panic!("fixture requires an access key")
    };
    *secret_access_key = serde_json::from_value(serde_json::json!("incorrect-secret")).unwrap();
    desired.volumes[0].storage = Some(wrong);
    host.write(desired).await;
    host.until(
        "invalid initial credentials to be reported on the serving shared volume",
        |report| {
            report.volumes.iter().any(|v| {
                v.volume_id == app.volume_id
                    && v.state == protocol::VolumeState::Ready
                    && v.message
                        .as_ref()
                        .is_some_and(|m| m.as_str().contains("serving the shared volume"))
            })
        },
    )
    .await;
    for _ in 0..2 {
        let answer = host.get(app, "/read?path=before").await.unwrap();
        assert_eq!(answer.status, 200);
        assert_eq!(answer.body, "from-shared");
        assert_eq!(
            host.get(app, "/checksum?path=fixture.sqlite").await.unwrap().body,
            expected_db
        );
    }
    assert_eq!(
        host.get(app, "/write?path=after-refusal&body=still-writable")
            .await
            .unwrap()
            .status,
        200
    );
    desired.instances[0].desired_state = DesiredInstanceState::Stopped;
    host.write(desired).await;
    host.until_state(&app.app_id, InstanceState::Stopped).await;
    desired.instances[0].desired_state = DesiredInstanceState::Running;
    host.write(desired).await;
    host.until_state(&app.app_id, InstanceState::Running).await;
    assert_eq!(
        host.get(app, "/read?path=after-refusal").await.unwrap().body,
        "still-writable"
    );
    assert_eq!(
        host.get(app, "/checksum?path=fixture.sqlite").await.unwrap().body,
        expected_db
    );
}

async fn conflicting_image_restores_the_source(
    host: &nibrunnerd::test_support::machine::RunningHost,
    app: &nibrunnerd::test_support::machine::Tenant,
    desired: &mut protocol::HostDesiredState,
) {
    let unit = "nibrunner-customer-mount@app-1.service";
    let dropin = Path::new("/run/systemd/system/nibrunner-customer-mount@app-1.service.d");
    std::fs::create_dir_all(dropin).unwrap();
    let hook = dropin.join("conflict.conf");
    let other = Path::new("/run/nibrunner-customer-mounts/app-1/.nbd/vol-1");
    std::fs::write(&hook, "[Service]\nExecStartPost=/bin/sh -ec 'set -C; echo foreign-image > /run/nibrunner-customer-mounts/app-1/.nbd/vol-1'\n").unwrap();
    run("systemctl", &["daemon-reload"]);
    host.write(desired).await;
    host.until(
        "a conflicting image to return the guest to the shared disk",
        |report| {
            report.volumes.iter().any(|v| {
                v.volume_id == app.volume_id
                    && v.state == protocol::VolumeState::Ready
                    && v.message
                        .as_ref()
                        .is_some_and(|m| m.as_str().contains("migrated image length differs"))
            })
        },
    )
    .await;
    host.until_state(&app.app_id, InstanceState::Running).await;
    assert_eq!(
        host.get(app, "/read?path=after-refusal").await.unwrap().body,
        "still-writable"
    );
    assert_eq!(
        host.get(app, "/write?path=after-copy-failure&body=recovered")
            .await
            .unwrap()
            .status,
        200
    );
    std::fs::remove_file(hook).unwrap();
    run("systemctl", &["daemon-reload"]);
    run("systemctl", &["start", unit]);
    assert_eq!(std::fs::read_to_string(other).unwrap(), "foreign-image\n");
    std::fs::remove_file(other).unwrap();
}
