//! The WebSocket wire protocol for attaching to a terminal, as OpenCode's
//! `PtyProtocol`. Outbound frames are UTF-8 terminal output (text frames);
//! one control frame, a binary frame of a 0x00 byte and UTF-8 JSON, carries
//! the absolute output cursor after the replay so a client can resume
//! later. Inbound frames are the terminal's input, as text or as binary
//! UTF-8; binary input that is not UTF-8 is dropped.

/// The largest replay frame: replay can be megabytes, so it goes out in
/// frames of at most this many bytes.
pub const REPLAY_CHUNK: usize = 64 * 1024;

/// The control frame that carries `cursor`.
pub fn meta_frame(cursor: u64) -> Vec<u8> {
    let mut out = vec![0u8];
    out.extend_from_slice(
        serde_json::json!({ "cursor": cursor })
            .to_string()
            .as_bytes(),
    );
    out
}

/// `data` in frames of at most [`REPLAY_CHUNK`] bytes, each cut on a
/// character boundary; none for empty data.
pub fn chunks(data: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = data;
    while !rest.is_empty() {
        let mut at = rest.len().min(REPLAY_CHUNK);
        while !rest.is_char_boundary(at) {
            at -= 1;
        }
        let (head, tail) = rest.split_at(at);
        out.push(head);
        rest = tail;
    }
    out
}

/// A binary input frame as text, or `None` (dropped) when it is not UTF-8.
pub fn decode_input(frame: &[u8]) -> Option<&str> {
    std::str::from_utf8(frame).ok()
}
