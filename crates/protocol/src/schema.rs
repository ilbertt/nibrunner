use schemars::generate::SchemaSettings;
use schemars::{JsonSchema, Schema};

use crate::{HostDesiredState, HostReportedState};

pub const SCHEMA_ID_BASE: &str =
    "https://raw.githubusercontent.com/ilbertt/nibrunner/main/crates/protocol/schema/";

pub const DESIRED_STATE_SCHEMA: &str = "desired-state.schema.json";
pub const REPORTED_STATE_SCHEMA: &str = "reported-state.schema.json";

/// The document a host converges on, as the JSON Schema of [`HostDesiredState`].
pub fn desired_state() -> Schema {
    published::<HostDesiredState>(DESIRED_STATE_SCHEMA)
}

/// The document a host writes back, as the JSON Schema of [`HostReportedState`].
pub fn reported_state() -> Schema {
    published::<HostReportedState>(REPORTED_STATE_SCHEMA)
}

/// Every schema this crate publishes, each under the filename its `$id` ends in.
pub fn all() -> [(&'static str, Schema); 2] {
    [
        (DESIRED_STATE_SCHEMA, desired_state()),
        (REPORTED_STATE_SCHEMA, reported_state()),
    ]
}

fn published<T: JsonSchema>(filename: &str) -> Schema {
    let mut schema = SchemaSettings::draft2020_12()
        .into_generator()
        .into_root_schema_for::<T>();
    schema.insert("$id".into(), format!("{SCHEMA_ID_BASE}{filename}").into());
    schema
}
