//! Handler for `zed --agent-new`: resolves which open workspace owns the
//! requested working directory and spawns a terminal thread there, without
//! activating, focusing or switching any window.
//!
//! Design: `designs/cpzed-3-agent-new.md` (fork-private).
//!
//! Target resolution: the deepest open workspace root containing the (canonical)
//! cwd wins. Otherwise, if cwd is inside a git work tree whose repository is
//! already open (same git common dir, e.g. only a linked worktree of it is
//! open), a workspace for the cwd's git *toplevel* (never the raw cwd or a
//! subdirectory) is added to that repo's window with `OpenMode::Add`, joining
//! the repo's sidebar group. A cwd that is neither inside an open root nor in
//! an open repo is "no match" (exit 3 by default).
//!
//! The success reply is one JSON line: `id`, `kind`, `title` (the `--title`
//! given, else null; never the live shell title), `working_directory`,
//! `folder_paths`, `window_id`, `workspace_id`.
//!
//! Known limitation (tracked as a follow-up): when the CLI launches Zed
//! itself, this handler can run before session restore has reopened the user's
//! windows. A cwd inside a project that restore is about to bring back then
//! reports "no match" (exit 3), and `--open-project` / `--new-window` can open
//! a duplicate. Fixing it needs the startup path in `main.rs`, which this
//! module does not own.

use agent_ui::terminal_thread_metadata_store::TerminalThreadMetadataStore;
use agent_ui::{AgentPanel, AgentPanelEvent, TerminalId};
use cli::{CliResponse, CliResponseSink, NoMatchBehavior, ParentChange, SectionChange};
use cpzed_hierarchy::{GroupKey, HierarchyStore, Refusal, RowKey};
use futures::channel::mpsc;
use futures::{FutureExt, StreamExt, select_biased};
use gpui::{App, AppContext as _, AsyncApp, Entity, WindowHandle};
use project::DisableAiSettings;
use settings::Settings as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use util::ResultExt as _;
use workspace::{AppState, MultiWorkspace, OpenMode, Workspace};

/// Exit codes, as documented in the design.
const EXIT_INTERNAL: i32 = 1;
const EXIT_NOT_CONFIRMED: i32 = 2;
const EXIT_NO_MATCH: i32 = 3;
const EXIT_UNSUPPORTED: i32 = 4;
/// `--agent-close --title` matched several threads; none was closed.
const EXIT_AMBIGUOUS: i32 = 5;
/// Longest accepted `--agent-rename` title, in characters.
const MAX_TITLE_CHARS: usize = 200;
/// The id belongs to an agent-panel (non-terminal) thread; refused.
const EXIT_NOT_TERMINAL: i32 = 7;
/// The hierarchy refused the change (a cycle, an invalid section name, ...).
const EXIT_REFUSED: i32 = 6;

/// Serializes `--agent-new` requests: `AgentPanel::pending_terminal_spawn`
/// holds a single slot, so two overlapping spawns could let the panel create a
/// stray initial terminal. Also makes "create the linked-worktree workspace"
/// idempotent across concurrent requests.
static SPAWN_LOCK: smol::lock::Mutex<()> = smol::lock::Mutex::new(());

#[derive(Clone, Copy)]
pub struct Timeouts {
    /// Waiting for the new terminal to show up in the panel.
    pub confirm: Duration,
    /// Waiting for the terminal metadata store to load.
    pub metadata: Duration,
    /// Waiting for a freshly added workspace to register its agent panel.
    pub panel_load: Duration,
    /// Running `git rev-parse` for the linked-worktree fallback.
    pub git: Duration,
    /// The `git` executable (overridable so tests can fake a hung git).
    pub git_program: &'static str,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            confirm: Duration::from_secs(15),
            metadata: Duration::from_secs(5),
            panel_load: Duration::from_secs(10),
            git: Duration::from_secs(5),
            git_program: "git",
        }
    }
}

struct Failure {
    status: i32,
    message: String,
}

impl Failure {
    fn new(status: i32, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    fn internal(message: impl Into<String>) -> Self {
        Self::new(EXIT_INTERNAL, message)
    }
}

pub async fn handle_new_agent_terminal(
    cwd: PathBuf,
    command: Option<String>,
    title: Option<String>,
    on_no_match: NoMatchBehavior,
    parent: Option<String>,
    section: Option<String>,
    app_state: Arc<AppState>,
    responses: &dyn CliResponseSink,
    cx: &mut AsyncApp,
) {
    handle_new_agent_terminal_with_timeouts(
        cwd,
        command,
        title,
        on_no_match,
        parent,
        section,
        app_state,
        Timeouts::default(),
        responses,
        cx,
    )
    .await
}

pub async fn handle_new_agent_terminal_with_timeouts(
    cwd: PathBuf,
    command: Option<String>,
    title: Option<String>,
    on_no_match: NoMatchBehavior,
    parent: Option<String>,
    section: Option<String>,
    app_state: Arc<AppState>,
    timeouts: Timeouts,
    responses: &dyn CliResponseSink,
    cx: &mut AsyncApp,
) {
    // Never hold the spawn lock when replying: the reply can block on IPC.
    let outcome = run(
        cwd,
        command,
        title,
        on_no_match,
        parent,
        section,
        app_state,
        timeouts,
        cx,
    )
    .await;
    let status = match outcome {
        Ok(message) => {
            responses.send(CliResponse::Stdout { message }).log_err();
            0
        }
        Err(failure) => {
            responses
                .send(CliResponse::Stderr {
                    message: failure.message,
                })
                .log_err();
            failure.status
        }
    };
    responses.send(CliResponse::Exit { status }).log_err();
}

async fn run(
    cwd: PathBuf,
    command: Option<String>,
    title: Option<String>,
    on_no_match: NoMatchBehavior,
    parent: Option<String>,
    section: Option<String>,
    app_state: Arc<AppState>,
    timeouts: Timeouts,
    cx: &mut AsyncApp,
) -> Result<String, Failure> {
    // The command is typed into the shell verbatim; a newline would run
    // several commands (and an embedded CR can submit early). Reject.
    if let Some(command) = &command
        && command.contains(['\n', '\r'])
    {
        return Err(Failure::internal(
            "--command must be a single line (it is typed into the shell); \
             it contains a newline or carriage return",
        ));
    }
    if !cwd.is_absolute() {
        return Err(Failure::internal(format!(
            "working directory must be absolute: {}",
            cwd.display()
        )));
    }

    if cx.update(|cx| DisableAiSettings::get_global(cx).disable_ai) {
        return Err(Failure::new(
            EXIT_UNSUPPORTED,
            "AI features are disabled (disable_ai); cannot create an agent terminal",
        ));
    }

    // Validate the hierarchy request before anything is created, so a bad
    // `--parent` or an unwritable store never leaves a stray thread behind.
    let hierarchy = if parent.is_some() || section.is_some() {
        if let Some(name) = &section {
            validate_section_name(name)?;
        }
        let store = cx.update(|cx| HierarchyStore::init(cx));
        if cx.update(|cx| store.read(cx).is_read_only()) {
            return Err(refusal_failure(Refusal::ReadOnly));
        }
        if let Some(parent) = &parent {
            let metadata = cx.update(|cx| TerminalThreadMetadataStore::try_global(cx));
            find_terminal(metadata.as_ref(), parent, timeouts, cx)
                .await?
                .ok_or_else(|| {
                    Failure::new(EXIT_NO_MATCH, format!("unknown parent thread id: {parent}"))
                })?;
        }
        Some(store)
    } else {
        None
    };

    let _guard = SPAWN_LOCK.lock().await;
    #[cfg(test)]
    let _probe = tests::ConcurrencyProbe::enter();

    let target = resolve_target(&cwd, on_no_match, &app_state, timeouts, cx).await?;
    let panel = wait_for_panel(&target, timeouts.panel_load, cx).await?;
    let (terminal_id, mut reply) =
        spawn_and_confirm(&target, &panel, cwd, command, title, timeouts, cx).await?;
    drop(_guard);

    if let Some(store) = hierarchy {
        let id = terminal_id.to_string();
        let row = RowKey::Terminal(id.clone());
        let metadata = cx.update(|cx| TerminalThreadMetadataStore::try_global(cx));
        let group = match find_terminal(metadata.as_ref(), &id, timeouts, cx).await {
            Ok(Some(group)) => group,
            _ => group_key_from_reply(&reply),
        };
        let applied = cx.update(|cx| {
            store.update(cx, |store, cx| {
                store.edit(cx, |h| {
                    let mut created = None;
                    if let Some(parent) = &parent {
                        h.set_parent(&row, Some(&RowKey::Terminal(parent.clone())))?;
                    }
                    if let Some(name) = &section {
                        let (section_id, was_created) = h.create_section(&group, name)?;
                        h.set_section(&row, &group, Some(&section_id))?;
                        created = Some(was_created);
                    }
                    Ok(created)
                })
            })
        });
        match applied {
            Ok(created) => {
                reply["parent"] = parent.map_or(serde_json::Value::Null, Into::into);
                if let Some(name) = section {
                    reply["section"] = name.into();
                    reply["section_created"] = created.unwrap_or(false).into();
                }
            }
            Err(refusal) => {
                let mut failure = refusal_failure(refusal);
                failure.message = format!(
                    "{} (the terminal thread {id} was created, but not placed)",
                    failure.message
                );
                return Err(failure);
            }
        }
    }
    Ok(reply.to_string())
}

fn refusal_failure(refusal: Refusal) -> Failure {
    match refusal {
        Refusal::ReadOnly => Failure::internal(refusal.to_string()),
        other => Failure::new(EXIT_REFUSED, other.to_string()),
    }
}

/// The project group of a terminal thread: its main worktree paths (the
/// project it belongs to even from a linked worktree), else its folder paths.
/// A local host (`None`); remote projects are out of scope (CPZED-10).
fn group_key(
    metadata: &agent_ui::terminal_thread_metadata_store::TerminalThreadMetadata,
) -> GroupKey {
    let main = metadata.main_worktree_paths();
    let paths = if main.paths().is_empty() {
        metadata.folder_paths()
    } else {
        main
    };
    GroupKey::new(None, paths.paths().to_vec())
}

/// `folder_paths` of a spawn reply, as a fallback group key.
fn group_key_from_reply(reply: &serde_json::Value) -> GroupKey {
    let paths = reply["folder_paths"]
        .as_array()
        .map(|paths| {
            paths
                .iter()
                .filter_map(|path| path.as_str().map(PathBuf::from))
                .collect()
        })
        .unwrap_or_default();
    GroupKey::new(None, paths)
}

/// Longest section name accepted by the CLI (the hierarchy crate only rejects
/// empty names).
const MAX_SECTION_NAME_CHARS: usize = 200;

/// Checked before anything is created, so a bad name never leaves a stray thread.
fn validate_section_name(name: &str) -> Result<(), Failure> {
    if name.trim().is_empty() {
        return Err(Failure::new(EXIT_REFUSED, Refusal::EmptyName.to_string()));
    }
    if name.trim().chars().count() > MAX_SECTION_NAME_CHARS {
        return Err(Failure::new(
            EXIT_REFUSED,
            format!("section names are limited to {MAX_SECTION_NAME_CHARS} characters"),
        ));
    }
    Ok(())
}

/// Looks a terminal thread up by its id string, after the metadata store has
/// loaded. `Ok(None)` = unknown id (exit 3); a store that is missing or did not
/// load in time is an error (exit 1 / 2), not "unknown".
async fn find_terminal(
    store: Option<&Entity<TerminalThreadMetadataStore>>,
    id: &str,
    timeouts: Timeouts,
    cx: &mut AsyncApp,
) -> Result<Option<GroupKey>, Failure> {
    let store = store.ok_or_else(|| Failure::internal("agent thread store is not initialized"))?;
    let reload = cx.update(|cx| store.read(cx).reload_task());
    if bounded(reload, timeouts.metadata, cx).await.is_none() {
        return Err(Failure::new(
            EXIT_NOT_CONFIRMED,
            "timed out loading agent thread metadata",
        ));
    }
    Ok(cx.update(|cx| {
        store
            .read(cx)
            .entries()
            .find(|metadata| metadata.terminal_id.to_string() == id)
            .map(group_key)
    }))
}

/// `--agent-set-parent` / `--agent-set-section`.
pub async fn handle_update_agent_thread(
    id: String,
    parent: Option<ParentChange>,
    section: Option<SectionChange>,
    responses: &dyn CliResponseSink,
    cx: &mut AsyncApp,
) {
    handle_update_agent_thread_with_timeouts(
        id,
        parent,
        section,
        Timeouts::default(),
        responses,
        cx,
    )
    .await
}

pub async fn handle_update_agent_thread_with_timeouts(
    id: String,
    parent: Option<ParentChange>,
    section: Option<SectionChange>,
    timeouts: Timeouts,
    responses: &dyn CliResponseSink,
    cx: &mut AsyncApp,
) {
    let status = match update_thread(id, parent, section, timeouts, cx).await {
        Ok(message) => {
            responses.send(CliResponse::Stdout { message }).log_err();
            0
        }
        Err(failure) => {
            responses
                .send(CliResponse::Stderr {
                    message: failure.message,
                })
                .log_err();
            failure.status
        }
    };
    responses.send(CliResponse::Exit { status }).log_err();
}

async fn update_thread(
    id: String,
    parent: Option<ParentChange>,
    section: Option<SectionChange>,
    timeouts: Timeouts,
    cx: &mut AsyncApp,
) -> Result<String, Failure> {
    let metadata = cx.update(|cx| TerminalThreadMetadataStore::try_global(cx));
    let group = find_terminal(metadata.as_ref(), &id, timeouts, cx)
        .await?
        .ok_or_else(|| Failure::new(EXIT_NO_MATCH, format!("unknown thread id: {id}")))?;
    if let Some(SectionChange::Set(name)) = &section {
        validate_section_name(name)?;
    }
    let row = RowKey::Terminal(id.clone());

    if let Some(ParentChange::Set(parent_id)) = &parent {
        find_terminal(metadata.as_ref(), parent_id, timeouts, cx)
            .await?
            .ok_or_else(|| {
                Failure::new(
                    EXIT_NO_MATCH,
                    format!("unknown parent thread id: {parent_id}"),
                )
            })?;
    }

    let store = cx.update(|cx| HierarchyStore::init(cx));
    let result = cx.update(|cx| {
        store.update(cx, |store, cx| {
            store.edit(cx, |h| {
                let mut created = None;
                match &parent {
                    Some(ParentChange::Set(parent_id)) => {
                        h.set_parent(&row, Some(&RowKey::Terminal(parent_id.clone())))?
                    }
                    Some(ParentChange::Clear) => h.set_parent(&row, None)?,
                    None => {}
                }
                match &section {
                    Some(SectionChange::Set(name)) => {
                        let (section_id, was_created) = h.create_section(&group, name)?;
                        h.set_section(&row, &group, Some(&section_id))?;
                        created = Some(was_created);
                    }
                    Some(SectionChange::Clear) => h.set_section(&row, &group, None)?,
                    None => {}
                }
                Ok(created)
            })
        })
    });
    let created = result.map_err(refusal_failure)?;

    let mut reply = serde_json::json!({ "id": id });
    match &parent {
        Some(ParentChange::Set(parent_id)) => reply["parent"] = parent_id.clone().into(),
        Some(ParentChange::Clear) => reply["parent"] = serde_json::Value::Null,
        None => {}
    }
    match &section {
        Some(SectionChange::Set(name)) => {
            reply["section"] = name.clone().into();
            reply["section_created"] = created.unwrap_or(false).into();
        }
        Some(SectionChange::Clear) => reply["section"] = serde_json::Value::Null,
        None => {}
    }
    Ok(reply.to_string())
}

/// Adds `parent`, `section` and `depth` to each row of the `--agent-list`
/// JSON array, from the hierarchy store. A missing store reads as an empty
/// hierarchy (all roots).
pub fn annotate_agent_list(list: &mut serde_json::Value, cx: &App) {
    let store = HierarchyStore::try_global(cx);
    let hierarchy = store.as_ref().map(|store| store.read(cx).hierarchy());
    let Some(rows) = list.as_array_mut() else {
        return;
    };
    for row in rows {
        let Some(id) = row["id"].as_str().map(str::to_string) else {
            continue;
        };
        let key = RowKey::Terminal(id);
        let (parent, section, depth) = match hierarchy {
            Some(h) => {
                let parent = h.parents.get(&key).map(|parent| match parent {
                    RowKey::Terminal(id) => (id.clone(), "terminal"),
                    RowKey::Thread(id) => (id.clone(), "thread"),
                });
                let section = h.section_of(&key).and_then(|(group, section_id)| {
                    h.section(group, section_id).map(|s| s.name.clone())
                });
                // Walk the ancestors with a visited guard (a stored cycle is
                // repaired on load, but never loop on one).
                let mut depth = 0usize;
                let mut seen = std::collections::BTreeSet::new();
                let mut cursor = &key;
                while let Some(parent) = h.parents.get(cursor) {
                    if !seen.insert(parent) {
                        break;
                    }
                    depth += 1;
                    cursor = parent;
                }
                (parent, section, depth)
            }
            None => (None, None, 0),
        };
        // `parent_kind` disambiguates ids: "terminal" (a row of this list) or
        // "thread" (a UI-made agent thread, not listed here).
        row["parent_kind"] = parent
            .as_ref()
            .map_or(serde_json::Value::Null, |(_, kind)| (*kind).into());
        row["parent"] = parent.map_or(serde_json::Value::Null, |(id, _)| id.into());
        row["section"] = section.map_or(serde_json::Value::Null, Into::into);
        row["depth"] = depth.into();
    }
}

#[derive(Clone)]
struct Target {
    window: WindowHandle<MultiWorkspace>,
    workspace: Entity<Workspace>,
}

/// Plain-data view of an open local workspace, used for scoring off the
/// foreground thread.
#[derive(Debug, Clone)]
struct RawCandidate {
    roots: Vec<PathBuf>,
    displayed: bool,
    active_window: bool,
}

/// Linked worktrees (and the common git dir) reported by one open workspace's
/// repositories.
#[derive(Debug, Clone, Default)]
struct RawLinked {
    linked_roots: Vec<PathBuf>,
    common_dirs: Vec<PathBuf>,
}

async fn resolve_target(
    cwd: &Path,
    on_no_match: NoMatchBehavior,
    app_state: &Arc<AppState>,
    timeouts: Timeouts,
    cx: &mut AsyncApp,
) -> Result<Target, Failure> {
    // Phase 1 (foreground, read-only): snapshot every open local workspace.
    let (handles, mut raw, mut linked) = cx.update(|cx| collect_candidates(cx));

    // Phase 2 (background): canonicalize both sides. UI-opened roots may be
    // non-canonical and macOS maps /tmp to /private/tmp.
    let cwd_owned = cwd.to_path_buf();
    let (canonical_cwd, raw_out, linked_out) = cx
        .background_spawn(async move {
            for candidate in &mut raw {
                candidate.roots = candidate
                    .roots
                    .drain(..)
                    .map(|root| canonical_or_self(&root))
                    .collect();
            }
            for entry in &mut linked {
                entry.linked_roots = entry
                    .linked_roots
                    .drain(..)
                    .map(|root| canonical_or_self(&root))
                    .collect();
                entry.common_dirs = entry
                    .common_dirs
                    .drain(..)
                    .map(|dir| canonical_or_self(&dir))
                    .collect();
            }
            (canonical_or_self(&cwd_owned), raw, linked)
        })
        .await;
    raw = raw_out;
    linked = linked_out;

    // Step 1: deepest open workspace root containing cwd, across all windows.
    if let Some(index) = pick_best_candidate(&canonical_cwd, &raw) {
        let (window, workspace) = handles[index].clone();
        return Ok(Target { window, workspace });
    }

    // Step 2: cwd is inside a linked worktree of a repo that is open. Add a
    // workspace for that worktree to the repo's window without activating it.
    let mut linked_match = pick_linked_root(&canonical_cwd, &linked);
    if linked_match.is_none() {
        // Snapshots may not list linked worktrees (e.g. bare-repo layouts);
        // ask git directly and compare common dirs.
        let git_result = bounded(
            git_toplevel_and_common_dir(timeouts.git_program, &canonical_cwd),
            timeouts.git,
            cx,
        )
        .await
        .flatten();
        if let Some((toplevel, common_dir)) = git_result {
            let common_dir = canonical_or_self(&common_dir);
            if let Some(index) = pick_by_common_dir(&common_dir, &linked) {
                linked_match = Some((index, canonical_or_self(&toplevel)));
            }
        }
    }
    if let Some((index, root)) = linked_match {
        let (window, _) = handles[index].clone();
        return open_workspace(vec![root], Some(window), OpenMode::Add, app_state, cx).await;
    }

    // Step 3: nothing matches.
    let (window, mode) = match on_no_match {
        NoMatchBehavior::Error => {
            return Err(Failure::new(
                EXIT_NO_MATCH,
                format!(
                    "no open Zed project contains {}; pass --open-project to add it",
                    cwd.display()
                ),
            ));
        }
        NoMatchBehavior::OpenProject => {
            let window = cx.update(|cx| {
                let active = cx
                    .active_window()
                    .and_then(|window| window.downcast::<MultiWorkspace>());
                active.or_else(|| {
                    cx.windows()
                        .into_iter()
                        .find_map(|window| window.downcast::<MultiWorkspace>())
                })
            });
            match window {
                Some(window) => (Some(window), OpenMode::Add),
                None => (None, OpenMode::NewWindow),
            }
        }
        NoMatchBehavior::NewWindow => (None, OpenMode::NewWindow),
    };
    open_workspace(vec![cwd.to_path_buf()], window, mode, app_state, cx).await
}

fn collect_candidates(
    cx: &App,
) -> (
    Vec<(WindowHandle<MultiWorkspace>, Entity<Workspace>)>,
    Vec<RawCandidate>,
    Vec<RawLinked>,
) {
    let active_window = cx.active_window().map(|window| window.window_id());
    let mut handles = Vec::new();
    let mut raw = Vec::new();
    let mut linked = Vec::new();
    for window in cx.windows() {
        let Some(window) = window.downcast::<MultiWorkspace>() else {
            continue;
        };
        let Ok(multi_workspace) = window.read(cx) else {
            continue;
        };
        let displayed = multi_workspace.workspace().clone();
        let is_active = active_window == Some(window.window_id());
        for workspace in multi_workspace.workspaces() {
            let project = workspace.read(cx).project().read(cx);
            if !project.is_local() {
                continue;
            }
            let roots = project
                .visible_worktrees(cx)
                .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
                .collect();
            let mut raw_linked = RawLinked::default();
            for repository in project.git_store().read(cx).repositories().values() {
                let snapshot = repository.read(cx).snapshot();
                raw_linked
                    .linked_roots
                    .extend(snapshot.linked_worktrees().iter().map(|wt| wt.path.clone()));
                raw_linked
                    .common_dirs
                    .push(snapshot.common_dir_abs_path.to_path_buf());
            }
            handles.push((window, workspace.clone()));
            raw.push(RawCandidate {
                roots,
                displayed: *workspace == displayed,
                active_window: is_active,
            });
            linked.push(raw_linked);
        }
    }
    (handles, raw, linked)
}

/// Best candidate: deepest containing root, then exact root, then displayed
/// workspace, then active window.
fn pick_best_candidate(cwd: &Path, candidates: &[RawCandidate]) -> Option<usize> {
    candidates
        .iter()
        .enumerate()
        .filter_map(|(index, candidate)| {
            let root = candidate
                .roots
                .iter()
                .filter(|root| cwd.starts_with(root))
                .max_by_key(|root| root.components().count())?;
            let score = (
                root.components().count(),
                root.as_path() == cwd,
                candidate.displayed,
                candidate.active_window,
            );
            Some((score, index))
        })
        // On a tie prefer the earliest candidate.
        .max_by(|(a, ai), (b, bi)| a.cmp(b).then(bi.cmp(ai)))
        .map(|(_, index)| index)
}

/// Deepest linked worktree (across open workspaces) containing `cwd`.
fn pick_linked_root(cwd: &Path, linked: &[RawLinked]) -> Option<(usize, PathBuf)> {
    linked
        .iter()
        .enumerate()
        .flat_map(|(index, entry)| {
            entry
                .linked_roots
                .iter()
                .filter(|root| cwd.starts_with(root))
                .map(move |root| (index, root.clone()))
        })
        .max_by(|(ai, a), (bi, b)| {
            a.components()
                .count()
                .cmp(&b.components().count())
                .then(bi.cmp(ai))
        })
}

/// First open workspace whose repository shares `common_dir` (canonical).
fn pick_by_common_dir(common_dir: &Path, linked: &[RawLinked]) -> Option<usize> {
    linked
        .iter()
        .position(|entry| entry.common_dirs.iter().any(|dir| dir == common_dir))
}

fn canonical_or_self(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Awaits `future`, giving up (`None`) after `timeout`.
async fn bounded<T>(
    future: impl std::future::Future<Output = T>,
    timeout: Duration,
    cx: &AsyncApp,
) -> Option<T> {
    let mut timer = cx.background_executor().timer(timeout).fuse();
    let mut future = std::pin::pin!(future.fuse());
    select_biased! {
        value = future => Some(value),
        _ = timer => None,
    }
}

/// `(--show-toplevel, --git-common-dir)` for `cwd`, or `None` when `cwd` is
/// not inside a git work tree or git is unavailable.
async fn git_toplevel_and_common_dir(program: &str, cwd: &Path) -> Option<(PathBuf, PathBuf)> {
    let output = util::command::new_command(program)
        .kill_on_drop(true)
        .args([
            "rev-parse",
            "--path-format=absolute",
            "--show-toplevel",
            "--git-common-dir",
        ])
        .current_dir(cwd)
        .output()
        .await
        .log_err()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8(output.stdout).ok()?;
    let mut lines = stdout.lines();
    let toplevel = lines.next()?;
    let common_dir = lines.next()?;
    Some((PathBuf::from(toplevel), PathBuf::from(common_dir)))
}

async fn open_workspace(
    paths: Vec<PathBuf>,
    window: Option<WindowHandle<MultiWorkspace>>,
    mode: OpenMode,
    app_state: &Arc<AppState>,
    cx: &mut AsyncApp,
) -> Result<Target, Failure> {
    let task = cx
        .update(|cx| Workspace::new_local(paths, app_state.clone(), window, None, None, mode, cx));
    let result = task
        .await
        .map_err(|error| Failure::internal(format!("failed to open project: {error:#}")))?;
    Ok(Target {
        window: result.window,
        workspace: result.workspace,
    })
}

async fn wait_for_panel(
    target: &Target,
    timeout: Duration,
    cx: &mut AsyncApp,
) -> Result<Entity<AgentPanel>, Failure> {
    let panels_task = cx.update(|cx| {
        target
            .workspace
            .update(cx, |workspace, _| workspace.take_panels_task())
    });
    if let Some(task) = panels_task {
        task.await
            .map_err(|error| Failure::internal(format!("failed to load panels: {error:#}")))?;
    }

    // `take_panels_task` is take-once: if another caller took it the panel may
    // still be loading, so poll briefly.
    let mut timer = cx.background_executor().timer(timeout).fuse();
    loop {
        if let Some(panel) = cx.update(|cx| target.workspace.read(cx).panel::<AgentPanel>(cx)) {
            return Ok(panel);
        }
        let mut tick = cx
            .background_executor()
            .timer(Duration::from_millis(50))
            .fuse();
        select_biased! {
            _ = tick => {}
            _ = timer => {
                return Err(Failure::internal(
                    "the agent panel did not load in the target workspace in time",
                ));
            }
        }
    }
}

async fn spawn_and_confirm(
    target: &Target,
    panel: &Entity<AgentPanel>,
    cwd: PathBuf,
    command: Option<String>,
    title: Option<String>,
    timeouts: Timeouts,
    cx: &mut AsyncApp,
) -> Result<(TerminalId, serde_json::Value), Failure> {
    let (events_tx, mut events_rx) = mpsc::unbounded::<()>();
    let spawn_cwd = cwd.clone();
    // Do the spawn through the target window only: updating a background
    // window neither raises nor focuses it. No activate/focus_panel calls.
    let spawned = cx.update(|cx| {
        target.window.update(cx, |_, window, cx| {
            let subscription = cx.subscribe(panel, move |_, _, event: &AgentPanelEvent, _| {
                if matches!(event, AgentPanelEvent::EntryChanged) {
                    events_tx.unbounded_send(()).ok();
                }
            });
            let result = panel.update(cx, |panel, cx| {
                panel.spawn_terminal_for_cli(
                    spawn_cwd,
                    command,
                    title.map(Into::into),
                    false,
                    window,
                    cx,
                )
            });
            (result, subscription)
        })
    });
    let (result, subscription) =
        spawned.map_err(|_| Failure::internal("the target window was closed"))?;
    let terminal_id =
        result.map_err(|error| Failure::new(EXIT_UNSUPPORTED, format!("{error:#}")))?;

    // `spawn_terminal_for_cli` returns before the shell exists: only report
    // success once the terminal is really in the panel.
    let mut timer = cx.background_executor().timer(timeouts.confirm).fuse();
    let confirmed = loop {
        if cx.update(|cx| panel.read(cx).has_terminal(terminal_id)) {
            break true;
        }
        select_biased! {
            event = events_rx.next() => {
                if event.is_none() {
                    break false;
                }
            }
            _ = timer => break false,
        }
    };
    drop(subscription);

    if !confirmed {
        return Err(Failure::new(
            EXIT_NOT_CONFIRMED,
            format!(
                "terminal {terminal_id} was not confirmed within {}s; it may still appear",
                timeouts.confirm.as_secs()
            ),
        ));
    }

    Ok((
        terminal_id,
        reply_json(target, panel, terminal_id, &cwd, timeouts, cx).await,
    ))
}

/// The reply's `title` is exactly the custom title given with `--title`, else
/// `None`. Never the live terminal title: the shell sets that asynchronously,
/// so reporting it would make the reply racy.
fn reply_title(info: &agent_ui::AgentPanelTerminalInfo) -> Option<String> {
    info.custom_title
        .as_ref()
        .map(|title| title.to_string())
        .filter(|title| !title.is_empty())
}

async fn reply_json(
    target: &Target,
    panel: &Entity<AgentPanel>,
    terminal_id: TerminalId,
    cwd: &Path,
    timeouts: Timeouts,
    cx: &mut AsyncApp,
) -> serde_json::Value {
    let store = cx.update(|cx| TerminalThreadMetadataStore::try_global(cx));
    let mut folder_paths: Option<Vec<String>> = None;
    if let Some(store) = store {
        let reload = cx.update(|cx| store.read(cx).reload_task());
        if bounded(reload, timeouts.metadata, cx).await.is_some() {
            folder_paths = cx.update(|cx| {
                store.read(cx).entry(terminal_id).map(|metadata| {
                    metadata
                        .folder_paths()
                        .paths()
                        .iter()
                        .map(|path| path.to_string_lossy().into_owned())
                        .collect()
                })
            });
        }
    }

    cx.update(|cx| {
        let info = panel
            .read(cx)
            .terminals(cx)
            .into_iter()
            .find(|info| info.id == terminal_id);
        let folder_paths = folder_paths.unwrap_or_else(|| {
            target
                .workspace
                .read(cx)
                .root_paths(cx)
                .iter()
                .map(|path| path.to_string_lossy().into_owned())
                .collect()
        });
        let workspace_id = target
            .workspace
            .read(cx)
            .database_id()
            .and_then(|id| serde_json::to_value(id).ok())
            .unwrap_or(serde_json::Value::Null);
        serde_json::json!({
            "id": terminal_id.to_string(),
            "kind": "terminal",
            "title": info.as_ref().and_then(reply_title),
            "working_directory": info
                .and_then(|info| info.working_directory)
                .unwrap_or_else(|| cwd.to_path_buf())
                .to_string_lossy(),
            "folder_paths": folder_paths,
            "window_id": target.window.window_id().as_u64(),
            "workspace_id": workspace_id,
        })
    })
}

/// A terminal thread known either from an open agent panel (`live`), from the
/// persisted metadata store, or both. Stale threads of closed workspaces exist
/// only in the store.
struct KnownTerminal {
    id: TerminalId,
    /// Custom title, if any, else the terminal's own title.
    title: String,
    live: Option<LiveTerminal>,
}

#[derive(Clone)]
struct LiveTerminal {
    target: Target,
    panel: Entity<AgentPanel>,
}

fn info_title(info: &agent_ui::AgentPanelTerminalInfo) -> String {
    reply_title(info).unwrap_or_else(|| info.title.to_string())
}

/// Every terminal thread in the open agent panels (any window, local or
/// remote workspace) plus the persisted ones, keyed by id. Read-only.
fn known_terminals(cx: &App) -> Vec<KnownTerminal> {
    let mut known: Vec<KnownTerminal> = Vec::new();
    for window in cx.windows() {
        let Some(window) = window.downcast::<MultiWorkspace>() else {
            continue;
        };
        let Ok(multi_workspace) = window.read(cx) else {
            continue;
        };
        for workspace in multi_workspace.workspaces() {
            let Some(panel) = workspace.read(cx).panel::<AgentPanel>(cx) else {
                continue;
            };
            for info in panel.read(cx).terminals(cx) {
                known.push(KnownTerminal {
                    id: info.id,
                    title: info_title(&info),
                    live: Some(LiveTerminal {
                        target: Target {
                            window,
                            workspace: workspace.clone(),
                        },
                        panel: panel.clone(),
                    }),
                });
            }
        }
    }
    if let Some(store) = TerminalThreadMetadataStore::try_global(cx) {
        for metadata in store.read(cx).entries() {
            if known.iter().all(|entry| entry.id != metadata.terminal_id) {
                known.push(KnownTerminal {
                    id: metadata.terminal_id,
                    title: metadata
                        .custom_title
                        .as_ref()
                        .map(|title| title.to_string())
                        .unwrap_or_else(|| metadata.title.to_string()),
                    live: None,
                });
            }
        }
    }
    known
}

/// Waits (bounded) for the terminal metadata store; stale threads live only
/// there, so an unloaded store must never read as "no such thread".
async fn wait_for_terminal_store(
    timeouts: Timeouts,
    cx: &mut AsyncApp,
) -> Result<Entity<TerminalThreadMetadataStore>, Failure> {
    let store = cx
        .update(|cx| TerminalThreadMetadataStore::try_global(cx))
        .ok_or_else(|| Failure::internal("agent thread store is not initialized"))?;
    let reload = cx.update(|cx| store.read(cx).reload_task());
    bounded(reload, timeouts.metadata, cx).await;
    if !cx.update(|cx| store.read(cx).is_loaded()) {
        return Err(Failure::new(
            EXIT_NOT_CONFIRMED,
            "timed out loading agent thread metadata",
        ));
    }
    Ok(store)
}

/// An id that parses but belongs to no terminal: is it an agent-panel thread?
fn is_agent_panel_thread(id: &str, cx: &App) -> bool {
    agent_ui::thread_metadata_store::ThreadMetadataStore::try_global(cx).is_some_and(|store| {
        store
            .read(cx)
            .entry_ids()
            .any(|thread_id| thread_id.to_key_string() == id)
    })
}

fn parse_terminal_id(id: &str) -> Result<TerminalId, Failure> {
    TerminalId::from_key_string(id)
        .map_err(|_| Failure::internal(format!("invalid thread id: {id:?}")))
}

fn refuse_non_terminal(id: &str) -> Failure {
    Failure::new(
        EXIT_NOT_TERMINAL,
        format!("{id} is an agent-panel thread, not a terminal thread; refusing"),
    )
}

async fn send_outcome(outcome: Result<String, Failure>, responses: &dyn CliResponseSink) {
    let status = match outcome {
        Ok(message) => {
            responses.send(CliResponse::Stdout { message }).log_err();
            0
        }
        Err(failure) => {
            responses
                .send(CliResponse::Stderr {
                    message: failure.message,
                })
                .log_err();
            failure.status
        }
    };
    responses.send(CliResponse::Exit { status }).log_err();
}

/// `zed --agent-close <id>` / `--agent-close --title <title>`.
pub async fn handle_close_agent_thread(
    id: Option<String>,
    title: Option<String>,
    responses: &dyn CliResponseSink,
    cx: &mut AsyncApp,
) {
    handle_close_agent_thread_with_timeouts(id, title, Timeouts::default(), responses, cx).await
}

pub async fn handle_close_agent_thread_with_timeouts(
    id: Option<String>,
    title: Option<String>,
    timeouts: Timeouts,
    responses: &dyn CliResponseSink,
    cx: &mut AsyncApp,
) {
    let outcome = close_agent_thread(id, title, timeouts, cx).await;
    send_outcome(outcome, responses).await;
}

async fn close_agent_thread(
    id: Option<String>,
    title: Option<String>,
    timeouts: Timeouts,
    cx: &mut AsyncApp,
) -> Result<String, Failure> {
    if id.is_some() == title.is_some() {
        return Err(Failure::internal(
            "close needs exactly one of a thread id or a title",
        ));
    }
    if cx.update(|cx| DisableAiSettings::get_global(cx).disable_ai) {
        return Err(Failure::new(
            EXIT_UNSUPPORTED,
            "AI features are disabled (disable_ai); there are no agent terminals",
        ));
    }
    let wanted_id = id.as_deref().map(parse_terminal_id).transpose()?;
    let store = wait_for_terminal_store(timeouts, cx).await?;

    let _guard = SPAWN_LOCK.lock().await;
    let mut matches: Vec<KnownTerminal> = cx
        .update(|cx| known_terminals(cx))
        .into_iter()
        .filter(|entry| match (&wanted_id, &title) {
            (Some(wanted), _) => entry.id == *wanted,
            (None, Some(title)) => entry.title == *title,
            (None, None) => false,
        })
        .collect();

    let not_found = |what: String| {
        serde_json::json!({ "closed": false, "message": format!("no such thread: {what}") })
            .to_string()
    };
    let known = match matches.len() {
        0 => {
            if let Some(id) = &id
                && cx.update(|cx| is_agent_panel_thread(id, cx))
            {
                return Err(refuse_non_terminal(id));
            }
            return Ok(not_found(id.or(title).unwrap_or_default()));
        }
        1 => matches.remove(0),
        _ => {
            matches.sort_by_key(|entry| entry.id.to_string());
            let ids: Vec<String> = matches.iter().map(|entry| entry.id.to_string()).collect();
            return Err(Failure::new(
                EXIT_AMBIGUOUS,
                format!(
                    "{} threads are titled {:?}; none closed. Close one by id: {}",
                    ids.len(),
                    title.unwrap_or_default(),
                    ids.join(", ")
                ),
            ));
        }
    };

    let terminal_id = known.id;
    if let Some(live) = &known.live {
        // Same call the sidebar's close makes, through the owning window only
        // (a background window is neither raised nor focused).
        cx.update(|cx| {
            live.target.window.update(cx, |_, window, cx| {
                live.panel.update(cx, |panel, cx| {
                    panel.close_terminal_without_activating_draft(terminal_id, window, cx);
                });
            })
        })
        .map_err(|_| Failure::internal("the window owning the thread was closed"))?;
        if cx.update(|cx| live.panel.read(cx).has_terminal(terminal_id)) {
            return Err(Failure::internal(format!(
                "terminal {terminal_id} is still open after the close request"
            )));
        }
    }
    // The panel's close already deletes the metadata entry; this also covers a
    // stale thread of a closed workspace, which has no panel (idempotent).
    cx.update(|cx| store.update(cx, |store, cx| store.delete(terminal_id, cx)));

    Ok(serde_json::json!({
        "id": terminal_id.to_string(),
        "kind": "terminal",
        "title": known.title,
        "closed": true,
        "was_open": known.live.is_some(),
    })
    .to_string())
}

/// `zed --agent-rename <id> <title>`.
pub async fn handle_rename_agent_thread(
    id: String,
    title: String,
    responses: &dyn CliResponseSink,
    cx: &mut AsyncApp,
) {
    handle_rename_agent_thread_with_timeouts(id, title, Timeouts::default(), responses, cx).await
}

pub async fn handle_rename_agent_thread_with_timeouts(
    id: String,
    title: String,
    timeouts: Timeouts,
    responses: &dyn CliResponseSink,
    cx: &mut AsyncApp,
) {
    let outcome = rename_agent_thread(id, title, timeouts, cx).await;
    send_outcome(outcome, responses).await;
}

async fn rename_agent_thread(
    id: String,
    title: String,
    timeouts: Timeouts,
    cx: &mut AsyncApp,
) -> Result<String, Failure> {
    if title.trim().is_empty() || title.contains(['\n', '\r']) {
        return Err(Failure::internal(
            "the title must be a non-empty single line",
        ));
    }
    if title.chars().count() > MAX_TITLE_CHARS {
        return Err(Failure::internal(format!(
            "the title is too long (max {MAX_TITLE_CHARS} characters)"
        )));
    }
    if cx.update(|cx| DisableAiSettings::get_global(cx).disable_ai) {
        return Err(Failure::new(
            EXIT_UNSUPPORTED,
            "AI features are disabled (disable_ai); there are no agent terminals",
        ));
    }
    let terminal_id = parse_terminal_id(&id)?;
    let store = wait_for_terminal_store(timeouts, cx).await?;

    let known = cx
        .update(|cx| known_terminals(cx))
        .into_iter()
        .find(|entry| entry.id == terminal_id);
    let Some(known) = known else {
        if cx.update(|cx| is_agent_panel_thread(&id, cx)) {
            return Err(refuse_non_terminal(&id));
        }
        return Err(Failure::new(EXIT_NO_MATCH, format!("no such thread: {id}")));
    };

    cx.update(|cx| {
        if let Some(live) = &known.live {
            live.panel.update(cx, |panel, cx| {
                panel.rename_terminal(terminal_id, title.clone().into(), cx);
            });
        }
        // Persist explicitly as well (also the only path for a stale thread).
        store.update(cx, |store, cx| {
            store.rename_terminal(terminal_id, title.clone().into(), cx);
        });
    });

    match known.live {
        Some(live) => Ok(reply_json(
            &live.target,
            &live.panel,
            terminal_id,
            Path::new(""),
            timeouts,
            cx,
        )
        .await
        .to_string()),
        None => Ok(cx.update(|cx| {
            let metadata = store.read(cx).entry(terminal_id).cloned();
            serde_json::json!({
                "id": terminal_id.to_string(),
                "kind": "terminal",
                "title": metadata
                    .as_ref()
                    .and_then(|metadata| metadata.custom_title.as_ref())
                    .map(|title| title.to_string()),
                "working_directory": metadata
                    .as_ref()
                    .and_then(|metadata| metadata.working_directory.as_ref())
                    .map(|path| path.to_string_lossy().into_owned()),
                "folder_paths": metadata
                    .as_ref()
                    .map(|metadata| {
                        metadata
                            .folder_paths()
                            .paths()
                            .iter()
                            .map(|path| path.to_string_lossy().into_owned())
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default(),
                "window_id": serde_json::Value::Null,
                "workspace_id": serde_json::Value::Null,
            })
            .to_string()
        })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::zed::tests::init_test;
    use gpui::{TaskExt as _, TestAppContext, UpdateGlobal as _};
    use serde_json::json;
    use settings::SettingsStore;
    use std::sync::Mutex;

    thread_local! {
        static ACTIVE: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
        static MAX_ACTIVE: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }

    /// Records how many requests are inside the serialized section at once
    /// (per test thread). With `SPAWN_LOCK` it never exceeds one.
    pub(super) struct ConcurrencyProbe;

    impl ConcurrencyProbe {
        pub(super) fn enter() -> Self {
            let active = ACTIVE.get() + 1;
            ACTIVE.set(active);
            MAX_ACTIVE.set(MAX_ACTIVE.get().max(active));
            Self
        }
    }

    impl Drop for ConcurrencyProbe {
        fn drop(&mut self) {
            ACTIVE.set(ACTIVE.get() - 1);
        }
    }

    #[derive(Default)]
    struct CapturingSink(Mutex<Vec<CliResponse>>);

    impl CliResponseSink for CapturingSink {
        fn send(&self, response: CliResponse) -> anyhow::Result<()> {
            self.0.lock().unwrap().push(response);
            Ok(())
        }
    }

    #[derive(Debug)]
    struct Outcome {
        stdout: Option<serde_json::Value>,
        stderr: String,
        status: i32,
    }

    impl Outcome {
        fn from(sink: &CapturingSink) -> Self {
            let mut outcome = Outcome {
                stdout: None,
                stderr: String::new(),
                status: -1,
            };
            for response in sink.0.lock().unwrap().iter() {
                match response {
                    CliResponse::Stdout { message } => {
                        outcome.stdout = Some(serde_json::from_str(message).unwrap());
                    }
                    CliResponse::Stderr { message } => outcome.stderr.push_str(message),
                    CliResponse::Exit { status } => outcome.status = *status,
                    _ => {}
                }
            }
            outcome
        }
    }

    /// A real, existing directory (terminals spawn real shells) whose path is
    /// also registered in the fake file system.
    struct TestDirs {
        root: PathBuf,
    }

    impl TestDirs {
        fn new(name: &str) -> Self {
            let root = std::env::temp_dir()
                .canonicalize()
                .unwrap()
                .join(format!("cpzed10-{name}-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&root).unwrap();
            Self { root }
        }

        fn dir(&self, relative: &str) -> PathBuf {
            let path = self.root.join(relative);
            std::fs::create_dir_all(&path).unwrap();
            path
        }
    }

    impl Drop for TestDirs {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.root).ok();
        }
    }

    fn init_agent_test(cx: &mut TestAppContext) -> Arc<AppState> {
        let app_state = init_test(cx);
        cx.update(|cx| {
            agent::ThreadStore::init_global(cx);
            prompt_store::init(cx);
            agent_ui::terminal_thread_metadata_store::TerminalThreadMetadataStore::init_global(cx);
            SettingsStore::update_global(cx, |store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.terminal.get_or_insert_default().project.shell =
                        Some(settings::Shell::Program("/bin/sh".to_string()));
                });
            });
        });
        // `initialize_workspace` skips AI panels under `cfg!(test)`; load the
        // same panel the app would, asynchronously, for every new workspace.
        cx.update(|cx| {
            cx.observe_new(|_: &mut Workspace, window, cx| {
                let Some(window) = window else {
                    return;
                };
                let workspace = cx.entity().downgrade();
                cx.spawn_in(window, async move |_, cx| {
                    let panel = AgentPanel::load(workspace.clone(), cx.clone()).await?;
                    workspace.update_in(cx, |workspace, window, cx| {
                        workspace.add_panel(panel, window, cx);
                    })
                })
                .detach_and_log_err(cx);
            })
            .detach();
        });
        cx.executor().allow_parking();
        app_state
    }

    async fn open_window(
        cx: &mut TestAppContext,
        app_state: &Arc<AppState>,
        root: &Path,
    ) -> (WindowHandle<MultiWorkspace>, Entity<Workspace>) {
        let task = cx.update(|cx| {
            Workspace::new_local(
                vec![root.to_path_buf()],
                app_state.clone(),
                None,
                None,
                None,
                OpenMode::NewWindow,
                cx,
            )
        });
        let result = task.await.unwrap();
        (result.window, result.workspace)
    }

    async fn request(
        cx: &mut TestAppContext,
        app_state: &Arc<AppState>,
        cwd: &Path,
        command: Option<&str>,
        on_no_match: NoMatchBehavior,
        timeouts: Timeouts,
    ) -> Outcome {
        let sink = CapturingSink::default();
        let mut async_cx = cx.to_async();
        handle_new_agent_terminal_with_timeouts(
            cwd.to_path_buf(),
            command.map(str::to_string),
            None,
            on_no_match,
            None,
            None,
            app_state.clone(),
            timeouts,
            &sink,
            &mut async_cx,
        )
        .await;
        Outcome::from(&sink)
    }

    fn terminal_count(cx: &mut TestAppContext, workspace: &Entity<Workspace>) -> usize {
        cx.update(|cx| {
            workspace
                .read(cx)
                .panel::<AgentPanel>(cx)
                .map_or(0, |panel| panel.read(cx).terminals(cx).len())
        })
    }

    #[gpui::test]
    async fn test_deepest_root_wins_across_two_windows(cx: &mut TestAppContext) {
        let app_state = init_agent_test(cx);
        let dirs = TestDirs::new("deepest");
        let parent = dirs.dir("code");
        let child = dirs.dir("code/stream");
        let sub = dirs.dir("code/stream/src");
        let fs = app_state.fs.as_fake();
        fs.insert_tree(&parent, json!({ "stream": { "src": {} } }))
            .await;

        // The parent window is opened second so it is the active one.
        let (child_window, child_ws) = open_window(cx, &app_state, &child).await;
        let (parent_window, parent_ws) = open_window(cx, &app_state, &parent).await;

        let outcome = request(
            cx,
            &app_state,
            &sub,
            None,
            NoMatchBehavior::Error,
            Timeouts::default(),
        )
        .await;
        assert_eq!(outcome.status, 0, "{outcome:?}");
        let reply = outcome.stdout.unwrap();
        assert_eq!(
            reply["window_id"].as_u64().unwrap(),
            child_window.window_id().as_u64()
        );
        cx.run_until_parked();
        assert_eq!(terminal_count(cx, &child_ws), 1);
        assert_eq!(terminal_count(cx, &parent_ws), 0);
        let _ = parent_window;
    }

    fn displayed_workspace(
        cx: &mut TestAppContext,
        window: WindowHandle<MultiWorkspace>,
    ) -> Entity<Workspace> {
        window
            .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
            .unwrap()
    }

    fn workspace_count(cx: &mut TestAppContext, window: WindowHandle<MultiWorkspace>) -> usize {
        window
            .read_with(cx, |multi_workspace, _| {
                multi_workspace.workspaces().count()
            })
            .unwrap()
    }

    async fn insert_dirs(app_state: &Arc<AppState>, root: &Path, tree: serde_json::Value) {
        app_state.fs.as_fake().insert_tree(root, tree).await;
    }

    #[gpui::test]
    async fn test_background_window_is_not_activated_and_display_is_unchanged(
        cx: &mut TestAppContext,
    ) {
        let app_state = init_agent_test(cx);
        let dirs = TestDirs::new("background");
        let shown_root = dirs.dir("a");
        let hidden_root = dirs.dir("b");
        let other_root = dirs.dir("c");
        insert_dirs(&app_state, &dirs.root, json!({ "a": {}, "b": {}, "c": {} })).await;

        // Window 1 shows workspace `a` and also holds a hidden workspace `b`.
        let (window, shown) = open_window(cx, &app_state, &shown_root).await;
        let hidden = cx
            .update(|cx| {
                Workspace::new_local(
                    vec![hidden_root.clone()],
                    app_state.clone(),
                    Some(window),
                    None,
                    None,
                    OpenMode::Add,
                    cx,
                )
            })
            .await
            .unwrap()
            .workspace;
        // Window 2 is opened last, so it is the active window.
        let (other_window, _) = open_window(cx, &app_state, &other_root).await;
        cx.run_until_parked();
        let active_before = cx.update(|cx| cx.active_window().map(|w| w.window_id()));
        assert_eq!(active_before, Some(other_window.window_id()));
        assert_eq!(displayed_workspace(cx, window), shown);

        let outcome = request(
            cx,
            &app_state,
            &hidden_root,
            None,
            NoMatchBehavior::Error,
            Timeouts::default(),
        )
        .await;
        assert_eq!(outcome.status, 0, "{outcome:?}");
        cx.run_until_parked();

        assert_eq!(terminal_count(cx, &hidden), 1);
        assert_eq!(terminal_count(cx, &shown), 0);
        assert_eq!(displayed_workspace(cx, window), shown);
        let active_after = cx.update(|cx| cx.active_window().map(|w| w.window_id()));
        assert_eq!(active_after, active_before);
    }

    #[gpui::test]
    async fn test_freshly_opened_worktree_resolves_without_waiting_for_scan(
        cx: &mut TestAppContext,
    ) {
        let app_state = init_agent_test(cx);
        let dirs = TestDirs::new("fresh");
        let root = dirs.dir("project");
        insert_dirs(&app_state, &root, json!({ "src": {} })).await;
        let (window, workspace) = open_window(cx, &app_state, &root).await;
        // No run_until_parked: the worktree scan may not have finished.
        let outcome = request(
            cx,
            &app_state,
            &root.join("src"),
            None,
            NoMatchBehavior::Error,
            Timeouts::default(),
        )
        .await;
        assert_eq!(outcome.status, 0, "{outcome:?}");
        assert_eq!(
            outcome.stdout.unwrap()["window_id"].as_u64(),
            Some(window.window_id().as_u64())
        );
        assert_eq!(terminal_count(cx, &workspace), 1);
    }

    #[gpui::test]
    async fn test_no_match_exits_3_and_opens_nothing(cx: &mut TestAppContext) {
        let app_state = init_agent_test(cx);
        let dirs = TestDirs::new("nomatch");
        let open_root = dirs.dir("open");
        let elsewhere = dirs.dir("elsewhere");
        insert_dirs(
            &app_state,
            &dirs.root,
            json!({ "open": {}, "elsewhere": {} }),
        )
        .await;
        let (window, workspace) = open_window(cx, &app_state, &open_root).await;

        let outcome = request(
            cx,
            &app_state,
            &elsewhere,
            None,
            NoMatchBehavior::Error,
            Timeouts::default(),
        )
        .await;
        assert_eq!(outcome.status, 3, "{outcome:?}");
        assert!(outcome.stdout.is_none());
        assert!(outcome.stderr.contains("--open-project"), "{outcome:?}");
        assert_eq!(workspace_count(cx, window), 1);
        assert_eq!(cx.update(|cx| cx.windows().len()), 1);
        assert_eq!(terminal_count(cx, &workspace), 0);
    }

    #[gpui::test]
    async fn test_open_project_adds_workspace_to_active_window(cx: &mut TestAppContext) {
        let app_state = init_agent_test(cx);
        let dirs = TestDirs::new("openproject");
        let open_root = dirs.dir("open");
        let elsewhere = dirs.dir("elsewhere");
        insert_dirs(
            &app_state,
            &dirs.root,
            json!({ "open": {}, "elsewhere": {} }),
        )
        .await;
        let (window, shown) = open_window(cx, &app_state, &open_root).await;

        let outcome = request(
            cx,
            &app_state,
            &elsewhere,
            None,
            NoMatchBehavior::OpenProject,
            Timeouts::default(),
        )
        .await;
        assert_eq!(outcome.status, 0, "{outcome:?}");
        assert_eq!(cx.update(|cx| cx.windows().len()), 1);
        assert_eq!(workspace_count(cx, window), 2);
        assert_eq!(displayed_workspace(cx, window), shown);
        assert_eq!(terminal_count(cx, &shown), 0);
    }

    #[gpui::test]
    async fn test_new_window_behavior_opens_a_window(cx: &mut TestAppContext) {
        let app_state = init_agent_test(cx);
        let dirs = TestDirs::new("newwindow");
        let open_root = dirs.dir("open");
        let elsewhere = dirs.dir("elsewhere");
        insert_dirs(
            &app_state,
            &dirs.root,
            json!({ "open": {}, "elsewhere": {} }),
        )
        .await;
        let (_window, shown) = open_window(cx, &app_state, &open_root).await;

        let outcome = request(
            cx,
            &app_state,
            &elsewhere,
            None,
            NoMatchBehavior::NewWindow,
            Timeouts::default(),
        )
        .await;
        assert_eq!(outcome.status, 0, "{outcome:?}");
        assert_eq!(cx.update(|cx| cx.windows().len()), 2);
        assert_eq!(terminal_count(cx, &shown), 0);
    }

    #[gpui::test]
    async fn test_linked_worktree_is_added_to_the_main_repo_group(cx: &mut TestAppContext) {
        let app_state = init_agent_test(cx);
        let dirs = TestDirs::new("linked");
        let main_root = dirs.dir("repo");
        let linked_root = dirs.dir("repo-feature");
        let cwd = dirs.dir("repo-feature/src");
        let fs = app_state.fs.as_fake();
        fs.insert_tree(&main_root, json!({ ".git": {}, "src": {} }))
            .await;
        fs.insert_tree(&linked_root, json!({ "src": {} })).await;
        fs.add_linked_worktree_for_repo(
            &main_root.join(".git"),
            false,
            git::repository::Worktree {
                path: linked_root.clone(),
                ref_name: Some("refs/heads/feature".into()),
                sha: "aaa".into(),
                is_main: false,
                is_bare: false,
            },
        )
        .await;

        let (window, main_ws) = open_window(cx, &app_state, &main_root).await;
        cx.run_until_parked();
        assert_eq!(workspace_count(cx, window), 1);

        let outcome = request(
            cx,
            &app_state,
            &cwd,
            None,
            NoMatchBehavior::Error,
            Timeouts::default(),
        )
        .await;
        assert_eq!(outcome.status, 0, "{outcome:?}");
        cx.run_until_parked();

        assert_eq!(cx.update(|cx| cx.windows().len()), 1);
        assert_eq!(workspace_count(cx, window), 2);
        assert_eq!(
            displayed_workspace(cx, window),
            main_ws,
            "must not activate"
        );
        assert_eq!(terminal_count(cx, &main_ws), 0);
        let keys = window
            .read_with(cx, |multi_workspace, _| {
                multi_workspace.project_group_keys()
            })
            .unwrap();
        assert_eq!(keys.len(), 1, "linked worktree shares the main repo group");
    }

    #[gpui::test]
    async fn test_bare_repo_linked_worktree_is_added(cx: &mut TestAppContext) {
        let app_state = init_agent_test(cx);
        let dirs = TestDirs::new("bare");
        let bare = dirs.dir("glider.git");
        let main_wt = dirs.dir("worktree-branch-main");
        let feature_wt = dirs.dir("worktree-branch-feature");
        let cwd = dirs.dir("worktree-branch-feature/crates");
        let fs = app_state.fs.as_fake();
        fs.insert_tree(&bare, json!({ "worktrees": {} })).await;
        fs.insert_tree(&main_wt, json!({ "crates": {} })).await;
        fs.insert_tree(&feature_wt, json!({ "crates": {} })).await;
        for (path, branch) in [(&main_wt, "main"), (&feature_wt, "feature")] {
            fs.add_linked_worktree_for_repo(
                &bare,
                false,
                git::repository::Worktree {
                    path: path.clone(),
                    ref_name: Some(format!("refs/heads/{branch}").into()),
                    sha: "aaa".into(),
                    is_main: false,
                    is_bare: false,
                },
            )
            .await;
        }

        let (window, main_ws) = open_window(cx, &app_state, &main_wt).await;
        cx.run_until_parked();

        let outcome = request(
            cx,
            &app_state,
            &cwd,
            None,
            NoMatchBehavior::Error,
            Timeouts::default(),
        )
        .await;
        assert_eq!(outcome.status, 0, "{outcome:?}");
        cx.run_until_parked();
        assert_eq!(workspace_count(cx, window), 2);
        assert_eq!(displayed_workspace(cx, window), main_ws);
        let keys = window
            .read_with(cx, |multi_workspace, _| {
                multi_workspace.project_group_keys()
            })
            .unwrap();
        assert_eq!(keys.len(), 1, "bare-repo worktrees share one group");
    }

    #[gpui::test]
    async fn test_concurrent_requests_yield_two_terminals(cx: &mut TestAppContext) {
        let app_state = init_agent_test(cx);
        let dirs = TestDirs::new("concurrent");
        let root = dirs.dir("project");
        insert_dirs(&app_state, &root, json!({})).await;
        let (_window, workspace) = open_window(cx, &app_state, &root).await;

        MAX_ACTIVE.set(0);
        let mut first_cx = cx.to_async();
        let mut second_cx = cx.to_async();
        let first_sink = CapturingSink::default();
        let second_sink = CapturingSink::default();
        futures::join!(
            handle_new_agent_terminal_with_timeouts(
                root.clone(),
                None,
                None,
                NoMatchBehavior::Error,
                None,
                None,
                app_state.clone(),
                Timeouts::default(),
                &first_sink,
                &mut first_cx,
            ),
            handle_new_agent_terminal_with_timeouts(
                root.clone(),
                None,
                None,
                NoMatchBehavior::Error,
                None,
                None,
                app_state.clone(),
                Timeouts::default(),
                &second_sink,
                &mut second_cx,
            ),
        );
        let first = Outcome::from(&first_sink);
        let second = Outcome::from(&second_sink);
        assert_eq!(
            (first.status, second.status),
            (0, 0),
            "{first:?} {second:?}"
        );
        let first_id = first.stdout.unwrap()["id"].as_str().unwrap().to_string();
        let second_id = second.stdout.unwrap()["id"].as_str().unwrap().to_string();
        assert_ne!(first_id, second_id);
        assert_eq!(MAX_ACTIVE.get(), 1, "requests must be serialized");
        cx.run_until_parked();
        // Exactly the two requested terminals: no stray "initial" terminal.
        assert_eq!(terminal_count(cx, &workspace), 2);
    }

    #[gpui::test]
    async fn test_unconfirmed_spawn_exits_2_with_the_id(cx: &mut TestAppContext) {
        let app_state = init_agent_test(cx);
        let dirs = TestDirs::new("timeout");
        let root = dirs.dir("project");
        insert_dirs(&app_state, &root, json!({})).await;
        let (_window, _workspace) = open_window(cx, &app_state, &root).await;

        let outcome = request(
            cx,
            &app_state,
            &root,
            None,
            NoMatchBehavior::Error,
            Timeouts {
                confirm: Duration::ZERO,
                ..Timeouts::default()
            },
        )
        .await;
        assert_eq!(outcome.status, 2, "{outcome:?}");
        assert!(
            outcome.stdout.is_none(),
            "no id on stdout before confirmation"
        );
        assert!(outcome.stderr.contains("not confirmed"), "{outcome:?}");
    }

    #[gpui::test]
    async fn test_bounded_gives_up_on_a_hung_future(cx: &mut TestAppContext) {
        let async_cx = cx.to_async();
        // A store whose load never finishes must not hang the CLI.
        let hung = bounded(
            futures::future::pending::<()>(),
            Duration::from_millis(10),
            &async_cx,
        )
        .await;
        assert_eq!(hung, None);
        let done = bounded(async { 7 }, Duration::from_secs(5), &async_cx).await;
        assert_eq!(done, Some(7));
    }

    #[gpui::test]
    async fn test_disabled_ai_exits_4_immediately(cx: &mut TestAppContext) {
        let app_state = init_agent_test(cx);
        let dirs = TestDirs::new("disabled");
        let root = dirs.dir("project");
        insert_dirs(&app_state, &root, json!({})).await;
        let (_window, workspace) = open_window(cx, &app_state, &root).await;
        cx.update(|cx| {
            SettingsStore::update_global(cx, |store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.project.disable_ai = Some(settings::SaturatingBool(true));
                });
            });
        });

        let outcome = request(
            cx,
            &app_state,
            &root,
            None,
            NoMatchBehavior::Error,
            Timeouts {
                // Without the up-front check this exits 1 after 1s instead of 4.
                panel_load: Duration::from_secs(1),
                ..Timeouts::default()
            },
        )
        .await;
        assert_eq!(outcome.status, 4, "{outcome:?}");
        assert!(outcome.stderr.contains("disabled"), "{outcome:?}");
        assert_eq!(terminal_count(cx, &workspace), 0);
    }

    #[gpui::test]
    async fn test_multiline_command_is_rejected(cx: &mut TestAppContext) {
        let app_state = init_agent_test(cx);
        let dirs = TestDirs::new("multiline");
        let root = dirs.dir("project");
        insert_dirs(&app_state, &root, json!({})).await;
        let (_window, workspace) = open_window(cx, &app_state, &root).await;
        for command in ["echo a\necho b", "echo a\recho b"] {
            let outcome = request(
                cx,
                &app_state,
                &root,
                Some(command),
                NoMatchBehavior::Error,
                Timeouts::default(),
            )
            .await;
            assert_eq!(outcome.status, 1, "{outcome:?}");
            assert!(outcome.stderr.contains("single line"), "{outcome:?}");
        }
        assert_eq!(terminal_count(cx, &workspace), 0);
    }

    #[cfg(unix)]
    #[gpui::test]
    async fn test_non_canonical_workspace_root_matches_canonical_cwd(cx: &mut TestAppContext) {
        let app_state = init_agent_test(cx);
        let dirs = TestDirs::new("canonical");
        let _real = dirs.dir("real");
        let real_sub = dirs.dir("real/sub");
        let link = dirs.root.join("link");
        std::os::unix::fs::symlink(dirs.root.join("real"), &link).unwrap();
        // The UI opened the project through the symlink; the CLI sends the
        // canonical cwd (macOS: /tmp -> /private/tmp).
        insert_dirs(&app_state, &link, json!({ "sub": {} })).await;
        let (_window, workspace) = open_window(cx, &app_state, &link).await;

        let outcome = request(
            cx,
            &app_state,
            &real_sub,
            None,
            NoMatchBehavior::Error,
            Timeouts::default(),
        )
        .await;
        assert_eq!(outcome.status, 0, "{outcome:?}");
        assert_eq!(terminal_count(cx, &workspace), 1);
    }

    #[test]
    fn test_candidate_scoring_prefers_deepest_then_exact_then_displayed() {
        let candidate = |roots: &[&str], displayed, active_window| RawCandidate {
            roots: roots.iter().map(PathBuf::from).collect(),
            displayed,
            active_window,
        };
        assert_eq!(
            pick_best_candidate(
                Path::new("/code/a/b"),
                &[
                    candidate(&["/code"], true, true),
                    candidate(&["/code/a"], false, false),
                    candidate(&["/elsewhere"], true, true),
                ]
            ),
            Some(1)
        );
        // Same depth: displayed beats not displayed, then active window.
        assert_eq!(
            pick_best_candidate(
                Path::new("/code/a"),
                &[
                    candidate(&["/code/a"], false, true),
                    candidate(&["/code/a"], true, false),
                ]
            ),
            Some(1)
        );
        // A prefix that is not a path component does not match.
        assert_eq!(
            pick_best_candidate(
                Path::new("/code/ab"),
                &[candidate(&["/code/a"], true, true)]
            ),
            None
        );
    }

    #[test]
    fn test_linked_root_picks_deepest_containing_worktree() {
        let linked = vec![
            RawLinked {
                linked_roots: vec![PathBuf::from("/wt"), PathBuf::from("/wt/inner")],
                common_dirs: vec![],
            },
            RawLinked::default(),
        ];
        assert_eq!(
            pick_linked_root(Path::new("/wt/inner/src"), &linked),
            Some((0, PathBuf::from("/wt/inner")))
        );
        assert_eq!(pick_linked_root(Path::new("/other"), &linked), None);
    }

    fn run_git(args: &[&str], cwd: &Path) {
        let mut command = util::command::new_command("git");
        command
            .args(["-c", "maintenance.auto=false", "-c", "gc.auto=0"])
            .args(args)
            .current_dir(cwd)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com");
        let output = gpui::block_on(command.output()).expect("git must be installed");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// The sandbox layout: `glider.git` (bare) plus `worktree-branch-*`.
    /// Returns (bare, main worktree, feature worktree).
    fn make_bare_layout(dirs: &TestDirs) -> (PathBuf, PathBuf, PathBuf) {
        let source = dirs.dir("source");
        run_git(&["init", "-q", "-b", "main"], &source);
        run_git(&["commit", "-q", "--allow-empty", "-m", "init"], &source);
        let bare = dirs.root.join("glider.git");
        run_git(
            &[
                "clone",
                "-q",
                "--bare",
                source.to_str().unwrap(),
                bare.to_str().unwrap(),
            ],
            &dirs.root,
        );
        run_git(
            &["worktree", "add", "-q", "../worktree-branch-main", "main"],
            &bare,
        );
        run_git(
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "feature",
                "../worktree-branch-feature",
                "main",
            ],
            &bare,
        );
        let feature = dirs.root.join("worktree-branch-feature");
        std::fs::create_dir_all(feature.join("crates")).unwrap();
        (bare, dirs.root.join("worktree-branch-main"), feature)
    }

    #[test]
    fn test_git_fallback_resolves_bare_repo_layout() {
        let dirs = TestDirs::new("git-fallback");
        let (bare, main_wt, feature_wt) = make_bare_layout(&dirs);

        let (toplevel, common_dir) = gpui::block_on(git_toplevel_and_common_dir(
            "git",
            &feature_wt.join("crates"),
        ))
        .expect("git should resolve the worktree");
        assert_eq!(canonical_or_self(&toplevel), canonical_or_self(&feature_wt));
        assert_eq!(canonical_or_self(&common_dir), canonical_or_self(&bare));
        assert!(gpui::block_on(git_toplevel_and_common_dir("git", &dirs.root)).is_none());

        // The fallback's common dir must match what an open workspace of the
        // main worktree reports (its repository's common dir).
        let (_, main_common) =
            gpui::block_on(git_toplevel_and_common_dir("git", &main_wt)).unwrap();
        let open = [RawLinked {
            linked_roots: vec![],
            common_dirs: vec![canonical_or_self(&main_common)],
        }];
        assert_eq!(
            pick_by_common_dir(&canonical_or_self(&common_dir), &open),
            Some(0)
        );
        assert_eq!(pick_by_common_dir(Path::new("/unrelated.git"), &open), None);
    }

    fn real_app_state(cx: &mut TestAppContext, fake: &Arc<AppState>) -> Arc<AppState> {
        Arc::new(AppState {
            languages: fake.languages.clone(),
            client: fake.client.clone(),
            user_store: fake.user_store.clone(),
            workspace_store: fake.workspace_store.clone(),
            fs: fs::RealFs::new(None, cx.executor()),
            build_window_options: fake.build_window_options,
            node_runtime: fake.node_runtime.clone(),
            session: fake.session.clone(),
        })
    }

    async fn wait_for_repo_scan(cx: &mut TestAppContext) {
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        loop {
            cx.run_until_parked();
            let ready = cx.update(|cx| {
                collect_candidates(cx)
                    .2
                    .iter()
                    .any(|entry| !entry.common_dirs.is_empty())
            });
            if ready {
                return;
            }
            assert!(std::time::Instant::now() < deadline, "repo never scanned");
            cx.executor().timer(Duration::from_millis(50)).await;
        }
    }

    /// Non-bare layout: `main/` checkout plus linked worktree `linked/`.
    fn make_linked_layout(dirs: &TestDirs) -> (PathBuf, PathBuf) {
        let main = dirs.dir("main");
        run_git(&["init", "-q", "-b", "main"], &main);
        run_git(&["commit", "-q", "--allow-empty", "-m", "init"], &main);
        run_git(&["worktree", "add", "-q", "-b", "feat", "../linked"], &main);
        std::fs::create_dir_all(main.join("sub")).unwrap();
        std::fs::create_dir_all(dirs.root.join("linked/sub")).unwrap();
        (main, dirs.root.join("linked"))
    }

    #[gpui::test]
    async fn test_git_fallback_alone_adds_the_worktree_toplevel(cx: &mut TestAppContext) {
        // The open project is a FakeFs view of the main worktree whose
        // snapshot knows the common dir but NOT the feature worktree, so only
        // the `git rev-parse` fallback (real git on the real cwd) can match.
        let app_state = init_agent_test(cx);
        let dirs = TestDirs::new("fallback-only");
        let (bare, main_wt, feature_wt) = make_bare_layout(&dirs);
        let fs = app_state.fs.as_fake();
        fs.insert_tree(
            &main_wt,
            json!({
                ".git": format!("gitdir: {}", bare.join("worktrees/worktree-branch-main").display()),
                "crates": {},
            }),
        )
        .await;
        fs.insert_tree(
            &bare.join("worktrees/worktree-branch-main"),
            json!({ "commondir": "../..", "HEAD": "ref: refs/heads/main" }),
        )
        .await;
        let (window, main_ws) = open_window(cx, &app_state, &main_wt).await;
        wait_for_repo_scan(cx).await;
        let (known_linked, known_common) = cx.update(|cx| {
            let linked = collect_candidates(cx).2;
            (
                linked
                    .iter()
                    .flat_map(|l| l.linked_roots.clone())
                    .collect::<Vec<_>>(),
                linked
                    .iter()
                    .flat_map(|l| l.common_dirs.clone())
                    .collect::<Vec<_>>(),
            )
        });
        assert!(
            !known_linked
                .iter()
                .any(|root| root.starts_with(&feature_wt)),
            "snapshot must not know the feature worktree: {known_linked:?}"
        );
        assert!(
            known_common.contains(&canonical_or_self(&bare)),
            "{known_common:?}"
        );

        let outcome = request(
            cx,
            &app_state,
            &feature_wt.join("crates"),
            None,
            NoMatchBehavior::Error,
            Timeouts::default(),
        )
        .await;
        assert_eq!(outcome.status, 0, "{outcome:?}");
        cx.run_until_parked();
        assert_eq!(workspace_count(cx, window), 2);
        assert_eq!(
            displayed_workspace(cx, window),
            main_ws,
            "must not activate"
        );
        assert_eq!(cx.update(|cx| cx.windows().len()), 1);
        // The new workspace root is the git toplevel, not the cwd subdir.
        let roots = window
            .read_with(cx, |multi_workspace, cx| {
                multi_workspace
                    .workspaces()
                    .flat_map(|ws| ws.read(cx).root_paths(cx))
                    .map(|path| canonical_or_self(&path))
                    .collect::<Vec<_>>()
            })
            .unwrap();
        assert!(roots.contains(&canonical_or_self(&feature_wt)), "{roots:?}");
        assert!(
            !roots.iter().any(|root| root.ends_with("crates")),
            "{roots:?}"
        );
        let keys = window
            .read_with(cx, |multi_workspace, _| {
                multi_workspace.project_group_keys()
            })
            .unwrap();
        assert_eq!(keys.len(), 1);
    }

    #[gpui::test]
    async fn test_main_checkout_cwd_adds_main_toplevel_when_only_linked_is_open(
        cx: &mut TestAppContext,
    ) {
        let fake_state = init_agent_test(cx);
        let real_state = real_app_state(cx, &fake_state);
        let dirs = TestDirs::new("main-from-linked");
        let (main, linked) = make_linked_layout(&dirs);
        let (window, linked_ws) = open_window(cx, &real_state, &linked).await;
        wait_for_repo_scan(cx).await;

        let outcome = request(
            cx,
            &real_state,
            &main.join("sub"),
            None,
            NoMatchBehavior::Error,
            Timeouts::default(),
        )
        .await;
        assert_eq!(outcome.status, 0, "{outcome:?}");
        assert_eq!(workspace_count(cx, window), 2);
        assert_eq!(displayed_workspace(cx, window), linked_ws);
        let reply = outcome.stdout.unwrap();
        assert_eq!(
            reply["folder_paths"][0]
                .as_str()
                .map(|p| canonical_or_self(Path::new(p))),
            Some(canonical_or_self(&main)),
            "workspace root is the main checkout toplevel, not main/sub"
        );
    }

    #[gpui::test]
    async fn test_subdirectory_of_open_linked_worktree_adds_no_workspace(cx: &mut TestAppContext) {
        let fake_state = init_agent_test(cx);
        let real_state = real_app_state(cx, &fake_state);
        let dirs = TestDirs::new("sub-of-linked");
        let (_main, linked) = make_linked_layout(&dirs);
        let (window, linked_ws) = open_window(cx, &real_state, &linked).await;
        wait_for_repo_scan(cx).await;

        let outcome = request(
            cx,
            &real_state,
            &linked.join("sub"),
            None,
            NoMatchBehavior::Error,
            Timeouts::default(),
        )
        .await;
        assert_eq!(outcome.status, 0, "{outcome:?}");
        assert_eq!(workspace_count(cx, window), 1);
        assert_eq!(terminal_count(cx, &linked_ws), 1);
    }

    #[test]
    fn test_reply_title_ignores_the_live_shell_title() {
        let info = |title: &str, custom: Option<&str>| agent_ui::AgentPanelTerminalInfo {
            id: TerminalId::test_new(),
            title: title.to_string().into(),
            created_at: chrono::Utc::now(),
            has_notification: false,
            custom_title: custom.map(|title| title.to_string().into()),
            working_directory: None,
        };
        assert_eq!(reply_title(&info("zsh: live shell title", None)), None);
        assert_eq!(reply_title(&info("zsh", Some(""))), None);
        assert_eq!(
            reply_title(&info("zsh: live shell title", Some("my agent"))),
            Some("my agent".to_string())
        );
    }

    #[gpui::test]
    async fn test_reply_title_is_the_given_title_else_null(cx: &mut TestAppContext) {
        let app_state = init_agent_test(cx);
        let dirs = TestDirs::new("title");
        let root = dirs.dir("project");
        insert_dirs(&app_state, &root, json!({})).await;
        let (_window, _workspace) = open_window(cx, &app_state, &root).await;

        // The command sets the shell's live title; it must never be reported.
        let untitled = request(
            cx,
            &app_state,
            &root,
            Some("printf '\\033]0;live-shell-title\\007'"),
            NoMatchBehavior::Error,
            Timeouts::default(),
        )
        .await;
        assert_eq!(untitled.status, 0, "{untitled:?}");
        assert!(untitled.stdout.unwrap()["title"].is_null());

        let sink = CapturingSink::default();
        let mut async_cx = cx.to_async();
        handle_new_agent_terminal_with_timeouts(
            root.clone(),
            None,
            Some("my agent".to_string()),
            NoMatchBehavior::Error,
            None,
            None,
            app_state.clone(),
            Timeouts::default(),
            &sink,
            &mut async_cx,
        )
        .await;
        let titled = Outcome::from(&sink);
        assert_eq!(titled.status, 0, "{titled:?}");
        assert_eq!(titled.stdout.unwrap()["title"], "my agent");
    }

    #[cfg(unix)]
    #[gpui::test]
    async fn test_hung_git_fallback_times_out_to_no_match(cx: &mut TestAppContext) {
        use std::os::unix::fs::PermissionsExt as _;
        let app_state = init_agent_test(cx);
        let dirs = TestDirs::new("hung-git");
        let (bare, main_wt, feature_wt) = make_bare_layout(&dirs);
        let fs = app_state.fs.as_fake();
        fs.insert_tree(
            &main_wt,
            json!({
                ".git": format!("gitdir: {}", bare.join("worktrees/worktree-branch-main").display()),
                "crates": {},
            }),
        )
        .await;
        fs.insert_tree(
            &bare.join("worktrees/worktree-branch-main"),
            json!({ "commondir": "../..", "HEAD": "ref: refs/heads/main" }),
        )
        .await;
        let (window, _main_ws) = open_window(cx, &app_state, &main_wt).await;
        wait_for_repo_scan(cx).await;

        let fake_git = dirs.root.join("fake-git");
        std::fs::write(&fake_git, "#!/bin/sh\nexec sleep 30\n").unwrap();
        std::fs::set_permissions(&fake_git, std::fs::Permissions::from_mode(0o755)).unwrap();
        let program: &'static str =
            Box::leak(fake_git.to_string_lossy().into_owned().into_boxed_str());

        // Drive the handler by hand and advance the fake clock, so the
        // timeout is what ends the wait, not the 30s sleep.
        let started = std::time::Instant::now();
        let sink = CapturingSink::default();
        let mut async_cx = cx.to_async();
        let mut handler = Box::pin(handle_new_agent_terminal_with_timeouts(
            feature_wt.join("crates"),
            None,
            None,
            NoMatchBehavior::Error,
            None,
            None,
            app_state.clone(),
            Timeouts {
                git: Duration::from_millis(100),
                git_program: program,
                ..Timeouts::default()
            },
            &sink,
            &mut async_cx,
        ));
        loop {
            cx.executor().advance_clock(Duration::from_millis(200));
            cx.run_until_parked();
            if futures::poll!(handler.as_mut()).is_ready() {
                break;
            }
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "git timeout not applied"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        drop(handler);
        let outcome = Outcome::from(&sink);
        assert_eq!(outcome.status, 3, "{outcome:?}");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "git timeout not applied"
        );
        assert_eq!(workspace_count(cx, window), 1);
    }

    #[gpui::test]
    async fn test_real_bare_repo_layout_end_to_end(cx: &mut TestAppContext) {
        let fake_state = init_agent_test(cx);
        let real_state = real_app_state(cx, &fake_state);
        let dirs = TestDirs::new("real-bare");
        let (_bare, main_wt, feature_wt) = make_bare_layout(&dirs);

        let (window, main_ws) = open_window(cx, &real_state, &main_wt).await;
        wait_for_repo_scan(cx).await;

        let outcome = request(
            cx,
            &real_state,
            &feature_wt.join("crates"),
            None,
            NoMatchBehavior::Error,
            Timeouts::default(),
        )
        .await;
        assert_eq!(outcome.status, 0, "{outcome:?}");
        assert_eq!(workspace_count(cx, window), 2);
        assert_eq!(
            displayed_workspace(cx, window),
            main_ws,
            "must not activate"
        );
        assert_eq!(terminal_count(cx, &main_ws), 0);
        let reply = outcome.stdout.unwrap();
        assert_eq!(
            reply["folder_paths"][0]
                .as_str()
                .map(|p| canonical_or_self(Path::new(p))),
            Some(canonical_or_self(&feature_wt))
        );
        cx.run_until_parked();
        let keys = window
            .read_with(cx, |multi_workspace, _| {
                multi_workspace.project_group_keys()
            })
            .unwrap();
        assert_eq!(keys.len(), 1, "bare-repo worktrees share one group");
    }
    // ---- `--agent-close` / `--agent-rename` ----
    // All of these run on the fake file system plus real shells in a
    // temp-prefix directory (TestDirs); nothing touches the real home, Dock,
    // keychain or Zed data dirs.

    async fn new_terminal(
        cx: &mut TestAppContext,
        app_state: &Arc<AppState>,
        cwd: &Path,
        title: Option<&str>,
    ) -> String {
        let sink = CapturingSink::default();
        let mut async_cx = cx.to_async();
        handle_new_agent_terminal_with_timeouts(
            cwd.to_path_buf(),
            None,
            title.map(str::to_string),
            NoMatchBehavior::Error,
            None,
            None,
            app_state.clone(),
            Timeouts::default(),
            &sink,
            &mut async_cx,
        )
        .await;
        let outcome = Outcome::from(&sink);
        assert_eq!(outcome.status, 0, "{outcome:?}");
        outcome.stdout.unwrap()["id"].as_str().unwrap().to_string()
    }

    async fn close(cx: &mut TestAppContext, id: Option<&str>, title: Option<&str>) -> Outcome {
        let sink = CapturingSink::default();
        let mut async_cx = cx.to_async();
        handle_close_agent_thread_with_timeouts(
            id.map(str::to_string),
            title.map(str::to_string),
            Timeouts::default(),
            &sink,
            &mut async_cx,
        )
        .await;
        Outcome::from(&sink)
    }

    async fn rename(cx: &mut TestAppContext, id: &str, title: &str) -> Outcome {
        let sink = CapturingSink::default();
        let mut async_cx = cx.to_async();
        handle_rename_agent_thread_with_timeouts(
            id.to_string(),
            title.to_string(),
            Timeouts::default(),
            &sink,
            &mut async_cx,
        )
        .await;
        Outcome::from(&sink)
    }

    fn stored_ids(cx: &mut TestAppContext) -> Vec<String> {
        cx.update(|cx| {
            TerminalThreadMetadataStore::global(cx)
                .read(cx)
                .entries()
                .map(|metadata| metadata.terminal_id.to_string())
                .collect()
        })
    }

    async fn project_with_window(
        cx: &mut TestAppContext,
        name: &str,
    ) -> (TestDirs, Arc<AppState>, PathBuf, Entity<Workspace>) {
        let app_state = init_agent_test(cx);
        let dirs = TestDirs::new(name);
        let root = dirs.dir("project");
        insert_dirs(&app_state, &root, json!({})).await;
        let (_window, workspace) = open_window(cx, &app_state, &root).await;
        (dirs, app_state, root, workspace)
    }

    #[gpui::test]
    async fn test_close_by_id_removes_the_terminal_and_is_idempotent(cx: &mut TestAppContext) {
        let (_dirs, app_state, root, workspace) = project_with_window(cx, "close-id").await;
        let keep = new_terminal(cx, &app_state, &root, Some("keep")).await;
        let gone = new_terminal(cx, &app_state, &root, Some("gone")).await;
        cx.run_until_parked();
        assert_eq!(terminal_count(cx, &workspace), 2);
        assert!(stored_ids(cx).contains(&gone));

        let outcome = close(cx, Some(&gone), None).await;
        assert_eq!(outcome.status, 0, "{outcome:?}");
        let reply = outcome.stdout.unwrap();
        assert_eq!(reply["closed"], json!(true));
        assert_eq!(reply["id"], json!(gone));
        assert_eq!(reply["was_open"], json!(true));
        cx.run_until_parked();
        assert_eq!(terminal_count(cx, &workspace), 1);
        let ids = stored_ids(cx);
        assert!(!ids.contains(&gone), "metadata entry must go too: {ids:?}");
        assert!(ids.contains(&keep));

        // Second close of the same id: exit 0, nothing closed.
        let again = close(cx, Some(&gone), None).await;
        assert_eq!(again.status, 0, "{again:?}");
        let reply = again.stdout.unwrap();
        assert_eq!(reply["closed"], json!(false));
        assert!(
            reply["message"]
                .as_str()
                .unwrap()
                .contains("no such thread")
        );
        assert_eq!(terminal_count(cx, &workspace), 1);
    }

    #[gpui::test]
    async fn test_close_unknown_id_exits_0_and_malformed_id_exits_1(cx: &mut TestAppContext) {
        let (_dirs, app_state, root, workspace) = project_with_window(cx, "close-unknown").await;
        let _id = new_terminal(cx, &app_state, &root, None).await;
        cx.run_until_parked();

        let unknown = close(cx, Some(&uuid::Uuid::new_v4().to_string()), None).await;
        assert_eq!(unknown.status, 0, "{unknown:?}");
        assert_eq!(unknown.stdout.unwrap()["closed"], json!(false));

        let malformed = close(cx, Some("not-a-uuid"), None).await;
        assert_eq!(malformed.status, 1, "{malformed:?}");
        assert!(malformed.stderr.contains("invalid thread id"));
        assert_eq!(terminal_count(cx, &workspace), 1, "nothing may be closed");
    }

    #[gpui::test]
    async fn test_close_by_title_closes_the_single_match(cx: &mut TestAppContext) {
        let (_dirs, app_state, root, workspace) = project_with_window(cx, "close-title").await;
        let _keep = new_terminal(cx, &app_state, &root, Some("other (bg)")).await;
        let gone = new_terminal(cx, &app_state, &root, Some("worker (bg)")).await;
        cx.run_until_parked();

        // Zero matches (also no substring matching): exit 0, nothing closed.
        let none = close(cx, None, Some("worker")).await;
        assert_eq!(none.status, 0, "{none:?}");
        assert_eq!(none.stdout.unwrap()["closed"], json!(false));
        assert_eq!(terminal_count(cx, &workspace), 2);

        let outcome = close(cx, None, Some("worker (bg)")).await;
        assert_eq!(outcome.status, 0, "{outcome:?}");
        assert_eq!(outcome.stdout.unwrap()["id"], json!(gone));
        cx.run_until_parked();
        assert_eq!(terminal_count(cx, &workspace), 1);
    }

    #[gpui::test]
    async fn test_close_by_duplicate_title_refuses_and_lists_ids(cx: &mut TestAppContext) {
        let (_dirs, app_state, root, workspace) = project_with_window(cx, "close-dup").await;
        let first = new_terminal(cx, &app_state, &root, Some("dup")).await;
        let second = new_terminal(cx, &app_state, &root, Some("dup")).await;
        cx.run_until_parked();

        let outcome = close(cx, None, Some("dup")).await;
        assert_eq!(outcome.status, EXIT_AMBIGUOUS, "{outcome:?}");
        assert!(outcome.stdout.is_none());
        assert!(outcome.stderr.contains(&first), "{outcome:?}");
        assert!(outcome.stderr.contains(&second), "{outcome:?}");
        cx.run_until_parked();
        assert_eq!(terminal_count(cx, &workspace), 2, "none may be closed");
        assert_eq!(stored_ids(cx).len(), 2);
    }

    #[gpui::test]
    async fn test_close_refuses_an_agent_panel_thread(cx: &mut TestAppContext) {
        let (_dirs, app_state, root, workspace) = project_with_window(cx, "close-nonterm").await;
        let _id = new_terminal(cx, &app_state, &root, None).await;
        cx.run_until_parked();

        use agent_ui::thread_metadata_store::{ThreadId, ThreadMetadata, ThreadMetadataStore};
        let thread_id = ThreadId::new();
        cx.update(ThreadMetadataStore::init_global);
        // Let the store's initial DB load finish; it would replace a thread saved before it.
        cx.run_until_parked();
        cx.update(|cx| {
            let store = ThreadMetadataStore::global(cx);
            store.update(cx, |store, cx| {
                store.save(
                    ThreadMetadata {
                        thread_id,
                        session_id: None,
                        agent_id: agent::ZED_AGENT_ID.clone(),
                        title: Some("an agent-panel thread".into()),
                        title_override: None,
                        updated_at: chrono::Utc::now(),
                        created_at: None,
                        interacted_at: None,
                        worktree_paths: Default::default(),
                        remote_connection: None,
                        archived: false,
                    },
                    cx,
                );
            });
        });
        cx.run_until_parked();

        let id = thread_id.to_key_string();
        let closed = close(cx, Some(&id), None).await;
        assert_eq!(closed.status, EXIT_NOT_TERMINAL, "{closed:?}");
        assert!(closed.stderr.contains("not a terminal"), "{closed:?}");
        let renamed = rename(cx, &id, "x").await;
        assert_eq!(renamed.status, EXIT_NOT_TERMINAL, "{renamed:?}");
        assert_eq!(terminal_count(cx, &workspace), 1);
        cx.update(|cx| {
            let store = ThreadMetadataStore::global(cx);
            assert!(store.read(cx).entry(thread_id).is_some(), "left untouched");
        });
    }

    #[gpui::test]
    async fn test_close_stale_store_only_thread(cx: &mut TestAppContext) {
        let (_dirs, app_state, root, workspace) = project_with_window(cx, "close-stale").await;
        let live = new_terminal(cx, &app_state, &root, Some("live")).await;
        cx.run_until_parked();

        // A thread of a stopped session: persisted, but in no open panel.
        let stale_id = agent_ui::TerminalId::test_new();
        cx.update(|cx| {
            let store = TerminalThreadMetadataStore::global(cx);
            store.update(cx, |store, cx| {
                store.save(
                    agent_ui::terminal_thread_metadata_store::TerminalThreadMetadata {
                        terminal_id: stale_id,
                        title: "stale".into(),
                        custom_title: Some("old session (bg)".into()),
                        created_at: chrono::Utc::now(),
                        worktree_paths: Default::default(),
                        remote_connection: None,
                        working_directory: None,
                    },
                    cx,
                );
            });
        });
        let stale = stale_id.to_string();
        assert!(stored_ids(cx).contains(&stale));

        let outcome = close(cx, None, Some("old session (bg)")).await;
        assert_eq!(outcome.status, 0, "{outcome:?}");
        let reply = outcome.stdout.unwrap();
        assert_eq!(reply["id"], json!(stale));
        assert_eq!(reply["was_open"], json!(false));
        cx.run_until_parked();
        let ids = stored_ids(cx);
        assert!(!ids.contains(&stale) && ids.contains(&live), "{ids:?}");
        assert_eq!(terminal_count(cx, &workspace), 1);
    }

    #[gpui::test]
    async fn test_close_exits_4_when_ai_is_disabled(cx: &mut TestAppContext) {
        let (_dirs, app_state, root, _workspace) = project_with_window(cx, "close-noai").await;
        let id = new_terminal(cx, &app_state, &root, None).await;
        cx.run_until_parked();
        cx.update(|cx| {
            SettingsStore::update_global(cx, |store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.project.disable_ai = Some(settings::SaturatingBool(true));
                });
            });
        });
        cx.run_until_parked();
        let outcome = close(cx, Some(&id), None).await;
        assert_eq!(outcome.status, 4, "{outcome:?}");
        let outcome = rename(cx, &id, "x").await;
        assert_eq!(outcome.status, 4, "{outcome:?}");
    }

    #[gpui::test]
    async fn test_rename_sets_the_custom_title_and_replies_like_agent_new(cx: &mut TestAppContext) {
        let (_dirs, app_state, root, _workspace) = project_with_window(cx, "rename").await;
        let id = new_terminal(cx, &app_state, &root, Some("before")).await;
        cx.run_until_parked();

        let outcome = rename(cx, &id, "after (bg)").await;
        assert_eq!(outcome.status, 0, "{outcome:?}");
        let reply = outcome.stdout.unwrap();
        assert_eq!(reply["id"], json!(id));
        assert_eq!(reply["kind"], json!("terminal"));
        assert_eq!(reply["title"], json!("after (bg)"));
        assert!(reply["window_id"].is_u64(), "{reply}");
        assert!(
            reply["folder_paths"]
                .as_array()
                .is_some_and(|p| !p.is_empty())
        );
        assert!(reply.get("workspace_id").is_some());
        cx.run_until_parked();

        // The stored custom title follows, and a title lookup finds it.
        let stored = cx.update(|cx| {
            TerminalThreadMetadataStore::global(cx)
                .read(cx)
                .entries()
                .find_map(|metadata| metadata.custom_title.clone())
        });
        assert_eq!(stored.as_deref(), Some("after (bg)"));
        let by_title = close(cx, None, Some("before")).await;
        assert_eq!(by_title.stdout.unwrap()["closed"], json!(false));
    }

    #[gpui::test]
    async fn test_rename_unknown_id_exits_3_and_bad_titles_are_rejected(cx: &mut TestAppContext) {
        let (_dirs, app_state, root, _workspace) = project_with_window(cx, "rename-bad").await;
        let id = new_terminal(cx, &app_state, &root, Some("t")).await;
        cx.run_until_parked();

        let unknown = rename(cx, &uuid::Uuid::new_v4().to_string(), "x").await;
        assert_eq!(unknown.status, 3, "{unknown:?}");
        assert!(unknown.stderr.contains("no such thread"));

        for bad in ["", "   ", "a\nb"] {
            let outcome = rename(cx, &id, bad).await;
            assert_eq!(outcome.status, 1, "{bad:?} {outcome:?}");
        }
        let long = "x".repeat(MAX_TITLE_CHARS + 1);
        let outcome = rename(cx, &id, &long).await;
        assert_eq!(outcome.status, 1, "{outcome:?}");
        assert!(outcome.stderr.contains("too long"), "{outcome:?}");
        let at_cap = "y".repeat(MAX_TITLE_CHARS);
        assert_eq!(rename(cx, &id, &at_cap).await.status, 0);
        let malformed = rename(cx, "nope", "x").await;
        assert_eq!(malformed.status, 1, "{malformed:?}");
    }

    #[gpui::test]
    async fn test_rename_stale_store_only_thread(cx: &mut TestAppContext) {
        let (_dirs, _app_state, _root, _workspace) = project_with_window(cx, "rename-stale").await;
        let stale_id = agent_ui::TerminalId::test_new();
        cx.update(|cx| {
            TerminalThreadMetadataStore::global(cx).update(cx, |store, cx| {
                store.save(
                    agent_ui::terminal_thread_metadata_store::TerminalThreadMetadata {
                        terminal_id: stale_id,
                        title: "stale".into(),
                        custom_title: None,
                        created_at: chrono::Utc::now(),
                        worktree_paths: Default::default(),
                        remote_connection: None,
                        working_directory: None,
                    },
                    cx,
                );
            });
        });
        let outcome = rename(cx, &stale_id.to_string(), "renamed").await;
        assert_eq!(outcome.status, 0, "{outcome:?}");
        let reply = outcome.stdout.unwrap();
        assert_eq!(reply["title"], json!("renamed"));
        assert!(reply["window_id"].is_null());
    }

    // ---- CPZED-S2: hierarchy (parent / section) -------------------------------------------
    //
    // These tests use the fake file system, real temp dirs under the OS temp
    // directory for terminal cwds, and a test key-value database. They never
    // touch the real HOME, Zed data dirs or keychain.

    async fn new_thread(
        cx: &mut TestAppContext,
        app_state: &Arc<AppState>,
        cwd: &Path,
        parent: Option<&str>,
        section: Option<&str>,
    ) -> Outcome {
        let sink = CapturingSink::default();
        let mut async_cx = cx.to_async();
        handle_new_agent_terminal_with_timeouts(
            cwd.to_path_buf(),
            None,
            None,
            NoMatchBehavior::Error,
            parent.map(str::to_string),
            section.map(str::to_string),
            app_state.clone(),
            Timeouts::default(),
            &sink,
            &mut async_cx,
        )
        .await;
        Outcome::from(&sink)
    }

    async fn update_thread_request(
        cx: &mut TestAppContext,
        id: &str,
        parent: Option<ParentChange>,
        section: Option<SectionChange>,
    ) -> Outcome {
        let sink = CapturingSink::default();
        let mut async_cx = cx.to_async();
        handle_update_agent_thread_with_timeouts(
            id.to_string(),
            parent,
            section,
            Timeouts::default(),
            &sink,
            &mut async_cx,
        )
        .await;
        Outcome::from(&sink)
    }

    fn with_hierarchy<T>(
        cx: &mut TestAppContext,
        f: impl FnOnce(&cpzed_hierarchy::Hierarchy) -> T,
    ) -> T {
        cx.update(|cx| f(HierarchyStore::global(cx).read(cx).hierarchy()))
    }

    fn reply_id(outcome: &Outcome) -> String {
        assert_eq!(outcome.status, 0, "{outcome:?}");
        outcome.stdout.as_ref().unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string()
    }

    async fn hierarchy_project(
        cx: &mut TestAppContext,
        name: &str,
    ) -> (Arc<AppState>, TestDirs, PathBuf) {
        let app_state = init_agent_test(cx);
        let dirs = TestDirs::new(name);
        let root = dirs.dir("project");
        insert_dirs(&app_state, &root, json!({})).await;
        let _ = open_window(cx, &app_state, &root).await;
        (app_state, dirs, root)
    }

    #[gpui::test]
    async fn test_agent_new_parent_nests_the_new_terminal(cx: &mut TestAppContext) {
        let (app_state, _dirs, root) = hierarchy_project(cx, "s2parent").await;
        let parent = reply_id(&new_thread(cx, &app_state, &root, None, None).await);
        let outcome = new_thread(cx, &app_state, &root, Some(&parent), None).await;
        let child = reply_id(&outcome);
        assert_eq!(outcome.stdout.as_ref().unwrap()["parent"], parent.as_str());
        let (p, c) = (RowKey::Terminal(parent), RowKey::Terminal(child));
        assert_eq!(with_hierarchy(cx, |h| h.parents.get(&c).cloned()), Some(p));
    }

    #[gpui::test]
    async fn test_agent_new_unknown_parent_is_refused_before_spawning(cx: &mut TestAppContext) {
        let (app_state, _dirs, root) = hierarchy_project(cx, "s2noparent").await;
        let outcome = new_thread(
            cx,
            &app_state,
            &root,
            Some("00000000-0000-0000-0000-000000000000"),
            None,
        )
        .await;
        assert_eq!(outcome.status, 3, "{outcome:?}");
        assert!(outcome.stderr.contains("unknown parent"), "{outcome:?}");
        let count = cx.update(|cx| {
            TerminalThreadMetadataStore::global(cx)
                .read(cx)
                .entries()
                .count()
        });
        assert_eq!(count, 0, "no terminal may be created");
    }

    #[gpui::test]
    async fn test_invalid_section_names_are_refused_before_spawning(cx: &mut TestAppContext) {
        let (app_state, _dirs, root) = hierarchy_project(cx, "s2badname").await;
        let too_long = "x".repeat(MAX_SECTION_NAME_CHARS + 1);
        for bad in ["", "   ", too_long.as_str()] {
            let outcome = new_thread(cx, &app_state, &root, None, Some(bad)).await;
            assert_eq!(outcome.status, 6, "{outcome:?}");
        }
        let count = cx.update(|cx| {
            TerminalThreadMetadataStore::global(cx)
                .read(cx)
                .entries()
                .count()
        });
        assert_eq!(count, 0, "no stray thread for a bad section name");
        // --agent-set-section validates the name too, and changes nothing.
        let a = reply_id(&new_thread(cx, &app_state, &root, None, None).await);
        let before = with_hierarchy(cx, |h| h.to_json().unwrap());
        let outcome =
            update_thread_request(cx, &a, None, Some(SectionChange::Set("  ".into()))).await;
        assert_eq!(outcome.status, 6, "{outcome:?}");
        assert_eq!(with_hierarchy(cx, |h| h.to_json().unwrap()), before);
    }

    #[gpui::test]
    async fn test_agent_new_section_creates_it_once_in_the_threads_project(
        cx: &mut TestAppContext,
    ) {
        let (app_state, _dirs, root) = hierarchy_project(cx, "s2section").await;
        let first = new_thread(cx, &app_state, &root, None, Some("Personas")).await;
        let a = reply_id(&first);
        assert_eq!(first.stdout.as_ref().unwrap()["section_created"], true);
        let second = new_thread(cx, &app_state, &root, None, Some("personas")).await;
        let b = reply_id(&second);
        assert_eq!(second.stdout.as_ref().unwrap()["section_created"], false);

        let (a_key, b_key) = (RowKey::Terminal(a), RowKey::Terminal(b));
        with_hierarchy(cx, |h| {
            assert_eq!(h.sections.len(), 1, "one project");
            let sections = h.sections.values().next().unwrap();
            assert_eq!(sections.len(), 1, "no duplicate section");
            assert_eq!(sections[0].name, "Personas");
            // The newest thread goes on top of the section.
            assert_eq!(sections[0].items, vec![b_key, a_key]);
        });
    }

    #[gpui::test]
    async fn test_set_parent_cycle_exits_6_and_changes_nothing(cx: &mut TestAppContext) {
        let (app_state, _dirs, root) = hierarchy_project(cx, "s2cycle").await;
        let a = reply_id(&new_thread(cx, &app_state, &root, None, None).await);
        let b = reply_id(&new_thread(cx, &app_state, &root, Some(&a), None).await);
        let before = with_hierarchy(cx, |h| h.to_json().unwrap());
        let outcome = update_thread_request(cx, &a, Some(ParentChange::Set(b.clone())), None).await;
        assert_eq!(outcome.status, 6, "{outcome:?}");
        assert!(outcome.stderr.contains("ancestor"), "{outcome:?}");
        assert_eq!(with_hierarchy(cx, |h| h.to_json().unwrap()), before);
        // A thread cannot be its own parent either.
        let outcome = update_thread_request(cx, &a, Some(ParentChange::Set(a.clone())), None).await;
        assert_eq!(outcome.status, 6, "{outcome:?}");
    }

    #[gpui::test]
    async fn test_unknown_id_and_unknown_parent_both_exit_3(cx: &mut TestAppContext) {
        let (app_state, _dirs, root) = hierarchy_project(cx, "s2unknown").await;
        let a = reply_id(&new_thread(cx, &app_state, &root, None, None).await);
        let ghost = "11111111-1111-1111-1111-111111111111";
        let outcome = update_thread_request(cx, ghost, Some(ParentChange::Clear), None).await;
        assert_eq!(outcome.status, 3, "{outcome:?}");
        let outcome =
            update_thread_request(cx, &a, Some(ParentChange::Set(ghost.into())), None).await;
        assert_eq!(outcome.status, 3, "{outcome:?}");
        assert!(outcome.stderr.contains("unknown parent"), "{outcome:?}");
    }

    #[gpui::test]
    async fn test_set_section_none_unsections_and_set_parent_leaves_the_section(
        cx: &mut TestAppContext,
    ) {
        let (app_state, _dirs, root) = hierarchy_project(cx, "s2setsection").await;
        let p = reply_id(&new_thread(cx, &app_state, &root, None, None).await);
        let a = reply_id(&new_thread(cx, &app_state, &root, None, Some("S")).await);
        let a_key = RowKey::Terminal(a.clone());
        assert!(with_hierarchy(cx, |h| h.section_of(&a_key).is_some()));

        let outcome = update_thread_request(cx, &a, None, Some(SectionChange::Clear)).await;
        assert_eq!(outcome.status, 0, "{outcome:?}");
        assert!(outcome.stdout.as_ref().unwrap()["section"].is_null());
        assert!(with_hierarchy(cx, |h| h.section_of(&a_key).is_none()));

        // Back into the section (created now: it exists, so `false`), then nest.
        let outcome =
            update_thread_request(cx, &a, None, Some(SectionChange::Set("s".into()))).await;
        assert_eq!(outcome.status, 0, "{outcome:?}");
        assert_eq!(outcome.stdout.as_ref().unwrap()["section_created"], false);
        assert!(with_hierarchy(cx, |h| h.section_of(&a_key).is_some()));
        let outcome = update_thread_request(cx, &a, Some(ParentChange::Set(p.clone())), None).await;
        assert_eq!(outcome.status, 0, "{outcome:?}");
        with_hierarchy(cx, |h| {
            assert!(h.section_of(&a_key).is_none(), "nesting leaves the section");
            assert_eq!(h.parents.get(&a_key), Some(&RowKey::Terminal(p.clone())));
        });

        // `none` un-nests.
        let outcome = update_thread_request(cx, &a, Some(ParentChange::Clear), None).await;
        assert_eq!(outcome.status, 0, "{outcome:?}");
        assert!(with_hierarchy(cx, |h| !h.parents.contains_key(&a_key)));
    }

    #[gpui::test]
    async fn test_agent_list_reports_parent_section_and_depth(cx: &mut TestAppContext) {
        let (app_state, _dirs, root) = hierarchy_project(cx, "s2list").await;
        let a = reply_id(&new_thread(cx, &app_state, &root, None, Some("Personas")).await);
        let b = reply_id(&new_thread(cx, &app_state, &root, Some(&a), None).await);
        let c = reply_id(&new_thread(cx, &app_state, &root, Some(&b), None).await);
        let d = reply_id(&new_thread(cx, &app_state, &root, None, None).await);

        let list = cx.update(|cx| {
            let store = TerminalThreadMetadataStore::global(cx);
            let mut list = store.read(cx).list_json();
            annotate_agent_list(&mut list, cx);
            list
        });
        let row = |id: &str| {
            list.as_array()
                .unwrap()
                .iter()
                .find(|row| row["id"] == id)
                .unwrap()
                .clone()
        };
        assert_eq!(row(&a)["depth"], 0);
        assert_eq!(row(&a)["section"], "Personas");
        assert!(row(&a)["parent"].is_null());
        assert_eq!(row(&b)["depth"], 1);
        assert_eq!(row(&b)["parent"], a.as_str());
        assert_eq!(row(&b)["parent_kind"], "terminal");
        assert_eq!(row(&a)["parent_kind"], serde_json::Value::Null);
        assert!(row(&b)["section"].is_null());
        assert_eq!(row(&c)["depth"], 2);
        assert_eq!(row(&c)["parent"], b.as_str());
        assert_eq!(row(&d)["depth"], 0);
        assert!(row(&d)["section"].is_null());
        // Existing fields are unchanged.
        assert_eq!(row(&a)["kind"], "terminal");
    }
}
