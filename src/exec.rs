//! Guarded subprocess execution shared by every external tool invocation.
//!
//! External tools (difft, gh, git fetch) are only ever invoked as isolated
//! subprocesses. Every invocation goes through one guarded execution path
//! with a wall-clock timeout and output caps on both pipes.
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
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

/// How long a wait may ignore the cancel flag. Bounds how long shutdown has
/// to wait for a running child to be killed and reaped.
const CANCEL_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// How often the already-closed child is checked for its exit status.
const EXIT_POLL_INTERVAL: Duration = Duration::from_millis(5);

/// Shared stop signal for in-flight subprocess runs. Cloning shares the flag.
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

/// The flag the SIGINT handler cancels. Process-global because a signal
/// handler cannot carry state; one guarded foreground wait exists at a time.
static SIGINT_TARGET: std::sync::OnceLock<CancelFlag> = std::sync::OnceLock::new();

extern "C" fn sigint_to_flag(_: libc::c_int) {
    // Only an atomic store: async-signal-safe.
    if let Some(flag) = SIGINT_TARGET.get() {
        flag.cancel();
    }
}

/// While alive, SIGINT cancels the returned flag instead of killing the
/// process. Guarded children run in their own process group, so a terminal
/// Ctrl-C never reaches them directly and the parent's default death would
/// orphan them mid-run; routing the signal through [`CancelFlag`] lets the
/// guarded wait kill and reap the child before the error unwinds. Dropping
/// the guard restores the previous disposition.
pub struct SigintCancel {
    previous: libc::sigaction,
}

impl SigintCancel {
    pub fn install() -> (Self, CancelFlag) {
        let flag = SIGINT_TARGET.get_or_init(CancelFlag::default).clone();
        // The static outlives one use; a fresh install starts uncancelled.
        flag.0.store(false, Ordering::Relaxed);
        // SAFETY: sigint_to_flag is async-signal-safe, and the previous
        // disposition is preserved for the Drop restore.
        let previous = unsafe {
            let mut new: libc::sigaction = std::mem::zeroed();
            new.sa_sigaction = sigint_to_flag as *const () as usize;
            libc::sigemptyset(&mut new.sa_mask);
            let mut old: libc::sigaction = std::mem::zeroed();
            libc::sigaction(libc::SIGINT, &new, &mut old);
            old
        };
        (Self { previous }, flag)
    }
}

impl Drop for SigintCancel {
    fn drop(&mut self) {
        // SAFETY: restores the disposition saved by install().
        unsafe {
            libc::sigaction(libc::SIGINT, &self.previous, std::ptr::null_mut());
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ExecError {
    /// The binary does not exist on PATH (spawn failed with NotFound).
    NotFound,
    Io,
    TimedOut,
    Cancelled,
    /// A pipe exceeded its cap. Cap trips win over the exit status: a reader
    /// that hit its cap has closed its pipe, which typically kills the child
    /// with SIGPIPE — reporting that as a process failure would hide the
    /// real cause.
    OutputTooLarge,
}

pub struct ExecOutput {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// Guard parameters for one subprocess invocation.
pub struct GuardedCommand {
    pub timeout: Duration,
    pub max_stdout_bytes: usize,
    pub max_stderr_bytes: usize,
    /// Set by the owner to abandon a run in progress; the child is killed
    /// and reaped before the call returns.
    pub cancel: CancelFlag,
}

impl GuardedCommand {
    /// Spawn under full guards and collect both pipes.
    pub fn run(&self, mut cmd: Command) -> Result<ExecOutput, ExecError> {
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        let mut child = cmd.spawn().map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => ExecError::NotFound,
            _ => ExecError::Io,
        })?;

        let stdout = child.stdout.take().expect("stdout was piped");
        let stderr = child.stderr.take().expect("stderr was piped");
        let max_stdout = self.max_stdout_bytes;
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
                    return Err(ExecError::Io);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if let Some(stop) = self.stop_reason(&mut child, deadline) {
                        return Err(stop);
                    }
                }
            }
        }
        let stdout = out_data.expect("loop exits only with both pipes collected");
        let stderr = err_data.expect("loop exits only with both pipes collected");

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
                    return Err(ExecError::Io);
                }
            }
        };

        Ok(ExecOutput {
            status,
            stdout,
            stderr,
        })
    }

    /// Kill and reap the child if the run must stop now, reporting why.
    /// Cancellation wins over the deadline: the caller is shutting down and
    /// a timeout would be a misleading label for a run we abandoned.
    fn stop_reason(&self, child: &mut Child, deadline: Instant) -> Option<ExecError> {
        if self.cancel.is_cancelled() {
            kill_group(child);
            return Some(ExecError::Cancelled);
        }
        if Instant::now() >= deadline {
            kill_group(child);
            return Some(ExecError::TimedOut);
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
fn read_capped(mut pipe: impl Read, cap: usize) -> Result<Vec<u8>, ExecError> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 64 * 1024];
    loop {
        match pipe.read(&mut chunk) {
            Ok(0) => return Ok(buf),
            Ok(n) => {
                if buf.len() + n > cap {
                    return Err(ExecError::OutputTooLarge);
                }
                buf.extend_from_slice(&chunk[..n]);
            }
            // A process-directed signal (the SigintCancel handler runs
            // without SA_RESTART) can land on this reader thread and
            // interrupt the read. That is not an I/O failure — reporting it
            // as one would beat the cancel poll to the punch and mislabel a
            // Ctrl-C as an error.
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return Err(ExecError::Io),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Signal tests share one process-wide handler and flag; running them
    /// concurrently would let one test's reset clobber another's raise.
    static SIGNAL_TESTS: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn a_sigint_while_the_guard_is_installed_cancels_instead_of_killing() {
        let _serial = SIGNAL_TESTS.lock().expect("serialize signal tests");
        let (guard, flag) = SigintCancel::install();
        assert!(!flag.is_cancelled(), "a fresh install starts uncancelled");
        // SAFETY: the handler installed above only stores an atomic; the
        // process must survive to run the assertions.
        unsafe {
            libc::raise(libc::SIGINT);
        }
        assert!(flag.is_cancelled(), "the handler set the shared flag");
        drop(guard);
    }

    #[test]
    fn a_real_sigint_during_a_guarded_run_reports_cancelled_not_io() {
        let _serial = SIGNAL_TESTS.lock().expect("serialize signal tests");
        let (guard, flag) = SigintCancel::install();
        let cancel = flag.clone();
        // Directed at this thread, not the process: a process-wide SIGINT
        // could land on any thread of the test binary and interrupt an
        // unrelated parallel test's blocking syscall. Thread delivery keeps
        // the signal→handler→flag→Cancelled path real while confining the
        // side effects to this test; the reader-thread EINTR case is
        // covered deterministically by the InterruptedReader test below.
        // pthread_t is a raw pointer on macOS, so it crosses the thread
        // boundary as usize.
        let target = unsafe { libc::pthread_self() } as usize;
        let sender = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            // SAFETY: the target thread outlives the test body it is
            // joined in, and the installed handler only sets the flag.
            unsafe {
                libc::pthread_kill(target as libc::pthread_t, libc::SIGINT);
            }
        });

        let mut cmd = Command::new("sleep");
        cmd.arg("60");
        let run = GuardedCommand {
            timeout: Duration::from_secs(30),
            max_stdout_bytes: 4096,
            max_stderr_bytes: 4096,
            cancel: flag,
        }
        .run(cmd);

        sender.join().expect("join sender");
        assert!(cancel.is_cancelled(), "the signal reached the handler");
        assert_eq!(run.err(), Some(ExecError::Cancelled));
        drop(guard);
    }

    /// Returns Interrupted a few times before the payload, the way a read
    /// hit by a signal without SA_RESTART does.
    struct InterruptedReader {
        interruptions: usize,
        payload: &'static [u8],
        done: bool,
    }

    impl Read for InterruptedReader {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.interruptions > 0 {
                self.interruptions -= 1;
                return Err(std::io::Error::from(std::io::ErrorKind::Interrupted));
            }
            if self.done {
                return Ok(0);
            }
            self.done = true;
            buf[..self.payload.len()].copy_from_slice(self.payload);
            Ok(self.payload.len())
        }
    }

    #[test]
    fn an_interrupted_read_is_retried_not_reported_as_io() {
        let reader = InterruptedReader {
            interruptions: 3,
            payload: b"survived",
            done: false,
        };
        assert_eq!(read_capped(reader, 4096), Ok(b"survived".to_vec()));
    }
}
