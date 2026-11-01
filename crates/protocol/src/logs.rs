use serde::{Deserialize, Serialize};

use crate::{AppId, DeploymentId, TenantLogStream, TenantRestart, Timestamp};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct TenantLogEvent {
    pub app_id: AppId,
    pub deployment_id: DeploymentId,
    pub source_id: String,
    pub sequence: u64,
    pub observed_at: Timestamp,
    #[serde(flatten)]
    pub body: TenantLogBody,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "kind", rename_all = "kebab-case", rename_all_fields = "camelCase")]
pub enum TenantLogBody {
    Data {
        stream: TenantLogStream,
        text: String,
    },
    CronData {
        stream: TenantLogStream,
        text: String,
        job_id: String,
        run_id: String,
    },
    Gap {
        dropped_bytes: u64,
    },
    Restart(TenantRestart),
}
