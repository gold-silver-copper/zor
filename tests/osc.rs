#![allow(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use std::sync::{Arc, Mutex};

use proptest::prelude::*;
use zor::osc::{AgentId, Flags, Report, State, format, parse};

fn report(state: State, flags: Flags) -> Report {
    let agent =
        (state != State::None).then(|| AgentId::new("claude.code-1").expect("valid fixture"));
    Report::new(state, agent, 42, flags, true, Some("wait; 50% ✓".into())).expect("valid fixture")
}

#[test]
fn every_state_and_flag_combination_round_trips() {
    // Phase 0 OSC contract: all states and flags survive deterministic formatting.
    for state in [State::Working, State::Blocked, State::Idle, State::None] {
        for bits in 0_u8..8 {
            let flags = Flags {
                idle: bits & 1 != 0,
                blocker: bits & 2 != 0,
                working: bits & 4 != 0,
            };
            let value = report(state, flags);
            assert_eq!(parse(&format(&value)), Ok(value));
        }
    }
}

#[test]
fn complete_bel_frames_and_unknown_keys_are_accepted() {
    // Phase 0 OSC contract: complete frames are convenient and extensions are forward-compatible.
    let input = b"\x1b]7877;state=idle;future=yes;agent=codex;seq=7;visible=;exited=0\x07";
    assert_eq!(parse(input).expect("valid report").state(), State::Idle);
}

#[test]
fn malformed_contract_inputs_are_rejected() {
    // Phase 0 OSC contract: malformed and ambiguous reports fail closed.
    let cases: &[&[u8]] = &[
        b"7877;agent=x;seq=1",
        b"7877;state=idle;agent=x",
        b"7877;state=mystery;agent=x;seq=1",
        b"7877;state=idle;agent=x;seq=1;seq=2",
        b"7877;state=idle;agent=x;seq=1;visible=nope",
        b"7877;state=idle;agent=x;seq=1;message=%GG",
        b"7877;state=idle;agent=bad space;seq=1",
        b"7877;state=none;agent=x;seq=1",
        b"7877;state=idle;seq=1",
        b"\x1b]7877;state=none;seq=1\x1b\\junk",
        b"7877;state=none;seq=1\x07junk",
        b"7877;state=none;seq=1;message=%FF",
    ];
    for input in cases {
        assert!(parse(input).is_err(), "accepted {input:?}");
    }
    let long = format!("7877;state=none;seq=1;message={}", "x".repeat(129));
    assert!(parse(long.as_bytes()).is_err());
}

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<Vec<u8>>>>);

impl vt100::Callbacks for Capture {
    fn unhandled_osc(&mut self, _: &mut vt100::Screen, params: &[&[u8]]) {
        let joined = params.to_vec().join(&b';');
        self.0.lock().expect("capture lock").push(joined);
    }
}

#[test]
fn vt100_callback_payload_parses_to_the_original_report() {
    // Phase 0 OSC callback seam: koh's joined vt100 params preserve the report.
    let expected = report(
        State::Blocked,
        Flags {
            blocker: true,
            ..Flags::default()
        },
    );
    let capture = Capture::default();
    let seen = Arc::clone(&capture.0);
    let mut parser = vt100::Parser::new_with_callbacks(24, 80, 0, capture);
    parser.process(&format(&expected));
    let reports = seen.lock().expect("capture lock");
    assert_eq!(reports.len(), 1);
    assert_eq!(parse(&reports[0]), Ok(expected));
}

proptest! {
    #[test]
    fn arbitrary_bytes_never_make_parse_panic(input in proptest::collection::vec(any::<u8>(), 0..2048)) {
        // Phase 0 OSC totality: arbitrary bytes return a value or an error without panicking.
        let _ = parse(&input);
    }
}
