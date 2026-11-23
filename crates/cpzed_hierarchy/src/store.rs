//! `HierarchyStore`: the single gpui entity that owns the [`Hierarchy`], reached through
//! `HierarchyStore::global(cx)` by every window's sidebar and by the CLI handler.
//!
//! Persistence: KVP scope [`KVP_SCOPE`], key [`KVP_KEY`], JSON. One debounced write task (single
//! writer, like `MultiWorkspace::serialize`): every edit replaces the pending task, so a burst of
//! edits produces one write.

use std::time::Duration;

use db::kvp::KeyValueStore;
use gpui::{App, AppContext as _, Context, Entity, Global, Subscription, Task};
use util::ResultExt as _;

use crate::{
    Container, GroupKey, Hierarchy, LoadOutcome, Refusal, Repair, RowKey, SectionId, Side,
};

pub const KVP_SCOPE: &str = "cpzed_sidebar_hierarchy";
pub const KVP_KEY: &str = "v2";
pub const WRITE_DEBOUNCE: Duration = Duration::from_millis(500);
/// `touch_seen` only rewrites an entry that is older than this, so a sidebar rebuild that sees
/// the same rows again is free (no write, no notify).
const TOUCH_GRANULARITY_SECS: i64 = 60 * 60;
const ONE_DAY_SECS: i64 = 24 * 60 * 60;

struct GlobalHierarchyStore(Entity<HierarchyStore>);

impl Global for GlobalHierarchyStore {}

pub struct HierarchyStore {
    hierarchy: Hierarchy,
    db: KeyValueStore,
    write_task: Option<Task<()>>,
    writes_started: usize,
    /// Edits not yet written.
    dirty: bool,
    /// The stored blob came from a newer build: never write, so a rollback cannot wipe it.
    read_only: bool,
    /// True once `touch_seen` ran: `prune` is a no-op before that (see [`Hierarchy::prune`]).
    touched: bool,
    /// Logical-clock skew (see [`Self::logical_now`]); fixed on first use in a session.
    clock_skew: Option<i64>,
    _quit_subscription: Option<Subscription>,
}

impl HierarchyStore {
    /// Create (once) and register the global store, loading it from the app database.
    pub fn init(cx: &mut App) -> Entity<Self> {
        if let Some(existing) = Self::try_global(cx) {
            return existing;
        }
        let db = KeyValueStore::global(cx);
        let store = cx.new(|cx| Self::new(db, cx));
        cx.set_global(GlobalHierarchyStore(store.clone()));
        store
    }

    pub fn global(cx: &App) -> Entity<Self> {
        cx.global::<GlobalHierarchyStore>().0.clone()
    }

    pub fn try_global(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<GlobalHierarchyStore>().map(|g| g.0.clone())
    }

    /// The key under which an unreadable stored blob is preserved before it is replaced.
    pub fn backup_key(blob: &str) -> String {
        format!("{KVP_KEY}.backup.{}", Hierarchy::backup_suffix(blob))
    }

    /// Load from `db`.
    ///
    /// - Nothing stored: empty.
    /// - Readable v2 blob: loaded, invariants repaired (cycles, duplicates; the group check needs
    ///   [`Self::validate`] with live groups).
    /// - Unreadable or older blob (v1, garbage, an unknown RowKey variant): empty, and the raw blob
    ///   is first copied to [`Self::backup_key`] (queued ahead of any later write).
    /// - Written by a NEWER version: empty and READ-ONLY. Nothing is ever written, so rolling back
    ///   a deploy does not destroy the newer build's data.
    pub fn new(db: KeyValueStore, cx: &mut Context<Self>) -> Self {
        let stored = db.scoped(KVP_SCOPE).read(KVP_KEY);
        Self::from_stored(db, stored, cx)
    }

    /// [`Self::new`] with the stored blob already read. A read ERROR (busy, I/O) is not "missing":
    /// the blob may be intact, so the store goes read-only rather than overwrite it.
    pub fn from_stored(
        db: KeyValueStore,
        stored: anyhow::Result<Option<String>>,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut read_only = false;
        let stored = match stored {
            Ok(stored) => stored,
            Err(error) => {
                log::error!(
                    "cpzed_hierarchy: could not read the stored hierarchy ({error:#}); staying read-only"
                );
                read_only = true;
                None
            }
        };
        let mut backup: Option<String> = None;
        let mut hierarchy = match if read_only {
            LoadOutcome::Missing
        } else {
            Hierarchy::load(stored.as_deref())
        } {
            LoadOutcome::Missing => Hierarchy::default(),
            LoadOutcome::Loaded(hierarchy) => hierarchy,
            LoadOutcome::Unreadable { reason } => {
                log::warn!(
                    "cpzed_hierarchy: stored hierarchy unreadable ({reason}); backing it up and starting empty"
                );
                backup = stored;
                Hierarchy::default()
            }
            LoadOutcome::FutureVersion(version) => {
                log::warn!(
                    "cpzed_hierarchy: stored version {version} is newer than this build; staying read-only"
                );
                read_only = true;
                Hierarchy::default()
            }
        };
        if let Some(blob) = backup {
            // Synchronous (rare path): the backup is durable before any v2 write can be queued.
            // If it cannot be written, the original must not be replaced: go read-only.
            let key = Self::backup_key(&blob);
            let written = gpui::block_on(db.scoped(KVP_SCOPE).write(key, blob));
            if let Err(error) = written {
                log::error!(
                    "cpzed_hierarchy: could not back up the unreadable blob ({error:#}); staying read-only"
                );
                read_only = true;
            }
        }
        let repairs = hierarchy.validate(&|_| None);
        let quit = cx.on_app_quit(|this, cx| this.flush_if_dirty(cx));
        let mut this = Self {
            hierarchy,
            db,
            write_task: None,
            writes_started: 0,
            dirty: false,
            read_only,
            touched: false,
            clock_skew: None,
            _quit_subscription: Some(quit),
        };
        if !repairs.is_empty() {
            log::warn!("cpzed_hierarchy: repaired stored hierarchy on load: {repairs:?}");
            this.schedule_write(cx);
        }
        this
    }

    /// True when the stored data came from a newer build, or could not be read or backed up. Every
    /// mutating op then returns [`Refusal::ReadOnly`] and nothing is ever written; the sidebar
    /// should say so.
    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    pub fn hierarchy(&self) -> &Hierarchy {
        &self.hierarchy
    }

    /// Number of persistence writes started so far (for tests and diagnostics).
    pub fn write_count(&self) -> usize {
        self.writes_started
    }

    /// Run a refusable edit. Notifies observers and schedules a write only when it succeeds.
    pub fn edit<T>(
        &mut self,
        cx: &mut Context<Self>,
        edit: impl FnOnce(&mut Hierarchy) -> Result<T, Refusal>,
    ) -> Result<T, Refusal> {
        if self.read_only {
            return Err(Refusal::ReadOnly);
        }
        let result = edit(&mut self.hierarchy)?;
        self.schedule_write(cx);
        cx.notify();
        Ok(result)
    }

    fn schedule_write(&mut self, cx: &mut Context<Self>) {
        if self.read_only {
            return;
        }
        self.dirty = true;
        let db = self.db.clone();
        self.write_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(WRITE_DEBOUNCE).await;
            let json = this
                .update(cx, |this, _| {
                    this.writes_started += 1;
                    this.dirty = false;
                    this.hierarchy.to_json()
                })
                .ok();
            if let Some(json) = json.and_then(|json| json.log_err()) {
                db.scoped(KVP_SCOPE)
                    .write(KVP_KEY.to_string(), json)
                    .await
                    .log_err();
            }
        }));
    }

    /// Write now, cancelling any pending debounced write (for quit and tests).
    pub fn flush(&mut self, cx: &mut Context<Self>) -> Task<()> {
        if self.read_only {
            return Task::ready(());
        }
        self.write_task = None;
        self.dirty = false;
        self.writes_started += 1;
        let db = self.db.clone();
        let json = self.hierarchy.to_json().log_err();
        cx.background_spawn(async move {
            if let Some(json) = json {
                db.scoped(KVP_SCOPE)
                    .write(KVP_KEY.to_string(), json)
                    .await
                    .log_err();
            }
        })
    }

    /// Flush only when there are unwritten edits. Registered as the app-quit hook, so the last
    /// <= [`WRITE_DEBOUNCE`] of edits is not lost; sidebars may also call it on window close.
    pub fn flush_if_dirty(&mut self, cx: &mut Context<Self>) -> Task<()> {
        if self.dirty {
            self.flush(cx)
        } else {
            Task::ready(())
        }
    }

    // ---- edits (each calls the pure op, then notifies) ------------------------------------

    pub fn create_section(
        &mut self,
        group: &GroupKey,
        name: &str,
        cx: &mut Context<Self>,
    ) -> Result<(SectionId, bool), Refusal> {
        if self.read_only {
            return Err(Refusal::ReadOnly);
        }
        // Idempotent: creating an existing name changes nothing, so it must not notify.
        match self.hierarchy.clone_section_id(group, name) {
            Some(existing) => Ok((existing, false)),
            None => self.edit(cx, |h| h.create_section(group, name)),
        }
    }

    pub fn rename_section(
        &mut self,
        group: &GroupKey,
        id: &SectionId,
        name: &str,
        cx: &mut Context<Self>,
    ) -> Result<(), Refusal> {
        self.edit(cx, |h| h.rename_section(group, id, name))
    }

    pub fn delete_section(
        &mut self,
        group: &GroupKey,
        id: &SectionId,
        cx: &mut Context<Self>,
    ) -> Result<Vec<RowKey>, Refusal> {
        self.edit(cx, |h| h.delete_section(group, id))
    }

    pub fn move_section(
        &mut self,
        group: &GroupKey,
        id: &SectionId,
        target_group: &GroupKey,
        target: &SectionId,
        side: Side,
        cx: &mut Context<Self>,
    ) -> Result<(), Refusal> {
        self.edit(cx, |h| {
            h.move_section(group, id, target_group, target, side)
        })
    }

    pub fn set_section_collapsed(
        &mut self,
        group: &GroupKey,
        id: &SectionId,
        collapsed: bool,
        cx: &mut Context<Self>,
    ) -> Result<(), Refusal> {
        self.edit(cx, |h| h.set_section_collapsed(group, id, collapsed))
    }

    pub fn set_section(
        &mut self,
        row: &RowKey,
        row_group: &GroupKey,
        section: Option<&SectionId>,
        cx: &mut Context<Self>,
    ) -> Result<(), Refusal> {
        self.edit(cx, |h| h.set_section(row, row_group, section))
    }

    pub fn set_parent(
        &mut self,
        child: &RowKey,
        parent: Option<&RowKey>,
        cx: &mut Context<Self>,
    ) -> Result<(), Refusal> {
        self.edit(cx, |h| h.set_parent(child, parent))
    }

    pub fn move_row(
        &mut self,
        row: &RowKey,
        row_group: &GroupKey,
        target: &RowKey,
        side: Side,
        container: &Container,
        visible: &[RowKey],
        cx: &mut Context<Self>,
    ) -> Result<(), Refusal> {
        self.edit(cx, |h| {
            h.move_row(row, row_group, target, side, container, visible)
        })
    }

    pub fn cross_project_move(
        &mut self,
        row: &RowKey,
        from: &GroupKey,
        to: &GroupKey,
        cx: &mut Context<Self>,
    ) -> Result<(), Refusal> {
        self.edit(cx, |h| {
            h.cross_project_move(row, from, to);
            Ok(())
        })
    }

    pub fn set_row_collapsed(
        &mut self,
        row: &RowKey,
        collapsed: bool,
        cx: &mut Context<Self>,
    ) -> Result<(), Refusal> {
        if self.read_only {
            return Err(Refusal::ReadOnly);
        }
        if self.hierarchy.collapsed_rows.contains(row) == collapsed {
            return Ok(());
        }
        self.edit(cx, |h| {
            h.set_row_collapsed(row, collapsed);
            Ok(())
        })
    }

    /// Activity-relative time. Retention counts days of use, not wall-clock days: after a long
    /// absence (or a forward clock jump) the first call fixes a skew so that logical time is at
    /// most one day past the newest recorded sighting. Rows and projects the user simply has not
    /// opened yet this session therefore are not "30 days stale" because of the gap. The skew then
    /// stays constant, so logical time advances at wall-clock speed while the app is used.
    fn logical_now(&mut self, now: i64) -> i64 {
        let skew = *self.clock_skew.get_or_insert_with(|| {
            let newest = self
                .hierarchy
                .last_seen
                .values()
                .chain(self.hierarchy.group_last_seen.values())
                .copied()
                .filter(|seen| *seen <= now)
                .max();
            newest.map_or(0, |newest| (now - newest - ONE_DAY_SECS).max(0))
        });
        now - skew
    }

    /// Record that rows and groups were seen. Quiet: never notifies (the sidebar calls this while
    /// rebuilding, so notifying would loop), and a no-op unless an entry is over an hour old.
    pub fn touch_seen(
        &mut self,
        rows: &[RowKey],
        groups: &[GroupKey],
        now: i64,
        cx: &mut Context<Self>,
    ) {
        self.touched = true;
        if self.read_only {
            return;
        }
        let now = self.logical_now(now);
        let stale =
            |seen: Option<&i64>| seen.is_none_or(|seen| now - *seen >= TOUCH_GRANULARITY_SECS);
        let rows: Vec<RowKey> = rows
            .iter()
            .filter(|r| stale(self.hierarchy.last_seen.get(*r)))
            .cloned()
            .collect();
        let groups: Vec<GroupKey> = groups
            .iter()
            .filter(|g| stale(self.hierarchy.group_last_seen.get(*g)))
            .cloned()
            .collect();
        if rows.is_empty() && groups.is_empty() {
            return;
        }
        self.hierarchy.touch_seen(&rows, &groups, now);
        self.schedule_write(cx);
    }

    /// Lazy 30-day prune. Notifies only if something was removed. A no-op until `touch_seen` has
    /// run in this session (callers must record the visible rows first; see [`Hierarchy::prune`]).
    pub fn prune(&mut self, now: i64, cx: &mut Context<Self>) -> usize {
        if !self.touched || self.read_only {
            return 0;
        }
        let now = self.logical_now(now);
        let pruned = self.hierarchy.prune(now);
        if pruned > 0 {
            self.schedule_write(cx);
            cx.notify();
        }
        pruned
    }

    /// Repair invariants against the live row -> group mapping (after re-homing).
    pub fn validate(
        &mut self,
        group_of: &dyn Fn(&RowKey) -> Option<GroupKey>,
        cx: &mut Context<Self>,
    ) -> Vec<Repair> {
        if self.read_only {
            return Vec::new();
        }
        let repairs = self.hierarchy.validate(group_of);
        if !repairs.is_empty() {
            log::warn!("cpzed_hierarchy: repaired hierarchy: {repairs:?}");
            self.schedule_write(cx);
            cx.notify();
        }
        repairs
    }
}
