use serde::Serialize;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Serialize)]
pub struct EventLine<'a> {
    pub t: &'a str,
    pub state: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<&'a str>,
    pub seq: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<&'a str>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub visible: Vec<&'a str>,
    #[serde(skip_serializing_if = "is_false")]
    pub exited: bool,
}
fn is_false(value: &bool) -> bool {
    !value
}

pub fn encode(event: &EventLine<'_>) -> Result<Vec<u8>, serde_json::Error> {
    let mut line = serde_json::to_vec(event)?;
    line.push(b'\n');
    Ok(line)
}

#[must_use]
pub fn timestamp() -> String {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    format!("{}.{:03}", duration.as_secs(), duration.subsec_millis())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn event_is_one_json_line() {
        // Phase Z §6: event output is parseable JSON Lines with optional fields omitted.
        let line = EventLine {
            t: "1.000",
            state: "idle",
            previous: None,
            agent: Some("a"),
            seq: 2,
            pid: None,
            code: None,
            title: None,
            visible: vec![],
            exited: false,
        };
        let encoded = encode(&line).unwrap_or_default();
        assert_eq!(encoded.last(), Some(&b'\n'));
        assert!(serde_json::from_slice::<serde_json::Value>(&encoded).is_ok());
    }
}
