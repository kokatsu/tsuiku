//! Guard behavior of the difft subprocess runner, exercised with fake
//! difft executables (shell scripts) so no real difft is needed.

use std::fs;
use std::io::Write as _;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::Duration;

use tsuiku::asyncstate::StructuralError;
use tsuiku::structural::runner::{CancelFlag, DifftRunner};

const VALID_JSON: &str = r#"{"language":"Text","path":"x","status":"unchanged"}"#;

/// Write an executable shell script standing in for difft.
fn fake_difft(dir: &tempfile::TempDir, body: &str) -> PathBuf {
    let path = dir.path().join("fake-difft");
    let mut f = fs::File::create(&path).expect("create script");
    writeln!(f, "#!/bin/sh\n{body}").expect("write script");
    f.set_permissions(fs::Permissions::from_mode(0o755))
        .expect("chmod script");
    path
}

fn runner(binary: PathBuf) -> DifftRunner {
    DifftRunner {
        binary,
        timeout: Duration::from_secs(5),
        max_stdout_bytes: 1024 * 1024,
        max_stderr_bytes: 4096,
        cancel: CancelFlag::default(),
    }
}

fn run(r: &DifftRunner) -> Result<(), StructuralError> {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let a = dir.path().join("a");
    let b = dir.path().join("b");
    fs::write(&a, "x").expect("write a");
    fs::write(&b, "y").expect("write b");
    r.run(&a, &b).map(|_| ())
}

#[test]
fn quiet_stderr_succeeds() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let bin = fake_difft(&dir, &format!("echo '{VALID_JSON}'"));
    assert!(run(&runner(bin)).is_ok());
}

#[test]
fn stderr_over_cap_fails_even_with_valid_stdout() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    // 8 KiB of stderr against a 4 KiB cap, plus valid JSON on stdout.
    let bin = fake_difft(
        &dir,
        &format!("head -c 8192 /dev/zero | tr '\\0' 'e' >&2\necho '{VALID_JSON}'"),
    );
    assert_eq!(run(&runner(bin)), Err(StructuralError::OutputTooLarge));
}

#[test]
fn stdout_over_cap_fails() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let bin = fake_difft(&dir, "head -c 2097152 /dev/zero | tr '\\0' 'o'");
    assert_eq!(run(&runner(bin)), Err(StructuralError::OutputTooLarge));
}

#[test]
fn cap_trip_fails_fast_while_child_keeps_running() {
    // The child overflows the stdout cap early, then lingers. The runner
    // must report OutputTooLarge as soon as the cap trips instead of
    // waiting out the timeout (which would also mislabel the error as
    // TimedOut).
    let dir = tempfile::TempDir::new().expect("temp dir");
    let bin = fake_difft(&dir, "head -c 2097152 /dev/zero | tr '\\0' 'o'\nsleep 60");
    let started = std::time::Instant::now();
    assert_eq!(run(&runner(bin)), Err(StructuralError::OutputTooLarge));
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "cap trip must not wait for child exit or timeout"
    );
}

#[test]
fn nonzero_exit_reports_process_failure() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let bin = fake_difft(&dir, "exit 3");
    assert_eq!(
        run(&runner(bin)),
        Err(StructuralError::ProcessFailed { exit_code: Some(3) })
    );
}

#[test]
fn hung_process_times_out() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let bin = fake_difft(&dir, "sleep 60");
    let mut r = runner(bin);
    r.timeout = Duration::from_millis(200);
    assert_eq!(run(&r), Err(StructuralError::TimedOut));
}

#[test]
fn hung_grandchild_holding_pipe_times_out() {
    // The shell forks a background child that inherits the pipes and
    // outlives its parent. Killing only the direct child would leave the
    // pipes open and block the runner forever; the process-group kill must
    // take the grandchild down too.
    let dir = tempfile::TempDir::new().expect("temp dir");
    let bin = fake_difft(&dir, "sleep 60 &\nsleep 60");
    let mut r = runner(bin);
    r.timeout = Duration::from_millis(200);
    let started = std::time::Instant::now();
    assert_eq!(run(&r), Err(StructuralError::TimedOut));
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "runner must not wait for the grandchild"
    );
}

#[test]
fn grandchild_left_after_clean_exit_times_out() {
    // The direct child exits successfully but leaks a background process
    // holding the pipes, so the readers never see EOF. The bounded reader
    // grace must convert this into TimedOut instead of hanging.
    let dir = tempfile::TempDir::new().expect("temp dir");
    let bin = fake_difft(&dir, &format!("sleep 60 &\necho '{VALID_JSON}'\nexit 0"));
    let mut r = runner(bin);
    r.timeout = Duration::from_millis(200);
    let started = std::time::Instant::now();
    assert_eq!(run(&r), Err(StructuralError::TimedOut));
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "runner must not wait for the leaked process"
    );
}

#[test]
fn cancelling_kills_and_reaps_the_running_child() {
    // Shutdown must not simply abandon the child: the run returns promptly
    // with `Cancelled`, and the killed script never reaches its second line.
    let dir = tempfile::TempDir::new().expect("temp dir");
    let finished = dir.path().join("finished");
    let bin = fake_difft(
        &dir,
        &format!("sleep 30\ntouch '{}'", finished.to_string_lossy()),
    );
    let mut r = runner(bin);
    r.timeout = Duration::from_secs(30);
    let cancel = r.cancel.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(50));
        cancel.cancel();
    });

    let started = std::time::Instant::now();
    assert_eq!(run(&r), Err(StructuralError::Cancelled));
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "cancellation must not wait for the timeout"
    );
    std::thread::sleep(Duration::from_millis(100));
    assert!(!finished.exists(), "the child must have been killed");
}

#[test]
fn version_times_out_like_run() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let bin = fake_difft(&dir, "sleep 60");
    let mut r = runner(bin);
    r.timeout = Duration::from_millis(200);
    assert_eq!(r.version(), Err(StructuralError::TimedOut));
}

#[test]
fn version_output_is_capped() {
    // 128 KiB of --version output against the 64 KiB version cap.
    let dir = tempfile::TempDir::new().expect("temp dir");
    let bin = fake_difft(&dir, "head -c 131072 /dev/zero | tr '\\0' 'v'");
    assert_eq!(runner(bin).version(), Err(StructuralError::OutputTooLarge));
}

#[test]
fn missing_binary_reports_tool_not_found() {
    let r = runner(PathBuf::from("/nonexistent/difft-definitely-absent"));
    assert_eq!(r.version(), Err(StructuralError::ToolNotFound));
    assert_eq!(run(&r), Err(StructuralError::ToolNotFound));
}

#[test]
fn version_reports_first_line() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let bin = fake_difft(&dir, "printf 'Fakestic 9.9.9\\nextra\\n'");
    assert_eq!(runner(bin).version().expect("version"), "Fakestic 9.9.9");
}
