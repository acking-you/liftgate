//! Incremental resumable Server-Sent Events (SSE) parser reusing frame buffers.

/// Single decoded SSE frame (event + concatenated data lines).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SseFrame {
    pub event: Option<String>,
    pub data: String,
}

/// Resumable line-oriented SSE state machine that reuses internal buffers across frames.
#[derive(Debug, Default)]
pub struct SseDecoder {
    current_event: Option<String>,
    current_data: String,
    has_data_line: bool,
}

impl SseDecoder {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Feeds one CR/LF-stripped text line from a text/event-stream body.
    /// Returns Some(frame) when a blank line dispatches a complete frame.
    pub fn push_line(&mut self, raw_line: &str) -> Option<SseFrame> {
        let line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
        if line.is_empty() {
            return self.take_frame();
        }
        if line.starts_with(':') {
            return None;
        }
        let (field, value) = match line.split_once(':') {
            Some((f, rest)) => (f, rest.strip_prefix(' ').unwrap_or(rest)),
            None => (line, ""),
        };
        match field {
            "event" => {
                self.current_event = Some(value.to_owned());
            }
            "data" => {
                if self.has_data_line {
                    self.current_data.push('\n');
                }
                self.current_data.push_str(value);
                self.has_data_line = true;
            }
            _ => {}
        }
        None
    }

    /// Flushes any trailing buffered frame when the HTTP stream closes cleanly without a final blank line.
    pub fn finish(&mut self) -> Option<SseFrame> {
        self.take_frame()
    }

    fn take_frame(&mut self) -> Option<SseFrame> {
        if !self.has_data_line && self.current_event.is_none() {
            return None;
        }
        let event = self.current_event.take();
        let data = std::mem::take(&mut self.current_data);
        self.has_data_line = false;
        if data.is_empty() && event.is_none() {
            return None;
        }
        Some(SseFrame { event, data })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse_decoder_parses_multiline_data_and_reuses_state() {
        let mut dec = SseDecoder::new();
        assert_eq!(dec.push_line("event: content_block_delta\r"), None);
        assert_eq!(dec.push_line("data: {\"a\": 1,"), None);
        assert_eq!(dec.push_line("data: \"b\": 2}"), None);
        let f1 = dec.push_line("").expect("frame 1");
        assert_eq!(f1.event.as_deref(), Some("content_block_delta"));
        assert_eq!(f1.data, "{\"a\": 1,\n\"b\": 2}");

        assert_eq!(dec.push_line(": keepalive"), None);
        assert_eq!(dec.push_line("data: [DONE]"), None);
        let f2 = dec.finish().expect("frame 2");
        assert_eq!(f2.event, None);
        assert_eq!(f2.data, "[DONE]");
    }
}
