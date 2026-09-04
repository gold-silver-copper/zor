use crate::rules::view::{Progress, ScreenView};
use std::borrow::Cow;

const SCROLLBACK_LINES: usize = u16::MAX as usize;
const MAX_TITLE_CHARS: usize = 256;

#[derive(Default)]
struct Callbacks {
    title: String,
    bells: u64,
    progress: Option<Progress>,
    event: bool,
}

impl vt100::Callbacks for Callbacks {
    fn audible_bell(&mut self, _: &mut vt100::Screen) {
        self.bells = self.bells.saturating_add(1);
        self.event = true;
    }

    fn set_window_title(&mut self, _: &mut vt100::Screen, title: &[u8]) {
        self.title = String::from_utf8_lossy(title)
            .chars()
            .filter(|character| !character.is_control())
            .take(MAX_TITLE_CHARS)
            .collect();
        self.event = true;
    }

    fn unhandled_osc(&mut self, _: &mut vt100::Screen, params: &[&[u8]]) {
        if params.first().copied() != Some(b"9") || params.get(1).copied() != Some(b"4") {
            return;
        }
        let Some(state) = params.get(2).and_then(parse_u8) else {
            return;
        };
        if state > 3 {
            return;
        }
        let Some(percent) = params.get(3).and_then(parse_u8) else {
            return;
        };
        if percent > 100 {
            return;
        }
        self.progress = Some(if state == 0 {
            Progress {
                state: 0,
                percent: 0,
            }
        } else {
            Progress { state, percent }
        });
        self.event = true;
    }
}

fn parse_u8(value: &&[u8]) -> Option<u8> {
    std::str::from_utf8(value).ok()?.parse().ok()
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum Sequence {
    #[default]
    Ground,
    Escape,
    Csi,
    String {
        osc: bool,
        escape: bool,
    },
}

#[derive(Default)]
struct BoundaryTracker {
    state: Sequence,
}

impl BoundaryTracker {
    fn feed(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.byte(byte);
        }
    }

    fn byte(&mut self, byte: u8) {
        self.state = match self.state {
            Sequence::Ground => match byte {
                0x1b => Sequence::Escape,
                0x9b => Sequence::Csi,
                0x90 | 0x98 | 0x9d | 0x9e | 0x9f => Sequence::String {
                    osc: byte == 0x9d,
                    escape: false,
                },
                _ => Sequence::Ground,
            },
            Sequence::Escape => match byte {
                b'[' => Sequence::Csi,
                b']' | b'P' | b'X' | b'^' | b'_' => Sequence::String {
                    osc: byte == b']',
                    escape: false,
                },
                0x18 | 0x1a => Sequence::Ground,
                0x20..=0x2f => Sequence::Escape,
                _ => Sequence::Ground,
            },
            Sequence::Csi => match byte {
                0x18 | 0x1a => Sequence::Ground,
                0x1b => Sequence::Escape,
                0x40..=0x7e => Sequence::Ground,
                _ => Sequence::Csi,
            },
            Sequence::String { osc, escape } => {
                if matches!(byte, 0x18 | 0x1a)
                    || byte == 0x9c
                    || (osc && byte == 0x07)
                    || (escape && byte == b'\\')
                {
                    Sequence::Ground
                } else if byte == 0x1b {
                    Sequence::String { osc, escape: true }
                } else {
                    Sequence::String { osc, escape: false }
                }
            }
        };
    }

    const fn ground(&self) -> bool {
        matches!(self.state, Sequence::Ground)
    }
}

pub struct Screen {
    parser: vt100::Parser<Callbacks>,
    boundary: BoundaryTracker,
    changed: bool,
    rows: u16,
    cols: u16,
    lines: Vec<String>,
    text: String,
}

impl Screen {
    #[must_use]
    pub fn new(rows: u16, cols: u16) -> Self {
        let mut value = Self {
            parser: vt100::Parser::new_with_callbacks(
                rows,
                cols,
                SCROLLBACK_LINES,
                Callbacks::default(),
            ),
            boundary: BoundaryTracker::default(),
            changed: true,
            rows,
            cols,
            lines: Vec::new(),
            text: String::new(),
        };
        value.refresh_window();
        value
    }

    pub fn process(&mut self, bytes: &[u8]) -> bool {
        let before = (
            self.parser.screen().contents(),
            self.parser.screen().cursor_position(),
            self.parser.screen().alternate_screen(),
        );
        self.parser.callbacks_mut().event = false;
        self.parser.process(bytes);
        self.boundary.feed(bytes);
        let after = self.parser.screen();
        self.changed = before
            != (
                after.contents(),
                after.cursor_position(),
                after.alternate_screen(),
            )
            || self.parser.callbacks().event;
        if self.changed {
            self.refresh_window();
        }
        self.boundary.ground()
    }

    pub fn resize(&mut self, rows: u16, cols: u16) {
        self.rows = rows;
        self.cols = cols;
        self.parser.screen_mut().set_size(rows, cols);
        self.changed = true;
        self.refresh_window();
    }

    #[must_use]
    pub const fn changed(&self) -> bool {
        self.changed
    }
    #[must_use]
    pub const fn ground(&self) -> bool {
        self.boundary.ground()
    }
    pub fn clear_changed(&mut self) {
        self.changed = false;
    }
    #[must_use]
    pub fn bell_count(&self) -> u64 {
        self.parser.callbacks().bells
    }

    fn refresh_window(&mut self) {
        let mut snapshot = self.parser.screen().clone();
        let alternate = snapshot.alternate_screen();
        let live: Vec<String> = snapshot
            .rows(0, self.cols)
            .map(|line| line.trim_end().to_owned())
            .collect();
        let mut lines = if alternate {
            live
        } else {
            let cursor = usize::from(snapshot.cursor_position().0);
            let last_non_blank = live.iter().rposition(|line| !line.is_empty());
            let live_end = last_non_blank.map_or_else(
                || usize::from(self.rows.saturating_sub(1)),
                |row| row.max(cursor),
            );
            snapshot.set_scrollback(usize::from(self.rows));
            let history_count = snapshot.scrollback();
            let mut combined: Vec<String> = snapshot
                .rows(0, self.cols)
                .take(history_count)
                .map(|line| line.trim_end().to_owned())
                .collect();
            combined.extend(live);
            let end = history_count
                .saturating_add(live_end)
                .saturating_add(1)
                .min(combined.len());
            let start = end.saturating_sub(usize::from(self.rows));
            combined.get(start..end).unwrap_or_default().to_vec()
        };
        while lines.last().is_some_and(String::is_empty) {
            lines.pop();
        }
        self.text = lines.join("\n");
        if !self.text.is_empty() {
            self.text.push('\n');
        }
        self.lines = lines;
    }
}

impl ScreenView for Screen {
    fn lines(&self) -> impl Iterator<Item = Cow<'_, str>> {
        self.lines.iter().map(|line| Cow::Borrowed(line.as_str()))
    }
    fn text(&self) -> &str {
        &self.text
    }
    fn title(&self) -> &str {
        &self.parser.callbacks().title
    }
    fn progress(&self) -> Option<Progress> {
        self.parser.callbacks().progress
    }
    fn size(&self) -> (u16, u16) {
        (self.rows, self.cols)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracker_recognizes_every_split_of_control_strings() {
        // Phase Z §1: injected output is safe only at ECMA-48 ground boundaries.
        for sequence in [
            b"\x1b]title\x1b\\".as_slice(),
            b"\x1bPpayload\x1b\\",
            b"\x1bXpayload\x1b\\",
            b"\x1b^payload\x1b\\",
            b"\x1b_payload\x1b\\",
            b"\x1b[31m",
            b"\x9dtitle\x07",
            b"\x90data\x9c",
        ] {
            for split in 0..=sequence.len() {
                let mut tracker = BoundaryTracker::default();
                tracker.feed(sequence.get(..split).unwrap_or_default());
                tracker.feed(sequence.get(split..).unwrap_or_default());
                assert!(tracker.ground());
            }
        }
    }

    #[test]
    fn callbacks_record_title_bell_and_progress() {
        // Phase Z §1: metadata callbacks update the observed screen.
        let mut screen = Screen::new(4, 20);
        screen.process(b"\x1b]2;hi\x07\x07\x1b]9;4;3;42\x07");
        assert_eq!(screen.title(), "hi");
        assert_eq!(screen.bell_count(), 1);
        assert_eq!(
            screen.progress(),
            Some(Progress {
                state: 3,
                percent: 42
            })
        );
    }

    #[test]
    fn repainting_identical_content_is_unchanged() {
        // Phase Z §1: identical terminal state does not trigger rule evaluation.
        let mut screen = Screen::new(4, 20);
        screen.process(b"x");
        screen.clear_changed();
        screen.process(b"\r\x1b[Kx");
        assert!(!screen.changed());
    }
}
