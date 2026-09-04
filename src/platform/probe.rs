use crate::osc::AgentId;
use std::time::{Duration, Instant};

pub struct Scheduler {
    next: Instant,
    last_full: Option<Instant>,
    acquisition: Option<Instant>,
    no_pgid_since: Option<Instant>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Detection {
    AgentFound { id: AgentId, pid: i32 },
    Exited { agent: AgentId },
    AgentLost,
}

pub struct LossTracker {
    current: Option<(AgentId, i32)>,
    misses: u8,
    exit_announced: bool,
}
impl LossTracker {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            current: None,
            misses: 0,
            exit_announced: false,
        }
    }
    pub fn update(
        &mut self,
        detected: Option<(AgentId, i32)>,
        shell_in_foreground: bool,
    ) -> Option<Detection> {
        if let Some(found) = detected {
            self.misses = 0;
            self.exit_announced = false;
            if self.current.as_ref() != Some(&found) {
                self.current = Some(found.clone());
                return Some(Detection::AgentFound {
                    id: found.0,
                    pid: found.1,
                });
            }
            return None;
        }
        let (id, _) = self.current.clone()?;
        if shell_in_foreground {
            if !self.exit_announced {
                self.exit_announced = true;
                return Some(Detection::Exited { agent: id });
            }
            self.current = None;
            self.exit_announced = false;
            self.misses = 0;
            return Some(Detection::AgentLost);
        }
        self.exit_announced = false;
        self.misses = self.misses.saturating_add(1);
        if self.misses >= 6 {
            self.current = None;
            self.misses = 0;
            Some(Detection::AgentLost)
        } else {
            None
        }
    }
    #[must_use]
    pub fn current(&self) -> Option<&(AgentId, i32)> {
        self.current.as_ref()
    }
}
impl Default for LossTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl Scheduler {
    #[must_use]
    pub fn new(now: Instant) -> Self {
        Self {
            next: now,
            last_full: None,
            acquisition: None,
            no_pgid_since: None,
        }
    }
    pub fn screen_changed_without_agent(&mut self, now: Instant) {
        self.acquisition = Some(now);
        self.next = now;
    }
    #[must_use]
    pub fn due(&self, now: Instant) -> bool {
        now >= self.next
    }
    pub fn completed(
        &mut self,
        now: Instant,
        identified: bool,
        hold: bool,
        pgid_changed: bool,
    ) -> bool {
        if self
            .acquisition
            .is_some_and(|opened| now.duration_since(opened) >= Duration::from_secs(8))
        {
            self.acquisition = None;
        }
        if pgid_changed && !identified {
            self.acquisition = Some(now);
        }
        let full = pgid_changed
            || self.last_full.is_none_or(|last| {
                identified && now.duration_since(last) >= Duration::from_secs(5)
            })
            || self
                .acquisition
                .is_some_and(|opened| now.duration_since(opened) < Duration::from_secs(8));
        if full {
            self.last_full = Some(now);
        }
        let cadence = if hold {
            Duration::from_millis(100)
        } else if identified {
            Duration::from_millis(300)
        } else if let Some(opened) = self.acquisition {
            if now.duration_since(opened) < Duration::from_millis(1500) {
                Duration::from_millis(500)
            } else {
                Duration::from_secs(2)
            }
        } else {
            Duration::from_millis(500)
        };
        self.next = now + cadence;
        full
    }
    pub fn pgid_presence(&mut self, present: bool, now: Instant) -> bool {
        if present {
            self.no_pgid_since = None;
            return false;
        }
        let opened = *self.no_pgid_since.get_or_insert(now);
        now.duration_since(opened) >= Duration::from_secs(30)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn scheduler_uses_acquisition_and_identified_cadences() {
        // Phase Z §4: acquisition starts fast and identified probes use 300 ms ticks.
        let now = Instant::now();
        let mut scheduler = Scheduler::new(now);
        assert!(scheduler.due(now));
        assert!(scheduler.completed(now, false, false, true));
        assert!(!scheduler.due(now + Duration::from_millis(499)));
        assert!(scheduler.due(now + Duration::from_millis(500)));
        let later = now + Duration::from_secs(1);
        let _ = scheduler.completed(later, true, false, false);
        assert!(scheduler.due(later + Duration::from_millis(300)));
        assert!(!scheduler.pgid_presence(false, later));
        assert!(scheduler.pgid_presence(false, later + Duration::from_secs(30)));
    }

    #[test]
    fn loss_tracker_distinguishes_exit_from_six_foreign_misses() {
        // Phase Z §4: shell return announces exit then loss; foreign jobs require six misses.
        let id = AgentId::new("agent").ok();
        let mut tracker = LossTracker::new();
        assert!(matches!(
            tracker.update(id.clone().map(|id| (id, 2)), false),
            Some(Detection::AgentFound { .. })
        ));
        assert!(matches!(
            tracker.update(None, true),
            Some(Detection::Exited { .. })
        ));
        assert_eq!(tracker.update(None, true), Some(Detection::AgentLost));
        assert!(matches!(
            tracker.update(id.map(|id| (id, 3)), false),
            Some(Detection::AgentFound { .. })
        ));
        for _ in 0..5 {
            assert_eq!(tracker.update(None, false), None);
        }
        assert_eq!(tracker.update(None, false), Some(Detection::AgentLost));
    }

    #[test]
    fn scheduler_runs_five_second_reprobe_and_bounded_acquisition_phases() {
        // Phase Z §4: full probes recur at 5 s and acquisition ends after its 8 s window.
        let start = Instant::now();
        let mut scheduler = Scheduler::new(start);
        assert!(scheduler.completed(start, false, false, true));
        for millis in [500, 1_000] {
            let now = start + Duration::from_millis(millis);
            assert!(scheduler.due(now));
            assert!(scheduler.completed(now, false, false, false));
        }
        let slow = start + Duration::from_millis(1_500);
        assert!(scheduler.completed(slow, false, false, false));
        assert!(!scheduler.due(slow + Duration::from_millis(1_999)));
        let ended = start + Duration::from_secs(8);
        let _ = scheduler.completed(ended, false, false, false);
        assert!(scheduler.due(ended + Duration::from_millis(500)));
        let mut identified = Scheduler::new(start);
        assert!(identified.completed(start, true, false, true));
        assert!(!identified.completed(start + Duration::from_secs(4), true, false, false));
        assert!(identified.completed(start + Duration::from_secs(5), true, false, false));
    }
}
