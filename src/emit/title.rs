use crate::osc::State;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Mode {
    Never,
    Prefix,
    Replace,
}

pub struct Titles {
    mode: Mode,
    original: String,
    changed: bool,
}
impl Titles {
    #[must_use]
    pub fn new(mode: Mode) -> Self {
        Self {
            mode,
            original: String::new(),
            changed: false,
        }
    }
    #[must_use]
    pub fn observe(
        &mut self,
        original: &str,
        state: State,
        agent: Option<&str>,
    ) -> Option<Vec<u8>> {
        self.original = original.to_owned();
        let title = match self.mode {
            Mode::Never => return None,
            Mode::Prefix => format!(
                "{}{}{}",
                glyph(state),
                if glyph(state).is_empty() { "" } else { " " },
                original
            ),
            Mode::Replace => format!(
                "{}{}{}",
                glyph(state),
                if glyph(state).is_empty() { "" } else { " " },
                agent.unwrap_or_default()
            ),
        };
        self.changed = true;
        Some(frame(&title))
    }
    #[must_use]
    pub fn restore(&self) -> Option<Vec<u8>> {
        self.changed.then(|| frame(&self.original))
    }
}
fn glyph(state: State) -> &'static str {
    match state {
        State::Working => "●",
        State::Blocked => "◐",
        State::Idle => "○",
        State::None => "",
    }
}
fn frame(title: &str) -> Vec<u8> {
    format!("\x1b]2;{title}\x1b\\").into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn title_modes_append_and_restore() {
        // Phase Z §6: title modes preserve and restore the child's original title.
        let mut titles = Titles::new(Mode::Prefix);
        assert_eq!(
            titles.observe("task", State::Working, Some("agent")),
            Some(b"\x1b]2;\xe2\x97\x8f task\x1b\\".to_vec())
        );
        assert_eq!(titles.restore(), Some(b"\x1b]2;task\x1b\\".to_vec()));
        assert!(
            Titles::new(Mode::Never)
                .observe("task", State::Idle, None)
                .is_none()
        );
    }
}
