use chrono::{DateTime, Utc};
use guest_contract::cron_registration::{RegistrationReply, RegistrationRequest, STATUS_OK, STATUS_REJECTED};
use protocol::{AppId, DeploymentId};

use super::registry::CronRegistry;

pub async fn answer(
    registry: &CronRegistry,
    app_id: &AppId,
    deployment_id: &DeploymentId,
    request: RegistrationRequest,
    after: DateTime<Utc>,
) -> RegistrationReply {
    let result = match request {
        RegistrationRequest::Replace(text) => registry
            .replace(app_id, deployment_id, text.expose(), after)
            .await
            .map(|()| String::new()),
        RegistrationRequest::List => registry
            .list(app_id, deployment_id)
            .await
            .map(|text| text.expose().to_owned()),
    };
    match result {
        Ok(text) => RegistrationReply {
            status: STATUS_OK,
            text,
        },
        Err(error) => RegistrationReply {
            status: STATUS_REJECTED,
            text: error.to_string(),
        },
    }
}
