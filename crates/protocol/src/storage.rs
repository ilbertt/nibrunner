use serde::{de::Error as _, Deserialize, Deserializer, Serialize};

use crate::{ObjectKey, SecretString};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct S3VolumeStorage {
    #[serde(deserialize_with = "https_endpoint")]
    #[cfg_attr(feature = "schema", schemars(regex(pattern = r"^https://[^\s]+$")))]
    pub endpoint: String,
    #[serde(deserialize_with = "bucket_name")]
    #[cfg_attr(
        feature = "schema",
        schemars(regex(pattern = "^[a-z0-9][a-z0-9.-]{1,61}[a-z0-9]$"))
    )]
    pub bucket: String,
    pub prefix: ObjectKey,
    /// Null leaves the signing region unspecified.
    #[serde(default, deserialize_with = "region")]
    #[cfg_attr(feature = "schema", schemars(length(min = 1)))]
    pub region: Option<String>,
    pub auth: S3VolumeAuth,
    #[serde(deserialize_with = "credential")]
    #[cfg_attr(feature = "schema", schemars(length(min = 1)))]
    pub encryption_password: SecretString,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(
    tag = "kind",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum S3VolumeAuth {
    Anonymous {},
    AccessKey {
        #[serde(deserialize_with = "credential")]
        #[cfg_attr(feature = "schema", schemars(length(min = 1)))]
        access_key_id: SecretString,
        #[serde(deserialize_with = "credential")]
        #[cfg_attr(feature = "schema", schemars(length(min = 1)))]
        secret_access_key: SecretString,
    },
    Session {
        #[serde(deserialize_with = "credential")]
        #[cfg_attr(feature = "schema", schemars(length(min = 1)))]
        access_key_id: SecretString,
        #[serde(deserialize_with = "credential")]
        #[cfg_attr(feature = "schema", schemars(length(min = 1)))]
        secret_access_key: SecretString,
        #[serde(deserialize_with = "credential")]
        #[cfg_attr(feature = "schema", schemars(length(min = 1)))]
        session_token: SecretString,
    },
}

fn https_endpoint<'de, D: Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
    let endpoint = String::deserialize(deserializer)?;
    if endpoint
        .strip_prefix("https://")
        .is_some_and(|rest| !rest.is_empty() && !rest.contains(char::is_whitespace))
    {
        Ok(endpoint)
    } else {
        Err(D::Error::custom("volume storage requires an HTTPS endpoint"))
    }
}

fn bucket_name<'de, D: Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
    let bucket = String::deserialize(deserializer)?;
    let edge = |c: u8| c.is_ascii_lowercase() || c.is_ascii_digit();
    let bytes = bucket.as_bytes();
    if (3..=63).contains(&bytes.len())
        && edge(bytes[0])
        && edge(bytes[bytes.len() - 1])
        && bucket.bytes().all(|c| edge(c) || matches!(c, b'.' | b'-'))
    {
        Ok(bucket)
    } else {
        Err(D::Error::custom("invalid volume storage bucket name"))
    }
}

fn region<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<String>, D::Error> {
    let region = Option::<String>::deserialize(deserializer)?;
    if region.as_deref() == Some("") {
        Err(D::Error::custom("volume storage region is empty"))
    } else {
        Ok(region)
    }
}

fn credential<'de, D: Deserializer<'de>>(deserializer: D) -> Result<SecretString, D::Error> {
    let secret = SecretString::deserialize(deserializer)?;
    if secret.expose().is_empty() {
        Err(D::Error::custom("volume storage credential is empty"))
    } else {
        Ok(secret)
    }
}

pub(crate) fn storage<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<S3VolumeStorage>, D::Error> {
    Option::<S3VolumeStorage>::deserialize(deserializer)
        .map_err(|_| D::Error::custom("invalid volume storage configuration"))
}
