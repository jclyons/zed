//! CPZed: fork-owned restore state for terminal threads, kept OUTSIDE the
//! upstream `TerminalThreadMetadataDb` domain (no fork migration on an
//! upstream table) in a small KVP blob keyed by terminal id, the same seam
//! the sidebar hierarchy uses.
//!
//! What it holds per thread: the command typed into the shell at start (so a
//! restart can re-run it), whether the thread was open, and whether its
//! target is gone (stale). Threads with no entry load as not open.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use db::kvp::KeyValueStore;
use gpui::{App, AppContext as _, Context, Entity, Global, Task};
use remote::RemoteConnectionOptions;
use serde::{Deserialize, Serialize};
use util::ResultExt as _;

use crate::terminal_thread_metadata_store::{TerminalThreadMetadata, TerminalThreadMetadataStore};

pub const KVP_SCOPE: &str = "cpzed_terminal_restore";
pub const KVP_KEY: &str = "v1";
const VERSION: u32 = 1;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalRestoreState {
    /// The command typed into the shell when the thread was started
    /// (e.g. `claude attach <id>`); re-run on restore unless stale.
    #[serde(default)]
    pub init_command: Option<String>,
    /// Open in a panel when last persisted. An explicit close deletes the
    /// thread's metadata row, which orphans (and later prunes) this entry.
    #[serde(default)]
    pub open: bool,
    /// The command's target is gone (dead claude session).
    #[serde(default)]
    pub stale: bool,
}

#[derive(Serialize, Deserialize)]
struct Blob {
    version: u32,
    threads: BTreeMap<String, TerminalRestoreState>,
    /// Sidebar chips per terminal id (`--badge` / `--agent-set-badges`). A
    /// separate map so `set` of the restore state never touches them.
    #[serde(default)]
    badges: BTreeMap<String, Vec<String>>,
}

struct GlobalTerminalRestoreStore(Entity<TerminalRestoreStore>);

impl Global for GlobalTerminalRestoreStore {}

pub struct TerminalRestoreStore {
    threads: BTreeMap<String, TerminalRestoreState>,
    badges: BTreeMap<String, Vec<String>>,
    db: KeyValueStore,
    write_task: Option<Task<()>>,
    /// A newer build wrote the blob: never write, so a rollback cannot wipe it.
    read_only: bool,
    /// Ids a panel already restored this session, so two open panels never
    /// both restore (and re-run the command of) one thread.
    claimed: HashSet<String>,
}

impl TerminalRestoreStore {
    pub fn init(cx: &mut App) -> Entity<Self> {
        if let Some(existing) = Self::try_global(cx) {
            return existing;
        }
        let db = KeyValueStore::global(cx);
        let store = cx.new(|cx| Self::new(db, cx));
        cx.set_global(GlobalTerminalRestoreStore(store.clone()));
        store
    }

    pub fn try_global(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<GlobalTerminalRestoreStore>()
            .map(|global| global.0.clone())
    }

    fn new(db: KeyValueStore, cx: &mut Context<Self>) -> Self {
        let mut read_only = false;
        let (threads, badges) = match db.scoped(KVP_SCOPE).read(KVP_KEY) {
            Ok(None) => Default::default(),
            Ok(Some(raw)) => match serde_json::from_str::<Blob>(&raw) {
                Ok(blob) if blob.version <= VERSION => (blob.threads, blob.badges),
                Ok(_) => {
                    read_only = true;
                    Default::default()
                }
                Err(error) => {
                    log::warn!(
                        "cpzed terminal restore: unreadable state ({error}); starting empty"
                    );
                    Default::default()
                }
            },
            Err(error) => {
                log::error!("cpzed terminal restore: could not read state ({error:#})");
                read_only = true;
                Default::default()
            }
        };
        // Let a pending write land before the app quits.
        cx.on_app_quit(|this, _cx| {
            let pending = this.write_task.take();
            async move {
                if let Some(pending) = pending {
                    pending.await;
                }
            }
        })
        .detach();
        Self {
            threads,
            badges,
            db,
            write_task: None,
            read_only,
            claimed: HashSet::new(),
        }
    }

    pub fn state(&self, terminal_id: &str) -> TerminalRestoreState {
        self.threads.get(terminal_id).cloned().unwrap_or_default()
    }

    /// A thread's badges, in display order (empty when none).
    pub fn badges(&self, terminal_id: &str) -> Vec<String> {
        self.badges.get(terminal_id).cloned().unwrap_or_default()
    }

    /// Replaces a thread's badges; an empty list removes the entry. The
    /// caller validates with `cli::validate_badges`.
    pub fn set_badges(&mut self, terminal_id: &str, badges: Vec<String>, cx: &mut Context<Self>) {
        let changed = if badges.is_empty() {
            self.badges.remove(terminal_id).is_some()
        } else if self.badges.get(terminal_id) == Some(&badges) {
            false
        } else {
            self.badges.insert(terminal_id.to_string(), badges);
            true
        };
        if changed {
            self.schedule_write(cx);
            cx.notify();
        }
    }

    /// Every thread with saved state (id, state), for the attention poller.
    pub fn states(&self) -> impl Iterator<Item = (&String, &TerminalRestoreState)> {
        self.threads.iter()
    }

    pub fn has_state(&self, terminal_id: &str) -> bool {
        self.threads.contains_key(terminal_id)
    }

    pub fn set(&mut self, terminal_id: &str, state: TerminalRestoreState, cx: &mut Context<Self>) {
        if self.threads.get(terminal_id) == Some(&state) {
            return;
        }
        self.threads.insert(terminal_id.to_string(), state);
        self.schedule_write(cx);
        cx.notify();
    }

    /// Drop entries whose thread no longer exists (closed or deleted).
    pub fn retain_ids(&mut self, keep: &HashSet<String>, cx: &mut Context<Self>) {
        // An empty/failed metadata load must never wipe the overlay.
        if keep.is_empty() {
            return;
        }
        let before = self.threads.len() + self.badges.len();
        self.threads.retain(|id, _| keep.contains(id));
        self.badges.retain(|id, _| keep.contains(id));
        if self.threads.len() + self.badges.len() != before {
            self.schedule_write(cx);
        }
    }

    /// `true` once per id per session: the caller that gets `true` restores it.
    pub fn try_claim(&mut self, terminal_id: &str) -> bool {
        self.claimed.insert(terminal_id.to_string())
    }

    /// Claims `rows` for this panel, skipping the active terminal (the load
    /// restores it itself; spawning it again would start a second shell) and
    /// anything another panel already claimed. The active id is claimed too.
    pub fn claim_for_restore(
        &mut self,
        rows: Vec<TerminalThreadMetadata>,
        active: Option<crate::TerminalId>,
    ) -> Vec<TerminalThreadMetadata> {
        let mut claimed = Vec::new();
        for row in rows {
            if Some(row.terminal_id) == active {
                continue;
            }
            if self.try_claim(&row.terminal_id.to_key_string()) {
                claimed.push(row);
            }
        }
        if let Some(active) = active {
            self.try_claim(&active.to_key_string());
        }
        claimed
    }

    fn schedule_write(&mut self, cx: &mut Context<Self>) {
        if self.read_only {
            return;
        }
        let blob = Blob {
            version: VERSION,
            threads: self.threads.clone(),
            badges: self.badges.clone(),
        };
        let Some(json) = serde_json::to_string(&blob).log_err() else {
            return;
        };
        let db = self.db.clone();
        self.write_task = Some(cx.background_spawn(async move {
            db.scoped(KVP_SCOPE)
                .write(KVP_KEY.to_string(), json)
                .await
                .log_err();
        }));
    }
}

/// Whether a thread belongs to the project with these roots. Matches on the
/// thread's folder paths only; its cwd is a fallback just for a thread with no
/// folder paths, and never through a root that is the home directory or a
/// filesystem root (a `~` or `/code` workspace must not claim sub-projects).
pub fn belongs_to_roots(
    folder_paths: &[PathBuf],
    cwd: Option<&Path>,
    roots: &[PathBuf],
    home: Option<&Path>,
) -> bool {
    if !folder_paths.is_empty() {
        return folder_paths
            .iter()
            .any(|folder| roots.iter().any(|root| root == folder));
    }
    let Some(cwd) = cwd else {
        return false;
    };
    roots.iter().any(|root| {
        let too_broad = root.components().count() <= 2 || Some(root.as_path()) == home;
        !too_broad && cwd.starts_with(root)
    })
}

/// Threads that were open before the last shutdown and belong to the project
/// with these roots, oldest first. Other projects' threads are never returned.
pub fn entries_to_restore(
    metadata: &TerminalThreadMetadataStore,
    restore: &TerminalRestoreStore,
    roots: &[PathBuf],
    remote_connection: Option<&RemoteConnectionOptions>,
    home: Option<&Path>,
) -> Vec<TerminalThreadMetadata> {
    let mut entries: Vec<_> = metadata
        .entries()
        .filter(|terminal| {
            restore.state(&terminal.terminal_id.to_key_string()).open
                && terminal.remote_connection.as_ref().map(|o| o.identity())
                    == remote_connection.map(|o| o.identity())
                && belongs_to_roots(
                    terminal.folder_paths().paths(),
                    terminal.working_directory.as_deref(),
                    roots,
                    home,
                )
        })
        .cloned()
        .collect();
    entries.sort_by_cached_key(|m| (m.created_at, m.terminal_id.to_key_string()));
    entries
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(path: &str) -> PathBuf {
        PathBuf::from(path)
    }

    #[test]
    fn test_belongs_matches_folder_paths_only() {
        let roots = [p("/code/a")];
        assert!(belongs_to_roots(&[p("/code/a")], None, &roots, None));
        assert!(!belongs_to_roots(
            &[p("/code/b")],
            Some(Path::new("/code/a")),
            &roots,
            None
        ));
    }

    #[test]
    fn test_broad_roots_do_not_claim_subprojects_by_cwd() {
        let cwd = Path::new("/Users/me/code/sub");
        assert!(!belongs_to_roots(
            &[],
            Some(cwd),
            &[p("/Users/me")],
            Some(Path::new("/Users/me"))
        ));
        assert!(!belongs_to_roots(&[], Some(cwd), &[p("/Users")], None));
        assert!(belongs_to_roots(
            &[],
            Some(cwd),
            &[p("/Users/me/code")],
            Some(Path::new("/Users/me"))
        ));
        // A thread with folder paths is never claimed through its cwd.
        assert!(!belongs_to_roots(
            &[p("/other")],
            Some(cwd),
            &[p("/Users/me/code")],
            None
        ));
    }

    /// The overlay survives a real quit/reopen: a second store opened on the
    /// same KVP db reads back command/open/stale (not the in-memory map).
    #[gpui::test]
    async fn test_overlay_round_trips_through_the_kvp_db(cx: &mut gpui::TestAppContext) {
        let db = KeyValueStore::open_test_db("terminal_restore_round_trip").await;
        let first = cx.new(|cx| TerminalRestoreStore::new(db.clone(), cx));
        let stale = TerminalRestoreState {
            init_command: Some("claude attach dead9999".into()),
            open: true,
            stale: true,
        };
        let live = TerminalRestoreState {
            init_command: Some("claude attach aaaa1111".into()),
            open: false,
            stale: false,
        };
        first.update(cx, |store, cx| {
            store.set("stale-id", stale.clone(), cx);
            store.set("live-id", live.clone(), cx);
        });
        cx.run_until_parked();
        drop(first);

        let reopened = cx.new(|cx| TerminalRestoreStore::new(db.clone(), cx));
        reopened.read_with(cx, |store, _| {
            assert_eq!(store.state("stale-id"), stale);
            assert_eq!(store.state("live-id"), live);
            assert!(!store.has_state("missing"));
        });
        // An empty keep-set must never wipe the overlay.
        reopened.update(cx, |store, cx| store.retain_ids(&HashSet::new(), cx));
        cx.run_until_parked();
        let again = cx.new(|cx| TerminalRestoreStore::new(db, cx));
        again.read_with(cx, |store, _| assert_eq!(store.state("stale-id"), stale));
    }

    fn meta(
        title: &str,
        root: &str,
        remote: Option<RemoteConnectionOptions>,
    ) -> TerminalThreadMetadata {
        TerminalThreadMetadata {
            terminal_id: crate::TerminalId::new(),
            title: title.to_string().into(),
            custom_title: None,
            created_at: chrono::Utc::now(),
            worktree_paths: crate::thread_metadata_store::WorktreePaths::from_folder_paths(
                &workspace::PathList::new(&[p(root)]),
            ),
            remote_connection: remote,
            working_directory: Some(p(root)),
        }
    }

    fn open_state() -> TerminalRestoreState {
        TerminalRestoreState {
            open: true,
            ..Default::default()
        }
    }

    /// `retain_ids` forgets state for ids not in the keep set, and a store
    /// reopened on the same db agrees.
    #[gpui::test]
    async fn test_retain_ids_prunes_and_persists(cx: &mut gpui::TestAppContext) {
        let db = KeyValueStore::open_test_db("terminal_restore_retain").await;
        let store = cx.new(|cx| TerminalRestoreStore::new(db.clone(), cx));
        store.update(cx, |store, cx| {
            store.set("keep", open_state(), cx);
            store.set("drop", open_state(), cx);
        });
        cx.run_until_parked();
        store.update(cx, |store, cx| {
            store.retain_ids(&HashSet::from(["keep".to_string()]), cx)
        });
        cx.run_until_parked();
        store.read_with(cx, |store, _| {
            assert!(store.has_state("keep"));
            assert!(!store.has_state("drop"));
        });
        let reopened = cx.new(|cx| TerminalRestoreStore::new(db, cx));
        reopened.read_with(cx, |store, _| {
            assert!(store.has_state("keep"));
            assert!(!store.has_state("drop"));
        });
    }

    /// A remote thread is never restored into a local workspace, nor a local
    /// one into a remote workspace; the same remote restores its own.
    #[gpui::test]
    async fn test_restore_filters_by_remote_connection(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let settings = settings::SettingsStore::test(cx);
            cx.set_global(settings);
            TerminalThreadMetadataStore::init_global(cx);
        });
        let db = KeyValueStore::open_test_db("terminal_restore_remote").await;
        let restore = cx.new(|cx| TerminalRestoreStore::new(db, cx));
        let remote = RemoteConnectionOptions::Mock(remote::MockConnectionOptions { id: 7 });
        let other_remote = RemoteConnectionOptions::Mock(remote::MockConnectionOptions { id: 8 });
        let local = meta("local", "/code/a", None);
        let remote_row = meta("remote", "/code/a", Some(remote.clone()));
        let store = cx.update(|cx| TerminalThreadMetadataStore::global(cx));
        let reload = store.read_with(cx, |store, _| store.reload_task());
        reload.await;
        for row in [&local, &remote_row] {
            store.update(cx, |store, cx| store.save((*row).clone(), cx));
            restore.update(cx, |restore, cx| {
                restore.set(&row.terminal_id.to_key_string(), open_state(), cx)
            });
        }
        cx.run_until_parked();
        let roots = [p("/code/a")];
        let ids = |remote: Option<&RemoteConnectionOptions>, cx: &mut gpui::TestAppContext| {
            store.read_with(cx, |store, cx| {
                entries_to_restore(store, restore.read(cx), &roots, remote, None)
                    .iter()
                    .map(|row| row.title.to_string())
                    .collect::<Vec<_>>()
            })
        };
        assert_eq!(ids(None, cx), vec!["local"]);
        assert_eq!(ids(Some(&remote), cx), vec!["remote"]);
        assert!(ids(Some(&other_remote), cx).is_empty());
    }

    /// The active terminal is restored by the load itself, so it is excluded
    /// from the extra rows (else a second shell would spawn), yet claimed so
    /// another panel does not restore it either; others are claimed once.
    #[gpui::test]
    async fn test_claim_skips_the_active_terminal_and_claims_once(cx: &mut gpui::TestAppContext) {
        let active = meta("active", "/code/a", None);
        let other = meta("other", "/code/a", None);
        let rows = vec![active.clone(), other.clone()];
        let db = KeyValueStore::open_test_db("terminal_restore_claim").await;
        let store = cx.new(|cx| TerminalRestoreStore::new(db, cx));
        let claimed = store.update(cx, |store, _| {
            store.claim_for_restore(rows.clone(), Some(active.terminal_id))
        });
        assert_eq!(
            claimed.iter().map(|r| r.terminal_id).collect::<Vec<_>>(),
            vec![other.terminal_id]
        );
        // Both are now claimed: a second panel restores nothing.
        assert!(
            store
                .update(cx, |store, _| store.claim_for_restore(rows, None))
                .is_empty()
        );
    }

    fn badges_of(list: &[&str]) -> Vec<String> {
        list.iter().map(|b| b.to_string()).collect()
    }

    /// Badges round-trip through the KVP db, replace, clear, and survive a
    /// `set` of the restore state (the persist-on-refresh path).
    #[gpui::test]
    async fn test_badges_set_clear_and_survive_state_set(cx: &mut gpui::TestAppContext) {
        let db = KeyValueStore::open_test_db("terminal_restore_badges").await;
        let store = cx.new(|cx| TerminalRestoreStore::new(db.clone(), cx));
        store.update(cx, |store, cx| {
            store.set_badges("a", badges_of(&["bg", "opus", "rc"]), cx);
            // Refreshing the restore state must not clobber the badges.
            store.set("a", open_state(), cx);
            store.set_badges("b", badges_of(&["fg"]), cx);
        });
        cx.run_until_parked();
        let reopened = cx.new(|cx| TerminalRestoreStore::new(db.clone(), cx));
        reopened.read_with(cx, |store, _| {
            assert_eq!(store.badges("a"), badges_of(&["bg", "opus", "rc"]));
            assert_eq!(store.badges("b"), badges_of(&["fg"]));
            assert!(store.badges("none").is_empty());
        });
        // Replace, then clear with an empty list.
        reopened.update(cx, |store, cx| {
            store.set_badges("a", badges_of(&["haiku"]), cx);
            store.set_badges("b", Vec::new(), cx);
        });
        cx.run_until_parked();
        let again = cx.new(|cx| TerminalRestoreStore::new(db, cx));
        again.read_with(cx, |store, _| {
            assert_eq!(store.badges("a"), badges_of(&["haiku"]));
            assert!(store.badges("b").is_empty());
        });
    }

    /// `set_badges` only persists and notifies on a real change: an identical
    /// list and a clear of nothing are no-ops; a replace and a real clear are
    /// not. Clearing removes the entry, so it is gone after a reopen.
    #[gpui::test]
    async fn test_set_badges_notifies_only_on_change(cx: &mut gpui::TestAppContext) {
        let db = KeyValueStore::open_test_db("terminal_restore_badges_notify").await;
        let store = cx.new(|cx| TerminalRestoreStore::new(db.clone(), cx));
        let notified = std::rc::Rc::new(std::cell::Cell::new(0usize));
        let counter = notified.clone();
        let _subscription =
            cx.update(|cx| cx.observe(&store, move |_, _| counter.set(counter.get() + 1)));
        let set = |cx: &mut gpui::TestAppContext, list: &[&str]| {
            store.update(cx, |store, cx| store.set_badges("a", badges_of(list), cx));
            cx.run_until_parked();
            notified.get()
        };
        assert_eq!(set(cx, &["bg", "opus"]), 1, "new badges notify");
        assert_eq!(set(cx, &["bg", "opus"]), 1, "identical list is a no-op");
        assert_eq!(set(cx, &["opus", "bg"]), 2, "reordering is a change");
        assert_eq!(set(cx, &["fg"]), 3, "replace notifies");
        assert_eq!(set(cx, &[]), 4, "clearing existing badges notifies");
        assert_eq!(set(cx, &[]), 4, "clearing nothing is a no-op");
        // Clearing REMOVES the entry (not an empty list left behind), and
        // clearing an id that never had badges adds none.
        set(cx, &[]);
        store.update(cx, |store, cx| store.set_badges("never", Vec::new(), cx));
        store.read_with(cx, |store, _| {
            assert!(!store.badges.contains_key("a"));
            assert!(!store.badges.contains_key("never"));
        });
        cx.run_until_parked();
        let reopened = cx.new(|cx| TerminalRestoreStore::new(db, cx));
        reopened.read_with(cx, |store, _| assert!(store.badges("a").is_empty()));
    }

    /// A badge-only thread is pruned with the others; kept ids keep theirs.
    #[gpui::test]
    async fn test_retain_ids_prunes_badges_too(cx: &mut gpui::TestAppContext) {
        let db = KeyValueStore::open_test_db("terminal_restore_badges_retain").await;
        let store = cx.new(|cx| TerminalRestoreStore::new(db.clone(), cx));
        store.update(cx, |store, cx| {
            store.set_badges("keep", badges_of(&["bg"]), cx);
            store.set_badges("drop", badges_of(&["bg"]), cx);
            store.retain_ids(&HashSet::from(["keep".to_string()]), cx);
        });
        store.read_with(cx, |store, _| {
            assert_eq!(store.badges("keep"), badges_of(&["bg"]));
            assert!(store.badges("drop").is_empty());
        });
    }

    /// A blob written before badges existed (no `badges` key) still loads.
    #[gpui::test]
    async fn test_old_blob_without_badges_loads(cx: &mut gpui::TestAppContext) {
        let db = KeyValueStore::open_test_db("terminal_restore_old_blob").await;
        db.scoped(KVP_SCOPE)
            .write(
                KVP_KEY.to_string(),
                r#"{"version":1,"threads":{"x":{"open":true}}}"#.to_string(),
            )
            .await
            .unwrap();
        let store = cx.new(|cx| TerminalRestoreStore::new(db, cx));
        store.read_with(cx, |store, _| {
            assert!(store.state("x").open);
            assert!(store.badges("x").is_empty());
        });
    }
}
