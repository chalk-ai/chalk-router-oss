//! Incremental Server-Sent Events framing for protocol taps that need complete
//! named frames. Responses uses it today; other frontends can adopt it without
//! coupling their protocol-specific event semantics here.

use bytes::Bytes;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SseFrame {
    pub event: Option<String>,
    pub data: String,
    pub wire: Bytes,
}

impl SseFrame {
    /// Retain field order, comments, id/retry fields, line endings and data-line
    /// prefixes when replacing JSON string tokens. Such edits never introduce
    /// literal newlines, so the original data-line layout remains valid.
    pub(crate) fn replace_data(&self, data: &str) -> Bytes {
        let mut values = data.split('\n');
        let mut output = Vec::with_capacity(self.wire.len() + data.len());
        for line in self.wire.split_inclusive(|byte| *byte == b'\n') {
            let content_end = line.len()
                - usize::from(line.ends_with(b"\n"))
                - usize::from(line.ends_with(b"\r\n") || line.ends_with(b"\r"));
            let content = &line[..content_end];
            if content == b"data" || content.starts_with(b"data:") {
                let start = if content == b"data" {
                    content.len()
                } else {
                    5 + usize::from(content.get(5) == Some(&b' '))
                };
                output.extend_from_slice(&line[..start]);
                output.extend_from_slice(values.next().expect("retained data line").as_bytes());
                output.extend_from_slice(&line[content_end..]);
            } else {
                output.extend_from_slice(line);
            }
        }
        debug_assert!(values.next().is_none());
        output.into()
    }
}

/// Encode a single SSE frame: an optional `event:` line followed by a `data:`
/// line and the blank-line terminator. `data` is written verbatim on one
/// `data:` line — callers with multi-line payloads (e.g. pretty-printed JSON)
/// must collapse them first, matching how every frame emitted in this codebase
/// carries a single-line JSON payload.
pub(crate) fn encode_frame(event: Option<&str>, data: &str) -> Bytes {
    match event {
        Some(event) => Bytes::from(format!("event: {event}\ndata: {data}\n\n")),
        None => Bytes::from(format!("data: {data}\n\n")),
    }
}

/// The `[DONE]` sentinel OpenAI-style (chat completions, responses) SSE
/// streams terminate with. Anthropic's wire format has no equivalent — the
/// stream simply ends after `message_stop` — so callers on that side never
/// use this.
pub(crate) fn done_frame() -> Bytes {
    Bytes::from_static(b"data: [DONE]\n\n")
}

#[derive(Debug, Default)]
pub(crate) struct SseDecoder {
    buffered: Vec<u8>,
    event: Option<String>,
    data: Vec<String>,
    /// Wire bytes in consumed lines belonging to the current incomplete frame.
    wire: Vec<u8>,
}

impl SseDecoder {
    pub(crate) fn push(&mut self, bytes: &[u8]) -> Vec<SseFrame> {
        self.buffered.extend_from_slice(bytes);
        let mut frames = Vec::new();
        while let Some(newline) = self.buffered.iter().position(|byte| *byte == b'\n') {
            let mut line = self.buffered.drain(..=newline).collect::<Vec<_>>();
            self.wire.extend_from_slice(&line);
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            self.consume_line(&line, &mut frames);
        }
        frames
    }

    /// Length of the unfinished wire suffix, including consumed `event:` and
    /// `data:` lines. Native taps can retain that suffix and forward complete
    /// frames byte-for-byte without duplicating the SSE parser.
    pub(crate) fn pending_bytes(&self) -> usize {
        self.wire.len() + self.buffered.len()
    }

    pub(crate) fn finish(&mut self) -> Vec<SseFrame> {
        let mut frames = Vec::new();
        if !self.buffered.is_empty() {
            let mut line = std::mem::take(&mut self.buffered);
            self.wire.extend_from_slice(&line);
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            self.consume_line(&line, &mut frames);
        }
        self.emit(&mut frames);
        frames
    }

    fn consume_line(&mut self, line: &[u8], frames: &mut Vec<SseFrame>) {
        if line.is_empty() {
            self.emit(frames);
            return;
        }
        if line[0] == b':' {
            return;
        }
        let (field, value) = match line.iter().position(|byte| *byte == b':') {
            Some(colon) => {
                let mut value = &line[colon + 1..];
                if value.first() == Some(&b' ') {
                    value = &value[1..];
                }
                (&line[..colon], value)
            }
            None => (line, &[][..]),
        };
        match field {
            b"event" => self.event = Some(String::from_utf8_lossy(value).into_owned()),
            b"data" => self.data.push(String::from_utf8_lossy(value).into_owned()),
            _ => {}
        }
    }

    fn emit(&mut self, frames: &mut Vec<SseFrame>) {
        if !self.wire.is_empty() {
            frames.push(SseFrame {
                event: self.event.take(),
                data: std::mem::take(&mut self.data).join("\n"),
                wire: std::mem::take(&mut self.wire).into(),
            });
        }
        self.event = None;
        self.data.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retained_frames_keep_comments_metadata_and_unterminated_multiline_data() {
        let wire = b": comment\r\n\r\nid: 1\r\nretry: 123\r\nevent: named\r\ndata:{\"id\":\"old\",\r\n: inside\r\ndata: \"number\":1.0000000000000001}";
        let mut decoder = SseDecoder::default();
        let mut frames = Vec::new();
        for chunk in wire.chunks(3) {
            frames.extend(decoder.push(chunk));
        }
        frames.extend(decoder.finish());
        assert_eq!(frames.len(), 2);
        assert_eq!(&frames[0].wire[..], b": comment\r\n\r\n");
        let data = frames[1].data.replace("\"old\"", "\"wrapped\"");
        let edited = frames[1].replace_data(&data);
        assert_eq!(
            &edited[..],
            String::from_utf8(wire[frames[0].wire.len()..].to_vec())
                .unwrap()
                .replace("\"old\"", "\"wrapped\"")
                .as_bytes()
        );
        assert_eq!(decoder.pending_bytes(), 0);
    }

    #[test]
    fn handles_fragmentation_crlf_and_multiline_data() {
        let mut decoder = SseDecoder::default();
        assert!(decoder
            .push(b"event: response.output_text.delta\r\nda")
            .is_empty());
        assert!(decoder.push(b"ta: hel").is_empty());
        let frames = decoder.push(b"lo\r\ndata: world\r\n\r\n");
        assert_eq!(
            frames,
            vec![SseFrame {
                event: Some("response.output_text.delta".into()),
                data: "hello\nworld".into(),
                wire: Bytes::from_static(
                    b"event: response.output_text.delta\r\ndata: hello\r\ndata: world\r\n\r\n"
                ),
            }]
        );
    }

    #[test]
    fn preserves_split_multibyte_utf8() {
        let mut decoder = SseDecoder::default();
        let encoded = "data: 🦀\n\n".as_bytes();
        assert!(decoder.push(&encoded[..8]).is_empty());
        assert_eq!(decoder.push(&encoded[8..])[0].data, "🦀");
    }

    #[test]
    fn emits_unterminated_trailing_frame_at_eof() {
        let mut decoder = SseDecoder::default();
        assert!(decoder.push(b"event: error\ndata: {}").is_empty());
        assert_eq!(
            decoder.finish(),
            vec![SseFrame {
                event: Some("error".into()),
                data: "{}".into(),
                wire: Bytes::from_static(b"event: error\ndata: {}"),
            }]
        );
    }

    #[test]
    fn ignores_comments_and_accepts_data_without_space() {
        let mut decoder = SseDecoder::default();
        let frames = decoder.push(b": keepalive\ndata:value\n\n");
        assert_eq!(frames[0].data, "value");
    }

    #[test]
    fn encode_frame_omits_event_line_when_none() {
        assert_eq!(&encode_frame(None, "{}")[..], b"data: {}\n\n".as_slice());
    }

    #[test]
    fn encode_frame_includes_event_line_when_some() {
        assert_eq!(
            &encode_frame(Some("error"), "{}")[..],
            b"event: error\ndata: {}\n\n".as_slice()
        );
    }

    #[test]
    fn done_frame_is_the_done_sentinel() {
        assert_eq!(&done_frame()[..], b"data: [DONE]\n\n".as_slice());
    }

    #[test]
    fn round_trips_through_the_decoder() {
        let mut decoder = SseDecoder::default();
        let mut frames = decoder.push(&encode_frame(Some("error"), "{\"a\":1}"));
        frames.extend(decoder.push(&done_frame()));
        assert_eq!(
            frames,
            vec![
                SseFrame {
                    event: Some("error".into()),
                    data: "{\"a\":1}".into(),
                    wire: encode_frame(Some("error"), "{\"a\":1}"),
                },
                SseFrame {
                    event: None,
                    data: "[DONE]".into(),
                    wire: done_frame(),
                },
            ]
        );
    }
}
