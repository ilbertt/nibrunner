//! Sleep and wake are the host's business, not the caller's: a request is answered whatever the
//! app was doing when it arrived, and what the app was holding survives.

use std::time::{Duration, Instant};

use protocol::InstanceState;

/// Long enough that a wake which booted from cold instead of restoring would still be caught by
/// what the tenant remembers, and short enough that a hung request fails the test rather than the
/// suite's patience.
const ANSWERED_WITHIN: Duration = Duration::from_secs(15);

/// The shortest a document may name. Nothing here waits one out except the test that is about
/// the timer.
const IDLE_TIMEOUT_MS: u64 = 60_000;

#[tokio::test(flavor = "multi_thread")]
async fn a_request_that_finds_an_app_asleep_is_answered_rather_than_refused() {
    let Some(host) = crate::host().await else {
        return;
    };
    let app = host.tenant(1).on_request(IDLE_TIMEOUT_MS);
    host.deploy(std::slice::from_ref(&app)).await;
    host.until_state(&app.app_id, InstanceState::Running).await;

    host.let_sleep(&app).await;

    let asking = Instant::now();
    let answer = host.get(&app, "/").await.expect("the proxy answers");
    assert_eq!(answer.status, 200, "{answer:?}");
    println!("woken and answered in {:?}", asking.elapsed());
    assert!(
        asking.elapsed() < ANSWERED_WITHIN,
        "the wake took {:?}",
        asking.elapsed()
    );

    host.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_app_that_slept_comes_back_with_what_it_had_in_memory() {
    let Some(host) = crate::host().await else {
        return;
    };
    let app = host.tenant(1).on_request(IDLE_TIMEOUT_MS);
    host.deploy(std::slice::from_ref(&app)).await;
    host.until_state(&app.app_id, InstanceState::Running).await;

    assert_eq!(host.get(&app, "/remember").await.expect("an answer").body, "1");
    assert_eq!(host.get(&app, "/remember").await.expect("an answer").body, "2");

    host.let_sleep(&app).await;

    let after = host.get(&app, "/remember").await.expect("an answer");
    assert_eq!(
        after.body, "3",
        "the microVM was booted afresh rather than restored: the tenant had forgotten"
    );

    host.stop().await;
}

// What the 2026-09-21 run found as 4.6: the proxy keeps an upstream connection for five seconds
// after it is done with it, and a sleep inside that window left the next request riding a
// connection into a guest that was being paused. It hung for the caller's whole timeout.
#[tokio::test(flavor = "multi_thread")]
async fn a_request_is_answered_across_a_sleep_the_proxy_was_holding_a_connection_through() {
    let Some(host) = crate::host().await else {
        return;
    };
    let app = host.tenant(1).on_request(IDLE_TIMEOUT_MS);
    host.deploy(std::slice::from_ref(&app)).await;
    host.until_state(&app.app_id, InstanceState::Running).await;

    let mut slowest = Duration::ZERO;
    for round in 1..=3 {
        // Leaves a pooled connection into the guest, which the sleep has to close behind it.
        let answer = host.get(&app, "/").await.expect("the proxy answers");
        assert_eq!(answer.status, 200, "round {round}: {answer:?}");

        host.let_sleep(&app).await;

        let asking = Instant::now();
        let answer = host.get(&app, "/").await.expect("the proxy answers");
        let took = asking.elapsed();
        slowest = slowest.max(took);
        assert_eq!(answer.status, 200, "round {round}: {answer:?}");
        assert!(
            took < ANSWERED_WITHIN,
            "round {round}: the request across the sleep took {took:?}"
        );
    }
    println!("slowest request across a sleep: {slowest:?}");

    host.stop().await;
}
