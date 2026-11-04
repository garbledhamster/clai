use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::{Child, Command};
use tokio::task::JoinHandle;
use tokio::time::{timeout, Duration};

use super::SandboxProfile;

/// Grace period for reaping a child after it is SIGKILLed on timeout.
///
/// SIGKILL cannot reap a process stuck in uninterruptible sleep (D state,
/// e.g. blocked on a shared `target/` build-dir lock or NFS) until it leaves
/// that state, so an unbounded post-kill `wait()` could hang the executor
/// forever. Bounding it means a wedged child can't take the whole tool call —
/// and, via the shared session, sibling sessions — down with it; the OS still
/// reaps the process once it unblocks. The same bound guards the stdout/stderr
/// reader joins so a grandchild holding a pipe fd open can't hang us either.
pub(crate) const POST_KILL_REAP_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone)]
pub struct SandboxCommand {
    pub argv: Vec<OsString>,
    pub cwd: PathBuf,
    pub timeout_ms: u64,
    pub max_output_chars: usize,
    // Consumed by the Unix sandbox backends; unused on platforms without a
    // sandbox (Windows runs the command unsandboxed via `unsupported.rs`).
    #[allow(dead_code)]
    pub profile: SandboxProfile,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxCommandOutput {
    pub timed_out: bool,
    pub cwd: PathBuf,
    pub exit_code: Option<i32>,
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

pub async fn run_command(command: SandboxCommand) -> Result<SandboxCommandOutput, String> {
    if command.argv.is_empty() {
        return Err("Sandbox command argv cannot be empty".to_string());
    }
    if !command.cwd.is_dir() {
        return Err(format!(
            "Shell cwd does not exist or is not a directory: {}",
            command.cwd.display()
        ));
    }

    #[cfg(target_os = "linux")]
    {
        super::linux_bwrap::run(command).await
    }

    #[cfg(target_os = "macos")]
    {
        super::macos_seatbelt::run(command).await
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        super::unsupported::run(command).await
    }
}

pub(crate) async fn run_spawned_child(
    mut child: Child,
    cwd: PathBuf,
    timeout_ms: u64,
    output_limit: usize,
    timeout_label: &str,
) -> Result<SandboxCommandOutput, String> {
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "Failed to capture command stdout".to_string())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "Failed to capture command stderr".to_string())?;

    // Keep draining after the retained prefix fills. A noisy child must never
    // grow our memory without bound or block on a full stdout/stderr pipe.
    let byte_limit = output_limit.saturating_mul(4);
    let stdout_capture = Arc::new(Mutex::new(Vec::new()));
    let stderr_capture = Arc::new(Mutex::new(Vec::new()));
    let mut stdout_task = tokio::spawn(read_bounded(stdout, byte_limit, stdout_capture.clone()));
    let mut stderr_task = tokio::spawn(read_bounded(stderr, byte_limit, stderr_capture.clone()));
    let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms);

    // Capture the pid up front: on timeout we signal the child's whole process
    // group (it was spawned as a group leader, see `prepare_stdio`), and
    // `child.id()` returns `None` once the child has been awaited/reaped.
    let child_pid = child.id();

    let status = match tokio::time::timeout_at(deadline, child.wait()).await {
        Ok(result) => result.map_err(|e| format!("Shell command failed: {}", e))?,
        Err(_) => {
            // Timed out. Kill the entire process tree so descendants
            // (bwrap/sandbox-exec -> sh -> cargo -> rustc, … or, on Windows,
            // bash -> node/cargo) die too — a plain `child.kill()` only reaps
            // the direct child and would leave the grandchildren orphaned and
            // alive. On Unix this SIGKILLs the child's process group; on
            // Windows it `taskkill /T`s the tree from the still-live root pid.
            // Then bound every post-kill await so a D-state child or a
            // grandchild holding a pipe open can't wedge the executor (see
            // POST_KILL_REAP_TIMEOUT).
            kill_process_tree(child_pid);
            // Fallback: also signal the direct child, for the unknown-pid case
            // (and any platform without a tree-kill) where the call above is a
            // no-op, and as a fast backstop while an async taskkill runs.
            let _ = child.start_kill();
            let _ = timeout(POST_KILL_REAP_TIMEOUT, child.wait()).await;
            drain_or_abort(stdout_task).await;
            drain_or_abort(stderr_task).await;
            return Ok(timeout_output(
                cwd,
                &stdout_capture,
                &stderr_capture,
                output_limit,
                format!("{timeout_label} timed out after {timeout_ms} ms"),
            ));
        }
    };

    // Descendants may hold pipes open after the immediate child exits. The
    // same deadline covers output draining, not just the direct child's wait.
    let output = tokio::time::timeout_at(deadline, async {
        (&mut stdout_task)
            .await
            .map_err(|e| format!("Failed to collect stdout: {e}"))?
            .map_err(|e| format!("Failed to read stdout: {e}"))?;
        (&mut stderr_task)
            .await
            .map_err(|e| format!("Failed to collect stderr: {e}"))?
            .map_err(|e| format!("Failed to read stderr: {e}"))?;
        Ok::<_, String>(())
    })
    .await;
    match output {
        Ok(output) => output?,
        Err(_) => {
            kill_process_tree(child_pid);
            stdout_task.abort();
            stderr_task.abort();
            return Ok(timeout_output(
                cwd,
                &stdout_capture,
                &stderr_capture,
                output_limit,
                format!("{timeout_label} timed out after {timeout_ms} ms while draining output"),
            ));
        }
    };
    let stdout_bytes = stdout_capture
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let stderr_bytes = stderr_capture
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();

    #[cfg_attr(not(unix), allow(unused_mut))]
    let mut stderr = truncate_string(
        String::from_utf8_lossy(&stderr_bytes).into_owned(),
        output_limit,
    );
    // A signal death has no exit code; without this the caller sees only
    // `exitCode: null` and empty output (e.g. /bin/sh aborting under seatbelt).
    #[cfg(unix)]
    if let Some(signal) = std::os::unix::process::ExitStatusExt::signal(&status) {
        if !stderr.is_empty() && !stderr.ends_with('\n') {
            stderr.push('\n');
        }
        stderr.push_str(&format!("process terminated by signal {signal}"));
    }

    Ok(SandboxCommandOutput {
        timed_out: false,
        cwd,
        exit_code: status.code(),
        success: status.success(),
        stdout: truncate_string(
            String::from_utf8_lossy(&stdout_bytes).into_owned(),
            output_limit,
        ),
        stderr,
    })
}

fn timeout_output(
    cwd: PathBuf,
    stdout: &Mutex<Vec<u8>>,
    stderr: &Mutex<Vec<u8>>,
    limit: usize,
    message: String,
) -> SandboxCommandOutput {
    let stdout = stdout.lock().unwrap_or_else(|e| e.into_inner());
    let stderr = stderr.lock().unwrap_or_else(|e| e.into_inner());
    SandboxCommandOutput {
        timed_out: true,
        cwd,
        exit_code: None,
        success: false,
        stdout: truncate_string(String::from_utf8_lossy(&stdout).into_owned(), limit),
        stderr: format!(
            "{}\n{message}",
            truncate_string(String::from_utf8_lossy(&stderr).into_owned(), limit)
        ),
    }
}

async fn read_bounded(
    mut reader: impl AsyncRead + Unpin,
    limit: usize,
    retained: Arc<Mutex<Vec<u8>>>,
) -> std::io::Result<()> {
    let mut chunk = [0u8; 8192];
    loop {
        let count = reader.read(&mut chunk).await?;
        if count == 0 {
            return Ok(());
        }
        let mut retained = retained.lock().unwrap_or_else(|e| e.into_inner());
        let keep = count.min(limit.saturating_sub(retained.len()));
        retained.extend_from_slice(&chunk[..keep]);
    }
}

/// Force-kill the child and its entire descendant tree.
///
/// Unix: the child is spawned as a process-group leader (`prepare_stdio` calls
/// `process_group(0)`), so its pgid equals its pid; SIGKILLing the negated pid
/// reaches every descendant that has not started its own group.
///
/// Windows: there is no Unix-style process group, so we `taskkill /T` the tree
/// rooted at the child pid (see [`kill_process_tree_windows`]).
///
/// A no-op when the pid is unknown (already reaped) — the caller's
/// `start_kill()` is then the only available reap — and on platforms with
/// neither mechanism.
#[cfg(unix)]
pub(crate) fn kill_process_tree(pid: Option<u32>) {
    let Some(pid) = pid else { return };
    // SAFETY: `kill(2)` with a negative pid targets a process group and has no
    // memory effects. A stale/already-dead group yields ESRCH, which we ignore.
    unsafe {
        libc::kill(-(pid as i32), libc::SIGKILL);
    }
}

#[cfg(windows)]
pub(crate) fn kill_process_tree(pid: Option<u32>) {
    let Some(pid) = pid else { return };
    kill_process_tree_windows(pid);
}

#[cfg(all(not(unix), not(windows)))]
pub(crate) fn kill_process_tree(_pid: Option<u32>) {}

/// Windows tree-kill via the built-in `taskkill` utility.
///
/// `taskkill /T` walks the parent->child relationships from `pid` and force
/// (`/F`) terminates the root and every descendant — the closest Windows
/// analog of a process-group SIGKILL. We call it *before* the caller's
/// `start_kill()` while the root is still alive, so the tree is intact and
/// fully reachable. Best-effort and detached (no `wait`): a slow `taskkill`
/// cannot wedge the timeout path, since the bounded post-kill wait in
/// `run_spawned_child` still applies. `taskkill.exe` ships with Windows and a
/// process may kill its own descendants without elevation.
#[cfg(windows)]
fn kill_process_tree_windows(pid: u32) {
    use crate::windows_console::HideConsoleWindow;
    let _ = std::process::Command::new("taskkill")
        .args(windows_taskkill_args(pid))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        // taskkill is a console app: unhidden it would flash a console
        // window every time a timed-out bash_exec child is killed.
        .hide_console_window()
        .spawn();
}

/// Build the `taskkill` argv that force-kills a pid and its whole tree.
/// Factored out so the argument construction is unit-testable on any host.
#[cfg(any(windows, test))]
fn windows_taskkill_args(pid: u32) -> [String; 4] {
    [
        "/F".to_string(),
        "/T".to_string(),
        "/PID".to_string(),
        pid.to_string(),
    ]
}

/// Await a reader task, but give up and abort it after `POST_KILL_REAP_TIMEOUT`
/// so a pipe fd held open by a surviving grandchild can't hang the timeout
/// path forever.
async fn drain_or_abort(mut task: JoinHandle<std::io::Result<()>>) {
    if timeout(POST_KILL_REAP_TIMEOUT, &mut task).await.is_err() {
        task.abort();
    }
}

pub(crate) fn prepare_stdio(command: &mut Command) {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Run the child as its own process-group leader (pgid == child pid) so a
    // timeout can SIGKILL the whole tree — sandbox launcher, shell, and any
    // build/test grandchildren — rather than only the direct child. See
    // `kill_process_tree`. Windows has no equivalent spawn-time flag; there the
    // tree is reaped at timeout via `taskkill /T` instead.
    #[cfg(unix)]
    command.process_group(0);
}

fn truncate_string(text: String, limit: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= limit {
        return text;
    }
    chars[..limit].iter().collect::<String>() + "\n...[truncated]"
}

#[cfg(test)]
mod kill_arg_tests {
    use super::windows_taskkill_args;

    /// The Windows tree-kill must force-kill (`/F`) the whole tree (`/T`) of
    /// the given pid. Guards the argv the timeout path hands to `taskkill`.
    #[test]
    fn taskkill_args_force_kill_the_pid_tree() {
        assert_eq!(windows_taskkill_args(4321), ["/F", "/T", "/PID", "4321"]);
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use tokio::process::Command;

    /// Regression guard for the orphaned-process-tree bug: on timeout we must
    /// kill the child's whole process group, not just the direct child. The
    /// script backgrounds a grandchild that touches a marker after a delay,
    /// then blocks. With only a direct-child kill the backgrounded grandchild
    /// survives and creates the marker; with the process-group kill it dies
    /// with its parent and the marker never appears.
    #[tokio::test]
    async fn timeout_kills_whole_process_group_not_just_direct_child() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("grandchild-ran");
        let script = format!(
            "( sleep 3; touch '{}' ) & sleep 30",
            marker.to_str().unwrap()
        );

        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c").arg(&script);
        prepare_stdio(&mut cmd);
        let child = cmd.spawn().expect("spawn /bin/sh");

        let result =
            run_spawned_child(child, dir.path().to_path_buf(), 300, 1_000, "Test command").await;

        let output = result.expect("timeout retains bounded captured output");
        assert!(output.timed_out);
        assert!(output.stderr.contains("timed out"));

        // Wait past the grandchild's 3s delay; if the group kill worked it was
        // reaped before it could run.
        tokio::time::sleep(Duration::from_secs(5)).await;
        assert!(
            !marker.exists(),
            "grandchild survived the timeout — its process group was not reaped"
        );
    }

    /// The success path still returns the child's output unchanged.
    #[tokio::test]
    async fn returns_output_when_command_completes_within_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c").arg("printf out; printf err 1>&2; exit 7");
        prepare_stdio(&mut cmd);
        let child = cmd.spawn().expect("spawn /bin/sh");

        let out = run_spawned_child(
            child,
            dir.path().to_path_buf(),
            5_000,
            1_000,
            "Test command",
        )
        .await
        .expect("command should complete");

        assert_eq!(out.stdout, "out");
        assert_eq!(out.stderr, "err");
        assert_eq!(out.exit_code, Some(7));
        assert!(!out.success);
    }

    /// A signal-killed child has no exit code; the signal must surface in
    /// stderr instead of an empty, unexplained failure.
    #[tokio::test]
    async fn reports_signal_when_child_is_killed() {
        let dir = tempfile::tempdir().unwrap();
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c").arg("kill -TERM $$");
        prepare_stdio(&mut cmd);
        let child = cmd.spawn().expect("spawn /bin/sh");

        let out = run_spawned_child(
            child,
            dir.path().to_path_buf(),
            5_000,
            1_000,
            "Test command",
        )
        .await
        .expect("command should complete");

        assert_eq!(out.exit_code, None);
        assert!(!out.success);
        assert_eq!(out.stderr, "process terminated by signal 15");
    }

    /// The signal line must survive stderr truncation: it is the only
    /// explanation for a missing exit code.
    #[tokio::test]
    async fn signal_line_survives_stderr_truncation() {
        let dir = tempfile::tempdir().unwrap();
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c").arg(
            "i=0; while [ $i -lt 50 ]; do printf xxxxxxxxxx 1>&2; i=$((i+1)); done; kill -TERM $$",
        );
        prepare_stdio(&mut cmd);
        let child = cmd.spawn().expect("spawn /bin/sh");

        let out = run_spawned_child(child, dir.path().to_path_buf(), 5_000, 100, "Test command")
            .await
            .expect("command should complete");

        assert!(out.stderr.contains("...[truncated]"), "{}", out.stderr);
        assert!(
            out.stderr.ends_with("process terminated by signal 15"),
            "{}",
            out.stderr
        );
    }
}

#[cfg(all(test, unix))]
mod scheduled_process_tests {
    use super::*;

    #[tokio::test]
    async fn noisy_process_is_drained_but_only_a_bounded_prefix_is_retained() {
        let dir = tempfile::tempdir().unwrap();
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "i=0; while [ $i -lt 20000 ]; do printf '1234567890'; printf 'abcdefghij' >&2; i=$((i+1)); done"]);
        prepare_stdio(&mut command);
        let output = run_spawned_child(
            command.spawn().unwrap(),
            dir.path().into(),
            10_000,
            100,
            "Noisy condition",
        )
        .await
        .unwrap();
        assert!(!output.timed_out);
        assert_eq!(output.exit_code, Some(0));
        assert!(output.stdout.starts_with("1234567890"));
        assert!(output.stderr.starts_with("abcdefghij"));
        assert!(output.stdout.len() < 150 && output.stderr.len() < 150);
        let capture = Arc::new(Mutex::new(Vec::new()));
        read_bounded(&vec![b'x'; 1_000_000][..], 400, capture.clone())
            .await
            .unwrap();
        assert_eq!(capture.lock().unwrap().len(), 400);
    }

    #[tokio::test]
    async fn timeout_preserves_partial_output_and_covers_a_descendant_holding_pipes() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("escaped");
        let mut command = Command::new("/bin/sh");
        command.arg("-c").arg(format!(
            "printf 'partial output'; printf 'partial error' >&2; (sleep 2; touch '{}') & exit 0",
            marker.display()
        ));
        prepare_stdio(&mut command);
        let output = run_spawned_child(
            command.spawn().unwrap(),
            dir.path().into(),
            100,
            100,
            "Condition",
        )
        .await
        .unwrap();
        assert!(output.timed_out);
        assert_eq!(output.stdout, "partial output");
        assert!(output.stderr.contains("partial error"));
        assert!(output.stderr.contains("timed out"));
        tokio::time::sleep(Duration::from_millis(2200)).await;
        assert!(
            !marker.exists(),
            "descendant survived timeout after direct parent exited"
        );
    }
}
