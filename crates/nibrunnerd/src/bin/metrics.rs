//! Writes `crates/nibrunnerd/metrics.json` — every series the scrape page publishes — to the file
//! it is given, from `domain::metrics::declared`. `just metrics` runs it; `just check-metrics`
//! fails when the file checked in is behind it, and the docs site renders its reference page
//! from it.

use std::path::PathBuf;
use std::process::ExitCode;

use nibrunnerd::domain::metrics::{declared, Metric};

fn main() -> ExitCode {
    let Some(path) = std::env::args_os().nth(1).map(PathBuf::from) else {
        eprintln!("usage: metrics <file>");
        return ExitCode::FAILURE;
    };
    if let Err(error) = write(&path) {
        eprintln!("metrics: {} could not be written: {error}", path.display());
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

/// One declaration as this listing carries it. Written here rather than derived on [`Metric`], so
/// what a reader of the file is handed is this file's contract and not whatever a domain type
/// happens to serialize as.
fn entry(metric: &Metric) -> serde_json::Value {
    serde_json::json!({
        "name": metric.name,
        "help": metric.help,
        "kind": metric.kind.as_str(),
        "labels": metric.labels,
    })
}

fn write(path: &std::path::Path) -> std::io::Result<()> {
    let catalogue: Vec<serde_json::Value> = declared().into_iter().map(entry).collect();
    let mut json = serde_json::to_string_pretty(&catalogue)?;
    json.push('\n');
    std::fs::write(path, json)
}
