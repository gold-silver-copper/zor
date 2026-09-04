use crate::{
    osc::{AgentId, Flags, State},
    rules::{RuleState, Verdict},
};
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

struct Hold {
    opened: Instant,
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
        verdict: Option<Verdict>,
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
        if verdict.state == RuleState::Skip {
            self.hold = None;
            return events;
        }
        let state = match verdict.state {
            RuleState::Working => State::Working,
            RuleState::Blocked => State::Blocked,
            RuleState::Idle => State::Idle,
            RuleState::Skip => return events,
        };
        if state == State::Idle && self.grace_until.is_some_and(|deadline| now < deadline) {
            self.hold = None;
            return events;
        }
        let held = self.current == State::Working && state == State::Idle && !verdict.visible.idle;
        if held {
            if let Some(hold) = &mut self.hold {
                hold.confirmations = hold.confirmations.saturating_add(1);
                if hold.confirmations > self.config.confirmations {
                    self.hold = None;
                    events.push(self.publish(state, verdict.visible, false, now));
                }
            } else if agent.is_some() {
                self.hold = Some(Hold {
                    opened: now,
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
            (hold.opened + self.config.confirmation).min(hold.opened + self.config.hold_cap)
        });
        match (hold, self.heartbeat_at) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (Some(a), None) | (None, Some(a)) => Some(a),
            (None, None) => None,
        }
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
