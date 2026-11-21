//! Team tools exchange JSON. A worker's chat response is published to readers.
use serde_json::Value;

pub(crate) const WRITE_FOR_READER: &str = "Present the result below to the reader in the user's language. Use natural prose and retain the figures, units, sources and relevant uncertainty. Keep structured files in the requested format.";

pub fn only_data(text: &str) -> bool {
    let text = text.trim();
    let body = if let Some(fenced) = text.strip_prefix("```") {
        let Some((language, rest)) = fenced.split_once('\n') else {
            return false;
        };
        if !language.trim().is_empty() && !language.trim().eq_ignore_ascii_case("json") {
            return false;
        }
        let body = rest.trim_end().strip_suffix("```").unwrap_or(rest);
        body.trim()
    } else {
        text
    };
    body.starts_with(['{', '['])
        && match serde_json::from_str::<Value>(body) {
            Ok(_) => true,
            Err(error) => error.is_eof(),
        }
}

/// Hold a possible standalone data block until the assembled message can
/// distinguish it from a JSON example accompanied by a human explanation.
pub(crate) struct Stream {
    pending: Option<String>,
    candidate: bool,
}

impl Stream {
    pub(crate) fn new(team_worker: bool) -> Self {
        Self {
            pending: team_worker.then(String::new),
            candidate: false,
        }
    }

    pub(crate) fn push(&mut self, delta: String) -> Option<String> {
        let Some(pending) = self.pending.as_mut() else {
            return Some(delta);
        };
        pending.push_str(&delta);
        if !self.candidate {
            let mut start = pending.trim_start();
            if start.is_empty() || start == "`" || start == "``" {
                return None;
            }
            if let Some(fence) = start.strip_prefix("```") {
                if let Some((lang, body)) = fence.split_once('\n') {
                    if !lang.trim().is_empty() && !lang.trim().eq_ignore_ascii_case("json") {
                        return self.pending.take();
                    }
                    start = body.trim_start();
                    if start.is_empty() {
                        return None;
                    }
                } else if "json".starts_with(&fence.trim().to_ascii_lowercase()) {
                    return None;
                } else {
                    return self.pending.take();
                }
            }
            self.candidate = if let Some(array) = start.strip_prefix('[') {
                let Some(first) = array.trim_start().as_bytes().first() else {
                    return None;
                };
                matches!(
                    first,
                    b']' | b'{' | b'[' | b'"' | b'-' | b'0'..=b'9' | b't' | b'f' | b'n'
                )
            } else {
                start.starts_with('{')
            };
        }
        if self.candidate {
            None
        } else {
            self.pending.take()
        }
    }

    pub(crate) fn finish(self) -> Option<String> {
        self.pending.filter(|text| !text.is_empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn holds_only_potential_data_and_preserves_streamed_prose() {
        let mut s = Stream::new(true);
        assert_eq!(s.push(" ".into()), None);
        assert_eq!(s.push("已核对".into()).as_deref(), Some(" 已核对"));
        assert_eq!(s.push("19 份。".into()).as_deref(), Some("19 份。"));
        assert!(s.finish().is_none());
        let mut data = Stream::new(true);
        for piece in ["`", "`", "`JSON\r\n", "{\"n\":19}", "\n```"] {
            assert!(data.push(piece.into()).is_none());
        }
        let fenced = data.finish().unwrap();
        assert!(only_data(&fenced));
        assert!(!only_data(&format!("{fenced}\n共 19 份。")));
        assert!(!only_data("[核对记录](report.md)"));
        assert_eq!(
            Stream::new(true).push("[日报](a.md)".into()).unwrap(),
            "[日报](a.md)"
        );
        assert_eq!(Stream::new(true).push("```rust".into()).unwrap(), "```rust");
        assert!(only_data("{\"peer_inbox\":["));
        assert!(only_data("```json\n{\"tunnels\":["));
        assert!(!only_data("```rust\nlet a=1;\n```"));
        assert_eq!(Stream::new(false).push("[".into()).as_deref(), Some("["));
    }
}
