//! Writes every schema the protocol crate publishes into the directory it is given.

use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    let Some(directory) = std::env::args_os().nth(1).map(PathBuf::from) else {
        eprintln!("usage: protocol-schema <directory>");
        return ExitCode::FAILURE;
    };
    if let Err(error) = write_all(&directory) {
        eprintln!("protocol-schema: {error}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

fn write_all(directory: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(directory)?;
    for (filename, schema) in protocol::schema::all() {
        let mut json = serde_json::to_string_pretty(&schema)?;
        json.push('\n');
        std::fs::write(directory.join(filename), json)?;
    }
    Ok(())
}
