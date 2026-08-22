//! A minimal `text/event-stream` decoder, shared by the typed client and the
//! tests that drive the server's SSE endpoints.
//!
//! It is deliberately hand-rolled (≈100 lines) rather than pulled from a
//! crate: the client feature must stay dependency-light for `kevin-tui`, and
//! `reqwest-eventsource` does not track `reqwest` 0.13. Only the subset of the
//! SSE grammar Kevin emits is supported: `id:`, `event:`, `data:` and `:`
//! comments (keep-alives), separated by a blank line.

/// One decoded SSE message.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Message {
    /// `id:` — the global position, or the log `seq`.
    pub id: Option<String>,
    /// `event:` — the event type, `resync` or `snapshot`.
    pub event: Option<String>,
    /// `data:` lines joined with `\n`.
    pub data: String,
}

impl Message {
    /// The `event:` name, defaulting to `message` as the SSE spec does.
    #[must_use]
    pub fn name(&self) -> &str {
        self.event.as_deref().unwrap_or("message")
    }
}

/// Incremental decoder: feed it response chunks, take complete messages out.
#[derive(Debug, Default)]
pub struct Decoder {
    buffer: String,
}

impl Decoder {
    /// A decoder with an empty buffer.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends `chunk` (lossily decoded as UTF-8) and returns every message
    /// that is now complete.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<Message> {
        self.buffer.push_str(&String::from_utf8_lossy(chunk));
        let mut out = Vec::new();
        // Messages are separated by a blank line; tolerate CRLF.
        while let Some(end) = find_separator(&self.buffer) {
            let (block, rest) = self.buffer.split_at(end.0);
            let block = block.to_owned();
            self.buffer = rest[end.1..].to_owned();
            if let Some(message) = parse_block(&block) {
                out.push(message);
            }
        }
        out
    }
}

/// Offset and length of the first message separator in `buffer`.
fn find_separator(buffer: &str) -> Option<(usize, usize)> {
    let lf = buffer.find("\n\n").map(|i| (i, 2));
    let crlf = buffer.find("\r\n\r\n").map(|i| (i, 4));
    match (lf, crlf) {
        (Some(a), Some(b)) => Some(if a.0 <= b.0 { a } else { b }),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

fn parse_block(block: &str) -> Option<Message> {
    let mut message = Message::default();
    let mut data_lines: Vec<&str> = Vec::new();
    let mut saw_field = false;

    for line in block.lines() {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.is_empty() || line.starts_with(':') {
            continue; // keep-alive comment
        }
        let (field, value) = line.split_once(':').unwrap_or((line, ""));
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "id" => {
                saw_field = true;
                message.id = Some(value.to_owned());
            }
            "event" => {
                saw_field = true;
                message.event = Some(value.to_owned());
            }
            "data" => {
                saw_field = true;
                data_lines.push(value);
            }
            _ => {}
        }
    }

    if !saw_field {
        return None;
    }
    message.data = data_lines.join("\n");
    Some(message)
}

#[cfg(test)]
mod tests {
    use super::Decoder;

    #[test]
    fn a_message_is_decoded_once_its_blank_line_arrives() {
        let mut decoder = Decoder::new();
        assert!(
            decoder
                .push(b"id: 7\nevent: run.started\ndata: {\"a\"")
                .is_empty()
        );
        let messages = decoder.push(b":1}\n\n");
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].id.as_deref(), Some("7"));
        assert_eq!(messages[0].name(), "run.started");
        assert_eq!(messages[0].data, "{\"a\":1}");
    }

    #[test]
    fn keepalive_comments_are_ignored_and_multiline_data_is_joined() {
        let mut decoder = Decoder::new();
        let messages = decoder.push(b":keepalive\n\nevent: x\ndata: a\ndata: b\n\n");
        assert_eq!(messages.len(), 1, "the comment block yields no message");
        assert_eq!(messages[0].data, "a\nb");
    }

    #[test]
    fn several_messages_in_one_chunk_are_all_returned() {
        let mut decoder = Decoder::new();
        let messages = decoder.push(b"id: 1\ndata: one\n\nid: 2\ndata: two\n\n");
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[1].id.as_deref(), Some("2"));
    }
}
