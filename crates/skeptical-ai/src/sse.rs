//! Minimal Server-Sent Events framing: bytes in, `(event, data)` frames out.

#[derive(Debug, Default)]
pub struct Parser {
    buf: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Frame {
    pub event: Option<String>,
    pub data: String,
}

impl Parser {
    /// Feed a chunk; returns every frame completed by it.
    pub fn push(&mut self, chunk: &str) -> Vec<Frame> {
        self.buf.push_str(&chunk.replace("\r\n", "\n"));
        let mut out = Vec::new();
        while let Some(end) = self.buf.find("\n\n") {
            let block: String = self.buf.drain(..end + 2).collect();
            if let Some(f) = parse_block(&block) {
                out.push(f);
            }
        }
        out
    }

    /// Flush a final frame that ended without a blank line.
    pub fn finish(&mut self) -> Option<Frame> {
        let rest = std::mem::take(&mut self.buf);
        parse_block(&rest)
    }
}

fn parse_block(block: &str) -> Option<Frame> {
    let mut event = None;
    let mut data: Vec<&str> = Vec::new();
    for line in block.lines() {
        if line.starts_with(':') {
            continue;
        }
        let (field, value) = match line.split_once(':') {
            Some((f, v)) => (f, v.strip_prefix(' ').unwrap_or(v)),
            None => (line, ""),
        };
        match field {
            "event" => event = Some(value.to_string()),
            "data" => data.push(value),
            _ => {}
        }
    }
    if data.is_empty() && event.is_none() {
        return None;
    }
    Some(Frame {
        event,
        data: data.join("\n"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_split_across_chunks() {
        let mut p = Parser::default();
        assert!(p.push("data: {\"a\"").is_empty());
        let f = p.push(":1}\n\ndata: [DONE]\n\n");
        assert_eq!(
            f,
            vec![
                Frame {
                    event: None,
                    data: "{\"a\":1}".into()
                },
                Frame {
                    event: None,
                    data: "[DONE]".into()
                },
            ]
        );
    }

    #[test]
    fn named_events_and_comments() {
        let mut p = Parser::default();
        let f = p.push(": keep-alive\n\nevent: message_start\ndata: {}\n\n");
        assert_eq!(
            f,
            vec![Frame {
                event: Some("message_start".into()),
                data: "{}".into()
            }]
        );
    }

    #[test]
    fn crlf_and_multiline_data() {
        let mut p = Parser::default();
        let f = p.push("data: a\r\ndata: b\r\n\r\n");
        assert_eq!(
            f,
            vec![Frame {
                event: None,
                data: "a\nb".into()
            }]
        );
    }

    #[test]
    fn finish_flushes_unterminated_frame() {
        let mut p = Parser::default();
        assert!(p.push("data: tail").is_empty());
        assert_eq!(
            p.finish(),
            Some(Frame {
                event: None,
                data: "tail".into()
            })
        );
        assert_eq!(p.finish(), None);
    }

    #[test]
    fn data_without_space() {
        let mut p = Parser::default();
        assert_eq!(p.push("data:x\n\n")[0].data, "x");
    }
}
