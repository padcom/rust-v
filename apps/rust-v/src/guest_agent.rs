//! The host side of the QEMU guest agent protocol: JSON commands, one per line, to the
//! guest's qemu-ga on the virtio-serial port `org.qemu.guest_agent.0`, and its JSON replies.
//! Used by the MCP server to run programs and move files without a login.

use std::sync::Mutex;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use rv_machine::virtio::console::GuestAgentHandle;
use serde_json::{Value, json};

pub const NOT_RUNNING: &str = "the guest agent is not running in the guest: install and start it \
    there (`apt install qemu-guest-agent`, then `systemctl start qemu-guest-agent` if it did not \
    start by itself), or use the console tools";

/// Largest file read or written through the agent.
pub const MAX_FILE: usize = 16 << 20;
/// Bytes per guest-file-read/write call.
const FILE_CHUNK: usize = 1 << 20;

pub struct GuestAgent {
    port: GuestAgentHandle,
    io: Mutex<Io>,
}

struct Io {
    rx: Receiver<Vec<u8>>,
    /// Bytes received and not yet consumed.
    buf: Vec<u8>,
    next_id: u64,
}

/// A finished command.
pub struct Exited {
    pub code: Option<i64>,
    pub signal: Option<i64>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub truncated: bool,
}

impl Io {
    /// Waits for more bytes from the guest until `deadline`.
    fn fill(&mut self, deadline: Instant) -> Result<(), String> {
        let left = deadline.saturating_duration_since(Instant::now());
        match self.rx.recv_timeout(left) {
            Ok(data) => {
                self.buf.extend_from_slice(&data);
                Ok(())
            }
            Err(RecvTimeoutError::Timeout) => Err("the guest agent did not answer in time".into()),
            Err(RecvTimeoutError::Disconnected) => Err("the guest agent's port is gone".into()),
        }
    }

    /// The next JSON object the agent sent.
    fn read_json(&mut self, deadline: Instant) -> Result<Value, String> {
        loop {
            while let Some(end) = self.buf.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = self.buf.drain(..=end).collect();
                if let Ok(v) = serde_json::from_slice::<Value>(&line) {
                    return Ok(v);
                }
            }
            self.fill(deadline)?;
        }
    }

    /// Resynchronizes with the agent, dropping whatever an earlier, abandoned command left.
    fn sync(&mut self, port: &GuestAgentHandle) -> Result<(), String> {
        self.next_id += 1;
        let id = self.next_id;
        // 0xff resets the agent's parser; guest-sync-delimited answers with 0xff before its
        // reply, so everything up to that byte can be thrown away.
        let mut request = vec![0xff];
        request.extend_from_slice(
            json!({"execute": "guest-sync-delimited", "arguments": {"id": id}}).to_string().as_bytes(),
        );
        request.push(b'\n');
        while self.rx.try_recv().is_ok() {}
        self.buf.clear();
        port.send(&request);
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(at) = self.buf.iter().position(|&b| b == 0xff) {
                self.buf.drain(..=at);
                break;
            }
            self.buf.clear();
            self.fill(deadline)?;
        }
        loop {
            if self.read_json(deadline)?.get("return").and_then(Value::as_u64) == Some(id) {
                return Ok(());
            }
        }
    }
}

impl GuestAgent {
    pub fn new(port: GuestAgentHandle, rx: Receiver<Vec<u8>>) -> Self {
        GuestAgent { port, io: Mutex::new(Io { rx, buf: Vec::new(), next_id: 0 }) }
    }

    pub fn is_running(&self) -> bool {
        self.port.is_open()
    }

    /// Runs agent command `execute` and returns what it returned.
    pub fn call(&self, execute: &str, arguments: Value, timeout: Duration) -> Result<Value, String> {
        if !self.port.is_open() {
            return Err(NOT_RUNNING.into());
        }
        let mut io = self.io.lock().unwrap();
        io.sync(&self.port)?;
        let mut request = json!({"execute": execute});
        if !arguments.is_null() {
            request["arguments"] = arguments;
        }
        self.port.send(format!("{request}\n").as_bytes());
        let reply = io.read_json(Instant::now() + timeout)?;
        if let Some(e) = reply.get("error") {
            let desc = e.get("desc").and_then(Value::as_str).unwrap_or("unknown error");
            return Err(format!("guest agent: {desc}"));
        }
        reply.get("return").cloned().ok_or_else(|| format!("guest agent: unexpected reply {reply}"))
    }

    /// Runs `command` with `/bin/sh -c`, feeding it `input`, and waits up to `timeout` for it.
    pub fn exec(&self, command: &str, input: Option<&[u8]>, timeout: Duration) -> Result<Exited, String> {
        let mut args = json!({"path": "/bin/sh", "arg": ["-c", command], "capture-output": true});
        if let Some(input) = input {
            args["input-data"] = base64_encode(input).into();
        }
        let started = self.call("guest-exec", args, Duration::from_secs(10))?;
        let pid =
            started.get("pid").and_then(Value::as_i64).ok_or("guest agent: guest-exec returned no pid")?;
        let deadline = Instant::now() + timeout;
        let mut pause = Duration::from_millis(50);
        loop {
            let status = self.call("guest-exec-status", json!({"pid": pid}), Duration::from_secs(10))?;
            if status.get("exited").and_then(Value::as_bool) == Some(true) {
                let data = |key: &str| {
                    status
                        .get(key)
                        .and_then(Value::as_str)
                        .map(base64_decode)
                        .transpose()
                        .map(Option::unwrap_or_default)
                };
                return Ok(Exited {
                    code: status.get("exitcode").and_then(Value::as_i64),
                    signal: status.get("signal").and_then(Value::as_i64),
                    stdout: data("out-data")?,
                    stderr: data("err-data")?,
                    truncated: ["out-truncated", "err-truncated"]
                        .iter()
                        .any(|k| status.get(*k).and_then(Value::as_bool) == Some(true)),
                });
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "still running after {}s (pid {pid} in the guest; its output is lost)",
                    timeout.as_secs()
                ));
            }
            std::thread::sleep(pause);
            pause = (pause * 2).min(Duration::from_millis(500));
        }
    }

    pub fn read_file(&self, path: &str) -> Result<Vec<u8>, String> {
        let timeout = Duration::from_secs(30);
        let handle = self.call("guest-file-open", json!({"path": path, "mode": "r"}), timeout)?;
        let mut data = Vec::new();
        let result = loop {
            let chunk =
                match self.call("guest-file-read", json!({"handle": handle, "count": FILE_CHUNK}), timeout) {
                    Ok(chunk) => chunk,
                    Err(e) => break Err(e),
                };
            match chunk.get("buf-b64").and_then(Value::as_str).map(base64_decode) {
                Some(Ok(bytes)) => data.extend_from_slice(&bytes),
                Some(Err(e)) => break Err(e),
                None => break Err("guest agent: guest-file-read returned no data".into()),
            }
            if data.len() > MAX_FILE {
                break Err(format!("{path} is larger than {} MiB", MAX_FILE >> 20));
            }
            if chunk.get("eof").and_then(Value::as_bool) != Some(false) {
                break Ok(data);
            }
        };
        let _ = self.call("guest-file-close", json!({"handle": handle}), timeout);
        result
    }

    pub fn write_file(&self, path: &str, data: &[u8], append: bool) -> Result<(), String> {
        let timeout = Duration::from_secs(30);
        let mode = if append { "a" } else { "w" };
        let handle = self.call("guest-file-open", json!({"path": path, "mode": mode}), timeout)?;
        let result = data.chunks(FILE_CHUNK).try_for_each(|chunk| {
            self.call("guest-file-write", json!({"handle": handle, "buf-b64": base64_encode(chunk)}), timeout)
                .map(drop)
        });
        let closed = self.call("guest-file-close", json!({"handle": handle}), timeout);
        result.and(closed.map(drop))
    }
}

const BASE64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub fn base64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = u32::from_be_bytes([0, b[0], b[1], b[2]]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(BASE64[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

pub fn base64_decode(text: &str) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(text.len() / 4 * 3);
    let (mut acc, mut bits) = (0u32, 0);
    for c in text.bytes().filter(|c| !c.is_ascii_whitespace() && *c != b'=') {
        let v = BASE64.iter().position(|&b| b == c).ok_or("invalid base64")? as u32;
        acc = acc << 6 | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_round_trip() {
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b""), "");
        for s in ["", "f", "fo", "foo", "foob", "fooba", "foobar"] {
            assert_eq!(base64_decode(&base64_encode(s.as_bytes())).unwrap(), s.as_bytes());
        }
        assert!(base64_decode("Zm9v!").is_err());
    }
}
