//! What it costs to read blob bytes through the git CLI versus through gix.
//!
//! Discovery tells us which blobs to read; this measures reading them. Three
//! strategies are compared over the same object ids:
//!
//! * one `git cat-file blob <oid>` process per blob — the naive shape
//! * one long-lived `git cat-file --batch` process fed every oid
//! * `gix`, reading straight from the object database
//!
//! Resolver selection recorded in 2026-07 (development repository, 32 blobs /
//! 247,788 bytes): direct gix resolution was retained. Best-of-five release
//! measurements were 817µs for gix, 5.913ms for `--batch`, and 218.033ms for
//! per-blob processes. Revisit only if a representative large-repository
//! fixture shows direct gix lookup becoming a startup bottleneck.
//!
//! ```text
//! cargo run --release --example resolve_bench -- <repo-dir>
//! ```

use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

enum Head {
    /// Blob ids reachable from `HEAD`'s tree, which stand in for "the old side
    /// of a large change set".
    Blobs(Vec<String>),
    /// A repository whose first commit has not been made. Nothing to measure,
    /// but nothing wrong either.
    Unborn,
    /// Not a repository, not a path that exists, a broken object database:
    /// anything the caller should hear about as a failure.
    Failed(String),
}

fn git(repo: &Path, args: &[&str]) -> Result<std::process::Output, std::io::Error> {
    Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
}

fn head_blob_ids(repo: &Path) -> Head {
    let out = match git(
        repo,
        &[
            "ls-tree",
            "-r",
            "-z",
            "--format=%(objectname) %(objecttype)",
            "HEAD",
        ],
    ) {
        Ok(out) => out,
        Err(e) => return Head::Failed(format!("cannot run git: {e}")),
    };

    if out.status.success() {
        return Head::Blobs(
            String::from_utf8_lossy(&out.stdout)
                .split('\0')
                .filter_map(|entry| {
                    let (oid, kind) = entry.trim().split_once(' ')?;
                    (kind == "blob").then(|| oid.to_string())
                })
                .collect(),
        );
    }

    // `ls-tree` fails the same way whether HEAD is merely unborn or the
    // repository cannot be read at all, so ask the two questions separately.
    let in_repository = git(repo, &["rev-parse", "--git-dir"]).is_ok_and(|o| o.status.success());
    let head_resolves =
        git(repo, &["rev-parse", "--verify", "--quiet", "HEAD"]).is_ok_and(|o| o.status.success());
    match (in_repository, head_resolves) {
        (true, false) => Head::Unborn,
        _ => Head::Failed(String::from_utf8_lossy(&out.stderr).trim().to_string()),
    }
}

fn cat_file_per_process(repo: &Path, oids: &[String]) -> usize {
    oids.iter()
        .map(|oid| {
            let out = Command::new("git")
                .arg("-C")
                .arg(repo)
                .args(["cat-file", "blob", oid])
                .output()
                .expect("run git cat-file");
            out.stdout.len()
        })
        .sum()
}

fn cat_file_batch(repo: &Path, oids: &[String]) -> usize {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["cat-file", "--batch"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn git cat-file --batch");

    let mut stdin = child.stdin.take().expect("batch stdin");
    let oids = oids.to_vec();
    // The pipe holds a bounded amount, so feeding and draining must not be
    // serialised or the process deadlocks on a large batch.
    let writer = std::thread::spawn(move || {
        for oid in &oids {
            writeln!(stdin, "{oid}").expect("write oid");
        }
        stdin.flush().expect("flush");
    });

    let mut reader = BufReader::new(child.stdout.take().expect("batch stdout"));
    let mut total = 0;
    let mut header = String::new();
    loop {
        header.clear();
        if reader.read_line(&mut header).expect("read header") == 0 {
            break;
        }
        let size: usize = header
            .trim_end()
            .rsplit(' ')
            .next()
            .expect("size field")
            .parse()
            .expect("parse size");
        let mut body = vec![0u8; size + 1]; // payload plus its trailing newline
        reader.read_exact(&mut body).expect("read body");
        total += size;
    }

    writer.join().expect("writer thread");
    child.wait().expect("reap batch process");
    total
}

fn gix_objects(repo: &Path, oids: &[String]) -> usize {
    let repo = gix::open(repo).expect("open repository");
    oids.iter()
        .map(|oid| {
            let id = gix::ObjectId::from_hex(oid.as_bytes()).expect("parse oid");
            repo.find_object(id).expect("find object").data.len()
        })
        .sum()
}

fn bench(label: &str, runs: u32, mut f: impl FnMut() -> usize) {
    let mut best = Duration::MAX;
    let mut bytes = 0;
    for _ in 0..runs {
        let t = Instant::now();
        bytes = f();
        best = best.min(t.elapsed());
    }
    println!("  {label:<28} {best:>12.3?}  ({bytes} bytes)");
}

fn main() -> ExitCode {
    let repo = std::env::args()
        .nth(1)
        .expect("usage: resolve_bench <repo-dir>");
    let repo = Path::new(&repo);

    let oids = match head_blob_ids(repo) {
        Head::Blobs(oids) => oids,
        Head::Unborn => {
            println!("no commit yet in {}", repo.display());
            return ExitCode::SUCCESS;
        }
        Head::Failed(message) => {
            eprintln!("cannot read {}: {message}", repo.display());
            return ExitCode::FAILURE;
        }
    };

    println!("resolving {} blobs from {}", oids.len(), repo.display());
    if oids.is_empty() {
        // An empty commit, or a HEAD holding nothing but trees. There is no
        // work to time, and extrapolating from an empty sample would divide by
        // zero and hand `Duration::mul_f64` a NaN.
        println!("  nothing to resolve");
        return ExitCode::SUCCESS;
    }

    // The per-process strategy is quadratically painful, so it only gets a
    // small slice and its number is scaled back up for comparison.
    let sample = &oids[..oids.len().min(200)];
    let t = Instant::now();
    let sample_bytes = cat_file_per_process(repo, sample);
    let sample_time = t.elapsed();
    let scaled = sample_time.mul_f64(oids.len() as f64 / sample.len() as f64);
    println!(
        "  {:<28} {:>12.3?}  ({} bytes over {} blobs, extrapolates to {:.3?})",
        "cat-file per process",
        sample_time,
        sample_bytes,
        sample.len(),
        scaled
    );

    bench("cat-file --batch", 5, || cat_file_batch(repo, &oids));
    bench("gix find_object", 5, || gix_objects(repo, &oids));

    // Opening the repository is part of what a fresh process pays.
    let t = Instant::now();
    let opened = gix::open(repo).expect("open repository");
    println!("\n  gix::open                    {:>12.3?}", t.elapsed());
    drop(opened);

    let t = Instant::now();
    Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "HEAD"])
        .output()
        .expect("run git rev-parse");
    println!("  git process spawn (rev-parse) {:>11.3?}", t.elapsed());
    ExitCode::SUCCESS
}
