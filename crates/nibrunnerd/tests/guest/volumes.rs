//! A volume belongs to exactly one app, and holds what that app put in it for as long as the
//! document names it — across a sleep, a stop, and a release that replaces the one before it.

use nibrunnerd::test_support::machine::{RunningHost, Tenant};
use protocol::{DesiredInstanceState, InstanceState};

const KEPT: &str = "kept";

fn release(host: &RunningHost, app: &Tenant, version: &str) -> Tenant {
    use sha2::Digest;
    let bytes = format!("#!/bin/sh\nexport RELEASE={version}\nexec /app/tenant\n").into_bytes();
    let key = format!("release-{version}");
    std::fs::write(
        std::path::Path::new(&host.host.config.artifact_store_url).join(&key),
        &bytes,
    )
    .expect("the release artifact");
    let layer = protocol::DesiredLayer::Executable {
        object: protocol::StoredObject {
            digest: protocol::Sha256Digest::parse(hex::encode(sha2::Sha256::digest(&bytes)))
                .expect("a digest"),
            object_key: protocol::ObjectKey::parse(key).expect("a key"),
        },
        destination_path: protocol::ExecutablePath::parse("/app/release").expect("a path"),
    };
    app.clone().redeployed(version).edited(|instance| {
        instance.layers.truncate(1);
        instance.layers.push(layer);
        instance.config.command.program = protocol::GuestPath::parse("/app/release").expect("a program");
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn replacing_a_sleeping_release_runs_its_new_artifact_and_preserves_only_the_volume() {
    let Some(host) = crate::host().await else {
        return;
    };
    let app = release(&host, &host.tenant(1).on_request(60_000), "one");
    host.deploy(std::slice::from_ref(&app)).await;
    host.until_state(&app.app_id, InstanceState::Running).await;
    assert_eq!(
        host.get(&app, "/env?name=RELEASE")
            .await
            .expect("the first release")
            .body,
        "one"
    );
    assert_eq!(
        host.get(&app, "/write?path=kept&body=persistent")
            .await
            .expect("a write")
            .status,
        200
    );
    assert_eq!(host.get(&app, "/remember").await.expect("an answer").body, "1");
    assert_eq!(host.get(&app, "/remember").await.expect("an answer").body, "2");
    host.let_sleep(&app).await;

    let replacement = release(&host, &app, "two");
    assert_ne!(replacement.instance.layers, app.instance.layers);
    host.deploy(std::slice::from_ref(&replacement)).await;
    host.until("the replacement deployment to be running", |report| {
        report.instances.iter().any(|instance| {
            instance.app_id == replacement.app_id
                && instance.deployment_id == replacement.instance.deployment_id
                && instance.state == InstanceState::Running
        })
    })
    .await;
    assert_eq!(
        host.get(&replacement, "/env?name=RELEASE")
            .await
            .expect("the replacement release")
            .body,
        "two"
    );
    assert_eq!(
        host.get(&replacement, "/remember")
            .await
            .expect("fresh process memory")
            .body,
        "1"
    );
    assert_eq!(kept(&host, &replacement).await, "persistent");
    let reported = host.instance(&replacement.app_id).await.expect("a record");
    assert!(reported
        .layer_digests
        .contains(&replacement.instance.layers[1].object().digest));
    assert!(!reported
        .layer_digests
        .contains(&app.instance.layers[1].object().digest));

    host.let_sleep(&replacement).await;
    assert_eq!(
        host.get(&replacement, "/env?name=RELEASE")
            .await
            .expect("the replacement wakes")
            .body,
        "two"
    );
    assert_eq!(
        host.get(&replacement, "/remember")
            .await
            .expect("replacement memory survives")
            .body,
        "2"
    );
    assert_eq!(kept(&host, &replacement).await, "persistent");
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
