//! CPZed: which `claude attach` terminal threads are waiting for the user.
//!
//! `claude agents --json` reports a `state` per background session
//! (`working` | `done` | `blocked`). A thread whose saved command is
//! `claude attach <id>` is mapped to its session through that id. Polling is
//! slow (5 s), only while at least one such thread exists, one subprocess at
//! a time, killed after 3 s, always on a background thread, and quiet on any
//! failure (the badge just disappears).

use std::collections::{BTreeMap, HashSet};
use std::time::{Duration, Instant};

use gpui::{App, AppContext as _, Context, Entity, Global};

use crate::terminal_restore::{agents_json_fetcher_with_timeout, attach_target};
use crate::terminal_restore_store::TerminalRestoreStore;
use crate::terminal_thread_metadata_store::TerminalThreadMetadataStore;

pub const POLL_INTERVAL: Duration = Duration::from_secs(5);
pub const POLL_TIMEOUT: Duration = Duration::from_secs(3);
/// Consecutive failed polls (CLI error, timeout, garbage) before the last
/// known state is dropped: a transient failure keeps the badges, a dead CLI
/// does not leave a red badge up forever.
pub const MAX_FAILURES: u8 = 3;
/// A poll "in flight" for longer than this is presumed lost (its task was
/// dropped) and no longer blocks the next one.
const IN_FLIGHT_STALE: Duration = Duration::from_secs(15);

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
    /// When the running poll started: the single in-flight guard shared by the
    /// 5 s loop and `--agent-list`, so at most one subprocess runs at a time.
    in_flight: Option<Instant>,
    failures: u8,
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
        self.threads.keys().map(|id| self.pending_count(id)).sum()
    }

    /// Claims the single poll slot. `false`: a poll is already running, so the
    /// caller reuses the last result instead of spawning another subprocess.
    pub fn try_begin_poll(&mut self) -> bool {
        match self.in_flight {
            Some(started) if started.elapsed() < IN_FLIGHT_STALE => false,
            _ => {
                self.in_flight = Some(Instant::now());
                true
            }
        }
    }

    /// Applies one poll result and releases the poll slot. `targets` maps
    /// terminal id to its attach target. `agents_json` is `None` (CLI failed or
    /// timed out) or not a JSON array: a failure. A failure keeps the previous
    /// state, until [`MAX_FAILURES`] in a row drop it (fail quiet).
    pub fn apply(
        &mut self,
        targets: &BTreeMap<String, String>,
        agents_json: Option<&str>,
        cx: &mut Context<Self>,
    ) {
        self.in_flight = None;
        let json = agents_json.filter(|json| {
            serde_json::from_str::<serde_json::Value>(json).is_ok_and(|value| value.is_array())
        });
        let next = match json {
            _ if targets.is_empty() => {
                self.failures = 0;
                BTreeMap::new()
            }
            None => {
                self.failures = self.failures.saturating_add(1);
                if self.failures < MAX_FAILURES {
                    return;
                }
                BTreeMap::new()
            }
            Some(json) => {
                self.failures = 0;
                let mut next = BTreeMap::new();
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
                next
            }
        };
        if next != self.threads {
            self.threads = next;
            cx.notify();
        }
    }
}

/// `false` when there is no `claude attach` thread: then no subprocess at all.
pub fn should_poll(targets: &BTreeMap<String, String>) -> bool {
    !targets.is_empty()
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

/// One poll: no subprocess at all when there is no attach thread, and never
/// two at once (a caller that finds one running returns and reuses its result).
pub async fn poll_once(this: &Entity<TerminalAttention>, cx: &mut gpui::AsyncApp) {
    let targets = cx.update(|cx| attach_targets(cx));
    let fetch = agents_json_fetcher_with_timeout(POLL_TIMEOUT);
    poll_targets(this, targets, fetch, cx).await;
}

/// [`poll_once`] with the targets and the fetch injected (the tests count fetches).
async fn poll_targets(
    this: &Entity<TerminalAttention>,
    targets: BTreeMap<String, String>,
    fetch: impl FnOnce() -> Option<String> + Send + 'static,
    cx: &mut gpui::AsyncApp,
) {
    if !should_poll(&targets) {
        cx.update(|cx| this.update(cx, |this, cx| this.apply(&targets, None, cx)));
        return;
    }
    if !cx.update(|cx| this.update(cx, |this, _| this.try_begin_poll())) {
        return;
    }
    let json = cx.background_executor().spawn(async move { fetch() }).await;
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
        assert_eq!(
            find_session(JSON, "aaaa1111"),
            Some(Some(SessionState::Blocked))
        );
        assert_eq!(
            find_session(JSON, "BBBB2222"),
            Some(Some(SessionState::Working))
        );
        assert_eq!(
            find_session(JSON, "cccc3333"),
            Some(Some(SessionState::Done))
        );
        assert_eq!(
            find_session(JSON, "dddd4444"),
            Some(Some(SessionState::Other))
        );
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
                Some(ThreadAttention {
                    attached: true,
                    state: SessionState::Working
                })
            );
            assert_eq!(
                store.thread("t3"),
                Some(ThreadAttention {
                    attached: false,
                    state: SessionState::Other
                })
            );
        });
        // Two blocked sessions: the header total is the sum.
        let two = JSON.replace(r#""working""#, r#""blocked""#);
        store.update(cx, |store, cx| store.apply(&targets(), Some(&two), cx));
        store.read_with(cx, |store, _| assert_eq!(store.total_pending(), 2));
        // A transient failure (None or garbage) KEEPS the last known state ...
        store.update(cx, |store, cx| store.apply(&targets(), None, cx));
        store.update(cx, |store, cx| store.apply(&targets(), Some("garbage"), cx));
        store.read_with(cx, |store, _| assert_eq!(store.total_pending(), 2));
        // ... a good poll resets the failure count ...
        store.update(cx, |store, cx| store.apply(&targets(), Some(JSON), cx));
        store.update(cx, |store, cx| store.apply(&targets(), None, cx));
        store.update(cx, |store, cx| store.apply(&targets(), None, cx));
        store.read_with(cx, |store, _| assert_eq!(store.total_pending(), 1));
        // ... and MAX_FAILURES in a row drop it (fail quiet).
        store.update(cx, |store, cx| store.apply(&targets(), None, cx));
        store.read_with(cx, |store, _| {
            assert_eq!(store.total_pending(), 0);
            assert!(store.thread("t1").is_none());
        });
        // No attach thread at all clears immediately.
        store.update(cx, |store, cx| store.apply(&targets(), Some(JSON), cx));
        store.update(cx, |store, cx| store.apply(&BTreeMap::new(), None, cx));
        store.read_with(cx, |store, _| assert_eq!(store.total_pending(), 0));
    }

    /// One subprocess at a time: the poll slot is single, released by `apply`,
    /// and a lost poll does not block forever.
    #[gpui::test]
    fn poll_slot_is_single_and_released_by_apply(cx: &mut gpui::TestAppContext) {
        let store = cx.update(|cx| TerminalAttention::init(cx));
        store.update(cx, |store, _| {
            assert!(store.try_begin_poll());
            assert!(!store.try_begin_poll(), "a second poll must not start");
        });
        store.update(cx, |store, cx| store.apply(&targets(), Some(JSON), cx));
        store.update(cx, |store, _| {
            assert!(store.try_begin_poll(), "apply releases the slot");
            store.in_flight = Some(Instant::now() - IN_FLIGHT_STALE - Duration::from_secs(1));
            assert!(store.try_begin_poll(), "a lost poll is presumed dead");
        });
    }

    #[test]
    fn no_attach_thread_means_no_subprocess() {
        assert!(!should_poll(&BTreeMap::new()));
        assert!(should_poll(&targets()));
    }

    #[test]
    fn poll_timeout_is_three_seconds() {
        assert_eq!(POLL_TIMEOUT, Duration::from_secs(3));
        assert_eq!(POLL_INTERVAL, Duration::from_secs(5));
    }

    /// Valid JSON that is not an array is a FAILED poll (state kept), not a
    /// good poll that finds every session missing.
    #[gpui::test]
    fn non_array_json_is_a_failure_not_a_good_poll(cx: &mut gpui::TestAppContext) {
        let store = cx.update(|cx| TerminalAttention::init(cx));
        store.update(cx, |store, cx| store.apply(&targets(), Some(JSON), cx));
        for bad in ["{}", "5", "null", r#"{"agents":[]}"#] {
            store.update(cx, |store, cx| store.apply(&targets(), Some(bad), cx));
            store.read_with(cx, |store, _| {
                assert_eq!(store.total_pending(), 1, "{bad} must keep the state");
                assert!(store.thread("t1").is_some());
            });
            // A good poll resets the failure count between the bad ones.
            store.update(cx, |store, cx| store.apply(&targets(), Some(JSON), cx));
        }
        // Three non-array results in a row drop the state like any failure.
        for _ in 0..MAX_FAILURES {
            store.update(cx, |store, cx| store.apply(&targets(), Some("{}"), cx));
        }
        store.read_with(cx, |store, _| assert_eq!(store.total_pending(), 0));
    }

    /// The header total is exactly the number of Blocked threads: not the thread
    /// count, not the attached count, not the working count.
    #[gpui::test]
    fn total_pending_is_exactly_the_blocked_count(cx: &mut gpui::TestAppContext) {
        let store = cx.update(|cx| TerminalAttention::init(cx));
        let ids: BTreeMap<String, String> = [
            ("t1", "aaaa1111"),
            ("t2", "bbbb2222"),
            ("t3", "cccc3333"),
            ("t4", "dddd4444"),
            ("t5", "gone0000"),
        ]
        .into_iter()
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .collect();
        let json = |states: [&str; 4]| {
            let ids = ["aaaa1111", "bbbb2222", "cccc3333", "dddd4444"];
            let rows: Vec<String> = ids
                .iter()
                .zip(states)
                .map(|(id, state)| format!(r#"{{"id":"{id}","state":"{state}"}}"#))
                .collect();
            format!("[{}]", rows.join(","))
        };
        let total = |cx: &mut gpui::TestAppContext, json: String| {
            store.update(cx, |store, cx| store.apply(&ids, Some(&json), cx));
            store.read_with(cx, |store, _| store.total_pending())
        };
        assert_eq!(total(cx, json(["working", "done", "weird", "working"])), 0);
        assert_eq!(total(cx, json(["blocked", "working", "done", "weird"])), 1);
        assert_eq!(
            total(cx, json(["blocked", "blocked", "blocked", "done"])),
            3
        );
        assert_eq!(
            total(cx, json(["blocked", "blocked", "blocked", "blocked"])),
            4
        );
    }

    /// A second poll while one is in flight spawns nothing; once the first
    /// result is applied the next poll fetches again.
    #[gpui::test]
    async fn poll_while_one_is_in_flight_spawns_no_fetch(cx: &mut gpui::TestAppContext) {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        let store = cx.update(|cx| TerminalAttention::init(cx));
        let fetches = Arc::new(AtomicUsize::new(0));
        let fetch = |fetches: &Arc<AtomicUsize>| {
            let fetches = fetches.clone();
            move || {
                fetches.fetch_add(1, Ordering::SeqCst);
                Some(JSON.to_string())
            }
        };
        let mut async_cx = cx.to_async();
        // Someone else holds the slot: no fetch.
        assert!(store.update(cx, |store, _| store.try_begin_poll()));
        poll_targets(&store, targets(), fetch(&fetches), &mut async_cx).await;
        assert_eq!(fetches.load(Ordering::SeqCst), 0);
        // Their result lands and frees the slot: the next poll fetches once ...
        store.update(cx, |store, cx| store.apply(&targets(), Some(JSON), cx));
        poll_targets(&store, targets(), fetch(&fetches), &mut async_cx).await;
        assert_eq!(fetches.load(Ordering::SeqCst), 1);
        store.read_with(cx, |store, _| assert_eq!(store.total_pending(), 1));
        // ... and no attach thread means no fetch at all.
        poll_targets(&store, BTreeMap::new(), fetch(&fetches), &mut async_cx).await;
        assert_eq!(fetches.load(Ordering::SeqCst), 1);
    }
}
