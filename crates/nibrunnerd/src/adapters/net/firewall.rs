use std::collections::BTreeMap;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use nft_render::{
    parse_app_traffic, parse_kernel_ruleset, render_ruleset, AppTraffic, FirewallState, NFTABLES_TABLE,
};
use protocol::AppId;
use tokio::sync::Mutex;

use crate::ports::{CommandError, CommandRequest, CommandRunner, CommandRunnerExt};

#[derive(Debug, Clone, PartialEq, Eq)]
struct Applied {
    source: String,
    installed: String,
}

#[derive(Clone)]
pub struct HostFirewall {
    commands: Arc<dyn CommandRunner>,
    applied: Arc<Mutex<Option<Applied>>>,
}

fn unverified() -> CommandError {
    CommandError::Unstartable {
        executable: "tenant network".into(),
        reason: "the installed isolation rules could not be verified".into(),
    }
}

impl HostFirewall {
    pub fn new(commands: Arc<dyn CommandRunner>) -> Self {
        Self {
            commands,
            applied: Arc::new(Mutex::new(None)),
        }
    }

    async fn run(&self, mut request: CommandRequest) -> Result<String, CommandError> {
        request.timeout = Duration::from_secs(5);
        self.commands.stdout_of(request).await
    }

    async fn inspect(&self) -> Result<String, CommandError> {
        let output = self
            .run(CommandRequest::new(&["nft", "-j", "list", "ruleset"]))
            .await?;
        parse_kernel_ruleset(&output).ok_or_else(unverified)
    }

    async fn set_taps(&self, state: &str) -> Result<(), CommandError> {
        let output = self
            .run(CommandRequest::new(&["ip", "-json", "link", "show"]))
            .await?;
        let links: Vec<serde_json::Value> = serde_json::from_str(&output).map_err(|_| unverified())?;
        let mut failure = None;
        for link in links {
            let name = link
                .get("ifname")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(unverified)?;
            if nft_render::is_tap_name(name) {
                if let Err(error) = self
                    .run(CommandRequest::new(&["ip", "link", "set", "dev", name, state]))
                    .await
                {
                    failure = Some(error);
                }
            }
        }
        failure.map_or(Ok(()), Err)
    }

    async fn contain(&self, applied: &mut Option<Applied>) -> Result<(), CommandError> {
        *applied = None;
        self.set_taps("down").await
    }

    async fn write(&self, state: &FirewallState, applied: &mut Option<Applied>) -> Result<(), CommandError> {
        let source = render_ruleset(state);
        let mut reconnect = applied.is_none();
        if let Some(last) = applied.as_ref() {
            if self
                .inspect()
                .await
                .is_ok_and(|current| current == last.installed)
            {
                if last.source == source {
                    return Ok(());
                }
            } else {
                self.contain(applied).await?;
                reconnect = true;
            }
        }
        // Echo describes the committed transaction. A flush before read-back must not become
        // the baseline, and counters must not turn every packet into a firewall replacement.
        let echo = self
            .run(CommandRequest::new(&["nft", "-j", "-e", "-f", "-"]).with_stdin(source.clone()))
            .await?;
        let installed = parse_kernel_ruleset(&echo).ok_or_else(unverified)?;
        if self.inspect().await? != installed {
            return Err(unverified());
        }
        if reconnect {
            self.set_taps("up").await?;
        }
        *applied = Some(Applied { source, installed });
        Ok(())
    }

    pub async fn apply(&self, state: &FirewallState) -> Result<(), CommandError> {
        let firewall = self.clone();
        let state = state.clone();
        // Controller cancellation must not interrupt disconnection or leave a partial reconnect.
        tokio::spawn(async move { firewall.apply_locked(&state).await })
            .await
            .map_err(|_| unverified())?
    }

    pub(crate) async fn apply_locked(&self, state: &FirewallState) -> Result<(), CommandError> {
        let mut applied = self.applied.lock().await;
        if let Err(error) = self.write(state, &mut applied).await {
            self.contain(&mut applied).await?;
            return Err(error);
        }
        Ok(())
    }

    pub async fn while_isolated<T: Send + 'static>(
        &self,
        action: impl Future<Output = T> + Send + 'static,
    ) -> Result<T, CommandError> {
        let firewall = self.clone();
        tokio::spawn(async move { firewall.with_isolation_lock(action).await })
            .await
            .map_err(|_| unverified())?
    }

    async fn with_isolation_lock<T>(&self, action: impl Future<Output = T>) -> Result<T, CommandError> {
        let mut applied = self.applied.lock().await;
        let verified = match applied.as_ref() {
            Some(last) => self
                .inspect()
                .await
                .is_ok_and(|current| current == last.installed),
            None => false,
        };
        if !verified {
            self.contain(&mut applied).await?;
            return Err(unverified());
        }
        Ok(action.await)
    }

    pub async fn traffic(&self) -> Result<BTreeMap<AppId, AppTraffic>, CommandError> {
        let json = self
            .commands
            .stdout_of(CommandRequest::new(&[
                "nft",
                "-j",
                "list",
                "counters",
                "table",
                "ip",
                NFTABLES_TABLE,
            ]))
            .await?;
        Ok(parse_app_traffic(&json))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ports::CommandResult;
    use crate::test_support::mocks;
    use std::sync::Mutex as StdMutex;

    const INSTALLED: &str = include_str!("../../../../nft-render/tests/fixtures/installed.json");
    struct Kernel {
        installed: String,
        fail_write: bool,
        fail_read: bool,
        corrupt_after_write: bool,
    }
    fn fixture() -> (Arc<HostFirewall>, Arc<StdMutex<Kernel>>, mocks::CommandLog) {
        let kernel = Arc::new(StdMutex::new(Kernel {
            installed: INSTALLED.into(),
            fail_write: false,
            fail_read: false,
            corrupt_after_write: false,
        }));
        let (commands, log) = mocks::commands_answering({
            let kernel = kernel.clone();
            move |request| {
                if request.command == ["ip", "-json", "link", "show"] {
                    return Ok(CommandResult::with_stdout(
                        r#"[{"ifname":"nbr0"},{"ifname":"nbr1"},{"ifname":"eth0"}]"#,
                    ));
                }
                if request.executable() == "ip" {
                    return Ok(CommandResult::succeeded());
                }
                let mut kernel = kernel.lock().unwrap();
                if request.command.contains(&"-f".into()) {
                    if kernel.fail_write {
                        return Err(unverified());
                    }
                    kernel.installed = if kernel.corrupt_after_write {
                        r#"{"nftables":[]}"#.into()
                    } else {
                        INSTALLED.into()
                    };
                    Ok(CommandResult::with_stdout(INSTALLED))
                } else {
                    if kernel.fail_read {
                        return Err(unverified());
                    }
                    Ok(CommandResult::with_stdout(kernel.installed.clone()))
                }
            }
        });
        (Arc::new(HostFirewall::new(commands)), kernel, log)
    }

    #[tokio::test]
    async fn unchanged_rules_are_not_rewritten_and_removed_rules_are_repaired_before_reconnection() {
        let (firewall, kernel, log) = fixture();
        let state = FirewallState::default();
        firewall.apply(&state).await.unwrap();
        firewall.apply(&state).await.unwrap();
        assert_eq!(
            log.calls()
                .iter()
                .filter(|call| call.command.contains(&"-f".into()))
                .count(),
            1
        );
        kernel.lock().unwrap().installed = r#"{"nftables":[]}"#.into();
        firewall.apply(&state).await.unwrap();
        let steps: Vec<String> = log.commands().iter().map(|command| command.join(" ")).collect();
        let down = steps
            .iter()
            .position(|step| step == "ip link set dev nbr0 down")
            .unwrap();
        let repair = steps.iter().rposition(|step| step == "nft -j -e -f -").unwrap();
        assert!(down < repair);
        assert_eq!(steps.last().unwrap(), "ip link set dev nbr1 up");
        assert!(!steps.iter().any(|step| step.contains("dev eth0")));
    }

    #[tokio::test]
    async fn failed_writes_reads_and_read_back_races_disconnect_existing_guests_until_verified_recovery() {
        for failure in 0..3 {
            let (firewall, kernel, log) = fixture();
            let state = FirewallState::default();
            firewall.apply(&state).await.unwrap();
            let before = log.calls().len();
            {
                let mut kernel = kernel.lock().unwrap();
                kernel.installed = r#"{"nftables":[]}"#.into();
                kernel.fail_write = failure == 0;
                kernel.fail_read = failure == 1;
                kernel.corrupt_after_write = failure == 2;
            }
            assert!(firewall.apply(&state).await.is_err());
            let connected = Arc::new(std::sync::atomic::AtomicBool::new(false));
            assert!(firewall
                .while_isolated({
                    let connected = connected.clone();
                    async move {
                        connected.store(true, std::sync::atomic::Ordering::SeqCst);
                    }
                })
                .await
                .is_err());
            assert!(!connected.load(std::sync::atomic::Ordering::SeqCst));
            assert!(!log.calls()[before..]
                .iter()
                .any(|call| call.command.last().is_some_and(|word| word == "up")));
            {
                let mut kernel = kernel.lock().unwrap();
                kernel.fail_write = false;
                kernel.fail_read = false;
                kernel.corrupt_after_write = false;
            }
            firewall.apply(&state).await.unwrap();
            firewall
                .while_isolated({
                    let connected = connected.clone();
                    async move {
                        connected.store(true, std::sync::atomic::Ordering::SeqCst);
                    }
                })
                .await
                .unwrap();
            assert!(connected.load(std::sync::atomic::Ordering::SeqCst));
        }
    }

    #[tokio::test]
    async fn containment_cannot_race_a_tap_being_connected_even_when_its_caller_is_cancelled() {
        let (firewall, kernel, log) = fixture();
        firewall.apply(&FirewallState::default()).await.unwrap();
        let (entered, reached) = tokio::sync::oneshot::channel();
        let (release, released) = tokio::sync::oneshot::channel();
        let setup = tokio::spawn({
            let firewall = firewall.clone();
            async move {
                firewall
                    .while_isolated(async move {
                        entered.send(()).unwrap();
                        released.await.unwrap();
                    })
                    .await
            }
        });
        reached.await.unwrap();
        setup.abort();
        kernel.lock().unwrap().installed = r#"{"nftables":[]}"#.into();
        let repair = tokio::spawn({
            let firewall = firewall.clone();
            async move { firewall.apply(&FirewallState::default()).await }
        });
        tokio::task::yield_now().await;
        assert!(!log
            .calls()
            .iter()
            .any(|call| call.command.last().is_some_and(|word| word == "down")));
        release.send(()).unwrap();
        assert!(setup.await.unwrap_err().is_cancelled());
        repair.await.unwrap().unwrap();
        assert!(log
            .calls()
            .iter()
            .any(|call| call.command.last().is_some_and(|word| word == "down")));
    }
}
