use serde::{Deserialize, Serialize};

use crate::{AppId, DeploymentId, InvalidValue, SecretString, TenantEnvironment, REDACTED};

pub const MAX_CRONTAB_BYTES: usize = 65_536;
pub const MAX_CRON_ENVIRONMENT_VARIABLES: usize = 256;
pub const MAX_CRON_SCHEDULE_LENGTH: usize = 256;
pub const MAX_CRON_COMMAND_LENGTH: usize = 4096;

fn is_nonempty_line(value: &str, limit: usize) -> bool {
    value.chars().count() <= limit
        && !value.trim_matches([' ', '\t']).is_empty()
        && !value.contains(['\0', '\r', '\n'])
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct CronSchedule(String);

impl CronSchedule {
    pub fn parse(value: impl Into<String>) -> Result<Self, InvalidValue> {
        let value = value.into();
        if !is_nonempty_line(&value, MAX_CRON_SCHEDULE_LENGTH) {
            return Err(InvalidValue::new_public(
                "a cron schedule must be a nonempty line within the length limit",
            ));
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for CronSchedule {
    type Error = InvalidValue;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

impl From<CronSchedule> for String {
    fn from(value: CronSchedule) -> Self {
        value.0
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "SecretString", into = "SecretString")]
pub struct CronCommand(SecretString);

impl CronCommand {
    pub fn parse(value: impl Into<String>) -> Result<Self, InvalidValue> {
        Self::try_from(SecretString::parse(value)?)
    }

    pub fn expose(&self) -> &str {
        self.0.expose()
    }
}

impl TryFrom<SecretString> for CronCommand {
    type Error = InvalidValue;

    fn try_from(value: SecretString) -> Result<Self, Self::Error> {
        if !is_nonempty_line(value.expose(), MAX_CRON_COMMAND_LENGTH) {
            return Err(InvalidValue::new_public(
                "a cron command must be a nonempty line within the length limit",
            ));
        }
        Ok(Self(value))
    }
}

impl From<CronCommand> for SecretString {
    fn from(value: CronCommand) -> Self {
        value.0
    }
}

impl std::fmt::Debug for CronCommand {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(REDACTED)
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Crontab(String);

impl Crontab {
    pub fn parse(value: impl Into<String>) -> Result<Self, InvalidValue> {
        let value = value.into();
        if value.len() > MAX_CRONTAB_BYTES || value.contains('\0') {
            return Err(InvalidValue::new_public(
                "a crontab must fit within the byte limit and contain no nul bytes",
            ));
        }
        Ok(Self(value))
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for Crontab {
    type Error = InvalidValue;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

impl From<Crontab> for String {
    fn from(value: Crontab) -> Self {
        value.0
    }
}

impl std::fmt::Debug for Crontab {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(REDACTED)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "CronJobFields", into = "CronJobFields")]
pub struct CronJobDefinition {
    pub schedule: CronSchedule,
    pub command: CronCommand,
    pub environment: Option<TenantEnvironment>,
}

#[derive(Clone, Serialize, Deserialize)]
struct CronJobFields {
    schedule: CronSchedule,
    command: CronCommand,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    environment: Option<TenantEnvironment>,
}

impl TryFrom<CronJobFields> for CronJobDefinition {
    type Error = InvalidValue;

    fn try_from(value: CronJobFields) -> Result<Self, Self::Error> {
        if value
            .environment
            .as_ref()
            .is_some_and(|environment| environment.len() > MAX_CRON_ENVIRONMENT_VARIABLES)
        {
            return Err(InvalidValue::new_public(
                "a cron job names too many environment variables",
            ));
        }
        Ok(Self {
            schedule: value.schedule,
            command: value.command,
            environment: value.environment,
        })
    }
}

impl From<CronJobDefinition> for CronJobFields {
    fn from(value: CronJobDefinition) -> Self {
        Self {
            schedule: value.schedule,
            command: value.command,
            environment: value.environment,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CronJobDefinitions(Vec<CronJobDefinition>);

impl CronJobDefinitions {
    pub fn validate_limit(&self, maximum: usize) -> Result<(), InvalidValue> {
        if self.0.len() > maximum {
            return Err(InvalidValue::new_public("an app names too many cron jobs"));
        }
        Ok(())
    }

    pub fn iter(&self) -> impl Iterator<Item = &CronJobDefinition> {
        self.0.iter()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl From<Vec<CronJobDefinition>> for CronJobDefinitions {
    fn from(value: Vec<CronJobDefinition>) -> Self {
        Self(value)
    }
}

impl From<CronJobDefinitions> for Vec<CronJobDefinition> {
    fn from(value: CronJobDefinitions) -> Self {
        value.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CronTable {
    pub app_id: AppId,
    pub deployment_id: DeploymentId,
    pub jobs: CronJobDefinitions,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub crontab: Option<Crontab>,
}
