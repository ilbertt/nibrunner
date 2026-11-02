use std::fmt;

use serde::{Deserialize, Serialize};

use crate::{AppId, DeploymentId, GuestPath, SqliteSessionId, REDACTED};

pub const SQLITE_MAX_STATEMENT_BYTES: usize = 65_536;
pub const SQLITE_MAX_VALUE_BYTES: usize = 65_536;
pub const SQLITE_MAX_PARAMETERS: usize = 256;
pub const SQLITE_MAX_COLUMNS: usize = 256;
pub const SQLITE_MAX_ROWS: usize = 1000;
pub const SQLITE_MAX_BODY_BYTES: usize = 4_194_304;
pub const SQLITE_MAX_RPC_BYTES: usize = SQLITE_MAX_BODY_BYTES + 16_384;
pub const SQLITE_MAX_PIPELINE_REQUESTS: usize = 64;
pub const SQLITE_MAX_BATCH_STEPS: usize = 256;
pub const SQLITE_MAX_ERROR_BYTES: usize = 1024;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct SqliteQuery {
    pub app_id: AppId,
    pub deployment_id: DeploymentId,
    pub session_id: SqliteSessionId,
    pub operation: SqliteOperation,
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum SqliteOperation {
    Open { root: GuestPath, path: GuestPath },
    Pipeline { body: HranaPipelineRequest },
    Close,
}

impl fmt::Debug for SqliteOperation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(REDACTED)
    }
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "status", rename_all = "lowercase")]
pub enum SqliteOutcome {
    Opened,
    Closed,
    Responded { body: HranaPipelineResponse },
    Failed { code: String, message: String },
}

impl fmt::Debug for SqliteOutcome {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(REDACTED)
    }
}

// Hrana v2 uses decimal strings for integers so JSON never rounds a SQLite int64.
// Wire provenance and the upstream license are in vendor/hrana-v2/NOTICE.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum HranaValue {
    Null,
    Integer { value: String },
    Float { value: f64 },
    Text { value: String },
    Blob { base64: String },
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct HranaNamedArg {
    pub name: String,
    pub value: HranaValue,
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct HranaSql {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sql: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sql_id: Option<i32>,
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct HranaStatement {
    #[serde(flatten)]
    pub source: HranaSql,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<HranaValue>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub named_args: Vec<HranaNamedArg>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub want_rows: Option<bool>,
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum HranaBatchCondition {
    Ok { step: i32 },
    Error { step: i32 },
    Not { cond: Box<HranaBatchCondition> },
    And { conds: Vec<HranaBatchCondition> },
    Or { conds: Vec<HranaBatchCondition> },
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct HranaBatchStep {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub condition: Option<HranaBatchCondition>,
    pub stmt: HranaStatement,
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct HranaBatch {
    pub steps: Vec<HranaBatchStep>,
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HranaRequest {
    Execute {
        stmt: HranaStatement,
    },
    Batch {
        batch: HranaBatch,
    },
    Describe {
        #[serde(flatten)]
        source: HranaSql,
    },
    Sequence {
        #[serde(flatten)]
        source: HranaSql,
    },
    StoreSql {
        sql_id: i32,
        sql: String,
    },
    CloseSql {
        sql_id: i32,
    },
    Close,
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct HranaPipelineRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baton: Option<String>,
    pub requests: Vec<HranaRequest>,
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct HranaColumn {
    pub name: Option<String>,
    pub decltype: Option<String>,
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct HranaStatementResult {
    pub cols: Vec<HranaColumn>,
    pub rows: Vec<Vec<HranaValue>>,
    pub affected_row_count: u64,
    pub last_insert_rowid: Option<String>,
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct HranaError {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    pub message: String,
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct HranaBatchResult {
    pub step_results: Vec<Option<HranaStatementResult>>,
    pub step_errors: Vec<Option<HranaError>>,
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct HranaParameter {
    pub name: Option<String>,
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct HranaDescribeResult {
    pub params: Vec<HranaParameter>,
    pub cols: Vec<HranaDescribeColumn>,
    pub is_explain: bool,
    pub is_readonly: bool,
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct HranaDescribeColumn {
    pub name: String,
    pub decltype: Option<String>,
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HranaResponse {
    Execute { result: HranaStatementResult },
    Batch { result: HranaBatchResult },
    Describe { result: HranaDescribeResult },
    Sequence,
    StoreSql,
    CloseSql,
    Close,
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum HranaStreamResult {
    Ok { response: HranaResponse },
    Error { error: HranaError },
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct HranaPipelineResponse {
    pub baton: Option<String>,
    pub base_url: Option<String>,
    pub results: Vec<HranaStreamResult>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sqlite_query_debug_output_keeps_sql_paths_and_values_private() {
        let query: SqliteQuery = serde_json::from_str(
            r#"{
          "appId":"app-1","deploymentId":"dep-1","sessionId":"session-1",
          "operation":{"type":"pipeline","body":{"baton":null,"requests":[
            {"type":"execute","stmt":{"sql":"SELECT 'tenant-secret'","args":[]}}
          ]}}
        }"#,
        )
        .unwrap();
        assert!(!format!("{query:?}").contains("tenant-secret"));
        assert!(serde_json::to_string(&query).unwrap().contains("tenant-secret"));
    }

    #[test]
    fn hrana_integer_and_blob_values_round_trip_without_json_number_conversion() {
        let value = serde_json::json!({
            "baton": null, "base_url": null, "results": [{
              "type":"ok", "response":{"type":"execute","result":{
                "cols":[{"name":"integer","decltype":null},{"name":"blob","decltype":"BLOB"}],
                "rows":[[{"type":"integer","value":"9223372036854775807"},{"type":"blob","base64":"AAH/"}]],
                "affected_row_count":0,"last_insert_rowid":null
              }}
            }]
        });
        let parsed: HranaPipelineResponse = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(serde_json::to_value(parsed).unwrap(), value);
    }
}
