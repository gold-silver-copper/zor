use crate::osc::Report;

#[must_use]
pub fn state(report: &Report) -> Vec<u8> {
    crate::osc::format(report)
}

pub fn parse(payload: &[u8]) -> Result<Report, crate::osc::Error> {
    crate::osc::parse(payload)
}
