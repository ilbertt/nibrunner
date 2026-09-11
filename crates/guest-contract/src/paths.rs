pub const CONFIG_DEVICE: &str = "/dev/vdb";
pub const DATA_DEVICE: &str = "/dev/vdc";

// vda, vdb and vdc are the three every guest has; the layers are the drives after them.
const FIRST_LAYER_DEVICE_INDEX: u8 = 3;

pub fn layer_device(index: usize) -> String {
    let letter = (b'a' + FIRST_LAYER_DEVICE_INDEX + index as u8) as char;
    format!("/dev/vd{letter}")
}

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
