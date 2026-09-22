//! When an app is allowed to go: the one invariant here waits out a real idle timeout, which a
//! document may not set under a minute, so it is kept apart from the rest and given a CI shard of
//! its own rather than made everything else queue behind it.

use std::time::{Duration, Instant};

use protocol::InstanceState;

/// The shortest a document may name.
const IDLE_TIMEOUT_MS: u64 = 60_000;

// The 2026-09-18 and 2026-09-21 runs both found loaded apps snapshotted under traffic, because
// the activity reading was minutes behind. Two apps, one timeout, one wait: the busy one must
// still be up and the quiet one must be asleep.
#[tokio::test(flavor = "multi_thread")]
async fn an_app_sleeps_because_it_is_quiet_and_not_because_time_passed() {
    let Some(host) = crate::host().await else {
        return;
    };
    let busy = host.tenant(1).on_request(IDLE_TIMEOUT_MS);
    let quiet = host.tenant(2).on_request(IDLE_TIMEOUT_MS);
    host.deploy(&[busy.clone(), quiet.clone()]).await;
    host.until_state(&busy.app_id, InstanceState::Running).await;
    host.until_state(&quiet.app_id, InstanceState::Running).await;

    let watching = Instant::now();
    let mut busy_was_let_go = Vec::new();
    while watching.elapsed() < Duration::from_millis(IDLE_TIMEOUT_MS + 20_000) {
        let answer = host.get(&busy, "/").await.expect("the proxy answers");
        assert_eq!(answer.status, 200, "{answer:?}");
        if let Some(state) = host.instance(&busy.app_id).await.map(|held| held.state) {
            if state != InstanceState::Running {
                busy_was_let_go.push(format!("{:?} at {:?}", state, watching.elapsed()));
            }
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }

    assert!(
        busy_was_let_go.is_empty(),
        "an app under traffic was let go: {}",
        busy_was_let_go.join(", ")
    );
    assert_eq!(
        host.instance(&quiet.app_id).await.map(|held| held.state),
        Some(InstanceState::Idle),
        "an app nobody asked for in {IDLE_TIMEOUT_MS}ms is still up"
    );

    host.stop().await;
}
