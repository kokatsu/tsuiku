//! difft subprocess runner.
//!
//! difft is only ever invoked as an isolated subprocess (never linked).
//! Every invocation — including `--version` — goes through one guarded
//! execution path with a wall-clock timeout and output caps on both pipes.
//!
//! The child is spawned into its own process group, and a timeout kills the
//! whole group: killing only the direct child would leave grandchildren
//! holding the pipe write-ends, which keeps the pipes open and would block a
//! reader `join()` forever. For the same reason the readers report through
//! channels with a bounded `recv_timeout` instead of being joined — even a
//! grandchild that escaped its process group can only cost us the timeout,
//! never a hang. The child is reaped only after all group kills are done
//! (its zombie keeps the group id reserved until then, so a kill can never
//! hit a recycled PID), and it is always waited on, so no zombie survives.
//!
//! Shutdown uses the same machinery: a [`CancelFlag`] shared with the owner
//! is checked on a short interval while waiting, so a run in progress is
//! killed and reaped instead of being abandoned to process exit. Both waits
//! are therefore polled in `CANCEL_POLL_INTERVAL` steps rather than blocking
//! for the whole remaining timeout.

use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use crate::asyncstate::StructuralError;
use crate::structural::json::{self, RawFileDiff};

/// `difft --version` output is a few lines; anything past this cap is a
/// misbehaving binary.
const VERSION_STDOUT_CAP: usize = 64 * 1024;

/// How long a wait may ignore the cancel flag. Bounds how long shutdown has
/// to wait for a running child to be killed and reaped.
const CANCEL_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// How often the already-closed child is checked for its exit status.
const EXIT_POLL_INTERVAL: Duration = Duration::from_millis(5);

/// Shared stop signal for in-flight difft runs. Cloning shares the flag.
#[derive(Clone, Default)]
pub struct CancelFlag(Arc<AtomicBool>);

impl CancelFlag {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

pub struct DifftRunner {
    pub binary: PathBuf,
    pub timeout: Duration,
    pub max_stdout_bytes: usize,
    pub max_stderr_bytes: usize,
    /// Set by the owner to abandon a run in progress; the child is killed
    /// and reaped before the call returns.
    pub cancel: CancelFlag,
}

impl Default for DifftRunner {
    fn default() -> Self {
        Self {
            binary: PathBuf::from("difft"),
            timeout: Duration::from_secs(5),
            max_stdout_bytes: 32 * 1024 * 1024,
            max_stderr_bytes: 64 * 1024,
            cancel: CancelFlag::default(),
        }
    }
}

impl DifftRunner {
    /// First line of `difft --version`, e.g. "Difftastic 0.69.0".
    pub fn version(&self) -> Result<String, StructuralError> {
        let mut cmd = Command::new(&self.binary);
        cmd.arg("--version");
        let (status, stdout) = self.exec_guarded(cmd, VERSION_STDOUT_CAP)?;
        if !status.success() {
            return Err(StructuralError::ProcessFailed {
                exit_code: status.code(),
            });
        }
        let text = String::from_utf8_lossy(&stdout);
        Ok(text.lines().next().unwrap_or("").trim().to_string())
    }

    /// Run difft on two files and parse its JSON output.
    pub fn run(&self, old_path: &Path, new_path: &Path) -> Result<RawFileDiff, StructuralError> {
        let mut cmd = Command::new(&self.binary);
        cmd.env("DFT_UNSTABLE", "yes")
            .arg("--display")
            .arg("json")
            .arg(old_path)
            .arg(new_path);
        let (status, stdout) = self.exec_guarded(cmd, self.max_stdout_bytes)?;
        if !status.success() {
            return Err(StructuralError::ProcessFailed {
                exit_code: status.code(),
            });
        }
        let text = std::str::from_utf8(&stdout).map_err(|_| StructuralError::InvalidJson)?;
        match json::parse(text) {
            Ok(raw) => Ok(raw),
            Err(e) if e.is_data() => Err(StructuralError::InvalidSchema),
            Err(_) => Err(StructuralError::InvalidJson),
        }
    }

    /// Spawn under full guards and collect stdout. Cap trips win over the
    /// exit status: a reader that hit its cap has closed its pipe, which
    /// typically kills the child with SIGPIPE — reporting that as
    /// ProcessFailed would hide the real cause.
    fn exec_guarded(
        &self,
        mut cmd: Command,
        max_stdout: usize,
    ) -> Result<(ExitStatus, Vec<u8>), StructuralError> {
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        let mut child = cmd.spawn().map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => StructuralError::ToolNotFound,
            _ => StructuralError::Io,
        })?;

        let stdout = child.stdout.take().expect("stdout was piped");
        let stderr = child.stderr.take().expect("stderr was piped");
        let max_stderr = self.max_stderr_bytes;
        let (tx, rx) = mpsc::channel();
        let out_tx = tx.clone();
        std::thread::spawn(move || {
            let _ = out_tx.send((PipeKind::Out, read_capped(stdout, max_stdout)));
        });
        std::thread::spawn(move || {
            let _ = tx.send((PipeKind::Err, read_capped(stderr, max_stderr)));
        });

        // Phase 1: wait for both pipes to reach EOF, failing fast the moment
        // either reader trips its cap or errors. The child stays unreaped
        // throughout this phase, so `kill_group` below always targets a
        // process group whose id is still reserved (at minimum by the
        // child's own zombie) and can never hit a recycled PID.
        let deadline = Instant::now() + self.timeout;
        let mut out_data = None;
        let mut err_data = None;
        while out_data.is_none() || err_data.is_none() {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match rx.recv_timeout(remaining.min(CANCEL_POLL_INTERVAL)) {
                Ok((kind, Ok(data))) => match kind {
                    PipeKind::Out => out_data = Some(data),
                    PipeKind::Err => err_data = Some(data),
                },
                Ok((_, Err(e))) => {
                    kill_group(&mut child);
                    return Err(e);
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    kill_group(&mut child);
                    return Err(StructuralError::Io);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if let Some(stop) = self.stop_reason(&mut child, deadline) {
                        return Err(stop);
                    }
                }
            }
        }
        let stdout = out_data.expect("loop exits only with both pipes collected");

        // Phase 2: pipes are closed; now wait for the exit status.
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => {
                    if let Some(stop) = self.stop_reason(&mut child, deadline) {
                        return Err(stop);
                    }
                    std::thread::sleep(EXIT_POLL_INTERVAL);
                }
                Err(_) => {
                    kill_group(&mut child);
                    return Err(StructuralError::Io);
                }
            }
        };

        Ok((status, stdout))
    }

    /// Kill and reap the child if the run must stop now, reporting why.
    /// Cancellation wins over the deadline: the caller is shutting down and
    /// a timeout would be a misleading label for a run we abandoned.
    fn stop_reason(&self, child: &mut Child, deadline: Instant) -> Option<StructuralError> {
        if self.cancel.is_cancelled() {
            kill_group(child);
            return Some(StructuralError::Cancelled);
        }
        if Instant::now() >= deadline {
            kill_group(child);
            return Some(StructuralError::TimedOut);
        }
        None
    }
}

#[derive(Clone, Copy)]
enum PipeKind {
    Out,
    Err,
}

/// Kill the child's whole process group (it is its own group leader), then
/// reap the direct child. Grandchildren are re-parented and reaped by the OS.
///
/// Must only be called while `child` is unreaped: alive or zombie, the child
/// keeps its PID (and thus the group id) reserved, so the negative-pid kill
/// cannot target a recycled PID.
fn kill_group(child: &mut Child) {
    let pid = child.id() as i32;
    // Negative pid targets the process group. The direct kill is a backup
    // for the narrow window at spawn before the child enters its own group.
    unsafe {
        libc::kill(-pid, libc::SIGKILL);
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// Read a pipe to EOF, failing once the cap is exceeded. On failure the pipe
/// is dropped, so a child still writing gets SIGPIPE instead of blocking.
fn read_capped(mut pipe: impl Read, cap: usize) -> Result<Vec<u8>, StructuralError> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 64 * 1024];
    loop {
        match pipe.read(&mut chunk) {
            Ok(0) => return Ok(buf),
            Ok(n) => {
                if buf.len() + n > cap {
                    return Err(StructuralError::OutputTooLarge);
                }
                buf.extend_from_slice(&chunk[..n]);
            }
            Err(_) => return Err(StructuralError::Io),
        }
    }
}
