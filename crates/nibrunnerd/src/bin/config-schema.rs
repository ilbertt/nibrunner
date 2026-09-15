//! Writes `deploy/config.schema.json` — the configuration file as a JSON Schema — to the file it
//! is given, from `HostConfig::schema`. `just config-schema` runs it; `just check-config-schema`
//! fails when the file checked in is behind it.

use std::path::PathBuf;
use std::process::ExitCode;

use nibrunnerd::config::HostConfig;

fn main() -> ExitCode {
    let Some(path) = std::env::args_os().nth(1).map(PathBuf::from) else {
        eprintln!("usage: config-schema <file>");
        return ExitCode::FAILURE;
    };
    if let Err(error) = write(&path) {
        eprintln!("config-schema: {} could not be written: {error}", path.display());
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

fn write(path: &std::path::Path) -> std::io::Result<()> {
    let mut json = serde_json::to_string_pretty(&HostConfig::schema())?;
    json.push('\n');
    std::fs::write(path, json)
}
