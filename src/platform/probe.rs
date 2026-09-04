use std::time::{Duration, Instant};

pub struct Scheduler {
    next: Instant,
    last_full: Option<Instant>,
    acquisition: Option<Instant>,
}
impl Scheduler {
    #[must_use]
    pub fn new(now: Instant) -> Self {
        Self {
            next: now,
            last_full: None,
            acquisition: None,
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
}
