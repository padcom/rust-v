//! The host ("client") side of the SPICE guest agent protocol, as far as clipboard sharing
//! needs it. The guest's spice-vdagentd talks it over the virtio-serial port
//! `com.redhat.spice.0`; this is what QEMU's `qemu-vdagent` character device does too.
//!
//! The byte stream is a sequence of chunks (`port`, `size`, then `size` bytes); the chunks'
//! payloads concatenated are messages (`protocol`, `type`, `opaque`, `size`, data).

/// Chunk header port for messages between the client and the agent.
const CLIENT_PORT: u32 = 1;
const PROTOCOL: u32 = 1;
const CHUNK_HEADER: usize = 8;
const MESSAGE_HEADER: usize = 20;
/// Largest chunk payload the agent accepts.
const MAX_CHUNK: usize = 2048;
/// Larger clipboard contents are not transferred.
pub const MAX_CLIPBOARD: usize = 16 << 20;

mod msg {
    pub const CLIPBOARD: u32 = 4;
    pub const ANNOUNCE_CAPABILITIES: u32 = 6;
    pub const CLIPBOARD_GRAB: u32 = 7;
    pub const CLIPBOARD_REQUEST: u32 = 8;
    pub const CLIPBOARD_RELEASE: u32 = 9;
}

mod cap {
    pub const CLIPBOARD_BY_DEMAND: u32 = 5;
    pub const CLIPBOARD_SELECTION: u32 = 6;
}

const TYPE_UTF8_TEXT: u32 = 1;
/// The selection that clipboard messages carry when both sides have CLIPBOARD_SELECTION.
const SELECTION_CLIPBOARD: u8 = 0;

/// What the agent told the host.
#[derive(Debug, PartialEq, Eq)]
pub enum Event {
    /// The guest's clipboard now holds this text.
    GuestText(String),
}

#[derive(Default)]
pub struct Agent {
    /// Bytes received that do not yet make up a whole chunk.
    stream: Vec<u8>,
    /// Chunk payloads that do not yet make up a whole message.
    message: Vec<u8>,
    /// The agent's capabilities word, once announced.
    agent_caps: Option<u32>,
    /// The host clipboard text the guest was last offered.
    host_text: Option<String>,
}

impl Agent {
    pub fn new() -> Self {
        Self::default()
    }

    fn selection(&self) -> bool {
        self.agent_caps.is_some_and(|c| c & (1 << cap::CLIPBOARD_SELECTION) != 0)
    }

    /// The guest opened the port: the host announces itself.
    pub fn opened(&mut self, out: &mut Vec<u8>) {
        self.stream.clear();
        self.message.clear();
        self.agent_caps = None;
        self.announce(true, out);
    }

    fn announce(&self, request: bool, out: &mut Vec<u8>) {
        let caps = (1u32 << cap::CLIPBOARD_BY_DEMAND) | (1 << cap::CLIPBOARD_SELECTION);
        let mut data = Vec::with_capacity(8);
        data.extend_from_slice(&(request as u32).to_le_bytes());
        data.extend_from_slice(&caps.to_le_bytes());
        message(msg::ANNOUNCE_CAPABILITIES, &data, out);
    }

    /// Clipboard message data: the selection prefix when negotiated, then `body`.
    fn clipboard(&self, kind: u32, body: &[u8], out: &mut Vec<u8>) {
        let mut data = Vec::with_capacity(4 + body.len());
        if self.selection() {
            data.extend_from_slice(&[SELECTION_CLIPBOARD, 0, 0, 0]);
        }
        data.extend_from_slice(body);
        message(kind, &data, out);
    }

    /// The host clipboard changed: offer the text to the guest (sent once the agent asks).
    pub fn host_text(&mut self, text: String, out: &mut Vec<u8>) {
        if text.len() > MAX_CLIPBOARD {
            return;
        }
        self.host_text = Some(text);
        if self.agent_caps.is_some() {
            self.clipboard(msg::CLIPBOARD_GRAB, &TYPE_UTF8_TEXT.to_le_bytes(), out);
        }
    }

    /// Bytes from the guest; returns what they mean for the host, and appends replies to `out`.
    pub fn received(&mut self, data: &[u8], out: &mut Vec<u8>) -> Vec<Event> {
        self.stream.extend_from_slice(data);
        let mut at = 0;
        while self.stream.len() - at >= CHUNK_HEADER {
            let size = u32_at(&self.stream, at + 4) as usize;
            if self.stream.len() - at - CHUNK_HEADER < size {
                break;
            }
            let start = at + CHUNK_HEADER;
            self.message.extend_from_slice(&self.stream[start..start + size]);
            at = start + size;
        }
        self.stream.drain(..at);
        let mut events = Vec::new();
        loop {
            if self.message.len() < MESSAGE_HEADER {
                break;
            }
            let size = u32_at(&self.message, 16) as usize;
            if size > MAX_CLIPBOARD + 64 {
                // Not something this side asked for: drop the stream rather than buffer it.
                self.message.clear();
                break;
            }
            if self.message.len() < MESSAGE_HEADER + size {
                break;
            }
            let kind = u32_at(&self.message, 4);
            let body: Vec<u8> = self.message[MESSAGE_HEADER..MESSAGE_HEADER + size].to_vec();
            self.message.drain(..MESSAGE_HEADER + size);
            self.handle(kind, &body, out, &mut events);
        }
        events
    }

    fn handle(&mut self, kind: u32, body: &[u8], out: &mut Vec<u8>, events: &mut Vec<Event>) {
        if kind == msg::ANNOUNCE_CAPABILITIES {
            if body.len() < 8 {
                return;
            }
            let first = self.agent_caps.is_none();
            self.agent_caps = Some(u32_at(body, 4));
            if u32_at(body, 0) != 0 {
                self.announce(false, out);
            }
            if first && self.host_text.is_some() {
                self.clipboard(msg::CLIPBOARD_GRAB, &TYPE_UTF8_TEXT.to_le_bytes(), out);
            }
            return;
        }
        let body = if self.selection() {
            // Only the CLIPBOARD selection is shared, not PRIMARY.
            match body.split_first_chunk::<4>() {
                Some((sel, rest)) if sel[0] == SELECTION_CLIPBOARD => rest,
                _ => return,
            }
        } else {
            body
        };
        match kind {
            msg::CLIPBOARD_GRAB => {
                if body.as_chunks::<4>().0.iter().any(|t| u32::from_le_bytes(*t) == TYPE_UTF8_TEXT) {
                    self.clipboard(msg::CLIPBOARD_REQUEST, &TYPE_UTF8_TEXT.to_le_bytes(), out);
                }
            }
            msg::CLIPBOARD_REQUEST => {
                let text = match self.host_text.as_deref() {
                    Some(t) if body.len() >= 4 && u32_at(body, 0) == TYPE_UTF8_TEXT => t,
                    // Nothing to give: an empty reply of type NONE ends the guest's wait.
                    _ => {
                        self.clipboard(msg::CLIPBOARD, &0u32.to_le_bytes(), out);
                        return;
                    }
                };
                let mut data = Vec::with_capacity(4 + text.len());
                data.extend_from_slice(&TYPE_UTF8_TEXT.to_le_bytes());
                data.extend_from_slice(text.as_bytes());
                self.clipboard(msg::CLIPBOARD, &data, out);
            }
            msg::CLIPBOARD if body.len() >= 4 && u32_at(body, 0) == TYPE_UTF8_TEXT => {
                let text = String::from_utf8_lossy(&body[4..]);
                // Some agents NUL-terminate the text.
                let text = text.trim_end_matches('\0').to_string();
                self.host_text = Some(text.clone());
                events.push(Event::GuestText(text));
            }
            msg::CLIPBOARD_RELEASE => {}
            _ => {}
        }
    }
}

/// Appends message `kind` with `data`, split into chunks.
fn message(kind: u32, data: &[u8], out: &mut Vec<u8>) {
    let mut m = Vec::with_capacity(MESSAGE_HEADER + data.len());
    m.extend_from_slice(&PROTOCOL.to_le_bytes());
    m.extend_from_slice(&kind.to_le_bytes());
    m.extend_from_slice(&0u64.to_le_bytes());
    m.extend_from_slice(&(data.len() as u32).to_le_bytes());
    m.extend_from_slice(data);
    for chunk in m.chunks(MAX_CHUNK) {
        out.extend_from_slice(&CLIENT_PORT.to_le_bytes());
        out.extend_from_slice(&(chunk.len() as u32).to_le_bytes());
        out.extend_from_slice(chunk);
    }
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Splits a stream of chunks into its messages: (type, data).
    fn messages(mut stream: &[u8]) -> Vec<(u32, Vec<u8>)> {
        let mut payload = Vec::new();
        while !stream.is_empty() {
            let size = u32_at(stream, 4) as usize;
            assert!(size <= MAX_CHUNK);
            payload.extend_from_slice(&stream[8..8 + size]);
            stream = &stream[8 + size..];
        }
        let mut out = Vec::new();
        let mut p = &payload[..];
        while !p.is_empty() {
            let size = u32_at(p, 16) as usize;
            out.push((u32_at(p, 4), p[20..20 + size].to_vec()));
            p = &p[20 + size..];
        }
        out
    }

    fn agent_message(kind: u32, data: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        message(kind, data, &mut out);
        out
    }

    fn connected() -> Agent {
        let mut a = Agent::new();
        let mut out = Vec::new();
        a.opened(&mut out);
        let m = messages(&out);
        assert_eq!(m[0].0, msg::ANNOUNCE_CAPABILITIES);
        assert_eq!(u32_at(&m[0].1, 0), 1, "asks for the agent's capabilities");
        let caps = (1u32 << cap::CLIPBOARD_BY_DEMAND) | (1 << cap::CLIPBOARD_SELECTION);
        let mut announce = 0u32.to_le_bytes().to_vec();
        announce.extend_from_slice(&caps.to_le_bytes());
        let mut out = Vec::new();
        assert!(a.received(&agent_message(msg::ANNOUNCE_CAPABILITIES, &announce), &mut out).is_empty());
        assert!(out.is_empty());
        a
    }

    #[test]
    fn guest_copy_is_fetched() {
        let mut a = connected();
        let mut out = Vec::new();
        let grab = [[0u8, 0, 0, 0], TYPE_UTF8_TEXT.to_le_bytes()].concat();
        a.received(&agent_message(msg::CLIPBOARD_GRAB, &grab), &mut out);
        assert_eq!(
            messages(&out),
            vec![(msg::CLIPBOARD_REQUEST, [[0u8, 0, 0, 0], TYPE_UTF8_TEXT.to_le_bytes()].concat())]
        );
        // The reply, large enough to span chunks and fed a few bytes at a time.
        let text = "héllo ".repeat(1000);
        let mut data = [[0u8, 0, 0, 0], TYPE_UTF8_TEXT.to_le_bytes()].concat();
        data.extend_from_slice(text.as_bytes());
        let stream = agent_message(msg::CLIPBOARD, &data);
        let mut events = Vec::new();
        for piece in stream.chunks(7) {
            events.extend(a.received(piece, &mut out));
        }
        assert_eq!(events, vec![Event::GuestText(text)]);
    }

    #[test]
    fn host_copy_is_offered_then_sent() {
        let mut a = connected();
        let mut out = Vec::new();
        a.host_text("from the host".into(), &mut out);
        assert_eq!(
            messages(&out),
            vec![(msg::CLIPBOARD_GRAB, [[0u8, 0, 0, 0], TYPE_UTF8_TEXT.to_le_bytes()].concat())]
        );
        let mut out = Vec::new();
        let request = [[0u8, 0, 0, 0], TYPE_UTF8_TEXT.to_le_bytes()].concat();
        a.received(&agent_message(msg::CLIPBOARD_REQUEST, &request), &mut out);
        let mut expected = [[0u8, 0, 0, 0], TYPE_UTF8_TEXT.to_le_bytes()].concat();
        expected.extend_from_slice(b"from the host");
        assert_eq!(messages(&out), vec![(msg::CLIPBOARD, expected)]);
    }

    #[test]
    fn host_text_before_the_agent_is_offered_on_connect() {
        let mut a = Agent::new();
        let mut out = Vec::new();
        a.host_text("early".into(), &mut out);
        a.opened(&mut out);
        out.clear();
        let mut announce = 1u32.to_le_bytes().to_vec();
        announce.extend_from_slice(&(1u32 << cap::CLIPBOARD_BY_DEMAND).to_le_bytes());
        a.received(&agent_message(msg::ANNOUNCE_CAPABILITIES, &announce), &mut out);
        let m = messages(&out);
        assert_eq!(m[0].0, msg::ANNOUNCE_CAPABILITIES);
        assert_eq!(u32_at(&m[0].1, 0), 0, "a reply, not a request");
        // Without CLIPBOARD_SELECTION there is no selection prefix.
        assert_eq!(m[1], (msg::CLIPBOARD_GRAB, TYPE_UTF8_TEXT.to_le_bytes().to_vec()));
    }

    #[test]
    fn primary_selection_is_ignored() {
        let mut a = connected();
        let mut out = Vec::new();
        let grab = [[1u8, 0, 0, 0], TYPE_UTF8_TEXT.to_le_bytes()].concat();
        a.received(&agent_message(msg::CLIPBOARD_GRAB, &grab), &mut out);
        assert!(out.is_empty());
    }
}
