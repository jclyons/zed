//! CPZed: helpers for restoring terminal threads after a restart.
//!
//! A terminal thread started with `claude attach <id>` is stale when that
//! background session no longer exists; restoring it must not silently list a
//! dead thread as if it were live.

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// The target of a `claude attach <target>` command, if `command` is one.
pub fn attach_target(command: &str) -> Option<&str> {
    let mut words = command.split_whitespace();
    let program = words.next()?;
    let program = program.rsplit('/').next().unwrap_or(program);
    if program != "claude" || words.next()? != "attach" {
        return None;
    }
    let target = words.next()?;
    (words.next().is_none()).then_some(target.trim_matches(|c| c == '"' || c == '\''))
}

/// Whether `target` names a live session in the output of
/// `claude agents --json`: an exact (case-insensitive) match on `id`,
/// `sessionId` or `name`, or, for a target of 6+ characters, an id prefix.
/// `None` when the output is not the expected JSON, so unknown is never
/// reported as stale.
pub fn target_is_alive(agents_json: &str, target: &str) -> Option<bool> {
    let agents: serde_json::Value = serde_json::from_str(agents_json).ok()?;
    let agents = agents.as_array()?;
    let target = target.to_lowercase();
    Some(agents.iter().any(|agent| {
        ["id", "sessionId", "name"].iter().any(|key| {
            agent
                .get(key)
                .and_then(|value| value.as_str())
                .is_some_and(|value| {
                    let value = value.to_lowercase();
                    value == target
                        || (*key != "name" && target.len() >= 6 && value.starts_with(&target))
                })
        })
    }))
}

/// Runs `claude agents --json` through `shell` (a login shell, so the app's
/// minimal GUI PATH still finds `claude`) and returns its stdout, or `None`
/// if it could not be run or exited non-zero.
/// How long `claude agents --json` may take before the answer is "unknown".
const AGENTS_TIMEOUT: Duration = Duration::from_secs(5);

// Only ever called from a background thread (see `agents_json_fetcher`).
#[allow(clippy::disallowed_methods)]
pub fn agents_json(shell: &str, login: bool) -> Option<String> {
    agents_json_with_timeout(shell, login, AGENTS_TIMEOUT)
}

/// [`agents_json`] with an explicit timeout (the 5 s attention poll uses 3 s).
#[allow(clippy::disallowed_methods)]
pub fn agents_json_with_timeout(shell: &str, login: bool, timeout: Duration) -> Option<String> {
    run_with_timeout(agents_command(shell, login), timeout)
}

/// The exact command the poller runs. Split out so tests can run the REAL
/// argv against a fake `claude` (PATH and HOME set on the returned command).
#[allow(clippy::disallowed_methods)]
fn agents_command(shell: &str, login: bool) -> Command {
    let mut command = Command::new(shell);
    if login {
        command.arg("-l");
    }
    // `exec` so killing the shell on timeout kills claude itself.
    command.args(["-c", "exec claude agents --json"]);
    command
}

/// Runs `command`, returning its stdout if it exits 0 within `timeout`. On
/// timeout the child is killed and the answer is `None` (unknown, never stale).
#[allow(clippy::disallowed_methods)]
fn run_with_timeout(mut command: Command, timeout: Duration) -> Option<String> {
    use std::io::Read as _;
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut out = String::new();
        stdout.read_to_string(&mut out).ok().map(|_| out)
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(25)),
            _ => {
                child.kill().ok();
                child.wait().ok();
                break None;
            }
        }
    };
    // On timeout the reader is left to finish on its own: a grandchild might
    // still hold the pipe open and joining could hang.
    let status = status.filter(|status| status.success())?;
    reader.join().ok().flatten().filter(|_| status.success())
}

/// `true` when `command` is a `claude attach <target>` whose target is not in
/// `agents_json`. Unknown (no JSON, not an attach command) is never stale.
pub fn is_stale(command: &str, agents_json: Option<&str>) -> bool {
    let Some(target) = attach_target(command) else {
        return false;
    };
    agents_json
        .and_then(|json| target_is_alive(json, target))
        .is_some_and(|alive| !alive)
}

#[cfg(test)]
thread_local! {
    static TEST_AGENTS_JSON: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
}

/// Test seam: the canned `claude agents --json` output. Under `cfg(test)` the
/// fetcher NEVER runs a shell or the real claude: it returns this (default
/// `None`, i.e. unknown, so nothing is marked stale).
#[cfg(test)]
pub fn set_test_agents_json(json: Option<&str>) {
    TEST_AGENTS_JSON.with(|cell| *cell.borrow_mut() = json.map(str::to_string));
}

/// A closure that fetches `claude agents --json` once (run it on a background
/// thread). Capture it on the foreground: under test it carries the canned
/// value and never spawns a process.
pub fn agents_json_fetcher() -> impl FnOnce() -> Option<String> + Send + 'static {
    agents_json_fetcher_with_timeout(AGENTS_TIMEOUT)
}

/// [`agents_json_fetcher`] with an explicit timeout.
pub fn agents_json_fetcher_with_timeout(
    timeout: Duration,
) -> impl FnOnce() -> Option<String> + Send + 'static {
    #[cfg(test)]
    {
        let _ = timeout;
        let canned = TEST_AGENTS_JSON.with(|cell| cell.borrow().clone());
        move || canned
    }
    #[cfg(not(test))]
    {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
        move || agents_json_with_timeout(&shell, true, timeout)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const AGENTS: &str =
        r#"[{"id":"6fadf34c","sessionId":"6fadf34c-af80","name":"cpzed-reviewer"}]"#;

    #[test]
    fn parses_attach_commands_only() {
        assert_eq!(attach_target("claude attach 6fadf34c"), Some("6fadf34c"));
        assert_eq!(
            attach_target("/opt/homebrew/bin/claude attach abc"),
            Some("abc")
        );
        assert_eq!(attach_target("claude attach 'a b'"), None);
        assert_eq!(attach_target("claude --resume x"), None);
        assert_eq!(attach_target("ls attach 1"), None);
        assert_eq!(attach_target("claude attach"), None);
    }

    #[test]
    fn alive_matches_id_session_or_name() {
        assert_eq!(target_is_alive(AGENTS, "6fadf34c"), Some(true));
        assert_eq!(target_is_alive(AGENTS, "6fadf34c-af80"), Some(true));
        assert_eq!(target_is_alive(AGENTS, "cpzed-reviewer"), Some(true));
        // Names match exactly, not by substring; ids match by a long prefix.
        assert_eq!(target_is_alive(AGENTS, "reviewer"), Some(false));
        assert_eq!(target_is_alive(AGENTS, "c"), Some(false));
        assert_eq!(target_is_alive(AGENTS, "6fadf3"), Some(true));
        assert_eq!(target_is_alive(AGENTS, "6fad"), Some(false));
        assert_eq!(target_is_alive(AGENTS, "deadbeef"), Some(false));
        assert_eq!(target_is_alive("not json", "x"), None);
    }

    /// Runs a fake `claude` placed on a temp PATH; touches no real HOME.
    #[test]
    #[allow(clippy::disallowed_methods)] // test-only, fake claude in a temp dir
    fn stale_detection_with_fake_claude() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("claude");
        std::fs::write(&bin, format!("#!/bin/sh\necho '{AGENTS}'\n")).unwrap();
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        let path = format!("{}:/usr/bin:/bin", dir.path().display());
        // SAFETY-free approach: run the check with PATH set on the shell via env.
        let run = |command: &str| {
            let target = attach_target(command).unwrap();
            let output = Command::new("/bin/sh")
                .args(["-c", "claude agents --json"])
                .env("PATH", &path)
                .output()
                .unwrap();
            let json = String::from_utf8_lossy(&output.stdout).into_owned();
            !target_is_alive(&json, target).unwrap()
        };
        assert!(!run("claude attach 6fadf34c"));
        assert!(run("claude attach deadbeef"));
    }

    #[test]
    fn non_attach_and_unknown_are_never_stale() {
        assert!(!is_stale("echo hi", Some(AGENTS)));
        assert!(!is_stale("claude attach deadbeef", None));
        assert!(!is_stale("claude attach deadbeef", Some("not json")));
        assert!(is_stale("claude attach deadbeef", Some(AGENTS)));
        assert!(!is_stale("claude attach 6fadf34c", Some(AGENTS)));
    }

    #[test]
    fn test_fetcher_never_runs_a_shell() {
        assert_eq!(agents_json_fetcher()(), None);
        set_test_agents_json(Some("[]"));
        assert_eq!(agents_json_fetcher()().as_deref(), Some("[]"));
        set_test_agents_json(None);
    }

    /// A hung `claude` (fake, sleeps) is killed at the timeout and reported as
    /// unknown (`None`), so it can neither block startup nor mark anything stale.
    #[test]
    #[allow(clippy::disallowed_methods)]
    fn hung_claude_times_out_as_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("claude");
        std::fs::write(&bin, "#!/bin/sh\nexec sleep 30\n").unwrap();
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", "exec claude agents --json"])
            .env("PATH", format!("{}:/usr/bin:/bin", dir.path().display()));
        let started = Instant::now();
        assert_eq!(run_with_timeout(command, Duration::from_millis(300)), None);
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(!is_stale("claude attach 6fadf34c", None));
    }

    /// A fake `claude` in a temp dir (with a temp HOME), first on PATH. It
    /// rejects any argv other than `agents --json`, records its pid, then runs
    /// `body`. Returns the production command, plus the pid file.
    #[allow(clippy::disallowed_methods)]
    fn fake_claude(body: &str) -> (tempfile::TempDir, Command, std::path::PathBuf) {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let bin_dir = dir.path().join("bin");
        let home = dir.path().join("home");
        std::fs::create_dir_all(&bin_dir).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        let pid_file = dir.path().join("pid");
        let script = format!(
            "#!/bin/sh\n[ \"$1 $2\" = \"agents --json\" ] || exit 64\necho $$ > '{}'\n{body}\n",
            pid_file.display()
        );
        let bin = bin_dir.join("claude");
        std::fs::write(&bin, script).unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        // Not a login shell: that would source the real profile and PATH.
        let mut command = agents_command("/bin/sh", false);
        command
            .env_clear()
            .env("HOME", &home)
            .env("PATH", format!("{}:/usr/bin:/bin", bin_dir.display()));
        (dir, command, pid_file)
    }

    #[test]
    fn poller_command_returns_the_fake_claude_stdout() {
        let (_dir, command, pid_file) = fake_claude(r#"echo '[{"id":"aaaa1111","state":"blocked"}]'"#);
        let out = run_with_timeout(command, Duration::from_secs(5)).expect("stdout");
        assert_eq!(out.trim(), r#"[{"id":"aaaa1111","state":"blocked"}]"#);
        assert!(pid_file.exists(), "the fake claude ran, with argv `agents --json`");
    }

    #[test]
    fn poller_command_kills_a_hung_claude_at_the_timeout() {
        let (_dir, command, pid_file) = fake_claude("exec sleep 30");
        let started = Instant::now();
        // Long enough for a loaded machine to start the fake before the kill.
        assert_eq!(run_with_timeout(command, Duration::from_secs(2)), None);
        let elapsed = started.elapsed();
        assert!(elapsed >= Duration::from_secs(2), "{elapsed:?}");
        assert!(elapsed < Duration::from_secs(10), "{elapsed:?}");
        // The process is gone, not leaked.
        let wait = Instant::now();
        while !pid_file.exists() && wait.elapsed() < Duration::from_secs(3) {
            std::thread::sleep(Duration::from_millis(25));
        }
        let pid = std::fs::read_to_string(&pid_file).expect("the fake claude started");
        #[allow(clippy::disallowed_methods)]
        let alive = Command::new("kill")
            .args(["-0", pid.trim()])
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success();
        assert!(!alive, "hung claude {pid} was left running");
    }

    #[test]
    fn poller_command_nonzero_exit_is_unknown() {
        let (_dir, command, _) = fake_claude("echo '[]'; exit 3");
        assert_eq!(run_with_timeout(command, Duration::from_secs(5)), None);
    }

    #[test]
    fn poller_command_with_claude_not_found_is_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let mut command = agents_command("/bin/sh", false);
        command
            .env_clear()
            .env("HOME", dir.path())
            .env("PATH", dir.path()); // an empty dir: no `claude`
        assert_eq!(run_with_timeout(command, Duration::from_secs(5)), None);
    }

    /// Garbage on stdout is returned as-is (the poller's parser then reports
    /// "unknown"): it never marks anything stale or blocked.
    #[test]
    fn poller_command_garbage_output_parses_to_unknown() {
        let (_dir, command, _) = fake_claude("echo 'this is not json'");
        let out = run_with_timeout(command, Duration::from_secs(5)).expect("stdout");
        assert_eq!(out.trim(), "this is not json");
        assert_eq!(target_is_alive(&out, "aaaa1111"), None);
        assert!(!is_stale("claude attach aaaa1111", Some(&out)));
    }
}
