use std::path::PathBuf;

use anyhow::Result;
use collections::HashMap;
pub use ipc_channel::ipc;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
pub struct IpcHandshake {
    pub requests: ipc::IpcSender<CliRequest>,
    pub responses: ipc::IpcReceiver<CliResponse>,
}

/// Controls how CLI paths are opened — whether to reuse existing windows,
/// create new ones, or add to the sidebar.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum OpenBehavior {
    /// Consult the user's `cli_default_open_behavior` setting.
    #[default]
    Default,
    /// Always create a new window. No matching against existing worktrees.
    /// Corresponds to `zed -n`.
    AlwaysNew,
    /// Create a new window unless opening a subpath of an existing project.
    PreferNewWindow,
    /// Match broadly including subdirectories, and fall back to any existing
    /// window if no worktree matched. Corresponds to `zed -a`.
    Add,
    /// Open directories as a new workspace in the current Zed window's sidebar.
    /// Reuse existing windows for files in open worktrees.
    /// Corresponds to `zed -e`.
    ExistingWindow,
    /// New window for directories, reuse existing window for files in open
    /// worktrees. The classic pre-sidebar behavior.
    /// Corresponds to `zed --classic`.
    Classic,
    /// Replace the content of an existing window with a new workspace.
    /// Corresponds to `zed -r`.
    Reuse,
}

/// The setting-level enum for configuring default behavior. This only has
/// two values because the other modes are always explicitly requested via
/// CLI flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CliBehaviorSetting {
    /// Open directories as a new workspace in the current Zed window's sidebar.
    ExistingWindow,
    /// Open paths in a new window unless they are subpaths of an existing project.
    NewWindow,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum CliRequest {
    Open {
        paths: Vec<String>,
        urls: Vec<String>,
        diff_paths: Vec<[String; 2]>,
        diff_all: bool,
        wsl: Option<String>,
        wait: bool,
        #[serde(default)]
        open_behavior: OpenBehavior,
        env: Option<HashMap<String, String>>,
        user_data_dir: Option<String>,
        dev_container: bool,
        #[serde(default)]
        cwd: Option<PathBuf>,
    },
    SetOpenBehavior {
        behavior: CliBehaviorSetting,
    },
    /// Print a diagnostic snapshot of the app's GPUI and platform state.
    DebugState,
    /// Print the running Zed's agent threads as JSON (currently terminal
    /// threads only).
    ListAgentThreads,
    /// Create a new terminal agent thread in the running Zed. Appended last
    /// to keep the enum append-only.
    NewAgentTerminal {
        /// Absolute, canonical working directory (resolved by the CLI).
        cwd: PathBuf,
        /// Command typed into the shell after startup; `None` = settings' init command.
        command: Option<String>,
        /// Custom title shown in the sidebar.
        title: Option<String>,
        #[serde(default)]
        on_no_match: NoMatchBehavior,
    },
    /// Close one terminal thread (`zed --agent-close`), by id or by exact
    /// title; exactly one of the two is set (the CLI enforces it). Appended
    /// last to keep the enum append-only.
    CloseAgentThread {
        id: Option<String>,
        title: Option<String>,
    },
    /// Set a terminal thread's custom title (`zed --agent-rename`).
    RenameAgentThread {
        id: String,
        title: String,
    },
    /// `--agent-set-parent` / `--agent-set-section`: change the hierarchy of
    /// an existing thread. Appended last to keep the enum append-only.
    UpdateAgentThread {
        id: String,
        parent: Option<ParentChange>,
        section: Option<SectionChange>,
    },
    /// `--agent-new` with `--parent` / `--section`. A separate, appended
    /// variant (never new fields on `NewAgentTerminal`): bincode is
    /// positional, so an old app would silently drop trailing fields and
    /// succeed without placing the thread. The CLI sends `NewAgentTerminal`
    /// when neither flag is given, so plain `--agent-new` stays compatible.
    NewAgentTerminalIn {
        cwd: PathBuf,
        command: Option<String>,
        title: Option<String>,
        on_no_match: NoMatchBehavior,
        /// Nest the new thread under this thread id (`--parent`).
        parent: Option<String>,
        /// Put the new thread in this section of its own project, creating
        /// the section if missing (`--section`).
        section: Option<String>,
    },
    /// `--agent-new` with `--badge`. A separate, appended variant (index 9):
    /// the CLI sends it only when at least one badge is given, so plain
    /// `--agent-new` / `--parent` keep working against an older app.
    NewAgentTerminalBadged {
        cwd: PathBuf,
        command: Option<String>,
        title: Option<String>,
        on_no_match: NoMatchBehavior,
        parent: Option<String>,
        section: Option<String>,
        /// Already validated by [`validate_badges`] in the CLI and again in the app.
        badges: Vec<String>,
    },
    /// `--agent-set-badges <id> a,b`: replace a thread's badges (empty
    /// clears). Appended last (index 10).
    SetAgentBadges {
        id: String,
        badges: Vec<String>,
    },
}

/// Most badges one thread can carry.
pub const MAX_BADGES: usize = 4;
/// Longest badge text, in characters.
pub const MAX_BADGE_LEN: usize = 12;

/// Validates and normalizes a badge list: at most [`MAX_BADGES`] distinct
/// entries (repeats are dropped, order kept), each 1..=[`MAX_BADGE_LEN`]
/// characters of `[a-z0-9-]`. Shared by the CLI and the app so neither
/// trusts the other.
pub fn validate_badges(badges: &[String]) -> Result<Vec<String>, String> {
    let mut out: Vec<String> = Vec::new();
    for badge in badges {
        if badge.is_empty()
            || badge.chars().count() > MAX_BADGE_LEN
            || !badge
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        {
            return Err(format!(
                "invalid badge {badge:?}: use 1-{MAX_BADGE_LEN} characters of a-z, 0-9 and '-'"
            ));
        }
        if !out.contains(badge) {
            out.push(badge.clone());
        }
    }
    if out.len() > MAX_BADGES {
        return Err(format!("at most {MAX_BADGES} badges per thread"));
    }
    Ok(out)
}

/// A requested change of a thread's parent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ParentChange {
    /// Nest under the thread with this id.
    Set(String),
    /// Un-nest (`none`).
    Clear,
}

/// A requested change of a thread's section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SectionChange {
    /// Move into the section with this name (in the thread's own project).
    Set(String),
    /// Make the thread unsectioned (`none`).
    Clear,
}

/// What `--agent-new` does when no open project contains the requested cwd.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum NoMatchBehavior {
    /// Fail with a deterministic error (default).
    #[default]
    Error,
    /// Add the directory as a workspace (`--open-project`).
    OpenProject,
    /// Open it in a new window (`--new-window`).
    NewWindow,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum CliResponse {
    Ping,
    Stdout { message: String },
    Stderr { message: String },
    Exit { status: i32 },
    PromptOpenBehavior,
}

/// When Zed started not as an *.app but as a binary (e.g. local development),
/// there's a possibility to tell it to behave "regularly".
///
/// Note that in the main zed binary, this variable is unset after it's read for the first time,
/// therefore it should always be accessed through the `FORCE_CLI_MODE` static.
pub const FORCE_CLI_MODE_ENV_VAR_NAME: &str = "ZED_FORCE_CLI_MODE";

/// Abstracts the transport for sending CLI responses (Zed → CLI).
///
/// Production code uses `IpcSender<CliResponse>`. Tests can provide in-memory
/// implementations to avoid OS-level IPC.
pub trait CliResponseSink: Send + 'static {
    fn send(&self, response: CliResponse) -> Result<()>;
}

impl CliResponseSink for ipc::IpcSender<CliResponse> {
    fn send(&self, response: CliResponse) -> Result<()> {
        ipc::IpcSender::send(self, response).map_err(|error| anyhow::anyhow!("{error}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_agent_terminal_round_trips() {
        let request = CliRequest::NewAgentTerminal {
            cwd: PathBuf::from("/tmp/project"),
            command: Some("echo hi".into()),
            title: Some("t".into()),
            on_no_match: NoMatchBehavior::NewWindow,
        };
        let json = serde_json::to_string(&request).unwrap();
        match serde_json::from_str::<CliRequest>(&json).unwrap() {
            CliRequest::NewAgentTerminal {
                cwd,
                command,
                title,
                on_no_match,
            } => {
                assert_eq!(cwd, PathBuf::from("/tmp/project"));
                assert_eq!(command.as_deref(), Some("echo hi"));
                assert_eq!(title.as_deref(), Some("t"));
                assert_eq!(on_no_match, NoMatchBehavior::NewWindow);
            }
            _ => panic!("wrong variant"),
        }
        let missing: CliRequest = serde_json::from_str(
            r#"{"NewAgentTerminal":{"cwd":"/x","command":null,"title":null}}"#,
        )
        .unwrap();
        assert!(matches!(
            missing,
            CliRequest::NewAgentTerminal {
                on_no_match: NoMatchBehavior::Error,
                ..
            }
        ));
    }

    #[test]
    fn close_and_rename_requests_round_trip() {
        let close = CliRequest::CloseAgentThread {
            id: None,
            title: Some("worker (bg)".into()),
        };
        let json = serde_json::to_string(&close).unwrap();
        assert!(matches!(
            serde_json::from_str::<CliRequest>(&json).unwrap(),
            CliRequest::CloseAgentThread { id: None, title: Some(t) } if t == "worker (bg)"
        ));
        let rename = CliRequest::RenameAgentThread {
            id: "i".into(),
            title: "t".into(),
        };
        let json = serde_json::to_string(&rename).unwrap();
        assert!(matches!(
            serde_json::from_str::<CliRequest>(&json).unwrap(),
            CliRequest::RenameAgentThread { id, title } if id == "i" && title == "t"
        ));
    }

    // The wire format is ipc-channel's bincode 1 (positional): variants are
    // identified by their index, so existing ones must never move and new
    // ones are only ever appended.
    fn variant_index(request: &CliRequest) -> u32 {
        let bytes = bincode::serialize(request).unwrap();
        u32::from_le_bytes(bytes[..4].try_into().unwrap())
    }

    fn bincode_string(s: &str) -> Vec<u8> {
        let mut bytes = (s.len() as u64).to_le_bytes().to_vec();
        bytes.extend_from_slice(s.as_bytes());
        bytes
    }

    #[test]
    fn bincode_variant_indices_are_stable() {
        let open = CliRequest::Open {
            paths: vec![],
            urls: vec![],
            diff_paths: vec![],
            diff_all: false,
            wsl: None,
            wait: false,
            open_behavior: OpenBehavior::default(),
            env: None,
            user_data_dir: None,
            dev_container: false,
            cwd: None,
        };
        let indices = [
            variant_index(&open),
            variant_index(&CliRequest::SetOpenBehavior {
                behavior: CliBehaviorSetting::ExistingWindow,
            }),
            variant_index(&CliRequest::DebugState),
            variant_index(&CliRequest::ListAgentThreads),
            variant_index(&CliRequest::NewAgentTerminal {
                cwd: PathBuf::from("/"),
                command: None,
                title: None,
                on_no_match: NoMatchBehavior::Error,
            }),
            variant_index(&CliRequest::CloseAgentThread {
                id: None,
                title: None,
            }),
            variant_index(&CliRequest::RenameAgentThread {
                id: String::new(),
                title: String::new(),
            }),
        ];
        assert_eq!(indices, [0, 1, 2, 3, 4, 5, 6]);
    }

    #[test]
    fn bincode_golden_bytes_of_existing_variants() {
        assert_eq!(
            bincode::serialize(&CliRequest::DebugState).unwrap(),
            2u32.to_le_bytes()
        );
        assert_eq!(
            bincode::serialize(&CliRequest::ListAgentThreads).unwrap(),
            3u32.to_le_bytes()
        );
        let mut expected = 4u32.to_le_bytes().to_vec();
        expected.extend(bincode_string("/tmp/p"));
        expected.push(1); // command: Some
        expected.extend(bincode_string("echo hi"));
        expected.push(0); // title: None
        expected.extend(2u32.to_le_bytes()); // NoMatchBehavior::NewWindow
        let request = CliRequest::NewAgentTerminal {
            cwd: PathBuf::from("/tmp/p"),
            command: Some("echo hi".into()),
            title: None,
            on_no_match: NoMatchBehavior::NewWindow,
        };
        assert_eq!(bincode::serialize(&request).unwrap(), expected);
    }

    #[test]
    fn close_and_rename_golden_bytes_and_bincode_round_trip() {
        let close = CliRequest::CloseAgentThread {
            id: None,
            title: Some("w (bg)".into()),
        };
        let mut expected = 5u32.to_le_bytes().to_vec();
        expected.push(0); // id: None
        expected.push(1); // title: Some
        expected.extend(bincode_string("w (bg)"));
        let bytes = bincode::serialize(&close).unwrap();
        assert_eq!(bytes, expected);
        assert!(matches!(
            bincode::deserialize::<CliRequest>(&bytes).unwrap(),
            CliRequest::CloseAgentThread { id: None, title: Some(t) } if t == "w (bg)"
        ));

        let rename = CliRequest::RenameAgentThread {
            id: "i".into(),
            title: "t".into(),
        };
        let mut expected = 6u32.to_le_bytes().to_vec();
        expected.extend(bincode_string("i"));
        expected.extend(bincode_string("t"));
        let bytes = bincode::serialize(&rename).unwrap();
        assert_eq!(bytes, expected);
        assert!(matches!(
            bincode::deserialize::<CliRequest>(&bytes).unwrap(),
            CliRequest::RenameAgentThread { id, title } if id == "i" && title == "t"
        ));
    }

    /// BADGES' variants are appended after S2's (7, 8): 9 and 10, with golden
    /// bytes and a bincode round trip.
    #[test]
    fn badge_variants_are_appended_with_stable_wire_bytes() {
        let badged = CliRequest::NewAgentTerminalBadged {
            cwd: PathBuf::from("/x"),
            command: None,
            title: None,
            on_no_match: NoMatchBehavior::Error,
            parent: None,
            section: None,
            badges: vec!["bg".into(), "rc".into()],
        };
        let mut expected = 9u32.to_le_bytes().to_vec();
        expected.extend(bincode_string("/x"));
        expected.push(0); // command: None
        expected.push(0); // title: None
        expected.extend(0u32.to_le_bytes()); // NoMatchBehavior::Error
        expected.push(0); // parent: None
        expected.push(0); // section: None
        expected.extend(2u64.to_le_bytes()); // badges.len()
        expected.extend(bincode_string("bg"));
        expected.extend(bincode_string("rc"));
        let bytes = bincode::serialize(&badged).unwrap();
        assert_eq!(bytes, expected);
        assert!(matches!(
            bincode::deserialize::<CliRequest>(&bytes).unwrap(),
            CliRequest::NewAgentTerminalBadged { badges, .. } if badges == ["bg", "rc"]
        ));

        let set = CliRequest::SetAgentBadges {
            id: "i".into(),
            badges: vec![],
        };
        let mut expected = 10u32.to_le_bytes().to_vec();
        expected.extend(bincode_string("i"));
        expected.extend(0u64.to_le_bytes());
        let bytes = bincode::serialize(&set).unwrap();
        assert_eq!(bytes, expected);
        assert!(matches!(
            bincode::deserialize::<CliRequest>(&bytes).unwrap(),
            CliRequest::SetAgentBadges { id, badges } if id == "i" && badges.is_empty()
        ));
        // The earlier appended variants did not move.
        let update = CliRequest::UpdateAgentThread {
            id: String::new(),
            parent: None,
            section: None,
        };
        assert_eq!(variant_index(&update), 7);
    }

    #[test]
    fn validate_badges_rules() {
        let v = |items: &[&str]| {
            validate_badges(&items.iter().map(|s| s.to_string()).collect::<Vec<_>>())
        };
        assert_eq!(v(&[]).unwrap(), Vec::<String>::new());
        assert_eq!(v(&["bg", "opus", "rc", "x-1"]).unwrap().len(), 4);
        // Repeats collapse, so four distinct + a repeat is fine.
        assert_eq!(v(&["bg", "bg", "rc"]).unwrap(), ["bg", "rc"]);
        assert!(v(&["a", "b", "c", "d", "e"]).is_err());
        assert!(v(&[""]).is_err());
        assert!(v(&["BG"]).is_err());
        assert!(v(&["a b"]).is_err());
        assert!(v(&["a,b"]).is_err());
        assert!(v(&["abcdefghijkl"]).is_ok()); // exactly 12
        assert!(v(&["abcdefghijklm"]).is_err()); // 13
        assert!(v(&["é"]).is_err());
    }

    #[test]
    fn an_unknown_variant_index_is_a_clean_decode_error() {
        // What an app built before these variants sees: an error, no panic.
        assert!(bincode::deserialize::<CliRequest>(&99u32.to_le_bytes()).is_err());
    }

    #[test]
    fn new_agent_terminal_in_round_trips() {
        let request = CliRequest::NewAgentTerminalIn {
            cwd: PathBuf::from("/x"),
            command: None,
            title: None,
            on_no_match: NoMatchBehavior::Error,
            parent: Some("p".into()),
            section: Some("Personas".into()),
        };
        let json = serde_json::to_string(&request).unwrap();
        match serde_json::from_str::<CliRequest>(&json).unwrap() {
            CliRequest::NewAgentTerminalIn {
                parent, section, ..
            } => {
                assert_eq!(parent.as_deref(), Some("p"));
                assert_eq!(section.as_deref(), Some("Personas"));
            }
            _ => panic!("wrong variant"),
        }
    }

    /// S2's variants are appended after #14's (indices 5/6): 7 and 8, with
    /// golden bytes, so old and new binaries agree on them.
    #[test]
    fn s2_variants_are_appended_with_stable_wire_bytes() {
        let update = CliRequest::UpdateAgentThread {
            id: "i".into(),
            parent: Some(ParentChange::Set("p".into())),
            section: Some(SectionChange::Clear),
        };
        let mut expected = 7u32.to_le_bytes().to_vec();
        expected.extend(bincode_string("i"));
        expected.push(1); // parent: Some
        expected.extend(0u32.to_le_bytes()); // ParentChange::Set
        expected.extend(bincode_string("p"));
        expected.push(1); // section: Some
        expected.extend(1u32.to_le_bytes()); // SectionChange::Clear
        assert_eq!(bincode::serialize(&update).unwrap(), expected);

        let new_in = CliRequest::NewAgentTerminalIn {
            cwd: PathBuf::from("/x"),
            command: None,
            title: None,
            on_no_match: NoMatchBehavior::Error,
            parent: Some("p".into()),
            section: None,
        };
        let mut expected = 8u32.to_le_bytes().to_vec();
        expected.extend(bincode_string("/x"));
        expected.push(0); // command: None
        expected.push(0); // title: None
        expected.extend(0u32.to_le_bytes()); // NoMatchBehavior::Error
        expected.push(1); // parent: Some
        expected.extend(bincode_string("p"));
        expected.push(0); // section: None
        assert_eq!(bincode::serialize(&new_in).unwrap(), expected);
    }

    /// The new variants survive a bincode round trip; an app that predates
    /// them fails to decode (clean rejection), it does not mis-decode them as
    /// `NewAgentTerminal`.
    #[test]
    fn new_variants_round_trip_through_bincode() {
        let request = CliRequest::NewAgentTerminalIn {
            cwd: PathBuf::from("/x"),
            command: None,
            title: Some("t".into()),
            on_no_match: NoMatchBehavior::NewWindow,
            parent: Some("p".into()),
            section: None,
        };
        let bytes = bincode::serialize(&request).unwrap();
        match bincode::deserialize::<CliRequest>(&bytes).unwrap() {
            CliRequest::NewAgentTerminalIn {
                title,
                on_no_match,
                parent,
                section,
                ..
            } => {
                assert_eq!(title.as_deref(), Some("t"));
                assert_eq!(on_no_match, NoMatchBehavior::NewWindow);
                assert_eq!(parent.as_deref(), Some("p"));
                assert_eq!(section, None);
            }
            _ => panic!("wrong variant"),
        }
        let update = CliRequest::UpdateAgentThread {
            id: "abc".into(),
            parent: Some(ParentChange::Set("p".into())),
            section: Some(SectionChange::Clear),
        };
        let bytes = bincode::serialize(&update).unwrap();
        match bincode::deserialize::<CliRequest>(&bytes).unwrap() {
            CliRequest::UpdateAgentThread {
                id,
                parent,
                section,
            } => {
                assert_eq!(id, "abc");
                assert_eq!(parent, Some(ParentChange::Set("p".into())));
                assert_eq!(section, Some(SectionChange::Clear));
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn update_agent_thread_round_trips() {
        for (parent, section) in [
            (Some(ParentChange::Set("p".into())), None),
            (Some(ParentChange::Clear), Some(SectionChange::Clear)),
            (None, Some(SectionChange::Set("S".into()))),
        ] {
            let request = CliRequest::UpdateAgentThread {
                id: "abc".into(),
                parent: parent.clone(),
                section: section.clone(),
            };
            let json = serde_json::to_string(&request).unwrap();
            match serde_json::from_str::<CliRequest>(&json).unwrap() {
                CliRequest::UpdateAgentThread {
                    id,
                    parent: p,
                    section: s,
                } => {
                    assert_eq!(id, "abc");
                    assert_eq!(p, parent);
                    assert_eq!(s, section);
                }
                _ => panic!("wrong variant"),
            }
        }
    }
}
