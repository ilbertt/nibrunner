pub const STARTUP_REQUEST: &str = "STARTUP";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartupMilestone {
    InitStarted,
    RootReady,
    TenantExec,
}

impl StartupMilestone {
    pub const ALL: [Self; 3] = [Self::InitStarted, Self::RootReady, Self::TenantExec];

    pub const fn index(self) -> usize {
        self as usize
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartupSnapshot {
    pub sampled_at_us: u64,
    milestones_us: [u64; StartupMilestone::ALL.len()],
}

impl StartupSnapshot {
    pub fn new(sampled_at_us: u64, milestones_us: [u64; StartupMilestone::ALL.len()]) -> Option<Self> {
        if milestones_us.contains(&0)
            || milestones_us.windows(2).any(|pair| pair[0] > pair[1])
            || milestones_us.last().is_some_and(|last| *last > sampled_at_us)
        {
            return None;
        }
        Some(Self {
            sampled_at_us,
            milestones_us,
        })
    }

    pub fn at(&self, milestone: StartupMilestone) -> u64 {
        self.milestones_us[milestone.index()]
    }

    pub fn reply(&self) -> String {
        let milestones = self
            .milestones_us
            .iter()
            .map(u64::to_string)
            .collect::<Vec<_>>()
            .join(" ");
        format!("{STARTUP_REQUEST} {} {milestones}\n", self.sampled_at_us)
    }

    pub fn from_reply(reply: &str) -> Option<Self> {
        let mut fields = reply.split_whitespace();
        if fields.next()? != STARTUP_REQUEST {
            return None;
        }
        let sampled = fields.next()?.parse().ok()?;
        let mut milestones = [0; StartupMilestone::ALL.len()];
        for milestone in &mut milestones {
            *milestone = fields.next()?.parse().ok()?;
        }
        if fields.next().is_some() {
            return None;
        }
        Self::new(sampled, milestones)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_checkpoints_round_trip_in_monotonic_order() {
        let sample = StartupSnapshot::new(500, [100, 200, 300]).unwrap();
        assert_eq!(StartupSnapshot::from_reply(&sample.reply()), Some(sample));
    }

    #[test]
    fn incomplete_reversed_and_future_checkpoints_are_refused() {
        for reply in [
            "STARTUP 500 0 200 300",
            "STARTUP 500 300 200 100",
            "STARTUP 200 100 200 300",
            "STARTUP 500 100 200 300 extra",
            "STARTUP 500 100 200",
            "OK",
        ] {
            assert!(StartupSnapshot::from_reply(reply).is_none(), "{reply}");
        }
    }
}
