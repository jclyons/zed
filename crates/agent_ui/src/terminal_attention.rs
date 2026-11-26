//! CPZed: which `claude attach` terminal threads are waiting for the user.
//!
//! `claude agents --json` reports a `state` per background session
//! (`working` | `done` | `blocked`). A thread whose saved command is
//! `claude attach <id>` is mapped to its session through that id. Polling is
//! slow (5 s), only while at least one such thread exists, one subprocess at
//! a time, killed after 3 s, always on a background thread, and quiet on any
//! failure (the badge just disappears).

use std::collections::{BTreeMap, HashSet};
use std::time::Duration;

use gpui::{App, AppContext as _, Context, Entity, Global};

use crate::terminal_restore::{agents_json_fetcher_with_timeout, attach_target};
use crate::terminal_restore_store::TerminalRestoreStore;
use crate::terminal_thread_metadata_store::TerminalThreadMetadataStore;

pub const POLL_INTERVAL: Duration = Duration::from_secs(5);
pub const POLL_TIMEOUT: Duration = Duration::from_secs(3);

/// A session's state as `claude agents --json` reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    Working,
    Done,
    /// Waiting for the user's input (permission prompt, question).
    Blocked,
    /// Any other / missing value: shown as nothing.
    Other,
}

impl SessionState {
    fn parse(value: Option<&str>) -> Self {
        match value {
            Some("working") => Self::Working,
            Some("done") => Self::Done,
            Some("blocked") => Self::Blocked,
            _ => Self::Other,
        }
    }
}

/// The live session `target` names in `agents_json`, using the same matching
/// as [`crate::terminal_restore::target_is_alive`] (exact `id` / `sessionId` /
/// `name`, or an id prefix of 6+ characters). Outer `None`: the output is not
/// the expected JSON (unknown). `Some(None)`: no such session.
pub fn find_session(agents_json: &str, target: &str) -> Option<Option<SessionState>> {
    let agents: serde_json::Value = serde_json::from_str(agents_json).ok()?;
    let agents = agents.as_array()?;
    let target = target.to_lowercase();
    let found = agents.iter().find(|agent| {
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
    });
    Some(found.map(|agent| SessionState::parse(agent.get("state").and_then(|s| s.as_str()))))
}

/// What the sidebar and `--agent-list` know about one attach thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThreadAttention {
    /// The `claude attach` target appears in `claude agents --json`.
    pub attached: bool,
    pub state: SessionState,
}

struct GlobalTerminalAttention(Entity<TerminalAttention>);
impl Global for GlobalTerminalAttention {}

#[derive(Default)]
pub struct TerminalAttention {
    threads: BTreeMap<String, ThreadAttention>,
}

impl TerminalAttention {
    pub fn init(cx: &mut App) -> Entity<Self> {
        if let Some(existing) = Self::try_global(cx) {
            return existing;
        }
        let store = cx.new(|_| Self::default());
        cx.set_global(GlobalTerminalAttention(store.clone()));
        store
    }

    pub fn try_global(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<GlobalTerminalAttention>()
            .map(|global| global.0.clone())
    }

    pub fn thread(&self, terminal_id: &str) -> Option<ThreadAttention> {
        self.threads.get(terminal_id).copied()
    }

    /// 1 per blocked session, 0 otherwise (the CLI exposes no request count).
    pub fn pending_count(&self, terminal_id: &str) -> usize {
        usize::from(
            self.threads
                .get(terminal_id)
                .is_some_and(|thread| thread.state == SessionState::Blocked),
        )
    }

    /// Sum over all threads: the sidebar header total.
    pub fn total_pending(&self) -> usize {
        self.threads
            .keys()
            .map(|id| self.pending_count(id))
            .sum()
    }

    /// Applies one poll result. `targets` maps terminal id to its attach
    /// target; `agents_json` is `None` when the CLI failed or timed out,
    /// which clears everything (fail quiet: no badge rather than a stale one).
    pub fn apply(
        &mut self,
        targets: &BTreeMap<String, String>,
        agents_json: Option<&str>,
        cx: &mut Context<Self>,
    ) {
        let mut next = BTreeMap::new();
        if let Some(json) = agents_json {
            for (id, target) in targets {
                match find_session(json, target) {
                    Some(Some(state)) => {
                        next.insert(
                            id.clone(),
                            ThreadAttention {
                                attached: true,
                                state,
                            },
                        );
                    }
                    Some(None) => {
                        next.insert(
                            id.clone(),
                            ThreadAttention {
                                attached: false,
                                state: SessionState::Other,
                            },
                        );
                    }
                    None => {}
                }
            }
        }
        if next != self.threads {
            self.threads = next;
            cx.notify();
        }
    }
}

/// Terminal ids (of threads that still exist) whose saved command is
/// `claude attach <target>`, with that target.
pub fn attach_targets(cx: &App) -> BTreeMap<String, String> {
    let Some(restore) = TerminalRestoreStore::try_global(cx) else {
        return BTreeMap::new();
    };
    let existing: HashSet<String> = TerminalThreadMetadataStore::try_global(cx)
        .map(|store| {
            store
                .read(cx)
                .entries()
                .map(|metadata| metadata.terminal_id.to_key_string())
                .collect()
        })
        .unwrap_or_default();
    restore
        .read(cx)
        .states()
        .filter(|(id, _)| existing.contains(*id))
        .filter_map(|(id, state)| {
            let target = attach_target(state.init_command.as_deref()?)?;
            Some((id.clone(), target.to_string()))
        })
        .collect()
}

/// One poll: no subprocess at all when there is no attach thread.
pub async fn poll_once(this: &Entity<TerminalAttention>, cx: &mut gpui::AsyncApp) {
    let targets = cx.update(|cx| attach_targets(cx));
    if targets.is_empty() {
        cx.update(|cx| this.update(cx, |this, cx| this.apply(&targets, None, cx)));
        return;
    }
    let fetch = cx.update(|_| agents_json_fetcher_with_timeout(POLL_TIMEOUT));
    let json = cx
        .background_executor()
        .spawn(async move { fetch() })
        .await;
    cx.update(|cx| this.update(cx, |this, cx| this.apply(&targets, json.as_deref(), cx)));
}

/// Starts the 5 s loop (sequential, so one subprocess at a time).
pub fn start_polling(cx: &mut App) {
    let this = TerminalAttention::init(cx);
    cx.spawn(async move |cx| {
        loop {
            cx.background_executor().timer(POLL_INTERVAL).await;
            poll_once(&this, cx).await;
        }
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::*;

    const JSON: &str = r#"[
      {"id":"aaaa1111","sessionId":"aaaa1111-0000","name":"w1","state":"blocked"},
      {"id":"bbbb2222","sessionId":"bbbb2222-0000","name":"w2","state":"working"},
      {"id":"cccc3333","sessionId":"cccc3333-0000","name":"w3","state":"done"},
      {"id":"dddd4444","sessionId":"dddd4444-0000","name":"w4","state":"weird"}
    ]"#;

    #[test]
    fn find_session_maps_states_and_missing() {
        assert_eq!(find_session(JSON, "aaaa1111"), Some(Some(SessionState::Blocked)));
        assert_eq!(find_session(JSON, "BBBB2222"), Some(Some(SessionState::Working)));
        assert_eq!(find_session(JSON, "cccc3333"), Some(Some(SessionState::Done)));
        assert_eq!(find_session(JSON, "dddd4444"), Some(Some(SessionState::Other)));
        assert_eq!(find_session(JSON, "w1"), Some(Some(SessionState::Blocked)));
        assert_eq!(find_session(JSON, "gone0000"), Some(None));
        // A short prefix never matches by id prefix.
        assert_eq!(find_session(JSON, "aaaa"), Some(None));
        // Unparseable output is unknown, not "missing".
        assert_eq!(find_session("not json", "aaaa1111"), None);
        assert_eq!(find_session("{}", "aaaa1111"), None);
    }

    fn targets() -> BTreeMap<String, String> {
        [("t1", "aaaa1111"), ("t2", "bbbb2222"), ("t3", "gone0000")]
            .into_iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect()
    }

    #[gpui::test]
    fn apply_counts_only_blocked_and_fails_quiet(cx: &mut gpui::TestAppContext) {
        let store = cx.update(|cx| TerminalAttention::init(cx));
        store.update(cx, |store, cx| store.apply(&targets(), Some(JSON), cx));
        store.read_with(cx, |store, _| {
            assert_eq!(store.pending_count("t1"), 1);
            assert_eq!(store.pending_count("t2"), 0);
            assert_eq!(store.pending_count("t3"), 0);
            assert_eq!(store.pending_count("unknown"), 0);
            assert_eq!(store.total_pending(), 1);
            assert_eq!(
                store.thread("t2"),
                Some(ThreadAttention { attached: true, state: SessionState::Working })
            );
            assert_eq!(
                store.thread("t3"),
                Some(ThreadAttention { attached: false, state: SessionState::Other })
            );
        });
        // Two blocked sessions: the header total is the sum.
        let two = JSON.replace(r#""working""#, r#""blocked""#);
        store.update(cx, |store, cx| store.apply(&targets(), Some(&two), cx));
        store.read_with(cx, |store, _| assert_eq!(store.total_pending(), 2));
        // CLI failure / timeout (None) or garbage: no badge at all.
        store.update(cx, |store, cx| store.apply(&targets(), None, cx));
        store.read_with(cx, |store, _| {
            assert_eq!(store.total_pending(), 0);
            assert!(store.thread("t1").is_none());
        });
        store.update(cx, |store, cx| store.apply(&targets(), Some(JSON), cx));
        store.update(cx, |store, cx| store.apply(&targets(), Some("garbage"), cx));
        store.read_with(cx, |store, _| assert_eq!(store.total_pending(), 0));
    }
}
