//! Finding and safely executing an interpreter the operator installed.
//!
//! Shared by every Phase 3 runtime module, because the dangerous part is the
//! same for all of them and must not be re-decided per module.
//!
//! # The threat this exists for
//!
//! `docs/THREAT-MODEL.md` T-EXEC. The adversary is **a2, a local unprivileged
//! user on the host** - not the operator, who controls the machine anyway.
//! Shared and reseller hosting is the common case in this market and the agent
//! is frequently run as root, so if a compromised account can write
//! `/usr/local/bin/php`, a benchmark that executes "the PHP it found" hands that
//! account root.
//!
//! Three things together make that not happen, and none of them is sufficient
//! alone:
//!
//! 1. **A compile-time allow-list of absolute paths.** `$PATH` is never
//!    consulted. It is environment, and a benchmark that runs whatever `php`
//!    resolves to runs whatever the environment says.
//! 2. **A safe-path check.** The binary *and every ancestor directory* must be
//!    owned by uid 0 and must not be group- or world-writable. Checking only
//!    the file is the classic mistake: `/usr/local/bin` being writable is just
//!    as good as `/usr/local/bin/php` being writable, because the attacker can
//!    replace the file.
//! 3. **Fixed argv, no shell, and a cleared environment.** Not a filtered one -
//!    PHP reads `PHP_INI_SCAN_DIR`, Node reads `NODE_OPTIONS`, and a filter is
//!    a list of the variables somebody thought of.
//!
//! See [ADR-0013](../../../docs/adr/0013-executing-a-discovered-runtime.md).

use crate::module::ModuleError;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Why a candidate interpreter was not used.
///
/// Reported rather than swallowed: "no PHP found" and "PHP found at a path a
/// non-root user can write" are very different facts, and the operator can act
/// on the second.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rejection {
    pub path: PathBuf,
    pub reason: String,
}

/// An interpreter that passed every check.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Interpreter {
    pub path: PathBuf,
}

/// Picks the first allow-listed path that exists and is safe to execute.
///
/// Returns the rejections alongside, so a module can disclose *why* it declined
/// a runtime that is visibly installed.
pub fn discover(candidates: &[&str]) -> (Option<Interpreter>, Vec<Rejection>) {
    let mut rejections = Vec::new();
    let mut chosen = None;
    for candidate in candidates {
        let path = Path::new(candidate);
        if !path.exists() {
            // Absence is the normal case for most of an allow-list and is not
            // worth reporting: a machine with PHP 8.3 is not misconfigured for
            // lacking PHP 7.4.
            continue;
        }
        match check_safe(path) {
            // Every candidate is checked even after one has been chosen,
            // because the rejection list is a security advisory in its own
            // right and returning early hid the most important entries. The
            // first safe path wins, but a *later* unsafe one still gets
            // reported - and on a default `$PATH`, `/usr/local/bin` precedes
            // `/usr/bin`, so the writable binary that used to go unreported was
            // exactly the one the operator's own shell would execute.
            Ok(resolved) => {
                if chosen.is_none() {
                    chosen = Some(Interpreter { path: resolved });
                }
            }
            Err(reason) => rejections.push(Rejection {
                path: path.to_path_buf(),
                reason,
            }),
        }
    }
    (chosen, rejections)
}

/// The safe-path check: is this file, and everything above it, root-owned and
/// unwritable by anyone else?
///
/// Symlinks are resolved first, because the check has to apply to what will
/// actually be executed rather than to the name used to reach it. `/usr/bin/php`
/// pointing at a world-writable file elsewhere must fail, and it does.
fn check_safe(path: &Path) -> Result<PathBuf, String> {
    let resolved = path
        .canonicalize()
        .map_err(|error| format!("cannot be resolved: {error}"))?;

    let metadata =
        std::fs::metadata(&resolved).map_err(|error| format!("cannot be inspected: {error}"))?;
    if !metadata.is_file() {
        return Err("is not a regular file".into());
    }
    if metadata.mode() & 0o111 == 0 {
        return Err("is not executable".into());
    }
    if let Some(problem) = unsafe_ownership(&metadata) {
        return Err(format!("{problem}, so a non-root user could replace it"));
    }

    // Every ancestor, up to and including `/`. A writable directory anywhere on
    // the path is as good as a writable file: whoever can write the directory
    // can replace what is in it.
    let mut ancestor = resolved.parent();
    while let Some(directory) = ancestor {
        let metadata = std::fs::metadata(directory)
            .map_err(|error| format!("`{}` cannot be inspected: {error}", directory.display()))?;
        if let Some(problem) = unsafe_ownership(&metadata) {
            return Err(format!(
                "its parent directory `{}` {problem}, so a non-root user could replace the binary",
                directory.display()
            ));
        }
        ancestor = directory.parent();
    }
    Ok(resolved)
}

/// `None` when a directory cannot be written by anyone but its owner.
///
/// A weaker check than [`unsafe_ownership`] on purpose: this is applied to the
/// agent's *own* scratch directory, which is owned by whatever user the agent
/// runs as rather than necessarily by root. What matters there is that nobody
/// *else* can write it, because whoever can write the directory chooses what
/// the interpreter executes.
///
/// The sticky bit is not an escape. `/tmp` is `1777` and a sticky directory
/// stops one user deleting another's files - it does nothing to stop a file
/// being created at a name nobody has claimed yet, which is the whole attack.
pub fn directory_is_writable_by_others(path: &Path) -> Option<String> {
    let metadata = std::fs::metadata(path).ok()?;
    let mode = metadata.mode();
    if mode & 0o020 != 0 {
        return Some(format!("is group-writable (mode {:o})", mode & 0o7777));
    }
    if mode & 0o002 != 0 {
        return Some(format!("is world-writable (mode {:o})", mode & 0o7777));
    }
    None
}

/// `None` when this inode is root-owned and writable only by root.
fn unsafe_ownership(metadata: &std::fs::Metadata) -> Option<String> {
    if metadata.uid() != 0 {
        return Some(format!("is owned by uid {}, not root", metadata.uid()));
    }
    let mode = metadata.mode();
    if mode & 0o020 != 0 {
        return Some(format!("is group-writable (mode {:o})", mode & 0o7777));
    }
    if mode & 0o002 != 0 {
        return Some(format!("is world-writable (mode {:o})", mode & 0o7777));
    }
    // Irrelevant while the agent is root, which is the common case - but the
    // agent is not *required* to be root, and an unprivileged agent executing a
    // root-owned setuid binary spawns a root child pointed at a script in a
    // directory that agent owns. That is the same attack with the privilege
    // direction reversed, and it costs one comparison to refuse.
    if mode & 0o4000 != 0 {
        return Some("is setuid".to_string());
    }
    if mode & 0o2000 != 0 {
        return Some("is setgid".to_string());
    }
    None
}

/// Shared by every runtime module, because getting this wrong is the same
/// mistake for all of them and a second copy is a second chance to make it.
/// Removes the workload script when the module returns, however it returns.
///
/// The same mechanism `storage.mixed` uses for its fixture, for the same
/// reason: explicit cleanup at the end of `run` covers only the happy path,
/// which is the one that never needed it.
///
/// **Not on a panic**, and the distinction is worth stating rather than
/// implying. The release profile sets `panic = "abort"`, so destructors do not
/// run and no `Drop` impl anywhere in this workspace covers that path. A test
/// cannot catch the gap either, because Cargo builds test harnesses with
/// `panic = "unwind"` whatever the profile says. What covers it is the removal
/// at the *start* of the next `write`, which is also what handles a machine
/// that lost power mid-run.
#[derive(Debug)]
pub struct ScriptFile {
    /// Absolute path the interpreter is given.
    pub path: PathBuf,
}

impl ScriptFile {
    /// Writes a workload script somewhere only this user can have written it.
    ///
    /// # Why this is as security-critical as the interpreter check
    ///
    /// [`check_safe`] guards *which binary* runs. This guards *what it is told
    /// to run*, and the second is worth exactly as much as the first: a root
    /// interpreter pointed at somebody else's script is root code execution
    /// just as surely as somebody else's interpreter is.
    ///
    /// An earlier version used `std::fs::write` followed by a `chmod`, and had
    /// two holes an audit found by executing them rather than reasoning about
    /// them. `fs::write` opens `O_CREAT|O_WRONLY|O_TRUNC` and **follows
    /// symlinks**, so a symlink planted at the path between the removal and the
    /// write redirected root's write wherever the attacker liked; and the file
    /// existed at the umask's mode - `0666` under a permissive umask - for the
    /// window between the write and the `chmod`, which then locked the
    /// attacker's content in at `0600`.
    ///
    /// Three changes close them:
    ///
    /// * The directory is checked before it is used, because `create_dir_all`
    ///   succeeding on an existing directory says nothing about its mode and
    ///   one run under a bad umask would otherwise leave a permanently
    ///   world-writable scratch.
    /// * The file is created `O_EXCL|O_NOFOLLOW`, so a planted symlink is
    ///   refused rather than followed and an existing file is refused rather
    ///   than truncated.
    /// * The mode is set *at creation*, so there is no window at all.
    pub fn write(
        scratch: &std::path::Path,
        name: &str,
        content: &str,
    ) -> Result<Self, ModuleError> {
        use std::os::unix::fs::OpenOptionsExt;

        std::fs::create_dir_all(scratch).map_err(|error| {
            ModuleError::Precondition(format!("scratch directory unusable: {error}"))
        })?;
        if let Some(problem) = directory_is_writable_by_others(scratch) {
            return Err(ModuleError::Precondition(format!(
                "the scratch directory `{}` {problem}. The workload script would be written \
                 there and then executed, so another user being able to replace it is another \
                 user choosing what this agent runs. See docs/THREAT-MODEL.md, T-EXEC.",
                scratch.display()
            )));
        }

        let path = scratch.join(name);
        // A script left by a crashed run is stale by definition, and
        // `create_new` below refuses rather than truncating if anything - stale
        // file or planted symlink - is still at the path. This removal is also
        // what covers the panic path, where `Drop` does not run.
        let _ = std::fs::remove_file(&path);
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            // Mode at creation, so the file is never briefly world-readable.
            .mode(0o600)
            // A symlink at this path is refused, never followed.
            .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
            .open(&path)
            .map_err(|error| {
                ModuleError::Precondition(format!("cannot create the workload script: {error}"))
            })?;
        std::io::Write::write_all(&mut file, content.as_bytes()).map_err(|error| {
            ModuleError::Precondition(format!("cannot write the workload script: {error}"))
        })?;
        Ok(Self { path })
    }
}

impl Drop for ScriptFile {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_file(&self.path) {
            if error.kind() != std::io::ErrorKind::NotFound {
                eprintln!(
                    "darcbench: could not remove the workload script at {}: {error}",
                    self.path.display()
                );
            }
        }
    }
}

/// What a child produced.
#[derive(Clone, Debug)]
pub struct Output {
    pub stdout: String,
    pub stderr: String,
    pub status: Option<i32>,
    pub elapsed: Duration,
}

impl Output {
    pub fn succeeded(&self) -> bool {
        self.status == Some(0)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ExecError {
    #[error("could not start `{path}`: {source}")]
    Spawn {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("`{path}` did not finish within {seconds}s and was killed")]
    Timeout { path: String, seconds: u64 },
    #[error("could not read the output of `{path}`: {source}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },
}

/// Longest to keep waiting for a child's output after the child itself has
/// gone, or has been killed.
///
/// Needed because "the child exited" and "the pipes reached EOF" are different
/// events: a pipe reaches EOF when *every* write end closes, including one a
/// grandchild inherited. Without this bound a double-forking child holds the
/// agent for as long as its orphan lives - measured at ten seconds under a
/// two-second timeout before the bound existed.
const DRAIN_GRACE: Duration = Duration::from_millis(500);

/// Longest to wait for a killed child to actually die.
///
/// `SIGKILL` cannot be caught, but it cannot interrupt uninterruptible sleep
/// either, so a child stuck in D-state does not die until it leaves it. An
/// unbounded `wait()` there would block the agent indefinitely - which is
/// precisely the hang the timeout exists to prevent.
const REAP_GRACE: Duration = Duration::from_secs(2);

/// Runs an interpreter with a fixed argument vector and a cleared environment.
///
/// # What this function will not do
///
/// It takes `&[&str]` rather than anything stringly-typed from a caller, it
/// never invokes a shell, and it never consults `$PATH` - `Command::new` is
/// given an already-resolved absolute path. Nothing here can be made to run a
/// different program by any input the agent accepts.
///
/// `stdin` is null so a child that decides to read from it exits rather than
/// blocking forever against a terminal the agent may not have.
///
/// # Why the output is drained on threads
///
/// The obvious shape - poll `try_wait` in a loop, then `wait_with_output` - is
/// wrong in a way that only shows up under load, and an audit reproduced it:
/// with both streams piped and nobody reading, a child that writes past the
/// 64 KiB pipe buffer blocks in `write()`, never exits, and is reported as a
/// timeout. Stderr alone is enough, because the two buffers are independent.
/// For a PHP whose `display_errors` is on and which emits one notice per call,
/// that is not a corner case.
///
/// So each stream gets a reader thread from the moment the child starts, and
/// every wait in here is bounded. A reader that is still blocked because a
/// grandchild holds the pipe open is abandoned rather than joined: its output
/// is lost, which is the right trade against holding the agent hostage.
pub fn run(
    interpreter: &Interpreter,
    args: &[&str],
    timeout: Duration,
) -> Result<Output, ExecError> {
    run_with_stdin(interpreter, args, None, timeout)
}

/// [`run`], with bytes written to the child's standard input.
///
/// # Why this exists, and why it is safer than the obvious alternative
///
/// `wordpress.*` has to get a 1.6 MB fixture into a container. There are three
/// ways to do that and two of them are worse.
///
/// A **bind mount** is forbidden outright: `no_host_path_can_reach_the_
/// argument_vector` is the isolation, and it is not negotiable for a
/// convenience.
///
/// A **`docker cp`** would put a host path into an argument vector - the thing
/// `Runtime::build` is already a grudging exception to - and the exception
/// would be much weaker here, because a build context is copied *into an image*
/// while `cp` writes into a running container's filesystem.
///
/// A **pipe** puts nothing in the vector at all. The data never has a name the
/// runtime can see, no path is constructed, and the container reads it the way
/// it would read any stream. It is strictly the smallest of the three.
///
/// The write happens on its own thread for the same reason the reads do: a
/// child that has not started reading yet, or that never will, must not be able
/// to block this process in `write()` past the timeout. The pipe is closed when
/// the write finishes, so a child waiting on EOF gets one.
pub fn run_with_stdin(
    interpreter: &Interpreter,
    args: &[&str],
    input: Option<&[u8]>,
    timeout: Duration,
) -> Result<Output, ExecError> {
    let path = interpreter.path.display().to_string();
    let started = Instant::now();

    let mut child = Command::new(&interpreter.path)
        .args(args)
        // Cleared, not filtered. See the module documentation.
        .env_clear()
        // A known directory rather than whatever the agent was launched from.
        // `/` because it is the one directory guaranteed to exist and to be
        // root-owned, and because nothing here should be able to reach a
        // relative path at all.
        .current_dir("/")
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|source| ExecError::Spawn {
            path: path.clone(),
            source,
        })?;

    // Before the readers, because a child that fills its output pipe while
    // waiting for input would otherwise deadlock against a writer that has not
    // started. Detached rather than joined: if the child never reads, the
    // thread dies with the pipe when the child is killed, and the timeout below
    // is what bounds this function either way.
    if let Some(bytes) = input {
        if let Some(mut pipe) = child.stdin.take() {
            let owned = bytes.to_vec();
            std::thread::spawn(move || {
                use std::io::Write;
                // Both errors are expected and neither is actionable: a child
                // that exited early gives `EPIPE`, and one that never read
                // gives nothing. The failure surfaces as the command's own
                // non-zero status, which the caller already checks.
                let _ = pipe.write_all(&owned);
                let _ = pipe.flush();
                // Dropped here, which is the EOF the child is waiting for.
            });
        }
    }

    let stdout = drain(child.stdout.take());
    let stderr = drain(child.stderr.take());

    // Polled rather than waited on, because `wait` has no timeout and a
    // benchmark that hangs is worse than one that fails.
    let poll = Duration::from_millis(5);
    let mut timed_out = false;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if started.elapsed() > timeout {
                    timed_out = true;
                    let _ = child.kill();
                    reap(&mut child);
                    break;
                }
                std::thread::sleep(poll);
            }
            Err(source) => {
                let _ = child.kill();
                reap(&mut child);
                return Err(ExecError::Read { path, source });
            }
        }
    }

    let stdout = collect(stdout);
    let stderr = collect(stderr);
    if timed_out {
        return Err(ExecError::Timeout {
            path,
            seconds: timeout.as_secs(),
        });
    }
    let status = child.try_wait().ok().flatten().and_then(|s| s.code());
    Ok(Output {
        stdout,
        stderr,
        status,
        elapsed: started.elapsed(),
    })
}

/// Starts a thread reading one stream to end, so the child never blocks on a
/// full pipe.
fn drain<R: std::io::Read + Send + 'static>(
    stream: Option<R>,
) -> Option<std::sync::mpsc::Receiver<String>> {
    let mut stream = stream?;
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("darcbench-child-io".into())
        .spawn(move || {
            let mut buffer = Vec::new();
            let _ = std::io::Read::read_to_end(&mut stream, &mut buffer);
            // A send failure means the receiver gave up waiting, which is the
            // abandoned-reader case and needs nothing further.
            let _ = sender.send(String::from_utf8_lossy(&buffer).into_owned());
        })
        .ok()?;
    Some(receiver)
}

/// Takes whatever a reader produced, waiting only briefly.
fn collect(receiver: Option<std::sync::mpsc::Receiver<String>>) -> String {
    receiver
        .and_then(|receiver| receiver.recv_timeout(DRAIN_GRACE).ok())
        .unwrap_or_default()
}

/// Waits for a killed child, but not forever.
///
/// Leaves it unreaped if it will not die - a zombie is a bounded, visible cost
/// and a blocked agent is not.
fn reap(child: &mut std::process::Child) {
    let deadline = Instant::now() + REAP_GRACE;
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(_)) | Err(_) => return,
            Ok(None) => std::thread::sleep(Duration::from_millis(5)),
        }
    }
}

#[cfg(test)]
// In tests, `unwrap`/`expect` panicking *is* the failure signal.
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic_in_result_fn)]
mod tests {
    use super::*;

    #[test]
    fn a_path_that_does_not_exist_is_not_a_rejection() {
        let (found, rejections) = discover(&["/nonexistent/definitely/not/here"]);
        assert!(found.is_none());
        assert!(
            rejections.is_empty(),
            "absence is the normal case for most of an allow-list; only an unsafe path is a \
             finding"
        );
    }

    /// The check that matters: a binary anyone can rewrite is never executed,
    /// however plausible its path looks.
    #[test]
    fn a_writable_binary_is_rejected_with_a_reason() {
        let dir = std::env::temp_dir().join(format!(
            "darcbench-exec-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let planted = dir.join("php");
        std::fs::write(&planted, b"#!/bin/sh\necho pwned\n").unwrap();
        std::fs::set_permissions(
            &planted,
            <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o777),
        )
        .unwrap();

        let (found, rejections) = discover(&[planted.to_str().unwrap()]);
        assert!(found.is_none(), "a world-writable binary must never be run");
        assert_eq!(rejections.len(), 1);
        let reason = &rejections[0].reason;
        assert!(
            reason.contains("writable") || reason.contains("uid"),
            "the rejection must say what was wrong: {reason}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A directory is as good as the file in it.
    #[test]
    fn a_writable_parent_directory_is_rejected_even_when_the_binary_is_not() {
        let dir = std::env::temp_dir().join(format!(
            "darcbench-exec-parent-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        // `std::env::temp_dir()` is `/tmp`, which is world-writable with the
        // sticky bit - exactly the shape this check exists to catch.
        let binary = dir.join("php");
        std::fs::write(&binary, b"x").unwrap();
        std::fs::set_permissions(
            &binary,
            <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o755),
        )
        .unwrap();

        let (found, rejections) = discover(&[binary.to_str().unwrap()]);
        assert!(found.is_none());
        assert_eq!(rejections.len(), 1, "{rejections:?}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The positive case, against something every Linux host has.
    #[test]
    fn a_root_owned_system_binary_passes_and_runs_with_a_cleared_environment() {
        let (found, _) = discover(&["/bin/sh", "/usr/bin/sh"]);
        let Some(shell) = found else {
            // A host without /bin/sh is not one this suite runs on, but the
            // test declines to fail for the wrong reason.
            return;
        };

        let output = run(&shell, &["-c", "echo ok; env"], Duration::from_secs(5)).unwrap();
        assert!(output.succeeded(), "{output:?}");
        assert!(output.stdout.starts_with("ok"));
        // The environment must not have been inherited. `env` in a cleared
        // environment prints nothing beyond what the shell itself sets.
        for leaked in ["DARCBENCH_HOME=", "PHP_INI_SCAN_DIR=", "NODE_OPTIONS="] {
            assert!(
                !output.stdout.contains(leaked),
                "`{leaked}` reached the child; the environment is cleared, not filtered"
            );
        }
    }

    /// A child that will not finish is killed, not waited on forever.
    #[test]
    fn a_child_that_hangs_is_killed_rather_than_waited_on() {
        let (found, _) = discover(&["/bin/sh", "/usr/bin/sh"]);
        let Some(shell) = found else { return };

        let started = Instant::now();
        let error = run(&shell, &["-c", "sleep 30"], Duration::from_millis(300))
            .expect_err("a hanging child must not be waited on");
        assert!(matches!(error, ExecError::Timeout { .. }), "{error:?}");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the timeout must actually bound the wait, took {:?}",
            started.elapsed()
        );
    }

    /// The classic bug in this shape of code: piped output nobody reads fills
    /// the pipe buffer, the child blocks in `write`, and a working program is
    /// reported as a timeout.
    #[test]
    fn a_child_that_writes_more_than_a_pipe_buffer_is_not_reported_as_a_timeout() {
        let (found, _) = discover(&["/bin/sh", "/usr/bin/sh"]);
        let Some(shell) = found else { return };

        // 200 KiB, comfortably past the 64 KiB pipe buffer, on each stream in
        // turn - the two have independent buffers, so either alone is enough to
        // deadlock a reader-less parent.
        for stream in ["1", "2"] {
            let script = format!(
                "i=0; while [ $i -lt 200 ]; do printf '%1024s' '' >&{stream}; i=$((i+1)); done"
            );
            let output = run(&shell, &["-c", &script], Duration::from_secs(10))
                .unwrap_or_else(|error| panic!("stream {stream} deadlocked: {error}"));
            assert!(output.succeeded(), "stream {stream}: {output:?}");
            let produced = if stream == "1" {
                output.stdout.len()
            } else {
                output.stderr.len()
            };
            assert!(
                produced >= 200 * 1024,
                "stream {stream} produced only {produced} bytes"
            );
        }
    }

    /// A pipe reaches EOF when every write end closes, including one a
    /// grandchild inherited - so "the child exited" does not bound the read.
    #[test]
    fn an_orphan_holding_the_pipe_cannot_hold_the_agent() {
        let (found, _) = discover(&["/bin/sh", "/usr/bin/sh"]);
        let Some(shell) = found else { return };

        let started = Instant::now();
        // The shell exits immediately; the backgrounded sleep inherits stdout
        // and keeps the write end open for ten seconds.
        let output = run(
            &shell,
            &["-c", "sleep 10 & echo done"],
            Duration::from_secs(2),
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "an orphan held the agent for {:?}",
            started.elapsed()
        );
        // Whatever it returns, it must return promptly. The output may be lost,
        // which is the accepted trade.
        let _ = output;
    }

    /// The rejection list is a security advisory, so it must not stop at the
    /// first usable interpreter.
    #[test]
    fn an_unsafe_candidate_after_a_safe_one_is_still_reported() {
        let dir = std::env::temp_dir().join(format!(
            "darcbench-exec-scan-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let planted = dir.join("php");
        std::fs::write(&planted, b"x").unwrap();

        let (found, rejections) = discover(&["/bin/sh", planted.to_str().unwrap()]);
        if found.is_none() {
            // No /bin/sh: the ordering this test is about does not arise.
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        assert_eq!(
            rejections.len(),
            1,
            "a writable candidate after the chosen one must still be reported: {rejections:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The script is what the interpreter is told to run, so the write must be
    /// as careful as the interpreter check.
    ///
    /// This test exists because the fix for it was written, described in a
    /// commit message, and silently not applied - a string replacement missed
    /// and nothing failed. A property that only a commit message asserts is not
    /// a property.
    #[test]
    fn the_script_refuses_a_planted_symlink_and_is_never_briefly_readable() {
        let dir = std::env::temp_dir().join(format!(
            "darcbench-script-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        // Owned by us and writable only by us, which is what a real scratch
        // directory looks like.
        std::fs::set_permissions(
            &dir,
            <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o700),
        )
        .unwrap();

        // The mode must be right the moment the file exists, not after a chmod.
        let script = ScriptFile::write(&dir, "probe.txt", "content").unwrap();
        let mode = std::fs::metadata(&script.path).unwrap().mode() & 0o777;
        assert_eq!(mode, 0o600, "created mode was {mode:o}");
        drop(script);
        assert!(!dir.join("probe.txt").exists());

        // A symlink planted at the path must be refused, never followed. Before
        // the fix this wrote *through* the link and clobbered the target.
        let victim = dir.join("victim");
        std::fs::write(&victim, b"do not touch").unwrap();
        std::os::unix::fs::symlink(&victim, dir.join("probe.txt")).unwrap();
        let refused = ScriptFile::write(&dir, "probe.txt", "content");
        // Either the symlink was removed and a fresh file created, or the open
        // was refused - both are safe. What must never happen is the victim
        // being overwritten.
        assert_eq!(
            std::fs::read(&victim).unwrap(),
            b"do not touch",
            "the write followed a symlink and clobbered another file"
        );
        drop(refused);

        // A scratch directory anyone can write is refused outright: whoever can
        // write it chooses what the interpreter executes.
        let shared = dir.join("shared");
        std::fs::create_dir_all(&shared).unwrap();
        std::fs::set_permissions(
            &shared,
            <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o777),
        )
        .unwrap();
        let error = ScriptFile::write(&shared, "probe.txt", "content")
            .expect_err("a world-writable scratch directory must be refused");
        let refusal = error.to_string();
        assert!(
            refusal.contains("writable") && refusal.contains("T-EXEC"),
            "the refusal must say why and point at the reasoning: {refusal}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_nonzero_exit_is_reported_rather_than_treated_as_success() {
        let (found, _) = discover(&["/bin/sh", "/usr/bin/sh"]);
        let Some(shell) = found else { return };
        let output = run(&shell, &["-c", "exit 3"], Duration::from_secs(5)).unwrap();
        assert!(!output.succeeded());
        assert_eq!(output.status, Some(3));
    }
}
