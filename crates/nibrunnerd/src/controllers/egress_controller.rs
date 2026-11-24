use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::adapters::net::egress::EgressMonitor;
use crate::controllers::Controller;
use crate::domain::metrics::egress::{Window, INTERVAL};
use crate::host::Host;

pub(crate) struct EgressController(pub(crate) Arc<Host>);

#[async_trait::async_trait]
impl Controller for EgressController {
    fn name(&self) -> &'static str {
        "outbound monitoring"
    }

    async fn run(&self) {
        let host = &self.0;
        let mut ticks = tokio::time::interval(INTERVAL);
        ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut last = Instant::now();
        loop {
            ticks.tick().await;
            let allocator = host.allocator.lock().await;
            let mut monitor = host.egress.lock().await;
            let bound = monitor
                .bind(host.commands.as_ref(), allocator.assignments(), allocator.limit())
                .await;
            drop(monitor);
            drop(allocator);
            let window = if bound {
                EgressMonitor::close_window(&host.egress, host.commands.as_ref()).await
            } else {
                None
            };
            let closed = Instant::now();
            let elapsed = closed.duration_since(last);
            last = closed;
            let window = window
                .filter(|_| elapsed.abs_diff(INTERVAL) <= Duration::from_secs(5))
                .map(|counts| Window { closed, counts });
            *host.metrics.egress.lock().expect("egress window lock") = window;
        }
    }
}
