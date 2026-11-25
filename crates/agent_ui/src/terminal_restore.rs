//! CPZed: helpers for restoring terminal threads after a restart.
//!
//! A terminal thread started with `claude attach <id>` is stale when that
//! background session no longer exists; restoring it must not silently list a
//! dead thread as if it were live.

use std::process::{Command, Stdio};

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

/// Whether `target` (an id or a name, whole or the start/part of a name, as
/// `claude attach` accepts) is in the output of `claude agents --json`.
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
                    value == target || (*key == "name" && value.contains(&target))
                })
        })
    }))
}

/// Runs `claude agents --json` through `shell` (a login shell, so the app's
/// minimal GUI PATH still finds `claude`) and returns its stdout, or `None`
/// if it could not be run or exited non-zero.
pub fn agents_json(shell: &str, login: bool) -> Option<String> {
    let mut command = Command::new(shell);
    if login {
        command.arg("-l");
    }
    let output = command
        .args(["-c", "claude agents --json"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

/// `Some(true)` when the command's target is gone. Blocking; call from a
/// background thread.
pub fn command_is_stale(command: &str, shell: &str, login: bool) -> bool {
    let Some(target) = attach_target(command) else {
        return false;
    };
    agents_json(shell, login)
        .and_then(|json| target_is_alive(&json, target))
        .is_some_and(|alive| !alive)
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
        assert_eq!(target_is_alive(AGENTS, "reviewer"), Some(true));
        assert_eq!(target_is_alive(AGENTS, "deadbeef"), Some(false));
        assert_eq!(target_is_alive("not json", "x"), None);
    }

    /// Runs a fake `claude` placed on a temp PATH; touches no real HOME.
    #[test]
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
    fn non_attach_commands_are_never_stale() {
        assert!(!command_is_stale("echo hi", "/bin/sh", false));
    }
}
