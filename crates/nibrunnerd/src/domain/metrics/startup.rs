use std::time::Duration;

use super::{Histogram, Kind, Metric, Page};

const BOUNDS_SECONDS: [f64; 15] = [
    0.0001, 0.0005, 0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuestStartupPhase {
    BootToInit,
    RootSetup,
    Launch,
    ExecToReady,
    SampleRoundTrip,
}

impl GuestStartupPhase {
    pub const ALL: [Self; 5] = [
        Self::BootToInit,
        Self::RootSetup,
        Self::Launch,
        Self::ExecToReady,
        Self::SampleRoundTrip,
    ];

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::BootToInit => "boot_to_init",
            Self::RootSetup => "root_setup",
            Self::Launch => "launch",
            Self::ExecToReady => "exec_to_ready",
            Self::SampleRoundTrip => "sample_round_trip",
        }
    }
}

#[derive(Debug)]
pub struct StartupMetrics {
    phases: Vec<Histogram>,
}

impl Default for StartupMetrics {
    fn default() -> Self {
        Self {
            phases: GuestStartupPhase::ALL
                .iter()
                .map(|_| Histogram::over(&BOUNDS_SECONDS))
                .collect(),
        }
    }
}

impl StartupMetrics {
    pub fn observed(&self, phase: GuestStartupPhase, took: Duration) {
        let index = GuestStartupPhase::ALL
            .iter()
            .position(|each| *each == phase)
            .unwrap_or(0);
        self.phases[index].observe(took);
    }
}

static GUEST_STARTUP_PHASE_SECONDS: Metric = Metric {
    name: "nibrunner_guest_startup_phase_seconds",
    help: "Guest startup checkpoints through first host-observed readiness. Exec-to-ready aligns the guest clock with the sampling handshake midpoint; sample_round_trip bounds calibration uncertainty rather than startup work.",
    kind: Kind::Histogram,
    labels: &["phase"],
};

pub(super) static DECLARED: &[&Metric] = &[&GUEST_STARTUP_PHASE_SECONDS];

pub(super) fn render(page: &mut Page, metrics: &StartupMetrics) {
    page.declare(&GUEST_STARTUP_PHASE_SECONDS);
    for (phase, histogram) in GuestStartupPhase::ALL.iter().zip(&metrics.phases) {
        page.histogram(
            &GUEST_STARTUP_PHASE_SECONDS,
            &[("phase", phase.as_str())],
            histogram,
        );
    }
}
