//! What drives this daemon, and the only things called from outside it.
//!
//! Three loops. One converges on the document, one probes what is running and writes the report,
//! one measures activity and lets quiet apps sleep. They own the timing and nothing else: what to
//! do is `services`, and where to keep it is `repositories`.
//!
//! Loops rather than endpoints, because there is no command surface. The nearest thing to a
//! request arriving is the proxy in `adapters`, and what that does is wake an app — a reflex
//! rather than a verb anybody calls.

use std::sync::Arc;
use std::time::Duration;

use crate::desired::DesiredStateWatch;
use crate::host::Host;
use crate::run::host_versions;
use crate::services::control_plane::SessionHolder;

/// One tick of the status loop on a host where nothing is settling. A probe cannot land sooner
/// than the tick that runs it, so the two share one cadence.
const STATUS_TICK: Duration = Duration::from_secs(1);

/// The tick while something is still coming up, which is the only time a faster one buys anything.
const SETTLING_TICK: Duration = Duration::from_millis(crate::services::health::STARTUP_PROBE_INTERVAL_MS);

/// The floor a signal cannot get under. A refresh probes every instance that is due and writes
/// the host's state to disk, so left ungated a host waking apps steadily would run it as fast as
/// the disk allows — and a host with enough on-request apps to be waking them steadily is the one
/// this whole feature is for.
const MIN_REFRESH_GAP: Duration = Duration::from_millis(250);

/// How often each guest is measured, and the window every activity reading is taken over. Slower
/// than the status tick because the counters only move at the speed a tenant is used.
const MEASUREMENT_INTERVAL: Duration = Duration::from_secs(60);

/// Converges against the last document this host was given before anything has written a new one,
/// then watches for one. A file that is not there yet is a host with nothing to run, which is the
/// ordinary state of a fresh machine rather than a failure.
pub async fn converge_loop(host: Arc<Host>) {
    let watch = DesiredStateWatch::on(&host.config.desired_state_file);
    // The cached copy first: a restart during an outage of whatever writes the file is a
    // non-event, because the host still knows what it is supposed to be running.
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
                    // Only a document moving runs a pass, and work the last one deferred does not
                    // move it — so a volume waiting on an instance to stop would never be carried.
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

/// Probes what is due, puts the result in the kernel and the routes, and writes down what
/// changed. Raced rather than slept, because the length is chosen from the state as it stands
/// now: a microVM that comes up during it is one this decision could not have known about.
pub async fn status_loop(host: Arc<Host>) {
    let versions = host_versions(&host);
    let reported_state_file = crate::services::report::writer::reported_state_file(&host);
    loop {
        crate::services::reconcile::refresh(&host).await;
        let report = crate::services::report::writer::build(&host, versions.clone()).await;
        crate::services::report::writer::write(&reported_state_file, &report);

        let now = crate::clock::now_ms();
        // Exactly the condition the fast probe grid runs on, rather than the states it tends to
        // appear in: a tick taken for something no longer being probed that fast is one taken for
        // as long as that instance is up.
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

/// Measures what the host's counters say, decides which apps have gone quiet, and lets them
/// sleep. Deciding straight after measuring, because the measurement is the only thing that moves
/// the answer: an app is let go on the reading that found it quiet rather than on a tick that
/// happened to come later.
pub async fn measurement_loop(host: Arc<Host>) {
    loop {
        tokio::time::sleep(MEASUREMENT_INTERVAL).await;
        crate::services::reconcile::idle::record_activity(&host).await;
        crate::services::reconcile::idle::apply_sleep(&host).await;
        // After the sleep and not before it, so an app that has just been let go is measured as
        // the asleep app it now is rather than reported spending what it spent on the way down.
        crate::services::usage::measure(&host).await;
    }
}

/// A floor under one poll rather than a pause between two.
///
/// On the path that matters nothing is left to wait out: the control plane holds an idle request
/// open until a read arrives, so a poll that came back with nothing has already spent longer than
/// this and asks again at once. What the floor is for is a control plane that answers "none" the
/// instant it is asked — which is what one deployed before the hold does, and one of those stands
/// in front of this daemon for the length of every rollout. Without it the two would spin against
/// each other as fast as the link allows.
///
/// Reading back to back still cannot run away: a query exists only while its caller waits, so
/// demand bounds the rate and demand stops when people stop asking.
const IDLE_POLL_FLOOR: Duration = Duration::from_secs(5);

/// How long this host waits before asking a control plane that would not answer at all.
const CONTROL_PLANE_BACKOFF: Duration = Duration::from_secs(15);

/// Polls the control plane and writes what comes back into the file the converge loop watches.
///
/// Started only where a host was given a control plane to talk to. One that was not runs on the
/// file alone, which is the ordinary single-machine case and the reason the reconciler has exactly
/// one source either way.
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

/// Collects one read of a tenant's files at a time and answers it.
///
/// Deliberately not part of the reconcile. A read observes the host without changing it, so a slow
/// device delays the person waiting on it and nothing else — where a backlog of reads inside the
/// pass could hold up a stop. Nor may the request this loop spends most of its life parked on.
pub async fn filesystem_loop(host: Arc<Host>, sessions: Arc<SessionHolder>) {
    loop {
        let started = tokio::time::Instant::now();
        match crate::services::control_plane::answer_one_query(&host, &sessions).await {
            Ok(Some(query)) => {
                tracing::info!(query_id = %query.query_id, app_id = %query.app_id, "a read was answered")
            }
            // Nothing was waiting. The control plane held the request open for as long as it had
            // nothing to give, so what is left of the floor is usually nothing at all.
            Ok(None) => tokio::time::sleep(IDLE_POLL_FLOOR.saturating_sub(started.elapsed())).await,
            Err(error) => {
                sessions.note(&error).await;
                tracing::warn!(error = %error.message(), "a read could not be collected");
                tokio::time::sleep(CONTROL_PLANE_BACKOFF).await;
            }
        }
    }
}
