//! Keeps the host clipboard and the guest's (through its SPICE agent) in step: text copied
//! on either side can be pasted on the other.

use std::sync::mpsc::Receiver;

use rv_machine::virtio::console::ClipboardHandle;

/// How often the host clipboard is checked for changes (it has no portable notification).
#[cfg(feature = "clipboard")]
const POLL: std::time::Duration = std::time::Duration::from_millis(300);

#[cfg(feature = "clipboard")]
pub fn start(handle: ClipboardHandle, guest_text: Receiver<String>) -> Result<(), String> {
    let mut host = arboard::Clipboard::new().map_err(|e| e.to_string())?;
    std::thread::Builder::new()
        .name("clipboard".into())
        .spawn(move || {
            // What the two sides last agreed on, so that a change is passed on only once.
            let mut last = host.get_text().ok().map(|t| from_host(&t));
            if let Some(t) = &last {
                handle.set_host_text(t.clone());
            }
            loop {
                match guest_text.recv_timeout(POLL) {
                    Ok(text) => {
                        if last.as_deref() != Some(&text) {
                            let _ = host.set_text(to_host(&text));
                            last = Some(text);
                        }
                        continue;
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
                }
                if let Ok(t) = host.get_text() {
                    let t = from_host(&t);
                    if last.as_deref() != Some(&t) {
                        handle.set_host_text(t.clone());
                        last = Some(t);
                    }
                }
            }
        })
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(not(feature = "clipboard"))]
pub fn start(_handle: ClipboardHandle, _guest_text: Receiver<String>) -> Result<(), String> {
    Err("this build has no clipboard support".into())
}

/// The guest uses `\n` line ends; Windows `\r\n`.
#[cfg(feature = "clipboard")]
fn to_host(text: &str) -> String {
    if cfg!(windows) { text.replace("\r\n", "\n").replace('\n', "\r\n") } else { text.to_string() }
}

#[cfg(feature = "clipboard")]
fn from_host(text: &str) -> String {
    if cfg!(windows) { text.replace("\r\n", "\n") } else { text.to_string() }
}
