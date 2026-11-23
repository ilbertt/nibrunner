use std::path::{Path, PathBuf};

use crate::config::{HostConfig, ZerofsSettings};
use crate::install::{render::header, service_user::ZEROFS_USER};

pub(crate) const WRITER: &str = "nibrunner-customer-zerofs@.service";
pub(crate) const MOUNT: &str = "nibrunner-customer-mount@.service";
pub(crate) const SOCKET_ROOT: &str = "/run/nibrunner-customer";
pub(crate) const MOUNT_ROOT: &str = "/run/nibrunner-customer-mounts";

pub(crate) fn profiles(config: &HostConfig) -> PathBuf {
    config.state_dir.join("customer-volumes")
}

pub(crate) const CACHE_DIRECTORY: &str = ".customer-zerofs-cache";

pub(crate) fn caches(config: &HostConfig) -> PathBuf {
    config.snapshot_dir.join(CACHE_DIRECTORY)
}

pub(crate) fn units(
    config: &HostConfig,
    settings: &ZerofsSettings,
    file: &Path,
) -> [(&'static str, String); 2] {
    let binary = settings.binary.display();
    let profiles = profiles(config);
    let caches = caches(config);
    let heading = header("One writer for the app's assigned bucket prefix.", file);
    let writer = format!("{heading}
[Unit]
Description=ZeroFS writer for nibrunner app %i
After=network-online.target
Wants=network-online.target
RequiresMountsFor=\"{cache}\"

[Service]
Type=exec
User={ZEROFS_USER}
Group={ZEROFS_USER}
UMask=0077
LoadCredential=zerofs.toml:{profile}/%i/writer.toml
ExecStart=\"{binary}\" run --config %d/zerofs.toml
RuntimeDirectory=nibrunner-customer/%i
RuntimeDirectoryMode=0700
BindReadOnlyPaths=\"{profile}/%i/s3.hosts:/etc/hosts\"
BindPaths=\"{cache}/%i:/var/cache/nibrunner-customer\"
ReadWritePaths=/var/cache/nibrunner-customer
Environment=AWS_EC2_METADATA_DISABLED=true
UnsetEnvironment=AWS_PROFILE AWS_ACCESS_KEY_ID AWS_SECRET_ACCESS_KEY AWS_SESSION_TOKEN AWS_WEB_IDENTITY_TOKEN_FILE AWS_ROLE_ARN AWS_CONTAINER_CREDENTIALS_RELATIVE_URI AWS_CONTAINER_CREDENTIALS_FULL_URI
IPAddressDeny=localhost link-local multicast 0.0.0.0/8 10.0.0.0/8 100.64.0.0/10 172.16.0.0/12 192.168.0.0/16 fc00::/7
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
PrivateTmp=true
ProtectKernelTunables=true
ProtectKernelModules=true
ProtectControlGroups=true
RestrictSUIDSGID=true
LockPersonality=true
StandardOutput=null
StandardError=null
LimitNOFILE=1048576
Restart=on-failure
RestartSec=5s
TimeoutStopSec=120
", profile = profiles.display(), cache = caches.display());
    let mount = format!("{heading}
[Unit]
Description=Volume image mount for nibrunner app %i
Requires=nibrunner-customer-zerofs@%i.service
After=nibrunner-customer-zerofs@%i.service
PartOf=nibrunner-customer-zerofs@%i.service
JobTimeoutSec=90

[Service]
Type=exec
UMask=0077
ExecStartPre=/usr/bin/mkdir -p {MOUNT_ROOT}/%i
ExecStartPre=/bin/sh -c 'for i in $(seq 1 120); do test -S {SOCKET_ROOT}/%i/9p.sock && exit 0; sleep 0.5; done; exit 1'
ExecStart=\"{binary}\" mount --writeback false unix:{SOCKET_ROOT}/%i/9p.sock {MOUNT_ROOT}/%i
ExecStartPost=/bin/sh -c 'for i in $(seq 1 100); do mountpoint -q {MOUNT_ROOT}/%i && exit 0; sleep 0.1; done; exit 1'
ExecStopPost=-/usr/bin/fusermount3 -u {MOUNT_ROOT}/%i
TimeoutStartSec=80
Restart=on-failure
RestartSec=2s
");
    [(WRITER, writer), (MOUNT, mount)]
}
