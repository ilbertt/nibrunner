//! One process. It reads where it keeps things, adopts whatever microVMs it finds still running,
//! converges on the document it watches, and answers for the apps it holds.

use std::sync::Arc;

use nibrunnerd::config::HostConfig;
use nibrunnerd::run;

fn main() -> std::process::ExitCode {
    nibrunnerd::install_crypto_provider();
    install_logger();

    let config = match HostConfig::from_environment() {
        Ok(config) => config,
        Err(error) => {
            tracing::error!(error = %error.message(), "this host is not configured");
            return std::process::ExitCode::FAILURE;
        }
    };

    let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(error) => {
            tracing::error!(%error, "the runtime could not be started");
            return std::process::ExitCode::FAILURE;
        }
    };
    runtime.block_on(serve(config))
}

async fn serve(config: HostConfig) -> std::process::ExitCode {
    let host = match run::build_host(config).await {
        Ok(host) => host,
        Err(error) => {
            tracing::error!(error = %error.to_string(), "this host could not be brought up");
            return std::process::ExitCode::FAILURE;
        }
    };
    tracing::info!(
        state_dir = %host.config.state_dir.display(),
        desired_state_file = %host.config.desired_state_file.display(),
        guest_memory_mib = host.guest_memory_mib,
        firecracker = nibrunnerd::vm::process::FIRECRACKER_VERSION,
        "nibrunnerd starting"
    );

    // Before the first document is read: the microVMs this host is already running are adopted
    // from what an earlier daemon wrote down, so a restart is a non-event for a tenant.
    host.load().await;
    let adopted = host.vms.adopted_app_ids().await;
    if !adopted.is_empty() {
        tracing::info!(adopted = adopted.len(), "microVMs from an earlier daemon adopted");
    }
    // A port answered before the first pass, because an app this host stopped has no forward and
    // its port would otherwise refuse connections rather than saying why.
    nibrunnerd::reconcile::network::apply_activators(&host).await;
    run::serve_proxy(&host);

    let loops = vec![
        tokio::spawn(run::converge_loop(host.clone())),
        tokio::spawn(run::status_loop(host.clone())),
        tokio::spawn(run::measurement_loop(host.clone())),
    ];

    shutdown().await;
    // Nothing stops a tenant: the microVMs are in sessions of their own, so what this cancels is
    // only the daemon's own work. That is what makes redeploying this component a non-event.
    tracing::info!("nibrunnerd stopping; every microVM on this host keeps running");
    for task in loops {
        task.abort();
    }
    persist_on_the_way_out(&host).await;
    std::process::ExitCode::SUCCESS
}

async fn persist_on_the_way_out(host: &Arc<nibrunnerd::host::Host>) {
    host.persist().await;
    let report = nibrunnerd::report::writer::build(host, run::host_versions(host)).await;
    nibrunnerd::report::writer::write(&nibrunnerd::report::writer::reported_state_file(host), &report);
}

#[cfg(unix)]
async fn shutdown() {
    use tokio::signal::unix::{signal, SignalKind};
    let mut terminate = match signal(SignalKind::terminate()) {
        Ok(terminate) => terminate,
        Err(_) => return std::future::pending().await,
    };
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = terminate.recv() => {}
    }
}

#[cfg(not(unix))]
async fn shutdown() {
    let _ = tokio::signal::ctrl_c().await;
}

/// JSON on stderr, one line per event. The `<n>` prefix is the only thing a journal reads a
/// severity from: without it every line a service writes is recorded at `info`, including a stack
/// trace, because the priority is a property of the stream rather than of what travels down it.
fn install_logger() {
    use tracing_subscriber::prelude::*;
    let filter = tracing_subscriber::EnvFilter::try_from_env("NIBRUNNER_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer().json().with_writer(std::io::stderr).with_current_span(false))
        .init();
}
