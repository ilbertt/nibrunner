//! Writes `deploy/config.example.toml` — the configuration with every section in it — to the file
//! it is given, from `HostConfig::example`. `just config-example` runs it; `just check-config-example`
//! fails when the file checked in is behind it.

use std::path::PathBuf;
use std::process::ExitCode;

use nibrunnerd::config::HostConfig;

const HEADER: &str = "\
# Written by `just config-example` from `HostConfig::example` in crates/nibrunnerd/src/config.rs.
# Edit that, not this: `just check-config-example` fails when this file is behind it.
#
# A host with every section: volumes in an object store, reached from the guest over NBD;
# artifacts and exports in S3; TLS behind an edge, which presents a client certificate; raw ports
# for a relay; a metrics page. What each key means is docs/config.md. A host with no
# configuration is given the smallest one instead, by `nibrunnerd install`.
#
# Nothing here is a secret. The AWS credentials and the ZeroFS encryption password live in
# host.env beside this file, which `install` creates empty and never writes into.

";

fn main() -> ExitCode {
    let Some(path) = std::env::args_os().nth(1).map(PathBuf::from) else {
        eprintln!("usage: config-example <file>");
        return ExitCode::FAILURE;
    };
    let rendered = format!("{HEADER}{}", HostConfig::example().to_toml());
    if let Err(error) = std::fs::write(&path, rendered) {
        eprintln!("config-example: {} could not be written: {error}", path.display());
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}
