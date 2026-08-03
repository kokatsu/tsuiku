//! difft subprocess runner.
//!
//! difft is only ever invoked as an isolated subprocess (never linked).
//! Every invocation — including `--version` — goes through the guarded
//! execution path in `crate::exec` with a wall-clock timeout and output
//! caps on both pipes; see that module for the process-group and shutdown
//! contract.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

pub use crate::exec::CancelFlag;
use crate::exec::{ExecError, ExecOutput, GuardedCommand};

use crate::asyncstate::StructuralError;
use crate::structural::json::{self, RawFileDiff};

/// `difft --version` output is a few lines; anything past this cap is a
/// misbehaving binary.
const VERSION_STDOUT_CAP: usize = 64 * 1024;

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
        let output = self.exec_guarded(cmd, VERSION_STDOUT_CAP)?;
        if !output.status.success() {
            return Err(StructuralError::ProcessFailed {
                exit_code: output.status.code(),
            });
        }
        let text = String::from_utf8_lossy(&output.stdout);
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
        let output = self.exec_guarded(cmd, self.max_stdout_bytes)?;
        if !output.status.success() {
            return Err(StructuralError::ProcessFailed {
                exit_code: output.status.code(),
            });
        }
        let text = std::str::from_utf8(&output.stdout).map_err(|_| StructuralError::InvalidJson)?;
        match json::parse(text) {
            Ok(raw) => Ok(raw),
            Err(e) if e.is_data() => Err(StructuralError::InvalidSchema),
            Err(_) => Err(StructuralError::InvalidJson),
        }
    }

    fn exec_guarded(&self, cmd: Command, max_stdout: usize) -> Result<ExecOutput, StructuralError> {
        let guard = GuardedCommand {
            timeout: self.timeout,
            max_stdout_bytes: max_stdout,
            max_stderr_bytes: self.max_stderr_bytes,
            cancel: self.cancel.clone(),
        };
        guard.run(cmd).map_err(|e| match e {
            ExecError::NotFound => StructuralError::ToolNotFound,
            ExecError::Io => StructuralError::Io,
            ExecError::TimedOut => StructuralError::TimedOut,
            ExecError::Cancelled => StructuralError::Cancelled,
            ExecError::OutputTooLarge => StructuralError::OutputTooLarge,
        })
    }
}
