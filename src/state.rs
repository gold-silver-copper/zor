use crate::osc::{AgentId, Flags, State};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug)]
pub struct Config {
    pub confirmation: Duration,
    pub hold_cap: Duration,
    pub startup_grace: Duration,
    pub heartbeat: Duration,
    pub confirmations: u8,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            confirmation: Duration::from_millis(100),
            hold_cap: Duration::from_millis(700),
            startup_grace: Duration::from_secs(3),
            heartbeat: Duration::from_millis(800),
            confirmations: 3,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Event {
    Changed {
        state: State,
        previous: State,
        agent: Option<AgentId>,
        seq: u64,
        visible: Flags,
        exited: bool,
    },
    Heartbeat {
        state: State,
        agent: Option<AgentId>,
        seq: u64,
        visible: Flags,
    },
    AgentFound {
        id: AgentId,
        pid: i32,
    },
    AgentLost,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ObservationState {
    Working,
    Blocked,
    Idle,
    Skip,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Observation {
    pub state: ObservationState,
    pub visible: Flags,
}

struct Hold {
    opened: Instant,
    next_confirmation: Instant,
    confirmations: u8,
}

pub struct Machine {
    config: Config,
    current: State,
    agent: Option<AgentId>,
    visible: Flags,
    seq: u64,
    hold: Option<Hold>,
    grace_until: Option<Instant>,
    heartbeat_at: Option<Instant>,
}

impl Machine {
    #[must_use]
    pub fn new(config: Config) -> Self {
        Self {
            config,
            current: State::None,
            agent: None,
            visible: Flags::default(),
            seq: 0,
            hold: None,
            grace_until: None,
            heartbeat_at: None,
        }
    }

    pub fn observe(
        &mut self,
        verdict: Option<Observation>,
        agent: Option<AgentId>,
        pid: Option<i32>,
        exited: bool,
        now: Instant,
    ) -> Vec<Event> {
        let mut events = Vec::new();
        if agent != self.agent {
            self.hold = None;
            if let Some(id) = agent.clone() {
                self.grace_until = Some(now + self.config.startup_grace);
                events.push(Event::AgentFound {
                    id,
                    pid: pid.unwrap_or_default(),
                });
            } else if self.agent.is_some() {
                events.push(Event::AgentLost);
                events.push(self.publish(State::None, Flags::default(), false, now));
            }
            self.agent = agent.clone();
        }
        if exited {
            events.push(self.publish(State::Idle, Flags::default(), true, now));
            return events;
        }
        let Some(verdict) = verdict else {
            self.hold = None;
            return events;
        };
        if verdict.state == ObservationState::Skip {
            self.hold = None;
            return events;
        }
        let state = match verdict.state {
            ObservationState::Working => State::Working,
            ObservationState::Blocked => State::Blocked,
            ObservationState::Idle => State::Idle,
            ObservationState::Skip => return events,
        };
        if state == State::Idle && self.grace_until.is_some_and(|deadline| now < deadline) {
            self.hold = None;
            return events;
        }
        let held = self.current == State::Working && state == State::Idle && !verdict.visible.idle;
        if held {
            if let Some(hold) = &mut self.hold {
                hold.confirmations = hold.confirmations.saturating_add(1);
                hold.next_confirmation = now + self.config.confirmation;
                if hold.confirmations > self.config.confirmations {
                    self.hold = None;
                    events.push(self.publish(state, verdict.visible, false, now));
                }
            } else if agent.is_some() {
                self.hold = Some(Hold {
                    opened: now,
                    next_confirmation: now + self.config.confirmation,
                    confirmations: 1,
                });
            }
        } else {
            self.hold = None;
            if state != self.current || verdict.visible != self.visible {
                events.push(self.publish(state, verdict.visible, false, now));
            }
        }
        events
    }

    pub fn tick(&mut self, now: Instant) -> Vec<Event> {
        if self
            .hold
            .as_ref()
            .is_some_and(|hold| now.duration_since(hold.opened) >= self.config.hold_cap)
        {
            self.hold = None;
            return vec![self.publish(State::Idle, Flags::default(), false, now)];
        }
        if self.heartbeat_at.is_some_and(|deadline| now >= deadline) {
            self.heartbeat_at = Some(now + self.config.heartbeat);
            return vec![Event::Heartbeat {
                state: self.current,
                agent: self.agent.clone(),
                seq: self.seq,
                visible: self.visible,
            }];
        }
        Vec::new()
    }

    #[must_use]
    pub fn next_deadline(&self) -> Option<Instant> {
        let hold = self.hold.as_ref().map(|hold| {
            hold.next_confirmation
                .min(hold.opened + self.config.hold_cap)
        });
        match (hold, self.heartbeat_at) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (Some(a), None) | (None, Some(a)) => Some(a),
            (None, None) => None,
        }
    }
    #[must_use]
    pub const fn current(&self) -> (State, Flags, u64) {
        (self.current, self.visible, self.seq)
    }
    #[must_use]
    pub const fn hold_pending(&self) -> bool {
        self.hold.is_some()
    }

    fn publish(&mut self, state: State, visible: Flags, exited: bool, now: Instant) -> Event {
        let previous = self.current;
        self.current = state;
        self.visible = visible;
        self.seq = self.seq.saturating_add(1);
        self.heartbeat_at = Some(now + self.config.heartbeat);
        Event::Changed {
            state,
            previous,
            agent: self.agent.clone(),
            seq: self.seq,
            visible,
            exited,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn verdict(state: ObservationState, visible: Flags) -> Observation {
        Observation { state, visible }
    }
    fn agent() -> Option<AgentId> {
        AgentId::new("test").ok()
    }

    #[test]
    fn plain_idle_publishes_on_the_fourth_verdict() {
        // Phase Z §3: working-to-plain-idle requires three confirmations after opening.
        let start = Instant::now();
        let mut machine = Machine::new(Config {
            startup_grace: Duration::ZERO,
            ..Config::default()
        });
        let _ = machine.observe(
            Some(verdict(ObservationState::Working, Flags::default())),
            agent(),
            Some(1),
            false,
            start,
        );
        for offset in 1..4 {
            assert!(
                machine
                    .observe(
                        Some(verdict(ObservationState::Idle, Flags::default())),
                        agent(),
                        Some(1),
                        false,
                        start + Duration::from_millis(offset)
                    )
                    .is_empty()
            );
        }
        assert!(matches!(
            machine
                .observe(
                    Some(verdict(ObservationState::Idle, Flags::default())),
                    agent(),
                    Some(1),
                    false,
                    start + Duration::from_millis(4)
                )
                .as_slice(),
            [Event::Changed {
                state: State::Idle,
                ..
            }]
        ));
    }

    #[test]
    fn hold_cap_and_visible_idle_publish() {
        // Phase Z §3: the cap forces idle while visible idle bypasses holding.
        let start = Instant::now();
        let mut machine = Machine::new(Config {
            startup_grace: Duration::ZERO,
            ..Config::default()
        });
        let _ = machine.observe(
            Some(verdict(ObservationState::Working, Flags::default())),
            agent(),
            Some(1),
            false,
            start,
        );
        let _ = machine.observe(
            Some(verdict(ObservationState::Idle, Flags::default())),
            agent(),
            Some(1),
            false,
            start,
        );
        assert!(matches!(
            machine.tick(start + Duration::from_millis(700)).as_slice(),
            [Event::Changed {
                state: State::Idle,
                ..
            }]
        ));
        let _ = machine.observe(
            Some(verdict(ObservationState::Working, Flags::default())),
            agent(),
            Some(1),
            false,
            start + Duration::from_secs(1),
        );
        assert!(matches!(
            machine
                .observe(
                    Some(verdict(
                        ObservationState::Idle,
                        Flags {
                            idle: true,
                            ..Flags::default()
                        }
                    )),
                    agent(),
                    Some(1),
                    false,
                    start + Duration::from_secs(1)
                )
                .as_slice(),
            [Event::Changed {
                state: State::Idle,
                ..
            }]
        ));
    }

    #[test]
    fn heartbeat_keeps_sequence_and_loss_publishes_none() {
        // Phase Z §3: heartbeats do not advance seq and agent loss clears state.
        let start = Instant::now();
        let mut machine = Machine::new(Config {
            startup_grace: Duration::ZERO,
            ..Config::default()
        });
        let events = machine.observe(
            Some(verdict(
                ObservationState::Blocked,
                Flags {
                    blocker: true,
                    ..Flags::default()
                },
            )),
            agent(),
            Some(2),
            false,
            start,
        );
        let seq = match events.last() {
            Some(Event::Changed { seq, .. }) => *seq,
            _ => 0,
        };
        assert!(
            matches!(machine.tick(start + Duration::from_millis(800)).as_slice(), [Event::Heartbeat { seq: value, .. }] if *value == seq)
        );
        let lost = machine.observe(None, None, None, false, start + Duration::from_secs(1));
        assert!(matches!(
            lost.as_slice(),
            [
                Event::AgentLost,
                Event::Changed {
                    state: State::None,
                    ..
                }
            ]
        ));
    }

    #[test]
    fn startup_grace_drops_idle_but_passes_blocked() {
        // Phase Z §3: only idle is suppressed during startup grace.
        let start = Instant::now();
        let mut machine = Machine::new(Config::default());
        let found = machine.observe(
            Some(verdict(ObservationState::Idle, Flags::default())),
            agent(),
            Some(1),
            false,
            start,
        );
        assert!(matches!(found.as_slice(), [Event::AgentFound { .. }]));
        assert!(matches!(
            machine
                .observe(
                    Some(verdict(ObservationState::Blocked, Flags::default())),
                    agent(),
                    Some(1),
                    false,
                    start
                )
                .as_slice(),
            [Event::Changed {
                state: State::Blocked,
                ..
            }]
        ));
    }

    #[test]
    fn skip_cancels_hold_and_flag_changes_publish_immediately() {
        // Phase Z §3: Skip cancels pending idle and visible flags are state changes.
        let start = Instant::now();
        let mut machine = Machine::new(Config {
            startup_grace: Duration::ZERO,
            ..Config::default()
        });
        let _ = machine.observe(
            Some(verdict(ObservationState::Working, Flags::default())),
            agent(),
            Some(1),
            false,
            start,
        );
        let _ = machine.observe(
            Some(verdict(ObservationState::Idle, Flags::default())),
            agent(),
            Some(1),
            false,
            start,
        );
        assert!(machine.next_deadline().is_some());
        assert!(
            machine
                .observe(
                    Some(verdict(ObservationState::Skip, Flags::default())),
                    agent(),
                    Some(1),
                    false,
                    start
                )
                .is_empty()
        );
        assert!(machine.tick(start + Duration::from_millis(700)).is_empty());
        let changed = machine.observe(
            Some(verdict(
                ObservationState::Working,
                Flags {
                    working: true,
                    ..Flags::default()
                },
            )),
            agent(),
            Some(1),
            false,
            start,
        );
        assert!(matches!(
            changed.as_slice(),
            [Event::Changed {
                visible: Flags { working: true, .. },
                ..
            }]
        ));
    }

    #[test]
    fn exit_publishes_idle_with_marker() {
        // Phase Z §3: agent exit is an immediate idle transition with exited set.
        let start = Instant::now();
        let mut machine = Machine::new(Config {
            startup_grace: Duration::ZERO,
            ..Config::default()
        });
        let _ = machine.observe(
            Some(verdict(ObservationState::Working, Flags::default())),
            agent(),
            Some(1),
            false,
            start,
        );
        assert!(matches!(
            machine
                .observe(None, agent(), Some(1), true, start)
                .as_slice(),
            [Event::Changed {
                state: State::Idle,
                exited: true,
                ..
            }]
        ));
    }
}
