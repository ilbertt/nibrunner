pub const ARTIFACT_DEVICE: &str = "/dev/vdb";
pub const CONFIG_DEVICE: &str = "/dev/vdc";
pub const DATA_DEVICE: &str = "/dev/vdd";

pub const ARTIFACT_MOUNT: &str = "/mnt/artifact";
pub const TENANT_BINARY: &str = "/mnt/artifact/server";

pub const CONFIG_MOUNT: &str = "/run/config";
pub const CONFIG_FILE: &str = "/run/config/instance.env";
pub const RESOLV_CONF: &str = "/run/resolv.conf";

pub const APP_DIR: &str = "/app";
pub const DATA_DIR: &str = "/app/data";
pub const TENANT_TMP_DIR: &str = "/tmp";

pub const TENANT_UID: u32 = 65534;
pub const TENANT_GID: u32 = 65534;
