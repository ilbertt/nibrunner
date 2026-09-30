//! A volume belongs to exactly one app, and holds what that app put in it for as long as the
//! document names it — across a sleep, a stop, and a release that replaces the one before it.

use nibrunnerd::test_support::machine::{RunningHost, Tenant};
use protocol::{DesiredInstanceState, InstanceState};

const KEPT: &str = "kept";

#[tokio::test(flavor = "multi_thread")]
async fn a_reused_slot_serves_only_its_new_app_and_carries_none_of_the_old_apps_state() {
    let Some(host) = crate::host_with(|config| config.max_apps = 1).await else {
        return;
    };
    let old = host.tenant(1).on_request(60_000);
    host.deploy(std::slice::from_ref(&old)).await;
    host.until_state(&old.app_id, InstanceState::Running).await;
    let original_slot = host.host.slot_of(&old.app_id).await.expect("a slot");
    assert_eq!(host.get(&old, "/remember").await.expect("an answer").body, "1");
    assert_eq!(
        host.get(&old, "/write?path=kept&body=old+app")
            .await
            .expect("a write")
            .status,
        200
    );
    host.let_sleep(&old).await;

    let mut removed = host.document(&[]);
    removed.volumes = vec![old.volume.clone()];
    host.write(&removed).await;
    host.until("the old app to release its slot", |report| {
        report.instances.is_empty()
    })
    .await;
    assert!(host.host.slot_of(&old.app_id).await.is_none());

    let new = host.tenant(2);
    let mut replacement = host.document(std::slice::from_ref(&new));
    replacement.volumes.push(old.volume.clone());
    host.write(&replacement).await;
    host.until_state(&new.app_id, InstanceState::Running).await;
    host.until_routed(&new).await;
    let reused_slot = host.host.slot_of(&new.app_id).await.expect("a reused slot");
    assert_eq!(reused_slot.host_port, original_slot.host_port);
    assert_eq!(reused_slot.guest_ipv4, original_slot.guest_ipv4);
    assert_eq!(
        host.get(&new, "/remember").await.expect("a fresh guest").body,
        "1"
    );
    let absent = host.get(&new, "/read?path=kept").await.expect("a read");
    assert_eq!(
        absent.status, 404,
        "the new app read the old app's volume: {absent:?}"
    );
    let unserved = host.get(&old, "/").await.expect("a refusal for the old hostname");
    assert_ne!(
        unserved.status, 200,
        "the old hostname reached the slot's new owner"
    );
    assert_eq!(
        host.get(&new, "/write?path=kept&body=new+app")
            .await
            .expect("a write")
            .status,
        200
    );
    assert_eq!(kept(&host, &new).await, "new app");
    host.stop().await;
}

/// Waits until the app is answering again. A release that replaces another is reachable a moment
/// after the document says so, and what is being asked here is about the volume rather than about
/// that window.
async fn answering(host: &RunningHost, app: &Tenant) {
    let waiting = std::time::Instant::now();
    loop {
        if matches!(host.get(app, "/").await, Ok(answer) if answer.status == 200) {
            return;
        }
        assert!(
            waiting.elapsed() < std::time::Duration::from_secs(60),
            "{} never came back",
            app.app_id
        );
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
}

async fn kept(host: &RunningHost, app: &Tenant) -> String {
    host.get(app, &format!("/read?path={KEPT}"))
        .await
        .expect("the proxy answers")
        .body
}

#[tokio::test(flavor = "multi_thread")]
async fn what_a_tenant_wrote_is_there_after_a_sleep_a_stop_and_a_redeploy() {
    let Some(host) = crate::host().await else {
        return;
    };
    let app = host.tenant(1).on_request(60_000);
    host.deploy(std::slice::from_ref(&app)).await;
    host.until_state(&app.app_id, InstanceState::Running).await;

    let written = host
        .get(&app, &format!("/write?path={KEPT}&body=what+it+was+given"))
        .await
        .expect("the proxy answers");
    assert_eq!(written.status, 200, "{written:?}");

    host.let_sleep(&app).await;
    assert_eq!(kept(&host, &app).await, "what it was given", "lost over a sleep");

    let stopped = app.clone().edited(|instance| {
        instance.desired_state = DesiredInstanceState::Stopped;
    });
    host.deploy(std::slice::from_ref(&stopped)).await;
    host.until_state(&app.app_id, InstanceState::Stopped).await;
    // Named on-request again, so nothing boots it until something asks — and the read below is
    // what asks.
    host.deploy(std::slice::from_ref(&app)).await;
    answering(&host, &app).await;
    assert_eq!(kept(&host, &app).await, "what it was given", "lost over a stop");

    let again = app.clone().redeployed("2");
    host.deploy(std::slice::from_ref(&again)).await;
    host.until("the new release to be the one this host holds", |report| {
        report.instances.iter().any(|instance| {
            instance.app_id == again.app_id && instance.deployment_id == again.instance.deployment_id
        })
    })
    .await;
    answering(&host, &again).await;
    assert_eq!(
        kept(&host, &again).await,
        "what it was given",
        "lost over a redeploy: the volume was formatted again"
    );

    host.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn two_apps_on_one_host_never_read_each_others_volumes() {
    let Some(host) = crate::host().await else {
        return;
    };
    let one = host.tenant(1);
    let another = host.tenant(2);
    host.deploy(&[one.clone(), another.clone()]).await;
    host.until_state(&one.app_id, InstanceState::Running).await;
    host.until_state(&another.app_id, InstanceState::Running).await;

    for (app, mine) in [(&one, "the first"), (&another, "the second")] {
        let written = host
            .get(
                app,
                &format!("/write?path={KEPT}&body={}", mine.replace(' ', "+")),
            )
            .await
            .expect("the proxy answers");
        assert_eq!(written.status, 200, "{written:?}");
    }

    for (app, mine) in [(&one, "the first"), (&another, "the second")] {
        let held = host
            .get(app, &format!("/read?path={KEPT}"))
            .await
            .expect("the proxy answers");
        assert_eq!(
            held.body, mine,
            "{} is reading a volume that is not its own",
            app.app_id
        );
    }

    host.stop().await;
}
