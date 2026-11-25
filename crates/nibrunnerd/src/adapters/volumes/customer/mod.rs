pub(super) mod checkpoints;
mod dns;
pub(super) mod images;
mod migration;
mod preflight;
mod profile;
mod runtime;
pub mod transfer;
pub(crate) use runtime::CustomerVolumes;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use protocol::{AppId, DesiredPresence, HostDesiredState};

use super::VolumeError;
use crate::config::HostConfig;
use crate::install::customer_writer;
use profile::{failed, io_error, Placement, PROFILE, RETIRED, WRITER_CONFIG};

pub struct CustomerPlacements {
    root: PathBuf,
    shared_url: reqwest::Url,
    shared_mount: PathBuf,
    held: RwLock<BTreeMap<AppId, Arc<Placement>>>,
}

impl CustomerPlacements {
    pub fn open(config: &HostConfig) -> Result<Self, VolumeError> {
        let settings = config
            .volumes
            .zerofs()
            .ok_or_else(|| failed("customer volumes require a ZeroFS host"))?;
        let root = customer_writer::profiles(config);
        crate::json_store::make_directory(&root, 0o700).map_err(io_error)?;
        let mut held = BTreeMap::new();
        for entry in std::fs::read_dir(&root).map_err(io_error)? {
            let path = entry.map_err(io_error)?.path();
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(|| failed("invalid customer placement directory"))?;
            if name.starts_with(".stage-") {
                continue;
            }
            let placement: Placement =
                toml::from_str(&std::fs::read_to_string(path.join(PROFILE)).map_err(io_error)?)
                    .map_err(|_| failed("customer volume placement is corrupt"))?;
            if placement.app.as_str() != name
                || held.values().any(|other: &Arc<Placement>| {
                    other.volume == placement.volume || other.overlaps(&placement)
                })
            {
                return Err(failed("customer placement ownership is ambiguous"));
            }
            if !path.join(RETIRED).try_exists().map_err(io_error)? && !path.join(WRITER_CONFIG).is_file() {
                return Err(failed("customer writer configuration is missing"));
            }
            migration::Migration::load(&path)?;
            held.insert(placement.app.clone(), Arc::new(placement));
        }
        Ok(Self {
            root,
            shared_url: reqwest::Url::parse(&settings.storage_url)
                .map_err(|_| failed("invalid host storage URL"))?,
            shared_mount: settings.mount_path.clone(),
            held: RwLock::new(held),
        })
    }

    pub(crate) fn prepare(
        &self,
        desired: &HostDesiredState,
        source_owners: &BTreeMap<protocol::VolumeId, AppId>,
        previous: Option<&HostDesiredState>,
    ) -> Result<(), VolumeError> {
        let mut held = self.held.write().expect("placement lock");
        let mut additions = Vec::new();
        let mut withdrawals = std::collections::BTreeSet::new();
        for volume in &desired.volumes {
            let existing = held.get(&volume.app_id);
            let Some(storage) = &volume.storage else {
                if let Some(p) = existing {
                    let dir = p.directory(&self.root);
                    let deleted_source = volume.desired_state == DesiredPresence::Absent
                        && std::fs::read_to_string(dir.join(RETIRED)).is_ok_and(|s| s == "shared");
                    if p.volume == volume.volume_id
                        && (deleted_source || migration::Migration::load(&dir)?.is_some_and(|m| m.rejected()))
                    {
                        withdrawals.insert(p.volume.clone());
                        continue;
                    }
                }
                if existing.is_some() || held.values().any(|p| p.volume == volume.volume_id) {
                    return Err(failed("a customer volume cannot switch to host storage"));
                }
                continue;
            };
            let placement = Placement::requested(&volume.app_id, &volume.volume_id, storage)?;
            let config = placement.config(storage)?;
            let dir = placement.directory(&self.root);
            if let Some(existing) = existing {
                if **existing != placement {
                    return Err(failed(
                        "changing an existing volume's storage requires a migration",
                    ));
                }
                if dir.join(RETIRED).try_exists().map_err(io_error)? {
                    if volume.desired_state == DesiredPresence::Present {
                        return Err(failed("this customer volume has been retired"));
                    }
                } else if std::fs::read_to_string(dir.join(WRITER_CONFIG)).map_err(io_error)? != config
                    && !migration::Migration::load(&dir)?.is_some_and(|m| m.rejected())
                {
                    return Err(failed(
                        "the requested writer settings differ from the staged customer writer",
                    ));
                }
                continue;
            }
            if volume.desired_state == DesiredPresence::Absent {
                return Err(failed("this host cannot delete an unclaimed customer volume"));
            }
            if self.shared_url.host_str() == Some(&placement.bucket)
                && profile::overlapping_prefixes(
                    self.shared_url.path().trim_matches('/'),
                    placement.storage_url()?.path().trim_matches('/'),
                )
            {
                return Err(failed("the requested prefix overlaps host storage"));
            }
            if held
                .values()
                .chain(additions.iter().map(|(p, _, _)| p))
                .any(|p| p.app == placement.app || p.volume == placement.volume || p.overlaps(&placement))
            {
                return Err(failed("the customer app, volume or prefix already has a writer"));
            }
            if !mounted(&self.shared_mount)? {
                return Err(failed(
                    "cannot confirm shared image placement while the shared writer is unmounted",
                ));
            }
            let source = images::image_size(&self.shared_mount, &placement.volume)?;
            let migration = match (source, source_owners.get(&placement.volume)) {
                (Some(bytes), Some(owner))
                    if owner == &placement.app && bytes > 0 && bytes <= volume.size_bytes =>
                {
                    Some(migration::Migration::new())
                }
                (None, None) => None,
                _ => {
                    return Err(failed(
                        "shared image ownership or size does not match the requested volume",
                    ))
                }
            };
            additions.push((Arc::new(placement), config, migration));
        }
        for placement in held.values().chain(additions.iter().map(|(p, _, _)| p)) {
            if desired
                .instances
                .iter()
                .any(|i| (i.app_id == placement.app) != (i.volume_id == placement.volume))
                || desired.volumes.iter().any(|v| {
                    (v.app_id == placement.app) != (v.volume_id == placement.volume)
                        || (v.volume_id == placement.volume
                            && v.storage.is_none()
                            && !withdrawals.contains(&placement.volume))
                })
            {
                return Err(failed(
                    "a customer volume belongs exclusively to its assigned app",
                ));
            }
            if desired.checkpoints.iter().any(|c| {
                c.volume_id == placement.volume
                    && c.desired_state == DesiredPresence::Present
                    && !previous.is_some_and(|old| old.checkpoints.contains(c))
            }) || desired
                .exports
                .iter()
                .any(|e| e.app_id == placement.app || e.volume_id == placement.volume)
            {
                return Err(failed(
                    "this host does not yet serve checkpoints or exports for customer volumes",
                ));
            }
        }
        for (placement, config, migration) in additions {
            placement.stage(&self.root, &config, migration.as_ref())?;
            held.insert(placement.app.clone(), placement);
        }
        Ok(())
    }
}

fn mounted(path: &Path) -> Result<bool, VolumeError> {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::MetadataExt;
        let Some(parent) = path.parent() else {
            return Ok(false);
        };
        match std::fs::metadata(path) {
            Ok(metadata) => Ok(metadata.dev() != std::fs::metadata(parent).map_err(io_error)?.dev()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(io_error(error)),
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = path;
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{desired_state, desired_volume, zerofs_settings};
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn a_reopened_customer_placement_refuses_shared_fallback_and_destination_changes() {
        let root = tempfile::tempdir().unwrap();
        let mut config = HostConfig::under(root.path());
        config.volumes = crate::config::VolumeBackend::Zerofs(Box::new(zerofs_settings(|_| {})));
        let storage = serde_json::from_value(serde_json::json!({
            "endpoint":"https://objects.example.com", "bucket":"example", "prefix":"app-1",
            "region":null, "auth":{"kind":"anonymous"}, "encryptionPassword":"private-password"
        }))
        .unwrap();
        let volume = desired_volume(|volume| volume.storage = Some(storage));
        let registry = CustomerPlacements::open(&config).unwrap();
        let storage = volume.storage.as_ref().unwrap();
        let placement = Placement::requested(&volume.app_id, &volume.volume_id, storage).unwrap();
        placement
            .stage(&registry.root, &placement.config(storage).unwrap(), None)
            .unwrap();
        let profile = placement.directory(&registry.root);
        assert_eq!(
            std::fs::metadata(profile.join(WRITER_CONFIG))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        drop(registry);

        let recovered = CustomerPlacements::open(&config).unwrap();
        let mut desired = desired_state(|desired| desired.volumes.push(volume));
        recovered.prepare(&desired, &BTreeMap::new(), None).unwrap();
        desired.volumes[0].storage.as_mut().unwrap().bucket = "different".into();
        assert!(recovered.prepare(&desired, &BTreeMap::new(), None).is_err());
        desired.volumes[0].storage = None;
        assert!(recovered.prepare(&desired, &BTreeMap::new(), None).is_err());
        desired.volumes.clear();
        recovered.prepare(&desired, &BTreeMap::new(), None).unwrap();
        assert!(profile.join(WRITER_CONFIG).exists());
    }
}
