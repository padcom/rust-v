//! An MCP (Model Context Protocol) server through which an AI assistant uses the guest: it
//! reads and types on the serial console, runs commands and moves files through the guest
//! agent, and looks at and operates the display. `--mcp stdio` speaks JSON-RPC on stdin and
//! stdout (the client starts rust-v); `--mcp [HOST:]PORT` serves Streamable HTTP at `/mcp`.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use rv_machine::display::Display;
use rv_machine::uart::Uart;
use rv_machine::virtio::input::{ABS_MAX, InputHandle, ev};
use serde_json::{Value, json};

use crate::guest_agent::{self, GuestAgent};

/// Protocol revisions this server speaks, newest first.
const PROTOCOL_VERSIONS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

const INSTRUCTIONS: &str = "This server controls a RISC-V Linux virtual machine (rust-v). \
The serial console tools work from boot on: console_read shows what the guest printed, \
console_write types (log in there first if a login prompt shows) and console_run runs a shell \
command at a console prompt and returns its output. The guest_* tools need no login but need \
the QEMU guest agent running in the guest (apt install qemu-guest-agent); they are the \
reliable way to run commands and read or write files. With a display, screenshot shows the \
screen, and type_text, press_keys and the mouse_* tools operate it (coordinates are screenshot \
pixels at scale 1).";

/// Console output kept for reading back.
const LOG_LIMIT: usize = 4 << 20;
/// Most console text one tool result carries.
const MAX_TEXT: usize = 64 << 10;

/// Everything the guest wrote on its serial console, for the MCP tools to read.
pub struct ConsoleLog {
    state: Mutex<LogState>,
    grew: Condvar,
}

struct LogState {
    data: Vec<u8>,
    /// Position (bytes since boot) of `data[0]`.
    start: u64,
}

impl ConsoleLog {
    pub fn new() -> Arc<Self> {
        Arc::new(ConsoleLog {
            state: Mutex::new(LogState { data: Vec::new(), start: 0 }),
            grew: Condvar::new(),
        })
    }

    fn append(&self, bytes: &[u8]) {
        let mut s = self.state.lock().unwrap();
        s.data.extend_from_slice(bytes);
        if s.data.len() > LOG_LIMIT {
            let drop = s.data.len() - LOG_LIMIT / 2;
            s.data.drain(..drop);
            s.start += drop as u64;
        }
        self.grew.notify_all();
    }

    fn end(&self) -> u64 {
        let s = self.state.lock().unwrap();
        s.start + s.data.len() as u64
    }

    /// Output from `pos` on (or from the oldest kept), and the position after it.
    fn since(&self, pos: u64) -> (Vec<u8>, u64) {
        let s = self.state.lock().unwrap();
        let from = pos.saturating_sub(s.start).min(s.data.len() as u64) as usize;
        (s.data[from..].to_vec(), s.start + s.data.len() as u64)
    }

    /// Waits until there is output beyond `pos`, then until it has been quiet for `settle`;
    /// gives up at `deadline`.
    fn wait_output(&self, pos: u64, settle: Duration, deadline: Instant) {
        let mut s = self.state.lock().unwrap();
        while s.start + (s.data.len() as u64) <= pos {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return;
            }
            s = self.grew.wait_timeout(s, left).unwrap().0;
        }
        loop {
            let left = deadline.saturating_duration_since(Instant::now()).min(settle);
            if left.is_zero() {
                return;
            }
            let (guard, timeout) = self.grew.wait_timeout(s, left).unwrap();
            s = guard;
            if timeout.timed_out() {
                return;
            }
        }
    }

    /// Waits until `pattern` appears at or after `pos`; returns where it starts.
    fn wait_for(&self, pos: u64, pattern: &[u8], deadline: Instant) -> Option<u64> {
        let mut s = self.state.lock().unwrap();
        loop {
            let from = pos.saturating_sub(s.start).min(s.data.len() as u64) as usize;
            if let Some(at) = s.data[from..].windows(pattern.len()).position(|w| w == pattern) {
                return Some(s.start + (from + at) as u64);
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return None;
            }
            s = self.grew.wait_timeout(s, left).unwrap().0;
        }
    }
}

/// The UART's output: into the console log, and on to `out` if there is one.
pub struct Tee {
    pub log: Arc<ConsoleLog>,
    pub out: Option<Box<dyn Write + Send>>,
}

impl Write for Tee {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if let Some(out) = &mut self.out {
            out.write_all(buf)?;
        }
        self.log.append(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.out.as_mut().map_or(Ok(()), |o| o.flush())
    }
}

/// Terminal output as plain text: escape sequences and carriage returns removed.
fn plain_text(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => match chars.next() {
                // CSI: parameters, then a final byte in @..~.
                Some('[') => {
                    for c in chars.by_ref() {
                        if ('@'..='~').contains(&c) {
                            break;
                        }
                    }
                }
                // OSC and friends: up to BEL or ESC \.
                Some(']' | 'P' | '_' | '^') => {
                    while let Some(c) = chars.next() {
                        if c == '\x07' || (c == '\x1b' && chars.next_if_eq(&'\\').is_some()) {
                            break;
                        }
                    }
                }
                // Character set selection takes one more character.
                Some('(' | ')') => drop(chars.next()),
                _ => {}
            },
            '\n' | '\t' => out.push(c),
            '\x08' => drop(out.pop()),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

/// Keeps at most the last `MAX_TEXT` bytes of `text`.
fn clip(text: String) -> String {
    if text.len() <= MAX_TEXT {
        return text;
    }
    let mut cut = text.len() - MAX_TEXT;
    while !text.is_char_boundary(cut) {
        cut += 1;
    }
    format!("[{cut} earlier bytes left out]\n{}", &text[cut..])
}

pub struct Devices {
    pub display: Option<Arc<Display>>,
    pub keyboard: Option<InputHandle>,
    pub tablet: Option<InputHandle>,
}

pub struct Server {
    log: Arc<ConsoleLog>,
    /// Where console_read continues.
    cursor: Mutex<u64>,
    uart: Arc<Uart>,
    agent: Option<GuestAgent>,
    devices: Devices,
    /// Tells console_run invocations apart.
    runs: Mutex<u64>,
    started: Instant,
}

type ToolResult = Result<Vec<Value>, String>;

fn text(s: impl Into<String>) -> Vec<Value> {
    vec![json!({"type": "text", "text": s.into()})]
}

fn str_arg<'a>(args: &'a Value, name: &str) -> Result<&'a str, String> {
    args.get(name).and_then(Value::as_str).ok_or_else(|| format!("missing string argument `{name}`"))
}

fn num_arg(args: &Value, name: &str, default: f64) -> Result<f64, String> {
    match args.get(name) {
        None | Some(Value::Null) => Ok(default),
        Some(v) => v.as_f64().ok_or_else(|| format!("argument `{name}` must be a number")),
    }
}

fn coord(args: &Value, name: &str) -> Result<f64, String> {
    args.get(name).and_then(Value::as_f64).ok_or_else(|| format!("missing number argument `{name}`"))
}

fn bool_arg(args: &Value, name: &str) -> bool {
    args.get(name).and_then(Value::as_bool).unwrap_or(false)
}

fn seconds(args: &Value, name: &str, default: f64) -> Result<Duration, String> {
    let s = num_arg(args, name, default)?;
    if !(0.0..=3600.0).contains(&s) {
        return Err(format!("`{name}` must be between 0 and 3600"));
    }
    Ok(Duration::from_secs_f64(s))
}

/// Linux key code and whether Shift is needed, for a character on a US keyboard.
fn char_key(c: char) -> Option<(u16, bool)> {
    const ROWS: [(&str, u16); 4] =
        [("1234567890", 2), ("qwertyuiop", 16), ("asdfghjkl", 30), ("zxcvbnm", 44)];
    const SHIFTED_DIGITS: &str = "!@#$%^&*()";
    const PUNCTUATION: [(char, char, u16); 11] = [
        ('-', '_', 12),
        ('=', '+', 13),
        ('[', '{', 26),
        (']', '}', 27),
        (';', ':', 39),
        ('\'', '"', 40),
        ('`', '~', 41),
        ('\\', '|', 43),
        (',', '<', 51),
        ('.', '>', 52),
        ('/', '?', 53),
    ];
    let lower = c.to_ascii_lowercase();
    for (row, first) in ROWS {
        if let Some(i) = row.find(lower) {
            return Some((first + i as u16, c.is_ascii_uppercase()));
        }
    }
    if let Some(i) = SHIFTED_DIGITS.find(c) {
        return Some((2 + i as u16, true));
    }
    for (plain, shifted, code) in PUNCTUATION {
        if c == plain || c == shifted {
            return Some((code, c == shifted));
        }
    }
    match c {
        ' ' => Some((57, false)),
        '\n' => Some((28, false)),
        '\t' => Some((15, false)),
        _ => None,
    }
}

/// Linux key code for a key name in a press_keys combination.
fn named_key(name: &str) -> Option<u16> {
    let lower = name.to_ascii_lowercase();
    Some(match lower.as_str() {
        "ctrl" | "control" => 29,
        "shift" => 42,
        "alt" | "option" => 56,
        "altgr" => 100,
        "super" | "meta" | "win" | "windows" | "cmd" | "command" => 125,
        "enter" | "return" => 28,
        "esc" | "escape" => 1,
        "tab" => 15,
        "backspace" => 14,
        "delete" | "del" => 111,
        "insert" => 110,
        "home" => 102,
        "end" => 107,
        "pageup" => 104,
        "pagedown" => 109,
        "up" => 103,
        "down" => 108,
        "left" => 105,
        "right" => 106,
        "space" => 57,
        "printscreen" => 99,
        f if f.starts_with('f') && f.len() > 1 => match f[1..].parse::<u16>().ok()? {
            n @ 1..=10 => 58 + n,
            11 => 87,
            12 => 88,
            _ => return None,
        },
        _ => {
            let mut chars = name.chars();
            let (Some(c), None) = (chars.next(), chars.next()) else { return None };
            return char_key(c.to_ascii_lowercase()).map(|(code, _)| code);
        }
    })
}

const KEY_SHIFT: u16 = 42;
/// Pause between key strokes and mouse actions, so the guest sees them as separate.
const STROKE: Duration = Duration::from_millis(8);

impl Server {
    pub fn new(
        log: Arc<ConsoleLog>,
        uart: Arc<Uart>,
        agent: Option<GuestAgent>,
        devices: Devices,
    ) -> Arc<Self> {
        Arc::new(Server {
            log,
            cursor: Mutex::new(0),
            uart,
            agent,
            devices,
            runs: Mutex::new(0),
            started: Instant::now(),
        })
    }

    fn tools(&self) -> Vec<Value> {
        let mut tools = vec![
            json!({
                "name": "console_read",
                "description": "Returns what the guest printed on its serial console since the last console_read (escape sequences removed). Waits up to wait_seconds for new output when there is none yet, and returns once it has been quiet briefly. With tail, returns the last tail bytes instead.",
                "inputSchema": {"type": "object", "properties": {
                    "wait_seconds": {"type": "number", "description": "How long to wait for output (default 0)."},
                    "tail": {"type": "integer", "description": "Return the last this many bytes of output, read or not."}
                }},
                "annotations": {"readOnlyHint": true}
            }),
            json!({
                "name": "console_write",
                "description": "Types text on the serial console, as a user at a terminal would (for a login prompt, a password, or an interactive program). Newlines become Enter; control characters are sent as they are (\\u0003 is Ctrl-C, \\u0004 Ctrl-D). Use console_read to see the response.",
                "inputSchema": {"type": "object", "properties": {
                    "text": {"type": "string"},
                    "enter": {"type": "boolean", "description": "Press Enter after the text (default false)."}
                }, "required": ["text"]}
            }),
            json!({
                "name": "console_run",
                "description": "Runs a shell command at the serial console's shell prompt (someone must be logged in there) and returns its output and exit status. Output of commands that are still running at the timeout is returned as far as it got; the command keeps running (console_write \\u0003 interrupts it).",
                "inputSchema": {"type": "object", "properties": {
                    "command": {"type": "string"},
                    "timeout_seconds": {"type": "number", "description": "Default 60."}
                }, "required": ["command"]}
            }),
            json!({
                "name": "guest_exec",
                "description": "Runs a command in the guest with /bin/sh -c through the QEMU guest agent (as root, no login or terminal needed) and returns its exit status, stdout and stderr.",
                "inputSchema": {"type": "object", "properties": {
                    "command": {"type": "string"},
                    "input": {"type": "string", "description": "Text for the command's standard input."},
                    "timeout_seconds": {"type": "number", "description": "Default 60."}
                }, "required": ["command"]}
            }),
            json!({
                "name": "guest_read_file",
                "description": "Reads a file in the guest through the QEMU guest agent (up to 16 MiB), as text, or as base64 with encoding=base64.",
                "inputSchema": {"type": "object", "properties": {
                    "path": {"type": "string"},
                    "encoding": {"type": "string", "enum": ["text", "base64"]}
                }, "required": ["path"]},
                "annotations": {"readOnlyHint": true}
            }),
            json!({
                "name": "guest_write_file",
                "description": "Writes (or with append, appends to) a file in the guest through the QEMU guest agent. content is text, or base64 with encoding=base64.",
                "inputSchema": {"type": "object", "properties": {
                    "path": {"type": "string"},
                    "content": {"type": "string"},
                    "encoding": {"type": "string", "enum": ["text", "base64"]},
                    "append": {"type": "boolean"}
                }, "required": ["path", "content"]}
            }),
            json!({
                "name": "machine_status",
                "description": "Reports the display size, whether the guest agent is running and how long the machine has been up.",
                "inputSchema": {"type": "object", "properties": {}},
                "annotations": {"readOnlyHint": true}
            }),
        ];
        if self.devices.display.is_some() {
            let point = json!({
                "x": {"type": "integer", "description": "Pixels from the left, in a scale-1 screenshot."},
                "y": {"type": "integer", "description": "Pixels from the top."}
            });
            let with = |extra: Value| {
                let mut p = point.clone();
                p.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
                p
            };
            tools.extend([
                json!({
                    "name": "screenshot",
                    "description": "Returns the guest's screen as a PNG image.",
                    "inputSchema": {"type": "object", "properties": {
                        "scale": {"type": "number", "description": "Shrink the image by this factor, 0.1 to 1 (default 1)."}
                    }},
                    "annotations": {"readOnlyHint": true}
                }),
                json!({
                    "name": "type_text",
                    "description": "Types text with the guest's keyboard (US layout: ASCII letters, digits, punctuation, space, newline as Enter, tab).",
                    "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]}
                }),
                json!({
                    "name": "press_keys",
                    "description": "Presses key combinations on the guest's keyboard, e.g. \"ctrl+alt+t\", or several separated by spaces: \"ctrl+a delete enter\". Names: ctrl, shift, alt, altgr, super, enter, esc, tab, backspace, delete, insert, home, end, pageup, pagedown, up, down, left, right, space, f1-f12, printscreen, and single characters.",
                    "inputSchema": {"type": "object", "properties": {"keys": {"type": "string"}}, "required": ["keys"]}
                }),
                json!({
                    "name": "mouse_move",
                    "description": "Moves the mouse pointer.",
                    "inputSchema": {"type": "object", "properties": point, "required": ["x", "y"]}
                }),
                json!({
                    "name": "mouse_click",
                    "description": "Moves the mouse pointer and clicks.",
                    "inputSchema": {"type": "object", "properties": with(json!({
                        "button": {"type": "string", "enum": ["left", "right", "middle"]},
                        "double": {"type": "boolean", "description": "Double-click."}
                    })), "required": ["x", "y"]}
                }),
                json!({
                    "name": "mouse_drag",
                    "description": "Presses the left button at (x, y), moves to (to_x, to_y) and releases it.",
                    "inputSchema": {"type": "object", "properties": with(json!({
                        "to_x": {"type": "integer"},
                        "to_y": {"type": "integer"}
                    })), "required": ["x", "y", "to_x", "to_y"]}
                }),
                json!({
                    "name": "mouse_scroll",
                    "description": "Moves the mouse pointer and turns the wheel: positive amounts scroll down, negative up (in notches).",
                    "inputSchema": {"type": "object", "properties": with(json!({
                        "amount": {"type": "integer"}
                    })), "required": ["x", "y", "amount"]}
                }),
            ]);
        }
        tools
    }

    fn call(&self, name: &str, args: &Value) -> Option<ToolResult> {
        Some(match name {
            "console_read" => self.console_read(args),
            "console_write" => self.console_write(args),
            "console_run" => self.console_run(args),
            "guest_exec" => self.guest_exec(args),
            "guest_read_file" => self.guest_read_file(args),
            "guest_write_file" => self.guest_write_file(args),
            "machine_status" => Ok(self.machine_status()),
            _ if self.devices.display.is_none() => return None,
            "screenshot" => self.screenshot(args),
            "type_text" => self.type_text(args),
            "press_keys" => self.press_keys(args),
            "mouse_move" => self.mouse(args, |s, _| Ok(s.pointer_settle())),
            "mouse_click" => self.mouse(args, Server::click),
            "mouse_drag" => self.mouse(args, Server::drag),
            "mouse_scroll" => self.mouse(args, Server::scroll),
            _ => return None,
        })
    }

    fn console_read(&self, args: &Value) -> ToolResult {
        if let Some(tail) = args.get("tail").and_then(Value::as_u64) {
            let end = self.log.end();
            let (bytes, _) = self.log.since(end.saturating_sub(tail));
            return Ok(text(clip(plain_text(&bytes))));
        }
        let wait = seconds(args, "wait_seconds", 0.0)?;
        let mut cursor = self.cursor.lock().unwrap();
        self.log.wait_output(*cursor, Duration::from_millis(300), Instant::now() + wait);
        let (bytes, end) = self.log.since(*cursor);
        *cursor = end;
        let out = plain_text(&bytes);
        Ok(text(if out.is_empty() { "(no new output)".to_string() } else { clip(out) }))
    }

    fn console_write(&self, args: &Value) -> ToolResult {
        let mut input = str_arg(args, "text")?.replace("\r\n", "\r").replace('\n', "\r");
        if bool_arg(args, "enter") {
            input.push('\r');
        }
        self.uart.push_input(input.as_bytes());
        Ok(text(format!("typed {} characters", input.chars().count())))
    }

    fn console_run(&self, args: &Value) -> ToolResult {
        let command = str_arg(args, "command")?;
        let timeout = seconds(args, "timeout_seconds", 60.0)?;
        let id = {
            let mut runs = self.runs.lock().unwrap();
            *runs += 1;
            format!("{}x{}", std::process::id(), *runs)
        };
        // The markers start with a 0x1e byte, which printf writes but the echoed command
        // line does not contain. The command goes in braces on its own line, so a trailing
        // comment or `&` in it cannot swallow the end marker.
        let (start, end) = (format!("\x1ervS{id}"), format!("\x1ervE{id}:"));
        let line = format!(
            "\x15printf '\\036rvS%s\\n' {id}; {{ {}\n}}; printf '\\036rvE%s:%d\\n' {id} $?\n",
            command.trim_end()
        );
        let pos = self.log.end();
        self.uart.push_input(line.replace('\n', "\r").as_bytes());
        let deadline = Instant::now() + timeout;
        let Some(at) = self.log.wait_for(pos, start.as_bytes(), deadline) else {
            let (seen, _) = self.log.since(pos);
            return Err(format!(
                "the command did not start within {}s: is a shell prompt showing on the console? (log in with console_write first). Console output:\n{}",
                timeout.as_secs(),
                clip(plain_text(&seen))
            ));
        };
        let body = at + start.len() as u64;
        let finished = self.log.wait_for(body, end.as_bytes(), deadline);
        let (bytes, _) = self.log.since(body);
        let upto = finished.map_or(bytes.len(), |f| (f - body) as usize);
        // Skip the rest of the start marker's line.
        let from = bytes[..upto].iter().position(|&b| b == b'\n').map_or(upto, |i| i + 1);
        let output = clip(plain_text(&bytes[from..upto]));
        let Some(f) = finished else {
            return Ok(text(format!("still running after {}s; output so far:\n{output}", timeout.as_secs())));
        };
        let after = f + end.len() as u64;
        // The UART writes a byte at a time: wait for the whole status line.
        self.log.wait_for(after, b"\n", Instant::now() + Duration::from_secs(1));
        let tail = self.log.since(after).0;
        let status: String = tail.iter().take_while(|b| b.is_ascii_digit()).map(|&b| b as char).collect();
        // console_read carries on after this command.
        let mut cursor = self.cursor.lock().unwrap();
        *cursor = (*cursor).max(after + status.len() as u64);
        Ok(text(format!("exit status {status}\n{output}")))
    }

    fn agent(&self) -> Result<&GuestAgent, String> {
        self.agent.as_ref().ok_or_else(|| guest_agent::NOT_RUNNING.to_string())
    }

    fn guest_exec(&self, args: &Value) -> ToolResult {
        let command = str_arg(args, "command")?;
        let input = args.get("input").and_then(Value::as_str);
        let timeout = seconds(args, "timeout_seconds", 60.0)?;
        let done = self.agent()?.exec(command, input.map(str::as_bytes), timeout)?;
        let mut out = match (done.code, done.signal) {
            (_, Some(sig)) => format!("killed by signal {sig}\n"),
            (Some(code), _) => format!("exit status {code}\n"),
            _ => "exited\n".to_string(),
        };
        for (name, data) in [("stdout", &done.stdout), ("stderr", &done.stderr)] {
            if !data.is_empty() {
                out += &format!("--- {name}\n{}\n", clip(String::from_utf8_lossy(data).into_owned()));
            }
        }
        if done.truncated {
            out += "(output truncated by the guest agent)\n";
        }
        Ok(text(out))
    }

    fn guest_read_file(&self, args: &Value) -> ToolResult {
        let data = self.agent()?.read_file(str_arg(args, "path")?)?;
        Ok(text(match args.get("encoding").and_then(Value::as_str) {
            Some("base64") => guest_agent::base64_encode(&data),
            _ => String::from_utf8_lossy(&data).into_owned(),
        }))
    }

    fn guest_write_file(&self, args: &Value) -> ToolResult {
        let path = str_arg(args, "path")?;
        let content = str_arg(args, "content")?;
        let data = match args.get("encoding").and_then(Value::as_str) {
            Some("base64") => guest_agent::base64_decode(content)?,
            _ => content.as_bytes().to_vec(),
        };
        if data.len() > guest_agent::MAX_FILE {
            return Err(format!("content is larger than {} MiB", guest_agent::MAX_FILE >> 20));
        }
        self.agent()?.write_file(path, &data, bool_arg(args, "append"))?;
        Ok(text(format!("wrote {} bytes to {path}", data.len())))
    }

    fn machine_status(&self) -> Vec<Value> {
        let display = match &self.devices.display {
            Some(d) => match d.with(|f, _| (f.width, f.height)) {
                (0, _) => "display: not set up by the guest yet".to_string(),
                (w, h) => format!("display: {w}x{h}"),
            },
            None => "display: none (serial console only)".to_string(),
        };
        let agent = match &self.agent {
            Some(a) if a.is_running() => "guest agent: running",
            _ => "guest agent: not running",
        };
        text(format!("{display}\n{agent}\nup {}s", self.started.elapsed().as_secs()))
    }

    fn screenshot(&self, args: &Value) -> ToolResult {
        let scale = num_arg(args, "scale", 1.0)?.clamp(0.1, 1.0);
        let display = self.devices.display.as_ref().unwrap();
        let (w, h, pixels) = display.with(|f, _| (f.width, f.height, f.pixels.clone()));
        if w == 0 || h == 0 {
            return Err("the guest has not set up its display yet".into());
        }
        let (sw, sh) = (((w as f64 * scale) as u32).max(1), ((h as f64 * scale) as u32).max(1));
        let scaled: Vec<u32> = if (sw, sh) == (w, h) {
            pixels
        } else {
            (0..sh)
                .flat_map(|y| {
                    let row = (y as u64 * h as u64 / sh as u64) as usize * w as usize;
                    let pixels = &pixels;
                    (0..sw).map(move |x| pixels[row + (x as u64 * w as u64 / sw as u64) as usize])
                })
                .collect()
        };
        let png = crate::screenshot::encode_png(sw, sh, &scaled);
        Ok(vec![
            json!({"type": "image", "data": guest_agent::base64_encode(&png), "mimeType": "image/png"}),
            json!({"type": "text", "text": format!("{sw}x{sh} (screen {w}x{h})")}),
        ])
    }

    fn keyboard(&self) -> Result<&InputHandle, String> {
        self.devices.keyboard.as_ref().ok_or_else(|| "the machine has no keyboard".to_string())
    }

    fn type_text(&self, args: &Value) -> ToolResult {
        let s = str_arg(args, "text")?;
        let keys: Vec<(u16, bool)> = s
            .chars()
            .map(|c| char_key(c).ok_or_else(|| format!("cannot type {c:?} (US keyboard layout, ASCII only)")))
            .collect::<Result<_, _>>()?;
        let keyboard = self.keyboard()?;
        for (code, shift) in keys {
            if shift {
                keyboard.send(&[(ev::KEY, KEY_SHIFT, 1)]);
            }
            keyboard.send(&[(ev::KEY, code, 1)]);
            keyboard.send(&[(ev::KEY, code, 0)]);
            if shift {
                keyboard.send(&[(ev::KEY, KEY_SHIFT, 0)]);
            }
            std::thread::sleep(STROKE);
        }
        Ok(text(format!("typed {} characters", s.chars().count())))
    }

    fn press_keys(&self, args: &Value) -> ToolResult {
        let combos: Vec<Vec<u16>> = str_arg(args, "keys")?
            .split_whitespace()
            .map(|combo| {
                combo
                    .split('+')
                    .map(|k| named_key(k).ok_or_else(|| format!("unknown key `{k}`")))
                    .collect::<Result<Vec<_>, _>>()
            })
            .collect::<Result<_, _>>()?;
        if combos.is_empty() {
            return Err("no keys given".into());
        }
        let keyboard = self.keyboard()?;
        for combo in &combos {
            for &k in combo {
                keyboard.send(&[(ev::KEY, k, 1)]);
                std::thread::sleep(STROKE);
            }
            for &k in combo.iter().rev() {
                keyboard.send(&[(ev::KEY, k, 0)]);
                std::thread::sleep(STROKE);
            }
        }
        Ok(text(format!("pressed {}", str_arg(args, "keys")?)))
    }

    /// Tablet coordinates of screen pixel (x, y).
    fn tablet_point(&self, x: f64, y: f64) -> Result<(i32, i32), String> {
        let (w, h) = self.devices.display.as_ref().unwrap().with(|f, _| (f.width, f.height));
        if w == 0 || h == 0 {
            return Err("the guest has not set up its display yet".into());
        }
        if !(0.0..w as f64).contains(&x) || !(0.0..h as f64).contains(&y) {
            return Err(format!("({x}, {y}) is off the {w}x{h} screen"));
        }
        let scale = |v: f64, size: u32| (v * ABS_MAX as f64 / (size.max(2) - 1) as f64).round() as i32;
        Ok((scale(x, w), scale(y, h)))
    }

    fn move_to(&self, tablet: &InputHandle, x: f64, y: f64) -> Result<(), String> {
        let (ax, ay) = self.tablet_point(x, y)?;
        tablet.send(&[(ev::ABS, ev::ABS_X, ax), (ev::ABS, ev::ABS_Y, ay)]);
        Ok(())
    }

    fn pointer_settle(&self) -> Vec<Value> {
        std::thread::sleep(STROKE * 4);
        text("done")
    }

    /// Moves to (x, y), then runs `action`.
    fn mouse(&self, args: &Value, action: fn(&Server, &Value) -> ToolResult) -> ToolResult {
        let tablet = self.devices.tablet.as_ref().ok_or("the machine has no mouse")?;
        self.move_to(tablet, coord(args, "x")?, coord(args, "y")?)?;
        std::thread::sleep(STROKE * 2);
        action(self, args)
    }

    fn click(&self, args: &Value) -> ToolResult {
        let tablet = self.devices.tablet.as_ref().unwrap();
        let button = match args.get("button").and_then(Value::as_str).unwrap_or("left") {
            "left" => ev::BTN_LEFT,
            "right" => ev::BTN_RIGHT,
            "middle" => ev::BTN_MIDDLE,
            other => return Err(format!("unknown button `{other}`")),
        };
        for _ in 0..if bool_arg(args, "double") { 2 } else { 1 } {
            tablet.send(&[(ev::KEY, button, 1)]);
            std::thread::sleep(STROKE * 4);
            tablet.send(&[(ev::KEY, button, 0)]);
            std::thread::sleep(STROKE * 4);
        }
        Ok(text("clicked"))
    }

    fn drag(&self, args: &Value) -> ToolResult {
        let tablet = self.devices.tablet.as_ref().unwrap();
        let (x0, y0) = (coord(args, "x")?, coord(args, "y")?);
        let (x1, y1) = (coord(args, "to_x")?, coord(args, "to_y")?);
        self.tablet_point(x1, y1)?;
        tablet.send(&[(ev::KEY, ev::BTN_LEFT, 1)]);
        // In steps, so the guest sees a drag rather than a jump.
        const STEPS: u32 = 10;
        for i in 1..=STEPS {
            let t = i as f64 / STEPS as f64;
            std::thread::sleep(STROKE * 2);
            self.move_to(tablet, x0 + (x1 - x0) * t, y0 + (y1 - y0) * t)?;
        }
        std::thread::sleep(STROKE * 4);
        tablet.send(&[(ev::KEY, ev::BTN_LEFT, 0)]);
        Ok(text("dragged"))
    }

    fn scroll(&self, args: &Value) -> ToolResult {
        let tablet = self.devices.tablet.as_ref().unwrap();
        let amount = num_arg(args, "amount", 0.0)? as i32;
        // REL_WHEEL counts up as the wheel turns away from the user, which scrolls up.
        for _ in 0..amount.unsigned_abs().min(100) {
            tablet.send(&[(ev::REL, ev::REL_WHEEL, -amount.signum())]);
            std::thread::sleep(STROKE * 2);
        }
        Ok(text("scrolled"))
    }

    /// Answers one JSON-RPC message; notifications and responses get no answer.
    pub fn handle(&self, msg: &Value) -> Option<Value> {
        let id = msg.get("id")?.clone();
        let method = msg.get("method").and_then(Value::as_str)?;
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        let result = match method {
            "initialize" => {
                let asked = params.get("protocolVersion").and_then(Value::as_str);
                let version = asked.filter(|v| PROTOCOL_VERSIONS.contains(v)).unwrap_or(PROTOCOL_VERSIONS[0]);
                Ok(json!({
                    "protocolVersion": version,
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "rust-v", "title": "rust-v RISC-V virtual machine", "version": env!("CARGO_PKG_VERSION")},
                    "instructions": INSTRUCTIONS,
                }))
            }
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({"tools": self.tools()})),
            "tools/call" => {
                let name = params.get("name").and_then(Value::as_str).unwrap_or_default();
                let args = params.get("arguments").cloned().unwrap_or(json!({}));
                match self.call(name, &args) {
                    Some(Ok(content)) => Ok(json!({"content": content, "isError": false})),
                    Some(Err(e)) => Ok(json!({"content": text(e), "isError": true})),
                    None => Err((-32602, format!("unknown tool `{name}`"))),
                }
            }
            _ => Err((-32601, format!("method `{method}` not found"))),
        };
        Some(match result {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err((code, message)) => {
                json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
            }
        })
    }
}

/// Serves MCP over stdin and stdout, one JSON message per line, until stdin closes.
pub fn serve_stdio(server: Arc<Server>) {
    std::thread::spawn(move || {
        let stdout = Arc::new(Mutex::new(std::io::stdout()));
        for line in std::io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            if line.trim().is_empty() {
                continue;
            }
            let reply = |v: Value, stdout: &Mutex<std::io::Stdout>| {
                let mut out = stdout.lock().unwrap();
                let _ = writeln!(out, "{v}");
                let _ = out.flush();
            };
            match serde_json::from_str::<Value>(&line) {
                // Each request on its own thread: a long console_run must not hold up pings.
                Ok(msg) => {
                    let (server, stdout) = (server.clone(), stdout.clone());
                    std::thread::spawn(move || {
                        if let Some(r) = server.handle(&msg) {
                            reply(r, &stdout);
                        }
                    });
                }
                Err(e) => reply(parse_error(&e), &stdout),
            }
        }
        // The client is gone: so is the reason to run.
        eprintln!("rust-v: MCP client disconnected");
        std::process::exit(0);
    });
}

fn parse_error(e: &serde_json::Error) -> Value {
    json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": format!("parse error: {e}")}})
}

/// Serves MCP's Streamable HTTP transport (POST only, JSON responses) at `/mcp` on `addr`.
pub fn serve_http(server: Arc<Server>, addr: &str) -> Result<String, String> {
    let addr = if addr.contains(':') { addr.to_string() } else { format!("127.0.0.1:{addr}") };
    let listener = TcpListener::bind(&addr).map_err(|e| format!("MCP: {addr}: {e}"))?;
    let local = listener.local_addr().map_err(|e| e.to_string())?;
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let server = server.clone();
            std::thread::spawn(move || {
                let _ = serve_connection(&server, stream);
            });
        }
    });
    Ok(format!("http://{local}/mcp"))
}

/// Whether a browser page from `origin` may talk to the server: only local ones, so a web
/// site cannot drive the machine through the user's browser (DNS rebinding).
fn local_origin(origin: &str) -> bool {
    let host = origin.split("://").nth(1).unwrap_or(origin);
    let host = host.split('/').next().unwrap_or_default();
    let host = match host.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or_default(),
        None => host.split(':').next().unwrap_or_default(),
    };
    matches!(host, "localhost" | "127.0.0.1" | "::1")
}

fn serve_connection(server: &Server, stream: TcpStream) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = stream;
    loop {
        let mut request_line = String::new();
        if reader.read_line(&mut request_line)? == 0 {
            return Ok(());
        }
        let mut parts = request_line.split_whitespace();
        let (method, path) = (parts.next().unwrap_or_default().to_string(), parts.next().unwrap_or_default());
        let path = path.split('?').next().unwrap_or_default().to_string();
        let mut headers = HashMap::new();
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line)? == 0 {
                return Ok(());
            }
            let line = line.trim_end();
            if line.is_empty() {
                break;
            }
            if let Some((k, v)) = line.split_once(':') {
                headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
            }
        }
        let length: usize = headers.get("content-length").and_then(|v| v.parse().ok()).unwrap_or(0);
        if length > 64 << 20 {
            return respond(&mut writer, 413, "Payload Too Large", None);
        }
        let mut body = vec![0; length];
        reader.read_exact(&mut body)?;

        if headers.get("origin").is_some_and(|o| !local_origin(o)) {
            respond(&mut writer, 403, "Forbidden", None)?;
        } else if path != "/mcp" && path != "/" {
            respond(&mut writer, 404, "Not Found", None)?;
        } else if method != "POST" {
            // No server-initiated messages, so no event stream to GET, and no sessions to DELETE.
            write!(writer, "HTTP/1.1 405 Method Not Allowed\r\nAllow: POST\r\nContent-Length: 0\r\n\r\n")?;
        } else {
            match serde_json::from_slice::<Value>(&body) {
                Ok(msg) => match server.handle(&msg) {
                    Some(reply) => respond(&mut writer, 200, "OK", Some(&reply))?,
                    None => respond(&mut writer, 202, "Accepted", None)?,
                },
                Err(e) => respond(&mut writer, 400, "Bad Request", Some(&parse_error(&e)))?,
            }
        }
        if headers.get("connection").is_some_and(|c| c.eq_ignore_ascii_case("close")) {
            return Ok(());
        }
    }
}

fn respond(w: &mut TcpStream, code: u16, reason: &str, body: Option<&Value>) -> std::io::Result<()> {
    let body = body.map(Value::to_string).unwrap_or_default();
    let kind = if body.is_empty() { "" } else { "Content-Type: application/json\r\n" };
    write!(w, "HTTP/1.1 {code} {reason}\r\n{kind}Content-Length: {}\r\n\r\n{body}", body.len())?;
    w.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_output_becomes_plain_text() {
        assert_eq!(plain_text(b"\x1b[1;32mok\x1b[0m\r\nnext\r\n"), "ok\nnext\n");
        assert_eq!(plain_text(b"\x1b]0;title\x07prompt$ "), "prompt$ ");
        assert_eq!(plain_text(b"ab\x08c\x1b(B"), "ac");
    }

    #[test]
    fn keys() {
        assert_eq!(char_key('a'), Some((30, false)));
        assert_eq!(char_key('Q'), Some((16, true)));
        assert_eq!(char_key('0'), Some((11, false)));
        assert_eq!(char_key(')'), Some((11, true)));
        assert_eq!(char_key('?'), Some((53, true)));
        assert_eq!(char_key('é'), None);
        assert_eq!(named_key("ctrl"), Some(29));
        assert_eq!(named_key("F1"), Some(59));
        assert_eq!(named_key("f12"), Some(88));
        assert_eq!(named_key("T"), Some(20));
        assert_eq!(named_key("f13"), None);
    }

    #[test]
    fn origins() {
        assert!(local_origin("http://localhost:3000"));
        assert!(local_origin("http://127.0.0.1"));
        assert!(local_origin("http://[::1]:8080"));
        assert!(!local_origin("https://evil.example"));
        assert!(!local_origin("http://localhost.evil.example"));
    }

    #[test]
    fn console_log_waits_and_finds() {
        let log = ConsoleLog::new();
        let mut tee = Tee { log: log.clone(), out: None };
        tee.write_all(b"hello\r\n").unwrap();
        assert_eq!(log.since(0).0, b"hello\r\n");
        let later = log.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            later.append(b"\x1emark:0\r\n");
        });
        let at = log.wait_for(0, b"\x1emark:", Instant::now() + Duration::from_secs(5));
        assert_eq!(at, Some(7));
        assert_eq!(log.wait_for(0, b"never", Instant::now() + Duration::from_millis(20)), None);
    }
}
