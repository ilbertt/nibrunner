//! Writes one sample of every series the scrape page declares to the file it is given, for
//! `just lint` to hand promtool. A sample rather than a description, because promtool rates a page
//! by what it emits — and a file rather than a pipe, so that failing to write one fails the lint
//! instead of leaving promtool reading nothing and finding nothing wrong with it.

use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    let Some(path) = std::env::args_os().nth(1).map(PathBuf::from) else {
        eprintln!("usage: metrics-page <file>");
        return ExitCode::FAILURE;
    };
    let page = nibrunnerd::domain::metrics::page_of_every_series();
    if let Err(error) = std::fs::write(&path, page) {
        eprintln!("metrics-page: {} could not be written: {error}", path.display());
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}
