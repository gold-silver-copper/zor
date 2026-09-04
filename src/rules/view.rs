#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Progress {
    pub state: u8,
    pub percent: u8,
}

pub trait ScreenView {
    fn lines(&self) -> impl Iterator<Item = std::borrow::Cow<'_, str>>;
    fn text(&self) -> &str;
    fn title(&self) -> &str;
    fn progress(&self) -> Option<Progress>;
    fn size(&self) -> (u16, u16);
}
