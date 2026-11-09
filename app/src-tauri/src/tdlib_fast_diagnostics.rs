//! Bounded private diagnostics for a native TDLib child worker.
//! Never record the JSON request or auth configuration: both contain secrets.
//! Keep one small stderr tail in memory and persist only on an observed failure.
#[cfg(unix)]
use std::os::unix::{
    fs::{OpenOptionsExt, PermissionsExt},
    process::ExitStatusExt,
};
use std::{
    fs::{self, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStderr},
    sync::{Arc, Mutex},
    thread::{self, JoinHandle},
};

const STDERR_TAIL_BYTES: usize = 12 * 1024;
const MAX_DIAGNOSTIC_BYTES: u64 = 256 * 1024;
const MAX_REASON_BYTES: usize = 2048;
const MAX_EVENT_BYTES: usize = 18 * 1024;
const LOG_NAME: &str = "tdlib-worker-diagnostics.log";

pub(super) struct WorkerDiagnostics {
    path: PathBuf,
    stderr_tail: Arc<Mutex<Vec<u8>>>,
    reader: Option<JoinHandle<()>>,
}

fn bounded_tail(buffer: &mut Vec<u8>, chunk: &[u8]) {
    if chunk.len() >= STDERR_TAIL_BYTES {
        buffer.clear();
        buffer.extend_from_slice(&chunk[chunk.len() - STDERR_TAIL_BYTES..]);
        return;
    }
    buffer.extend_from_slice(chunk);
    let extra = buffer.len().saturating_sub(STDERR_TAIL_BYTES);
    if extra > 0 {
        buffer.drain(..extra);
    }
}

fn bounded_message(message: &str, max: usize) -> String {
    // Do not permit multiline/ANSI control injection into structured headers.
    // Full Python stderr is separated below as a clearly marked bounded field.
    let mut out = String::new();
    for c in message.chars() {
        let c = if c.is_control() { ' ' } else { c };
        if out.len() + c.len_utf8() > max {
            break;
        }
        out.push(c);
    }
    out
}

fn append_private_log(
    path: &Path,
    action: &str,
    phase: &str,
    status: &str,
    reason: &str,
    stderr: &[u8],
) -> Result<(), String> {
    let sanitized_action = bounded_message(action, 48);
    let sanitized_phase = bounded_message(phase, 48);
    let sanitized_status = bounded_message(status, 128);
    let sanitized_reason = bounded_message(reason, MAX_REASON_BYTES);
    let stderr = String::from_utf8_lossy(stderr);
    let block = format!(
        "\n=== TDLib worker event @ {} ===\naction: {}\nphase: {}\nchild: {}\nerror: {}\nstderr tail (max 12 KiB; private; may contain file paths):\n{}\n=== END TDLib event ===\n",
        chrono::Utc::now().to_rfc3339(),
        sanitized_action,
        sanitized_phase,
        sanitized_status,
        sanitized_reason,
        stderr
    );
    let block = if block.len() > MAX_EVENT_BYTES {
        // Cap output even if native stderr contains non-UTF8 replacements.
        &block.as_bytes()[..MAX_EVENT_BYTES]
    } else {
        block.as_bytes()
    };
    if fs::metadata(path)
        .map(|m| m.len() > MAX_DIAGNOSTIC_BYTES)
        .unwrap_or(false)
    {
        let previous = path.with_extension("previous.log");
        // Rotation keeps both this and the previous log bounded. Avoid
        // truncating auth sessions, databases or any Telegram file.
        fs::rename(path, previous)
            .map_err(|e| format!("Could not rotate TDLib diagnostic log: {e}"))?;
    }
    let mut options = OpenOptions::new();
    options.append(true).create(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut output = options
        .open(path)
        .map_err(|e| format!("Could not open TDLib diagnostic log: {e}"))?;
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .map_err(|e| format!("Could not restrict TDLib diagnostic permissions: {e}"))?;
    output
        .write_all(block)
        .map_err(|e| format!("Could not write TDLib diagnostic log: {e}"))?;
    output
        .flush()
        .map_err(|e| format!("Could not flush TDLib diagnostic log: {e}"))
}

impl WorkerDiagnostics {
    #[cfg(test)]
    pub(super) fn test_stub() -> Self {
        Self {
            path: std::env::temp_dir().join(format!(
                "terarelay-tdlib-test-{}.log",
                rand::random::<u64>()
            )),
            stderr_tail: Arc::new(Mutex::new(Vec::new())),
            reader: None,
        }
    }

    pub(super) fn start(stderr: ChildStderr, app_data: &Path) -> Result<Self, String> {
        let stderr_tail = Arc::new(Mutex::new(Vec::with_capacity(STDERR_TAIL_BYTES)));
        let shared = stderr_tail.clone();
        let handle = thread::Builder::new()
            .name("terarelay-tdlib-stderr".into())
            .spawn(move || {
                let mut source = stderr;
                let mut buf = [0u8; 2048];
                loop {
                    match source.read(&mut buf) {
                        Ok(0) | Err(_) => return,
                        Ok(n) => {
                            if let Ok(mut tail) = shared.lock() {
                                bounded_tail(&mut tail, &buf[..n]);
                            } else {
                                return;
                            }
                        }
                    }
                }
            })
            .map_err(|e| format!("Could not start TDLib diagnostic reader: {e}"))?;
        Ok(Self {
            path: app_data.join(LOG_NAME),
            stderr_tail,
            reader: Some(handle),
        })
    }

    /// Called only after an actual unsuccessful request. If the child exited,
    /// joining the stderr reader guarantees the final crash trace is captured.
    pub(super) fn failure(
        &mut self,
        child: &mut Child,
        action: &str,
        phase: &str,
        reason: &str,
    ) -> String {
        let status = match child.try_wait() {
            Ok(Some(exit)) => {
                if let Some(reader) = self.reader.take() {
                    let _ = reader.join();
                }
                #[cfg(unix)]
                let summary = match exit.signal() {
                    Some(signal) => format!("exited due to signal {signal} ({exit})"),
                    None => format!("exited with {exit}"),
                };
                #[cfg(not(unix))]
                let summary = format!("exited with {exit}");
                summary
            }
            Ok(None) => "process still running at failure".to_string(),
            Err(error) => format!("could not query exit status: {error}"),
        };
        let tail = self
            .stderr_tail
            .lock()
            .map(|tail| tail.clone())
            .unwrap_or_default();
        match append_private_log(&self.path, action, phase, &status, reason, &tail) {
            Ok(()) => format!(
                "{reason} (worker {status}; diagnostic saved to {})",
                self.path.display()
            ),
            Err(log_error) => {
                format!("{reason} (worker {status}; could not save diagnostics: {log_error})")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stderr_capture_is_bounded() {
        let mut tail = vec![];
        bounded_tail(&mut tail, &vec![b'a'; STDERR_TAIL_BYTES + 100]);
        assert_eq!(tail.len(), STDERR_TAIL_BYTES);
        bounded_tail(&mut tail, b"\nfinal error");
        assert!(tail.ends_with(b"\nfinal error"));
        assert_eq!(tail.len(), STDERR_TAIL_BYTES);
    }
    #[test]
    fn private_diagnostic_is_bounded_and_omits_request_payload() {
        let dir = std::env::temp_dir().join(format!(
            "terarelay-tdlib-diag-test-{}",
            rand::random::<u64>()
        ));
        fs::create_dir(&dir).unwrap();
        let log = dir.join(LOG_NAME);
        append_private_log(
            &log,
            "upload",
            "stdin-write",
            "exited due to signal 11",
            "Broken pipe (os error 32)",
            b"Fatal Python error: Segmentation fault",
        )
        .unwrap();
        let text = fs::read_to_string(&log).unwrap();
        assert!(text.contains("action: upload"));
        assert!(text.contains("signal 11"));
        assert!(text.contains("Broken pipe"));
        assert!(text.contains("Fatal Python error"));
        assert!(!text.contains("api_hash"));
        #[cfg(unix)]
        assert_eq!(
            fs::metadata(&log).unwrap().permissions().mode() & 0o777,
            0o600
        );
        for _ in 0..30 {
            append_private_log(
                &log,
                "upload",
                "stderr",
                "exit 1",
                "fail",
                &vec![b'Z'; 11_000],
            )
            .unwrap();
        }
        assert!(fs::metadata(&log).unwrap().len() < MAX_DIAGNOSTIC_BYTES + MAX_EVENT_BYTES as u64);
        assert!(fs::metadata(log.with_extension("previous.log")).is_ok());
        let _ = fs::remove_dir_all(&dir);
    }
}
