//! Bounded subprocess execution for the runtime's command-driven probes.
//!
//! `Command::output` waits forever, so a hung `tmux`, `ps`, `lsof`, or `git`
//! would hold whatever lock its caller owns (for tmux, the server's state
//! operation lock) and keep the blocking pool, and therefore server shutdown,
//! waiting. [`output_with_timeout`] kills and reaps a child that outlives its
//! deadline and reports the timeout as an error, so callers see a failed
//! command instead of hanging.
//!
//! Std has no wait-with-timeout and no portable non-blocking pipe reads, so
//! output is captured in unlinked temporary files rather than pipes: the child
//! can never block on a full pipe, and the calling thread only has to poll
//! `try_wait` with a short proportional backoff. No helper thread is spawned
//! per call and no unsafe code is needed.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom};
use std::os::unix::fs::OpenOptionsExt;
use std::process::{Child, Command, Output, Stdio};
use std::sync::Once;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Default bound for tmux commands. They normally finish in milliseconds; a
/// few seconds tolerates a loaded machine without letting a wedged tmux
/// server stall the control plane indefinitely.
pub const TMUX_COMMAND_TIMEOUT: Duration = Duration::from_secs(5);
/// Default bound for process-table probes (`ps`, `kill`).
pub const PROCESS_PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Shortest `try_wait` pause, so a millisecond-scale tmux command is not
/// noticeably delayed.
const MIN_POLL: Duration = Duration::from_micros(250);
/// Longest `try_wait` pause, reached by long-running commands.
const MAX_POLL: Duration = Duration::from_millis(20);

static NEXT_CAPTURE: AtomicU64 = AtomicU64::new(0);
static SWEPT_LEAKED_CAPTURES: Once = Once::new();

const CAPTURE_PREFIX: &str = ".opensessions-cmd-";
/// Capture files are unlinked microseconds after creation, so one older than
/// this was leaked by a process killed inside that window.
const LEAKED_CAPTURE_AGE: Duration = Duration::from_secs(60);

/// Runs `command` like [`Command::output`] (stdin is null), but kills and
/// reaps the child if it has not exited within `timeout` and returns an
/// [`io::ErrorKind::TimedOut`] error.
pub fn output_with_timeout(command: &mut Command, timeout: Duration) -> io::Result<Output> {
    let (Ok(mut stdout), Ok(mut stderr)) = (capture_file(), capture_file()) else {
        // Without a writable temp dir, fall back to an unbounded run rather
        // than failing every command.
        return command.stdin(Stdio::null()).output();
    };
    let mut child = command
        .stdin(Stdio::null())
        .stdout(stdout.try_clone()?)
        .stderr(stderr.try_clone()?)
        .spawn()?;

    let status = match wait_with_deadline(&mut child, Instant::now() + timeout) {
        Ok(Some(status)) => status,
        Ok(None) => {
            kill_and_reap(&mut child);
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("command timed out after {}ms", timeout.as_millis()),
            ));
        }
        Err(err) => {
            kill_and_reap(&mut child);
            return Err(err);
        }
    };

    Ok(Output {
        status,
        stdout: read_capture(&mut stdout)?,
        stderr: read_capture(&mut stderr)?,
    })
}

fn wait_with_deadline(
    child: &mut Child,
    deadline: Instant,
) -> io::Result<Option<std::process::ExitStatus>> {
    let started = Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Some(status));
        }
        let now = Instant::now();
        if now >= deadline {
            return Ok(None);
        }
        // Back off in proportion to the time already waited, so noticing the
        // exit late adds at most ~1/8 to a command's runtime.
        let pause = ((now - started) / 8).clamp(MIN_POLL, MAX_POLL);
        thread::sleep(pause.min(deadline - now));
    }
}

fn kill_and_reap(child: &mut Child) {
    let _ = child.kill();
    // Reap so a timed-out child never lingers as a zombie.
    let _ = child.wait();
}

/// An anonymous, owner-only temp file: created exclusively, then unlinked
/// immediately so nothing is left behind once the command finishes.
fn capture_file() -> io::Result<File> {
    SWEPT_LEAKED_CAPTURES.call_once(sweep_leaked_captures);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.subsec_nanos())
        .unwrap_or_default();
    let path = std::env::temp_dir().join(format!(
        "{CAPTURE_PREFIX}{}-{}-{nanos}",
        std::process::id(),
        NEXT_CAPTURE.fetch_add(1, Ordering::Relaxed)
    ));
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)?;
    std::fs::remove_file(&path)?;
    Ok(file)
}

/// Removes capture files leaked by processes killed between creating and
/// unlinking one. Runs once per process.
fn sweep_leaked_captures() {
    let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else {
        return;
    };
    for entry in entries.flatten() {
        let leaked = entry
            .file_name()
            .to_string_lossy()
            .starts_with(CAPTURE_PREFIX)
            && entry
                .metadata()
                .and_then(|metadata| metadata.modified())
                .ok()
                .and_then(|modified| modified.elapsed().ok())
                .is_some_and(|age| age > LEAKED_CAPTURE_AGE);
        if leaked {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

fn read_capture(file: &mut File) -> io::Result<Vec<u8>> {
    file.seek(SeekFrom::Start(0))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(script: &str) -> Command {
        let mut command = Command::new("sh");
        command.args(["-c", script]);
        command
    }

    #[test]
    fn captures_output_and_exit_status_of_a_normal_command() {
        let output = output_with_timeout(
            &mut sh("printf out; printf err >&2; exit 3"),
            Duration::from_secs(5),
        )
        .unwrap();

        assert_eq!(output.status.code(), Some(3));
        assert_eq!(output.stdout, b"out");
        assert_eq!(output.stderr, b"err");
    }

    #[test]
    fn output_larger_than_a_pipe_buffer_does_not_stall_the_child() {
        let output =
            output_with_timeout(&mut sh("head -c 1048576 /dev/zero"), Duration::from_secs(5))
                .unwrap();

        assert!(output.status.success());
        assert_eq!(output.stdout.len(), 1_048_576);
    }

    #[test]
    fn leaked_capture_files_are_swept_but_in_flight_ones_are_kept() {
        let path = |tag: &str| {
            std::env::temp_dir().join(format!(
                "{CAPTURE_PREFIX}sweep-{tag}-{}-{}",
                std::process::id(),
                NEXT_CAPTURE.fetch_add(1, Ordering::Relaxed)
            ))
        };
        let (leaked, in_flight) = (path("leaked"), path("in-flight"));
        File::create(&leaked)
            .unwrap()
            .set_modified(SystemTime::now() - Duration::from_secs(3600))
            .unwrap();
        File::create(&in_flight).unwrap();

        sweep_leaked_captures();

        assert!(!leaked.exists());
        assert!(in_flight.exists());
        let _ = std::fs::remove_file(&in_flight);
    }

    #[test]
    fn a_command_outliving_its_timeout_is_killed_and_reported() {
        let pid_file = std::env::temp_dir().join(format!(
            "opensessions-timeout-pid-{}-{}",
            std::process::id(),
            NEXT_CAPTURE.fetch_add(1, Ordering::Relaxed)
        ));
        let script = format!("echo $$ > '{}'; exec sleep 30", pid_file.display());
        let started = Instant::now();

        let err = output_with_timeout(&mut sh(&script), Duration::from_millis(300)).unwrap_err();

        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
        let pid = std::fs::read_to_string(&pid_file).unwrap();
        let _ = std::fs::remove_file(&pid_file);
        let alive = Command::new("kill")
            .args(["-0", pid.trim()])
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success();
        assert!(
            !alive,
            "timed-out child {} was not killed and reaped",
            pid.trim()
        );
    }
}
