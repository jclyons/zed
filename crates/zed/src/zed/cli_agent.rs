//! Handler for `zed --agent-new`: resolves which open workspace owns the
//! requested working directory and spawns a terminal thread there, without
//! activating, focusing or switching any window.
//!
//! Design: `designs/cpzed-3-agent-new.md` (fork-private).
//!
//! Known limitation (tracked as a follow-up): when the CLI launches Zed
//! itself, this handler can run before session restore has reopened the user's
//! windows. A cwd inside a project that restore is about to bring back then
//! reports "no match" (exit 3), and `--open-project` / `--new-window` can open
//! a duplicate. Fixing it needs the startup path in `main.rs`, which this
//! module does not own.

use agent_ui::terminal_thread_metadata_store::TerminalThreadMetadataStore;
use agent_ui::{AgentPanel, AgentPanelEvent, TerminalId};
use cli::{CliResponse, CliResponseSink, NoMatchBehavior};
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
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            confirm: Duration::from_secs(15),
            metadata: Duration::from_secs(5),
            panel_load: Duration::from_secs(10),
            git: Duration::from_secs(5),
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
    app_state: Arc<AppState>,
    responses: &dyn CliResponseSink,
    cx: &mut AsyncApp,
) {
    handle_new_agent_terminal_with_timeouts(
        cwd,
        command,
        title,
        on_no_match,
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
    app_state: Arc<AppState>,
    timeouts: Timeouts,
    responses: &dyn CliResponseSink,
    cx: &mut AsyncApp,
) {
    // Never hold the spawn lock when replying: the reply can block on IPC.
    let outcome = run(cwd, command, title, on_no_match, app_state, timeouts, cx).await;
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

    let _guard = SPAWN_LOCK.lock().await;
    #[cfg(test)]
    let _probe = tests::ConcurrencyProbe::enter();

    let target = resolve_target(&cwd, on_no_match, &app_state, timeouts.git, cx).await?;
    let panel = wait_for_panel(&target, timeouts.panel_load, cx).await?;
    spawn_and_confirm(&target, &panel, cwd, command, title, timeouts, cx).await
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
    git_timeout: Duration,
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
        let git_result = bounded(git_toplevel_and_common_dir(&canonical_cwd), git_timeout, cx)
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
async fn git_toplevel_and_common_dir(cwd: &Path) -> Option<(PathBuf, PathBuf)> {
    let output = util::command::new_command("git")
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
) -> Result<String, Failure> {
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

    Ok(reply_json(target, panel, terminal_id, &cwd, timeouts, cx).await)
}

async fn reply_json(
    target: &Target,
    panel: &Entity<AgentPanel>,
    terminal_id: TerminalId,
    cwd: &Path,
    timeouts: Timeouts,
    cx: &mut AsyncApp,
) -> String {
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
            "title": info.as_ref().map(|info| info.title.to_string()),
            "working_directory": info
                .and_then(|info| info.working_directory)
                .unwrap_or_else(|| cwd.to_path_buf())
                .to_string_lossy(),
            "folder_paths": folder_paths,
            "window_id": target.window.window_id().as_u64(),
            "workspace_id": workspace_id,
        })
        .to_string()
    })
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
                // Would exit 1 after this long if the check were not up front.
                panel_load: Duration::from_secs(3600),
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
            .args(args)
            .current_dir(cwd)
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

        let (toplevel, common_dir) =
            gpui::block_on(git_toplevel_and_common_dir(&feature_wt.join("crates")))
                .expect("git should resolve the worktree");
        assert_eq!(canonical_or_self(&toplevel), canonical_or_self(&feature_wt));
        assert_eq!(canonical_or_self(&common_dir), canonical_or_self(&bare));
        assert!(gpui::block_on(git_toplevel_and_common_dir(&dirs.root)).is_none());

        // The fallback's common dir must match what an open workspace of the
        // main worktree reports (its repository's common dir).
        let (_, main_common) = gpui::block_on(git_toplevel_and_common_dir(&main_wt)).unwrap();
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

    #[gpui::test]
    async fn test_real_bare_repo_layout_end_to_end(cx: &mut TestAppContext) {
        let fake_state = init_agent_test(cx);
        let real_state = Arc::new(AppState {
            languages: fake_state.languages.clone(),
            client: fake_state.client.clone(),
            user_store: fake_state.user_store.clone(),
            workspace_store: fake_state.workspace_store.clone(),
            fs: fs::RealFs::new(None, cx.executor()),
            build_window_options: fake_state.build_window_options,
            node_runtime: fake_state.node_runtime.clone(),
            session: fake_state.session.clone(),
        });
        let dirs = TestDirs::new("real-bare");
        let (_bare, main_wt, feature_wt) = make_bare_layout(&dirs);

        let (window, main_ws) = open_window(cx, &real_state, &main_wt).await;
        // Let the real repository scan report the repo.
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
                break;
            }
            assert!(std::time::Instant::now() < deadline, "repo never scanned");
            cx.executor().timer(Duration::from_millis(50)).await;
        }

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
}
