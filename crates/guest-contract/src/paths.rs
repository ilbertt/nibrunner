pub const CONFIG_DEVICE: &str = "/dev/vdb";
pub const VOLUME_DEVICE: &str = "/dev/vdc";

// vda, vdb and vdc are the three every guest has; the layers are the drives after them.
const FIRST_LAYER_DEVICE_INDEX: u8 = 3;

pub fn layer_device(index: usize) -> String {
    let letter = (b'a' + FIRST_LAYER_DEVICE_INDEX + index as u8) as char;
    format!("/dev/vd{letter}")
}

pub const CONFIG_MOUNT: &str = "/run/config";
pub const CONFIG_FILE: &str = "/run/config/instance.env";

/// The guest's own root, bound here without its submounts, is the bottom of every stack: the
/// Debian this image is, under whatever the document layered on it.
pub const BASE_MOUNT: &str = "/mnt/base";
pub const LAYERS_MOUNT_DIR: &str = "/mnt/layers";

pub fn layer_mount(index: usize) -> String {
    format!("{LAYERS_MOUNT_DIR}/{index}")
}

/// The app's volume, and the two directories overlayfs keeps on it. `upper` is every write the
/// tenant ever made to its root, and so the only thing worth exporting; `work` is overlayfs's own.
pub const VOLUME_MOUNT: &str = "/mnt/volume";
pub const VOLUME_UPPER_DIR: &str = "/mnt/volume/upper";
pub const VOLUME_WORK_DIR: &str = "/mnt/volume/work";
pub const VOLUME_UPPER_NAME: &str = "upper";

/// The stacked root the program runs in.
pub const ROOT_MOUNT: &str = "/mnt/root";
pub const RESOLV_CONF: &str = "/mnt/root/etc/resolv.conf";
pub const TENANT_TMP_DIR: &str = "/tmp";

pub const TENANT_UID: u32 = 65534;
pub const TENANT_GID: u32 = 65534;
