//! The kernel's copy of the ruleset, and the memory of what was last put there.
//!
//! Which tenants are forwarded changes on the probe rather than on desired state, so this is
//! applied on every status tick — and an unchanged ruleset has to cost nothing rather than a
//! table replacement a second, which would also zero the counters the activity measurement reads.
//!
//! What makes skipping safe is that the memory is never the whole answer: the kernel is asked
//! whether the tables it names are still the ones this daemon created. Anything else that drops
//! or rebuilds them then becomes a ruleset the next pass rewrites, rather than one this process
//! never writes again because the text it would send has not changed.

use std::collections::BTreeMap;
use std::sync::Arc;

use nft_render::{
    parse_app_traffic, parse_kernel_tables, render_ruleset, AppTraffic, FirewallState, KernelTables,
    NFTABLES_TABLE,
};
use protocol::AppId;
use tokio::sync::Mutex;

use crate::services::{CommandError, CommandRequest, CommandRunner};

/// What was written, and the tables it was written into, which is what says it is still there.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Applied {
    ruleset: String,
    tables: KernelTables,
}

pub struct HostFirewall {
    commands: Arc<dyn CommandRunner>,
    applied: Mutex<Option<Applied>>,
}

impl HostFirewall {
    /// Never applied by this process yet, so the first apply always runs: what is in the kernel
    /// came from whichever daemon ran before, and the host may have changed since.
    pub fn new(commands: Arc<dyn CommandRunner>) -> Self {
        Self {
            commands,
            applied: Mutex::new(None),
        }
    }

    /// `None` when nft could not be asked at all, which has to stay apart from a host holding no
    /// tables of ours: both write the ruleset, but only the second is a state worth remembering
    /// having reached.
    async fn kernel_tables(&self) -> Option<KernelTables> {
        self.commands
            .stdout_of(CommandRequest::new(&["nft", "-j", "list", "tables"]))
            .await
            .ok()
            .map(|json| parse_kernel_tables(&json))
    }

    pub async fn apply(&self, state: &FirewallState) -> Result<(), CommandError> {
        let ruleset = render_ruleset(state);
        let mut applied = self.applied.lock().await;
        if let Some(last) = applied.as_ref() {
            if last.ruleset == ruleset {
                if let Some(current) = self.kernel_tables().await {
                    if current == last.tables {
                        return Ok(());
                    }
                }
            }
        }
        self.commands
            .stdout_of(CommandRequest::new(&["nft", "-f", "-"]).with_stdin(ruleset.clone()))
            .await?;
        // Tables that cannot be read back leave the next pass nothing to compare against, so it
        // writes them again rather than taking this pass's success as proof they are in place.
        *applied = self
            .kernel_tables()
            .await
            .map(|tables| Applied { ruleset, tables });
        Ok(())
    }

    /// What the kernel has counted against each app, which is a different question from what this
    /// process last wrote: the rules are ours, the counts are traffic's.
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
    use crate::services::{CommandResult, RecordingCommandRunner};
    use nft_render::{app_counter_name, ForwardedInstance};
    use protocol::{HostPort, HttpPort, Ipv4Address};

    fn holding(handles: (u64, u64)) -> String {
        format!(
            r#"{{"nftables":[{{"table":{{"family":"ip","name":"nibrun","handle":{}}}}},{{"table":{{"family":"ip6","name":"nibrun","handle":{}}}}}]}}"#,
            handles.0, handles.1
        )
    }

    fn instance() -> ForwardedInstance {
        ForwardedInstance {
            app_id: AppId::parse("app-1").unwrap(),
            host_port: HostPort::new(21_000).unwrap(),
            http_port: HttpPort::new(3000).unwrap(),
            extra_public_port: None,
            host_ipv4: Ipv4Address::parse("10.201.0.1").unwrap(),
            guest_ipv4: Ipv4Address::parse("10.201.0.2").unwrap(),
        }
    }

    fn listing(answer: String) -> Arc<RecordingCommandRunner> {
        RecordingCommandRunner::answering(move |request| {
            if request.command.contains(&"list".to_string()) {
                Ok(CommandResult::with_stdout(answer.clone()))
            } else {
                Ok(CommandResult::succeeded())
            }
        })
    }

    fn writes(commands: &RecordingCommandRunner) -> usize {
        commands
            .calls()
            .iter()
            .filter(|request| request.command.contains(&"-f".to_string()))
            .count()
    }

    #[tokio::test]
    async fn the_ruleset_is_piped_to_nft_whole_and_a_rerun_costs_nothing() {
        let commands = listing(holding((2, 4)));
        let firewall = HostFirewall::new(commands.clone());
        let state = FirewallState {
            instances: vec![instance()],
            ..Default::default()
        };
        firewall.apply(&state).await.unwrap();
        let written = commands
            .calls()
            .into_iter()
            .find(|request| request.command.contains(&"-f".to_string()))
            .unwrap();
        assert_eq!(written.command, vec!["nft", "-f", "-"]);
        assert!(written
            .stdin
            .as_deref()
            .unwrap()
            .starts_with("table ip nibrun\ndelete table ip nibrun"));

        firewall.apply(&state).await.unwrap();
        assert_eq!(
            writes(&commands),
            1,
            "an unchanged ruleset the kernel still holds is not rewritten"
        );
    }

    /// An operator's `nft flush ruleset` leaves the text unchanged and the kernel empty, which is
    /// exactly the case a memory of what was written cannot answer on its own.
    #[tokio::test]
    async fn a_ruleset_something_else_dropped_is_written_again() {
        let flushed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let commands = RecordingCommandRunner::answering({
            let flushed = flushed.clone();
            move |request| {
                if request.command.contains(&"list".to_string()) {
                    let answer = if flushed.load(std::sync::atomic::Ordering::SeqCst) {
                        r#"{"nftables":[]}"#.to_string()
                    } else {
                        holding((2, 4))
                    };
                    return Ok(CommandResult::with_stdout(answer));
                }
                Ok(CommandResult::succeeded())
            }
        });
        let firewall = HostFirewall::new(commands.clone());
        let state = FirewallState::default();
        firewall.apply(&state).await.unwrap();
        flushed.store(true, std::sync::atomic::Ordering::SeqCst);
        firewall.apply(&state).await.unwrap();
        assert_eq!(writes(&commands), 2);
    }

    /// Rebuilt by something else carries new handles, which reads as a table that is no longer
    /// the one this daemon wrote.
    #[tokio::test]
    async fn a_table_rebuilt_under_this_daemon_is_written_again() {
        let handles = Arc::new(std::sync::atomic::AtomicU64::new(2));
        let commands = RecordingCommandRunner::answering({
            let handles = handles.clone();
            move |request| {
                if request.command.contains(&"list".to_string()) {
                    let handle = handles.load(std::sync::atomic::Ordering::SeqCst);
                    return Ok(CommandResult::with_stdout(holding((handle, handle + 2))));
                }
                Ok(CommandResult::succeeded())
            }
        });
        let firewall = HostFirewall::new(commands.clone());
        firewall.apply(&FirewallState::default()).await.unwrap();
        handles.store(8, std::sync::atomic::Ordering::SeqCst);
        firewall.apply(&FirewallState::default()).await.unwrap();
        assert_eq!(writes(&commands), 2);
    }

    /// A kernel that cannot be asked leaves the next pass nothing to compare against.
    #[tokio::test]
    async fn a_kernel_that_would_not_answer_is_not_taken_as_proof_the_rules_are_in_place() {
        let commands = RecordingCommandRunner::answering(|request| {
            if request.command.contains(&"list".to_string()) {
                Err(CommandError::Unstartable {
                    executable: "nft".into(),
                    reason: "not found".into(),
                })
            } else {
                Ok(CommandResult::succeeded())
            }
        });
        let firewall = HostFirewall::new(commands.clone());
        firewall.apply(&FirewallState::default()).await.unwrap();
        firewall.apply(&FirewallState::default()).await.unwrap();
        assert_eq!(writes(&commands), 2);
    }

    #[tokio::test]
    async fn what_the_kernel_counted_is_read_back_per_app() {
        let app = AppId::parse("app-1").unwrap();
        let counters = format!(
            r#"{{"nftables":[{{"counter":{{"family":"ip","name":"{}","table":"nibrun","handle":2,"packets":3,"bytes":512}}}}]}}"#,
            app_counter_name(&app)
        );
        let firewall = HostFirewall::new(listing(counters));
        assert_eq!(
            firewall.traffic().await.unwrap().get(&app),
            Some(&AppTraffic {
                packets: 3,
                bytes: 512
            })
        );
    }
}
