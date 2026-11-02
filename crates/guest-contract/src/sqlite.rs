use protocol::GuestPath;
use std::time::Duration;

pub const MAX_SESSIONS_PER_GUEST: usize = 4;
pub const SESSION_IDLE_TIMEOUT: Duration = Duration::from_secs(30);
pub const PIPELINE_SUFFIX: &str = "/pipeline";
const PREFIX: &str = "/sqlite/";
const VERSION_SUFFIX: &str = "/v2";

#[derive(Debug, thiserror::Error)]
#[error("the SQLite route does not name a database within its root")]
pub struct InvalidSqliteRoute;

pub fn database_route(root: &GuestPath, path: &GuestPath) -> String {
    format!(
        "{PREFIX}{}/{}{VERSION_SUFFIX}",
        percent_encoding::utf8_percent_encode(root.as_str(), percent_encoding::NON_ALPHANUMERIC),
        percent_encoding::utf8_percent_encode(path.as_str(), percent_encoding::NON_ALPHANUMERIC)
    )
}

pub fn opening_route(target: &str) -> Result<(GuestPath, GuestPath), InvalidSqliteRoute> {
    let rest = target
        .strip_prefix(PREFIX)
        .and_then(|path| path.strip_suffix(VERSION_SUFFIX))
        .ok_or(InvalidSqliteRoute)?;
    let (root, path) = rest.split_once('/').ok_or(InvalidSqliteRoute)?;
    if path.contains('/') {
        return Err(InvalidSqliteRoute);
    }
    fn decoded(value: &str) -> Result<GuestPath, InvalidSqliteRoute> {
        let text = percent_encoding::percent_decode_str(value)
            .decode_utf8()
            .map_err(|_| InvalidSqliteRoute)?;
        GuestPath::parse(text.as_ref()).map_err(|_| InvalidSqliteRoute)
    }
    Ok((decoded(root)?, decoded(path)?))
}
