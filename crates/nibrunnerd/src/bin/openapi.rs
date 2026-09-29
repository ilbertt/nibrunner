//! Writes `crates/nibrunnerd/filesystem.openapi.json` — the socket that lists what a guest holds,
//! as an OpenAPI document — to the file it is given, from `adapters::filesystem::openapi`.
//! `just openapi` runs it; `just check-openapi` fails when the file checked in is behind it, and
//! the docs site renders its reference page from it.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use nibrunnerd::adapters::filesystem::openapi;

fn main() -> ExitCode {
    let Some(path) = std::env::args_os().nth(1).map(PathBuf::from) else {
        eprintln!("usage: openapi <file>");
        return ExitCode::FAILURE;
    };
    if let Err(error) = write(&path) {
        eprintln!("openapi: {} could not be written: {error}", path.display());
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

fn write(path: &Path) -> std::io::Result<()> {
    let mut json = serde_json::to_string_pretty(&openapi())?;
    json.push('\n');
    std::fs::write(path, json)
}
