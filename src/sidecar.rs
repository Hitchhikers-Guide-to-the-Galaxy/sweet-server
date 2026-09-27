//! The ONE embedded Node sidecar (Sweet Plugin Plan decision: hybrid
//! dispatch). Rust owns discovery, coercion, context, gating and the
//! document; this module owns only the part Rust cannot do — importing
//! plugin JavaScript and running its plain functions — through a single
//! child `node` process speaking NDJSON over stdio (src/sidecar_host.js,
//! embedded in the binary).
//!
//! Divergences from the reference, recorded per the plan:
//! - calls carry a TIMEOUT (the reference waits forever; a hung handler
//!   there hangs the request) — on expiry the child is killed and
//!   respawned, and the caller gets an honest error;
//! - a missing `node` runtime yields skipped entries, not a crash.

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

const HOST_JS: &str = include_str!("sidecar_host.js");
pub const CALL_TIMEOUT: Duration = Duration::from_secs(30);

/// CALL_TIMEOUT unless the operator sets SWEET_SIDECAR_TIMEOUT_SECS — a
/// farm whose handlers ask a model to write (audionote's summarise) needs
/// longer than a lookup does.
fn call_timeout() -> Duration {
    std::env::var("SWEET_SIDECAR_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|n| *n > 0)
        .map(Duration::from_secs)
        .unwrap_or(CALL_TIMEOUT)
}
const RESPAWN_BACKOFF: Duration = Duration::from_secs(5);

pub struct Sidecar {
    inner: Mutex<Inner>,
}

struct Inner {
    child: Option<Running>,
    next_id: u64,
    last_failure: Option<Instant>,
}

struct Running {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl Sidecar {
    pub fn new() -> Sidecar {
        Sidecar {
            inner: Mutex::new(Inner {
                child: None,
                next_id: 0,
                last_failure: None,
            }),
        }
    }

    fn spawn() -> Result<Running, String> {
        let mut child = Command::new("node")
            .args(["--input-type=commonjs", "-e", HOST_JS])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("node runtime unavailable — {e}"))?;
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        Ok(Running {
            child,
            stdin,
            stdout,
        })
    }

    /// One request/response over the sequential NDJSON pipe. Lazy spawn,
    /// respawn with backoff after a failure, kill+respawn on timeout.
    pub fn request(&self, op: &str, extra: Value) -> Result<Value, String> {
        let mut inner = self.inner.lock().unwrap();
        if inner.child.is_none() {
            if let Some(when) = inner.last_failure {
                if when.elapsed() < RESPAWN_BACKOFF {
                    return Err("node runtime unavailable — retry shortly".into());
                }
            }
            match Self::spawn() {
                Ok(r) => {
                    inner.child = Some(r);
                    inner.last_failure = None;
                }
                Err(e) => {
                    inner.last_failure = Some(Instant::now());
                    return Err(e);
                }
            }
        }
        inner.next_id += 1;
        let id = inner.next_id;
        let mut msg = extra;
        msg["id"] = json!(id);
        msg["op"] = json!(op);
        let running = inner.child.as_mut().unwrap();
        let line = msg.to_string() + "\n";
        if running.stdin.write_all(line.as_bytes()).is_err() {
            inner.child = None;
            return Err("sidecar died mid-request — respawning".into());
        }
        // sequential host: the next line is our answer. Reads happen on a
        // helper thread so a hung handler cannot hang the server.
        let deadline = Instant::now() + call_timeout();
        let mut buf = String::new();
        loop {
            buf.clear();
            match read_line_with_deadline(running, &mut buf, deadline) {
                ReadOutcome::Line => {
                    let Ok(resp) = serde_json::from_str::<Value>(&buf) else {
                        continue;
                    };
                    if resp["id"].as_u64() != Some(id) {
                        continue; // stale answer from a timed-out ancestor
                    }
                    if resp["ok"].as_bool() == Some(true) {
                        return Ok(resp["value"].clone());
                    }
                    let msg = resp["error"]["message"]
                        .as_str()
                        .unwrap_or("handler failed")
                        .to_string();
                    let status = resp["error"]["status"].as_u64();
                    return Err(match status {
                        Some(s) => format!("{s} {msg}"),
                        None => format!("500 {msg}"),
                    });
                }
                ReadOutcome::Timeout => {
                    let _ = running.child.kill();
                    inner.child = None;
                    return Err(format!(
                        "call timed out after {}s — the handler was killed \
                         (sweet divergence: the reference waits forever)",
                        call_timeout().as_secs()
                    ));
                }
                ReadOutcome::Eof => {
                    inner.child = None;
                    return Err("sidecar exited mid-request — respawning".into());
                }
            }
        }
    }
}

enum ReadOutcome {
    Line,
    Timeout,
    Eof,
}

/// Blocking read with a wall-clock deadline: poll the pipe on a helper
/// thread via a channel so the mutex-holder can give up cleanly.
fn read_line_with_deadline(
    running: &mut Running,
    buf: &mut String,
    deadline: Instant,
) -> ReadOutcome {
    use std::sync::mpsc;
    // reads must happen on the same BufReader to keep buffered bytes —
    // so read on THIS thread but in small waits: try_wait-style polling
    // is not possible on a BufReader, so accept a plain blocking read
    // guarded by a watchdog that kills the child at the deadline (the
    // read then returns Eof).
    let (tx, rx) = mpsc::channel::<()>();
    let pid = running.child.id();
    let wait = deadline.saturating_duration_since(Instant::now());
    let watchdog = std::thread::spawn(move || {
        if rx.recv_timeout(wait).is_err() {
            // deadline passed: kill the child so the blocking read ends
            #[cfg(unix)]
            unsafe {
                libc_kill(pid as i32);
            }
        }
    });
    let n = running.stdout.read_line(buf);
    let _ = tx.send(()); // disarm
    let _ = watchdog.join();
    match n {
        Ok(0) => {
            if Instant::now() >= deadline {
                ReadOutcome::Timeout
            } else {
                ReadOutcome::Eof
            }
        }
        Ok(_) => ReadOutcome::Line,
        Err(_) => ReadOutcome::Eof,
    }
}

#[cfg(unix)]
unsafe fn libc_kill(pid: i32) {
    extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    kill(pid, 9);
}
