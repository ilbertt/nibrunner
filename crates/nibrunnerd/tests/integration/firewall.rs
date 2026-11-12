use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use nibrunnerd::adapters::exec::HostCommands;
use nibrunnerd::adapters::net::firewall::HostFirewall;
use nibrunnerd::ports::{CommandError, CommandRequest, CommandResult, CommandRunner, CommandRunnerExt};

const TAP: &str = "nbr9001";
const MANAGEMENT: &str = "isolation-mgmt";

#[derive(Default)]
struct Faults {
    refuse_writes: AtomicBool,
    corrupt_after_write: AtomicBool,
}

#[async_trait::async_trait]
impl CommandRunner for Faults {
    async fn run(&self, request: CommandRequest) -> Result<CommandResult, CommandError> {
        let writing = request.executable() == "nft" && request.command.contains(&"-f".into());
        if writing && self.refuse_writes.load(Ordering::SeqCst) {
            return Err(CommandError::Unstartable {
                executable: "nft".into(),
                reason: "injected write failure".into(),
            });
        }
        let result = HostCommands.run(request).await?;
        if writing && self.corrupt_after_write.load(Ordering::SeqCst) {
            HostCommands
                .stdout_of(CommandRequest::new(&[
                    "nft", "flush", "chain", "ip", "nibrun", "input",
                ]))
                .await?;
        }
        Ok(result)
    }
}

async fn run(command: &[&str]) -> String {
    HostCommands
        .stdout_of(CommandRequest::new(command))
        .await
        .unwrap()
}

async fn link_is_up(name: &str) -> bool {
    let output = run(&["ip", "-json", "link", "show", "dev", name]).await;
    let value: serde_json::Value = serde_json::from_str(&output).unwrap();
    value[0]["flags"]
        .as_array()
        .unwrap()
        .contains(&serde_json::json!("UP"))
}

struct Interfaces;
impl Drop for Interfaces {
    fn drop(&mut self) {
        for name in [TAP, MANAGEMENT] {
            let _ = std::process::Command::new("ip")
                .args(["link", "del", name])
                .output();
        }
    }
}

#[tokio::test]
async fn missing_rules_and_failed_repair_disconnect_existing_taps_until_verified_recovery() {
    if !super::enabled() {
        return;
    }
    super::require_root();
    let _cleanup = Interfaces;
    run(&["ip", "link", "add", TAP, "type", "dummy"]).await;
    run(&["ip", "link", "add", MANAGEMENT, "type", "dummy"]).await;
    run(&["ip", "link", "set", MANAGEMENT, "up"]).await;
    let faults = Arc::new(Faults::default());
    let firewall = HostFirewall::new(faults.clone());
    let state = nft_render::FirewallState {
        denied_egress_addresses_v4: vec!["10.43.0.0/16".into()],
        denied_egress_addresses_v6: vec!["2600:1f18:abcd::/56".into()],
        ..Default::default()
    };
    firewall.apply(&state).await.unwrap();
    assert!(link_is_up(TAP).await);
    let original = run(&["nft", "-j", "list", "tables"]).await;
    firewall.apply(&state).await.unwrap();
    assert_eq!(run(&["nft", "-j", "list", "tables"]).await, original);
    for family in ["ip", "ip6"] {
        let before = run(&["nft", "-j", "list", "tables"]).await;
        run(&["nft", "flush", "chain", family, "nibrun", "forward"]).await;
        assert_eq!(run(&["nft", "-j", "list", "tables"]).await, before);
        firewall.apply(&state).await.unwrap();
        assert!(run(&["nft", "list", "chain", family, "nibrun", "forward"])
            .await
            .contains("reject"));
    }
    run(&["nft", "flush", "chain", "ip", "nibrun", "input"]).await;
    faults.refuse_writes.store(true, Ordering::SeqCst);
    assert!(firewall.apply(&state).await.is_err());
    assert!(!link_is_up(TAP).await);
    assert!(link_is_up(MANAGEMENT).await);
    assert!(firewall
        .while_isolated(async { run(&["ip", "link", "set", TAP, "up"]).await })
        .await
        .is_err());
    assert!(!link_is_up(TAP).await);
    faults.refuse_writes.store(false, Ordering::SeqCst);
    faults.corrupt_after_write.store(true, Ordering::SeqCst);
    assert!(firewall.apply(&state).await.is_err());
    assert!(!link_is_up(TAP).await);
    faults.corrupt_after_write.store(false, Ordering::SeqCst);
    firewall.apply(&state).await.unwrap();
    assert!(link_is_up(TAP).await);
}
