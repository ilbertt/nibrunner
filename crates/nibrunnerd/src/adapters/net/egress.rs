use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Mutex;

use nft_render::egress::{
    egress_listing, egress_transaction, parse_egress, EgressBindings, EgressCounts, EGRESS_TABLE,
};
use protocol::AppId;

use crate::ports::{CommandRequest, CommandRunner, CommandRunnerExt};

#[derive(Default)]
pub(crate) struct EgressMonitor {
    installed: Option<Arc<EgressBindings>>,
    baseline: BTreeMap<AppId, EgressCounts>,
    complete: bool,
}

impl EgressMonitor {
    pub(crate) fn bound_to(&self, slot: &nft_render::AppSlot) -> bool {
        self.installed
            .as_ref()
            .and_then(|bindings| bindings.get(&slot.app_id))
            == Some(&Some(slot.slot))
    }

    pub(crate) async fn bind(
        &mut self,
        commands: &dyn CommandRunner,
        leases: &BTreeMap<AppId, u32>,
        limit: u32,
    ) -> bool {
        let mut bindings: EgressBindings = leases
            .iter()
            .map(|(app, slot)| (app.clone(), Some(*slot)))
            .collect();
        if let Some(installed) = &self.installed {
            // Detached counters finish the current window. Keep one hostful of departed apps
            // even if every slot changes owner between samples.
            let mut retired = installed.keys().filter(|app| !leases.contains_key(*app));
            for app in retired.by_ref().take(limit as usize) {
                bindings.insert(app.clone(), None);
            }
            if retired.next().is_some() {
                self.complete = false;
                tracing::warn!("outbound monitoring reached the departed-app limit for this window");
            }
        }
        self.install(commands, bindings).await
    }

    async fn install(&mut self, commands: &dyn CommandRunner, bindings: EgressBindings) -> bool {
        if self.installed.as_deref() == Some(&bindings) {
            return true;
        }
        // A cancelled or failed write has no trustworthy baseline. Recreate the observer before
        // publishing anything from a TAP which might already belong to the next tenant.
        let previous = self.installed.take();
        if previous.is_none() {
            self.baseline.clear();
            self.complete = false;
        }
        self.baseline.retain(|app, _| bindings.contains_key(app));
        let batch = egress_transaction(previous.as_deref(), &bindings);
        let mut request = CommandRequest::new(&["nft", "-f", "-"]).with_stdin(batch);
        request.timeout = Duration::from_secs(5);
        match commands.stdout_of(request).await {
            Ok(_) => {
                self.installed = Some(Arc::new(bindings));
                true
            }
            Err(error) => {
                tracing::warn!(%error, "outbound monitoring could not bind the guest interfaces");
                false
            }
        }
    }

    async fn read(
        commands: &dyn CommandRunner,
        leases: &EgressBindings,
    ) -> Option<BTreeMap<AppId, EgressCounts>> {
        let mut counts = BTreeMap::new();
        let apps: Vec<_> = leases.keys().collect();
        for group in apps.chunks(16) {
            let expected: BTreeSet<_> = group.iter().map(|app| (*app).clone()).collect();
            let mut request =
                CommandRequest::new(&["nft", "-j", "-f", "-"]).with_stdin(egress_listing(&expected));
            request.timeout = Duration::from_secs(5);
            let json = commands.stdout_of(request).await.ok()?;
            let rows = parse_egress(&json, &expected)?;
            counts.extend(rows);
        }
        Some(counts)
    }

    pub(crate) async fn close_window(
        monitor: &Mutex<Self>,
        commands: &dyn CommandRunner,
    ) -> Option<BTreeMap<AppId, EgressCounts>> {
        // Reading the sets can take several batches. Slot allocation must be able to rebind a
        // TAP meanwhile, and a reading assembled across two binding transactions is ambiguous.
        for _ in 0..2 {
            let bindings = monitor.lock().await.installed.clone()?;
            let counts = Self::read(commands, &bindings).await;
            let mut monitor = monitor.lock().await;
            if !Arc::ptr_eq(monitor.installed.as_ref()?, &bindings) {
                continue;
            }
            let Some(counts) = counts else {
                monitor.installed = None;
                tracing::warn!("outbound monitoring could not read a complete set of counters");
                return None;
            };
            return monitor.finish(commands, counts).await;
        }
        monitor.lock().await.complete = false;
        None
    }

    async fn finish(
        &mut self,
        commands: &dyn CommandRunner,
        mut current: BTreeMap<AppId, EgressCounts>,
    ) -> Option<BTreeMap<AppId, EgressCounts>> {
        let previous = std::mem::replace(&mut self.baseline, current.clone());
        let mut active = self.installed.as_deref()?.clone();
        active.retain(|_, slot| slot.is_some());
        if !self.install(commands, active).await {
            return None;
        }
        if !std::mem::replace(&mut self.complete, true) {
            return None;
        }
        for (app, row) in &mut current {
            let prior = previous.get(app).copied().unwrap_or_default();
            row.attempts = row.attempts.checked_sub(prior.attempts)?;
            row.passed = row.passed.checked_sub(prior.passed)?;
            row.unrecorded = row.unrecorded.checked_sub(prior.unrecorded)?;
        }
        Some(current)
    }
}

pub(crate) async fn remove(commands: &dyn CommandRunner) {
    let mut request = CommandRequest::new(&["nft", "-f", "-"])
        .with_stdin(format!("add table {EGRESS_TABLE}\ndelete table {EGRESS_TABLE}\n"));
    request.timeout = Duration::from_secs(5);
    if let Err(error) = commands.stdout_of(request).await {
        tracing::warn!(%error, "the outbound monitoring table could not be removed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ports::CommandResult;
    use crate::test_support::mocks::commands_answering;
    use std::sync::Mutex as StdMutex;

    fn listing(attempts: u64, passed: u64, missed: u64) -> String {
        let counter = |prefix: &str, packets| serde_json::json!({"counter": {"name": format!("{prefix}_app"), "packets": packets}});
        serde_json::json!({"nftables":[counter("before", attempts), counter("after", passed),
            counter("missed", missed), {"set":{"name":"ip_app","elem":["192.0.2.4"]}}, {"set":{"name":"ip6_app"}}]}).to_string()
    }

    #[tokio::test]
    async fn a_window_uses_deltas_and_a_failed_read_starts_another_warmup() {
        let reply = Arc::new(StdMutex::new(listing(10, 7, 1)));
        let (commands, _) = commands_answering({
            let reply = reply.clone();
            move |request| {
                Ok(if request.command.contains(&"-j".into()) {
                    CommandResult::with_stdout(reply.lock().unwrap().clone())
                } else {
                    CommandResult::succeeded()
                })
            }
        });
        let app = AppId::parse("app").unwrap();
        let leases = [(app.clone(), 0)].into();
        let monitor = Mutex::new(EgressMonitor::default());
        assert!(monitor.lock().await.bind(commands.as_ref(), &leases, 1).await);
        assert!(EgressMonitor::close_window(&monitor, commands.as_ref())
            .await
            .is_none());
        *reply.lock().unwrap() = listing(15, 10, 2);
        let window = EgressMonitor::close_window(&monitor, commands.as_ref())
            .await
            .unwrap();
        assert_eq!(
            window[&app],
            EgressCounts {
                attempts: 5,
                passed: 3,
                destinations: 1,
                unrecorded: 1
            }
        );
        *reply.lock().unwrap() = "{}".into();
        assert!(EgressMonitor::close_window(&monitor, commands.as_ref())
            .await
            .is_none());
        assert!(monitor.lock().await.bind(commands.as_ref(), &leases, 1).await);
        *reply.lock().unwrap() = listing(8, 5, 0);
        assert!(EgressMonitor::close_window(&monitor, commands.as_ref())
            .await
            .is_none());
        let quiet = EgressMonitor::close_window(&monitor, commands.as_ref())
            .await
            .unwrap();
        assert_eq!(quiet[&app].attempts, 0);
        assert_eq!(quiet[&app].unrecorded, 0);
    }
}
