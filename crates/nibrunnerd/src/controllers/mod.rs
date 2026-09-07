use std::sync::Arc;
use std::time::Duration;

use crate::desired::DesiredStateWatch;
use crate::host::Host;
use crate::run::host_versions;
use crate::services::control_plane::SessionHolder;

const STATUS_TICK: Duration = Duration::from_secs(1);

const SETTLING_TICK: Duration = Duration::from_millis(crate::services::health::STARTUP_PROBE_INTERVAL_MS);

const MIN_REFRESH_GAP: Duration = Duration::from_millis(250);

const MEASUREMENT_INTERVAL: Duration = Duration::from_secs(60);

pub async fn converge_loop(host: Arc<Host>) {
    let watch = DesiredStateWatch::on(&host.config.desired_state_file);
    if let Some(cached) = host.cached_desired_state().await {
        if host.cache.lock().await.accept(cached.clone()) {
            crate::services::reconcile::reconcile(&host, &cached).await;
        }
    }
    loop {
        match crate::desired::read_desired_state(&host.config.desired_state_file) {
            Ok(Some(desired)) => {
                let news = host.cache.lock().await.accept(desired.clone());
                if news {
                    let _ = crate::desired::cache_desired_state(
                        &host.config.cached_desired_state_file(),
                        &desired,
                    );
                    crate::services::reconcile::reconcile(&host, &desired).await;
                } else if host.state.snapshot().await.deferred_work {
                    crate::services::reconcile::reconcile(&host, &desired).await;
                }
            }
            Ok(None) => {}
            Err(error) => {
                tracing::error!(error = %error.message(), "the desired state file was not read");
            }
        }
        watch.changed().await;
    }
}

pub async fn status_loop(host: Arc<Host>) {
    let versions = host_versions(&host);
    let reported_state_file = crate::services::report::writer::reported_state_file(&host);
    loop {
        crate::services::reconcile::refresh(&host).await;
        let report = crate::services::report::writer::build(&host, versions.clone()).await;
        crate::services::report::writer::write(&reported_state_file, &report);

        let now = crate::clock::now_ms();
        let settling = host.state.records().await.iter().any(|record| {
            crate::services::health::is_on_startup_grid(&record.health, &record.grace_inputs(now))
        });
        let tick = if settling { SETTLING_TICK } else { STATUS_TICK };
        tokio::select! {
            _ = tokio::time::sleep(tick) => {}
            _ = async {
                tokio::time::sleep(MIN_REFRESH_GAP).await;
                host.state.refresh_signalled().await;
            } => {}
        }
    }
}

pub async fn measurement_loop(host: Arc<Host>) {
    loop {
        tokio::time::sleep(MEASUREMENT_INTERVAL).await;
        crate::services::reconcile::idle::record_activity(&host).await;
        crate::services::reconcile::idle::apply_sleep(&host).await;
        crate::services::usage::measure(&host).await;
    }
}

const IDLE_POLL_FLOOR: Duration = Duration::from_secs(5);

const CONTROL_PLANE_BACKOFF: Duration = Duration::from_secs(15);

pub async fn control_plane_loop(host: Arc<Host>, sessions: Arc<SessionHolder>) {
    loop {
        match crate::services::control_plane::poll_desired_state(&host, &sessions).await {
            Ok(true) => tracing::info!("the control plane gave this host a new document"),
            Ok(false) => {}
            Err(error) => {
                sessions.note(&error).await;
                tracing::warn!(error = %error.message(), "the control plane could not be polled");
                tokio::time::sleep(CONTROL_PLANE_BACKOFF).await;
            }
        }
        tokio::time::sleep(IDLE_POLL_FLOOR).await;
    }
}

pub async fn filesystem_loop(host: Arc<Host>, sessions: Arc<SessionHolder>) {
    loop {
        let started = tokio::time::Instant::now();
        match crate::services::control_plane::answer_one_query(&host, &sessions).await {
            Ok(Some(query)) => {
                tracing::info!(query_id = %query.query_id, app_id = %query.app_id, "a read was answered")
            }
            Ok(None) => tokio::time::sleep(IDLE_POLL_FLOOR.saturating_sub(started.elapsed())).await,
            Err(error) => {
                sessions.note(&error).await;
                tracing::warn!(error = %error.message(), "a read could not be collected");
                tokio::time::sleep(CONTROL_PLANE_BACKOFF).await;
            }
        }
    }
}
