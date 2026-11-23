use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use protocol::{AppId, ObjectKey, S3VolumeAuth, S3VolumeStorage, VolumeId};
use serde::{Deserialize, Serialize};

use super::super::VolumeError;
use crate::install::customer_writer::SOCKET_ROOT;

pub(super) const DISK_GIB: u64 = 4;
pub(super) const CACHE_MEMORY_MIB: u32 = 256;
pub(super) const PROFILE: &str = "placement.toml";
pub(super) const WRITER_CONFIG: &str = "writer.toml";
pub(super) const RETIRED: &str = "retired";

pub(super) fn failed(reason: impl Into<String>) -> VolumeError {
    VolumeError::Unusable(reason.into())
}

pub(super) fn io_error(error: io::Error) -> VolumeError {
    failed(format!("customer volume state is unavailable: {error}"))
}

#[derive(Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Placement {
    pub app: AppId,
    pub volume: VolumeId,
    pub endpoint: String,
    pub bucket: String,
    pub prefix: ObjectKey,
}

impl Placement {
    pub(super) fn requested(
        app: &AppId,
        volume: &VolumeId,
        storage: &S3VolumeStorage,
    ) -> Result<Self, VolumeError> {
        let password = storage.encryption_password.expose();
        if password.len() < 8 || password == "CHANGEME" {
            return Err(failed("ZeroFS requires an encryption password of at least eight bytes other than its example password"));
        }
        let url =
            reqwest::Url::parse(&storage.endpoint).map_err(|_| failed("invalid customer S3 endpoint"))?;
        if url.scheme() != "https"
            || url.host().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(failed(
                "customer storage requires an HTTPS endpoint without URL credentials, query or fragment",
            ));
        }
        let prefix = storage.prefix.as_str();
        if prefix.contains(char::is_control)
            || prefix.contains('\\')
            || prefix.split('/').any(|part| matches!(part, "" | "." | ".."))
        {
            return Err(failed(
                "the customer S3 prefix must contain normal, nonempty path components",
            ));
        }
        Ok(Self {
            app: app.clone(),
            volume: volume.clone(),
            endpoint: url.as_str().trim_end_matches('/').to_owned(),
            bucket: storage.bucket.clone(),
            prefix: storage.prefix.clone(),
        })
    }

    pub(super) fn storage_url(&self) -> Result<reqwest::Url, VolumeError> {
        let mut url = reqwest::Url::parse(&format!("s3://{}", self.bucket))
            .map_err(|_| failed("invalid customer bucket"))?;
        url.path_segments_mut()
            .map_err(|()| failed("invalid customer prefix"))?
            .extend(self.prefix.as_str().split('/'));
        Ok(url)
    }

    pub(super) fn overlaps(&self, other: &Self) -> bool {
        self.endpoint == other.endpoint
            && self.bucket == other.bucket
            && overlapping_prefixes(self.prefix.as_str(), other.prefix.as_str())
    }

    pub(super) fn directory(&self, root: &Path) -> PathBuf {
        root.join(self.app.as_str())
    }
    pub(super) fn socket(&self, name: &str) -> PathBuf {
        Path::new(SOCKET_ROOT).join(self.app.as_str()).join(name)
    }

    pub(super) fn config(&self, storage: &S3VolumeStorage) -> Result<String, VolumeError> {
        use std::collections::BTreeMap;
        let mut aws = BTreeMap::from([
            ("endpoint", self.endpoint.as_str()),
            ("region", storage.region.as_deref().unwrap_or("us-east-1")),
            ("virtual_hosted_style_request", "false"),
        ]);
        match &storage.auth {
            S3VolumeAuth::Anonymous {} => {
                aws.insert("skip_signature", "true");
            }
            S3VolumeAuth::AccessKey {
                access_key_id,
                secret_access_key,
            }
            | S3VolumeAuth::Session {
                access_key_id,
                secret_access_key,
                ..
            } => {
                aws.insert("access_key_id", access_key_id.expose());
                aws.insert("secret_access_key", secret_access_key.expose());
                if let S3VolumeAuth::Session { session_token, .. } = &storage.auth {
                    aws.insert("session_token", session_token.expose());
                }
            }
        }
        // ZeroFS expands environment expressions after decoding TOML, including inside credentials.
        let literal = |s: &str| toml::Value::String(s.replace('$', "$$")).to_string();
        let mut config = format!(
            "[storage]\nurl = {}\nencryption_password = {}\n\n[aws]\n",
            literal(self.storage_url()?.as_str()),
            literal(storage.encryption_password.expose())
        );
        for (name, value) in aws {
            use std::fmt::Write;
            writeln!(config, "{name} = {}", literal(value)).expect("writing to a string");
        }
        config.push_str(&format!(
            "
[cache]
dir = \"/var/cache/nibrunner-customer\"
disk_size_gb = {DISK_GIB}.0
memory_size_gb = {}
[filesystem]
ignore_fsync = true
compression = \"zstd-3\"
[lsm]
flush_interval_secs = 5
[telemetry]
enabled = false
",
            f64::from(CACHE_MEMORY_MIB) / 1024.0
        ));
        for (name, socket) in [("nbd", "nbd.sock"), ("ninep", "9p.sock"), ("rpc", "rpc.sock")] {
            use std::fmt::Write;
            writeln!(
                config,
                "[servers.{name}]\nunix_socket = {}",
                literal(&self.socket(socket).to_string_lossy())
            )
            .expect("writing to a string");
        }
        Ok(config)
    }

    pub(super) fn stage(&self, root: &Path, config: &str) -> Result<(), VolumeError> {
        let staging = tempfile::Builder::new()
            .prefix(".stage-")
            .tempdir_in(root)
            .map_err(io_error)?;
        let placement = toml::to_string(self).map_err(|_| failed("cannot encode customer placement"))?;
        write_new(&staging.path().join(PROFILE), &placement)?;
        write_new(&staging.path().join(WRITER_CONFIG), config)?;
        sync_dir(staging.path())?;
        std::fs::rename(staging.path(), self.directory(root)).map_err(io_error)?;
        sync_dir(root)
    }
}

pub(super) fn overlapping_prefixes(left: &str, right: &str) -> bool {
    let includes = |parent: &str, child: &str| {
        parent.is_empty()
            || child
                .strip_prefix(parent)
                .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
    };
    includes(left, right) || includes(right, left)
}

pub(super) fn write_new(path: &Path, contents: &str) -> Result<(), VolumeError> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = File::options()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(io_error)?;
    file.write_all(contents.as_bytes())
        .and_then(|()| file.sync_all())
        .map_err(io_error)
}

pub(super) fn sync_dir(path: &Path) -> Result<(), VolumeError> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(io_error)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_writer_preserves_literal_customer_credentials_and_has_no_shared_socket_or_cache() {
        let storage: S3VolumeStorage = serde_json::from_value(serde_json::json!({
            "endpoint":"https://objects.example.com:9000", "bucket":"customer", "prefix":"one/a b#c",
            "region":"auto", "auth":{"kind":"session", "accessKeyId":"access", "secretAccessKey":"key", "sessionToken":"a\"b"},
            "encryptionPassword":"${PRIVATE_PASSWORD}"
        })).unwrap();
        let placement = Placement::requested(
            &crate::test_support::app_id(),
            &crate::test_support::volume_id(),
            &storage,
        )
        .unwrap();
        let contents: toml::Table = toml::from_str(&placement.config(&storage).unwrap()).unwrap();
        assert_eq!(
            contents["storage"]["url"].as_str(),
            Some("s3://customer/one/a%20b%23c")
        );
        assert_eq!(
            contents["storage"]["encryption_password"].as_str(),
            Some("$${PRIVATE_PASSWORD}")
        );
        assert_eq!(contents["aws"]["session_token"].as_str(), Some("a\"b"));
        assert_eq!(contents["aws"]["region"].as_str(), Some("auto"));
        assert_eq!(
            contents["servers"]["nbd"]["unix_socket"].as_str(),
            Some("/run/nibrunner-customer/app-1/nbd.sock")
        );
    }
}
