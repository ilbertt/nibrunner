use std::time::Duration;

use super::{Histogram, Kind, Metric, Page};

const BOUNDS_SECONDS: [f64; 15] = [
    0.0001, 0.0005, 0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayerPhase {
    Read,
    Verify,
    Pack,
    Publish,
}

impl LayerPhase {
    fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Verify => "verify",
            Self::Pack => "pack",
            Self::Publish => "publish",
        }
    }
}

const PHASES: [LayerPhase; 4] = [
    LayerPhase::Read,
    LayerPhase::Verify,
    LayerPhase::Pack,
    LayerPhase::Publish,
];
const OUTCOMES: [&str; 2] = ["ok", "failed"];

#[derive(Debug)]
pub struct LayerMetrics {
    phases: Vec<Histogram>,
}

impl Default for LayerMetrics {
    fn default() -> Self {
        Self {
            phases: (0..PHASES.len() * OUTCOMES.len())
                .map(|_| Histogram::over(&BOUNDS_SECONDS))
                .collect(),
        }
    }
}

impl LayerMetrics {
    fn phase(&self, phase: LayerPhase, ok: bool) -> &Histogram {
        let position = PHASES.iter().position(|each| *each == phase).unwrap_or(0);
        &self.phases[position * OUTCOMES.len() + usize::from(!ok)]
    }

    pub fn observed(&self, phase: LayerPhase, ok: bool, took: Duration) {
        self.phase(phase, ok).observe(took);
    }
}

static LAYER_PREPARATION_PHASE_SECONDS: Metric = Metric {
    name: "nibrunner_layer_preparation_phase_seconds",
    help: "Cache-miss layer preparation by stage: reading an artifact, verifying its digest, packing an executable directly to a staged file, and publishing its image. Cache hits do not repeat these stages. OCI extraction has its own storage operation.",
    kind: Kind::Histogram,
    labels: &["phase", "outcome"],
};

pub(super) static DECLARED: &[&Metric] = &[&LAYER_PREPARATION_PHASE_SECONDS];

pub(super) fn render(page: &mut Page, metrics: &LayerMetrics) {
    page.declare(&LAYER_PREPARATION_PHASE_SECONDS);
    for phase in PHASES {
        for (index, outcome) in OUTCOMES.iter().enumerate() {
            page.histogram(
                &LAYER_PREPARATION_PHASE_SECONDS,
                &[("phase", phase.as_str()), ("outcome", outcome)],
                metrics.phase(phase, index == 0),
            );
        }
    }
}
