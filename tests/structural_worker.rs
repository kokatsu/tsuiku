//! End-to-end coordinator tests with a fake difft executable.
//!
//! The fakes signal what they are doing through marker files, so the tests
//! synchronize on the real state of the subprocess instead of on sleeps.

use std::fs;
use std::io::Write as _;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tsuiku::asyncstate::{AsyncState, StructuralDiffState, StructuralError, StructuralSkip};
use tsuiku::ids::{ContentId, ContentIdentity, ContentPairId};
use tsuiku::structural::runner::DifftRunner;
use tsuiku::structural::tempfiles::LanguagePathHint;
use tsuiku::structural_worker::{MAX_STRUCTURAL_LINES, StructuralDiffCoordinator};
use tsuiku::text::{ClassifiedContent, TextContent, classify};

const VERSION_OK: &str = "echo 'Difftastic 0.69.0'";
const SLOW_JSON: &str = r#"{"language":"Slow","path":"x.rs","status":"unchanged"}"#;
const LATEST_JSON: &str = r#"{"language":"Latest","path":"x.rs","status":"unchanged"}"#;

/// Write an executable stand-in for difft. `run_body` sees the real argument
/// list, so `$3` and `$4` are the old and new temp files.
fn fake_difft(dir: &tempfile::TempDir, version_body: &str, run_body: &str) -> PathBuf {
    let path = dir.path().join("fake-difft");
    let mut file = fs::File::create(&path).expect("create fake difft");
    writeln!(
        file,
        "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then\n{version_body}\nexit 0\nfi\n{run_body}"
    )
    .expect("write fake difft");
    file.set_permissions(fs::Permissions::from_mode(0o755))
        .expect("chmod fake difft");
    path
}

fn runner(binary: PathBuf) -> DifftRunner {
    DifftRunner {
        binary,
        timeout: Duration::from_secs(5),
        ..DifftRunner::default()
    }
}

fn text(source: &str) -> Arc<TextContent> {
    match classify(Arc::from(source.as_bytes())) {
        ClassifiedContent::Text(text) => Arc::new(text),
        ClassifiedContent::Binary(_) => panic!("text fixture"),
    }
}

fn hint() -> LanguagePathHint {
    LanguagePathHint {
        extension: Some(b"rs".to_vec()),
        basename: Some(b"x.rs".to_vec()),
    }
}

fn pair(old: &str, new: &str) -> ContentPairId {
    ContentPairId {
        old: ContentIdentity::Present(ContentId::compute(old.as_bytes())),
        new: ContentIdentity::Present(ContentId::compute(new.as_bytes())),
    }
}

fn request(worker: &mut StructuralDiffCoordinator, old: &str, new: &str) {
    worker.request(pair(old, new), hint(), hint(), text(old), text(new));
}

/// Wait for a marker the fake difft writes. The worker is polled meanwhile,
/// because a request held for the version probe is only dispatched by a poll
/// — exactly as the event loop does it.
fn wait_for(worker: &mut StructuralDiffCoordinator, path: &Path, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !path.exists() {
        worker.poll();
        assert!(Instant::now() < deadline, "{what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn poll_until(
    worker: &mut StructuralDiffCoordinator,
    what: &str,
    reached: impl Fn(&StructuralDiffState) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        worker.poll();
        if reached(worker.state()) {
            return;
        }
        assert!(Instant::now() < deadline, "{what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn stale_completion_never_replaces_the_latest_pending_request() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let started = dir.path().join("slow-started");
    let bin = fake_difft(
        &dir,
        VERSION_OK,
        &format!(
            "if grep -q slow \"$3\"; then\n  touch '{}'\n  sleep 1\n  echo '{SLOW_JSON}'\nelse\n  echo '{LATEST_JSON}'\nfi",
            started.to_string_lossy()
        ),
    );
    let mut worker = StructuralDiffCoordinator::with_runner(1024 * 1024, runner(bin));

    request(&mut worker, "slow old\n", "slow new\n");
    // Only once the marker exists is the stale job genuinely running, which
    // is the case this test is about — replacing a queued job is a different
    // and much easier path.
    wait_for(&mut worker, &started, "the slow difft never started");

    request(&mut worker, "old\n", "new\n");
    poll_until(&mut worker, "latest job did not finish", |state| {
        matches!(state, AsyncState::Ready(_))
    });

    let AsyncState::Ready(overlay) = worker.state() else {
        unreachable!("polled until ready")
    };
    assert_eq!(overlay.language, "Latest");
}

#[test]
fn dropping_the_coordinator_kills_a_running_difft() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let started = dir.path().join("started");
    let finished = dir.path().join("finished");
    let bin = fake_difft(
        &dir,
        VERSION_OK,
        &format!(
            "touch '{}'\nsleep 30\ntouch '{}'",
            started.to_string_lossy(),
            finished.to_string_lossy()
        ),
    );
    let mut worker = StructuralDiffCoordinator::with_runner(1024 * 1024, runner(bin));

    request(&mut worker, "old\n", "new\n");
    wait_for(&mut worker, &started, "difft never started");

    let dropped_at = Instant::now();
    drop(worker);
    assert!(
        dropped_at.elapsed() < Duration::from_secs(2),
        "shutdown must be bounded, not blocked on the running child"
    );
    std::thread::sleep(Duration::from_millis(200));
    assert!(
        !finished.exists(),
        "the running difft must be killed on shutdown"
    );
}

#[test]
fn one_sided_pairs_never_spawn_difft() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let ran = dir.path().join("ran");
    let bin = fake_difft(
        &dir,
        VERSION_OK,
        &format!("touch '{}'\necho '{LATEST_JSON}'", ran.to_string_lossy()),
    );
    let mut worker = StructuralDiffCoordinator::with_runner(1024 * 1024, runner(bin));

    let added = "new file\n";
    worker.request(
        ContentPairId {
            old: ContentIdentity::Absent,
            new: ContentIdentity::Present(ContentId::compute(added.as_bytes())),
        },
        LanguagePathHint::none(),
        hint(),
        text(""),
        text(added),
    );

    assert!(matches!(
        worker.state(),
        AsyncState::Skipped(StructuralSkip::OneSided)
    ));
    std::thread::sleep(Duration::from_millis(100));
    worker.poll();
    assert!(!ran.exists(), "an added file must not spawn difft");
}

#[test]
fn a_slow_version_probe_does_not_block_the_first_request() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let bin = fake_difft(
        &dir,
        &format!("sleep 0.3\n{VERSION_OK}"),
        &format!("echo '{LATEST_JSON}'"),
    );
    let mut worker = StructuralDiffCoordinator::with_runner(1024 * 1024, runner(bin));

    // The probe is still sleeping, so no cache key exists yet; the request is
    // held rather than answered or lost.
    let requested_at = Instant::now();
    request(&mut worker, "old\n", "new\n");
    assert!(
        requested_at.elapsed() < Duration::from_millis(100),
        "requesting must not wait for the version probe"
    );
    assert!(matches!(worker.state(), AsyncState::Pending { .. }));

    poll_until(&mut worker, "deferred request was never dispatched", |s| {
        matches!(s, AsyncState::Ready(_))
    });
    let AsyncState::Ready(overlay) = worker.state() else {
        unreachable!("polled until ready")
    };
    assert_eq!(overlay.language, "Latest");
}

#[test]
fn a_process_failure_backs_off_before_retrying() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let runs = dir.path().join("runs");
    let bin = fake_difft(
        &dir,
        VERSION_OK,
        &format!("echo x >> '{}'\nexit 3", runs.to_string_lossy()),
    );
    let mut worker = StructuralDiffCoordinator::with_runner(1024 * 1024, runner(bin));

    request(&mut worker, "old\n", "new\n");
    poll_until(&mut worker, "failure was never reported", |state| {
        matches!(state, AsyncState::Failed(_))
    });
    assert!(matches!(
        worker.state(),
        AsyncState::Failed(StructuralError::ProcessFailed { exit_code: Some(3) })
    ));

    // Navigating away and back must reuse the recorded failure: the five
    // second window has not elapsed, so difft is not spawned again.
    worker.reset();
    request(&mut worker, "old\n", "new\n");
    assert!(matches!(worker.state(), AsyncState::Failed(_)));
    assert_eq!(
        fs::read_to_string(&runs).expect("run log").lines().count(),
        1,
        "difft must not be retried inside the backoff window"
    );
}

#[test]
fn oversized_output_is_cached_without_rerunning_difft() {
    // Unlike a timeout or a non-zero exit, output over the cap is a property
    // of this exact content pair, so it is remembered for the session rather
    // than retried after a delay.
    let dir = tempfile::TempDir::new().expect("temp dir");
    let runs = dir.path().join("runs");
    let bin = fake_difft(
        &dir,
        VERSION_OK,
        &format!(
            "echo x >> '{}'\nhead -c 8192 /dev/zero | tr '\\0' 'o'",
            runs.to_string_lossy()
        ),
    );
    let mut worker = StructuralDiffCoordinator::with_runner(
        1024 * 1024,
        DifftRunner {
            binary: bin,
            max_stdout_bytes: 1024,
            ..DifftRunner::default()
        },
    );

    request(&mut worker, "old\n", "new\n");
    poll_until(
        &mut worker,
        "oversized output was never reported",
        |state| matches!(state, AsyncState::Failed(StructuralError::OutputTooLarge)),
    );

    worker.reset();
    request(&mut worker, "old\n", "new\n");
    assert!(matches!(
        worker.state(),
        AsyncState::Failed(StructuralError::OutputTooLarge)
    ));
    assert_eq!(
        fs::read_to_string(&runs).expect("run log").lines().count(),
        1,
        "a cached oversized-output failure must not spawn difft again"
    );
}

#[test]
fn oversized_input_is_skipped_before_spawning_difft() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let ran = dir.path().join("ran");
    let bin = fake_difft(
        &dir,
        VERSION_OK,
        &format!("touch '{}'\necho '{LATEST_JSON}'", ran.to_string_lossy()),
    );
    let mut worker = StructuralDiffCoordinator::with_runner(1024 * 1024, runner(bin));
    let huge = "line\n".repeat(MAX_STRUCTURAL_LINES + 1);

    request(&mut worker, "", &huge);

    assert!(matches!(
        worker.state(),
        AsyncState::Skipped(StructuralSkip::SizeLimited)
    ));
    std::thread::sleep(Duration::from_millis(100));
    worker.poll();
    assert!(!ran.exists(), "oversized input must not spawn difft");
}

#[test]
fn missing_tool_is_a_capability_skip() {
    let mut worker = StructuralDiffCoordinator::with_runner(
        1024,
        runner(PathBuf::from("/nonexistent/difft-definitely-absent")),
    );

    request(&mut worker, "old\n", "new\n");

    poll_until(&mut worker, "missing tool was never reported", |state| {
        matches!(state, AsyncState::Skipped(StructuralSkip::ToolUnavailable))
    });
}
