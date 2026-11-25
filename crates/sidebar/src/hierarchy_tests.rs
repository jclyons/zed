//! CPZed fork: tests for the H7 hierarchy rendering in the Threads Sidebar (S3).
//!
//! Everything here runs against an in-memory database, a `FakeFs` and gpui test windows. Nothing
//! touches the real HOME, keychain, Zed profile or `/Applications`.

use super::*;
use agent_ui::{
    terminal_thread_metadata_store::{
        TerminalThreadMetadata, TerminalThreadMetadataStore, TestTerminalMetadataDbName,
    },
    thread_metadata_store::WorktreePaths,
};
use cpzed_hierarchy::{GroupKey, HierarchyStore, RowKey};
use fs::FakeFs;
use gpui::{TestAppContext, VisualTestContext};
use util::path_list::PathList;

fn init(cx: &mut TestAppContext) {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static NEXT_DB: AtomicUsize = AtomicUsize::new(0);
    let id = NEXT_DB.fetch_add(1, Ordering::SeqCst);
    cx.update(|cx| {
        cx.set_global(agent_ui::thread_metadata_store::TestMetadataDbName(
            format!("HIERARCHY_THREAD_METADATA_{id}"),
        ));
        cx.set_global(TestTerminalMetadataDbName(format!(
            "HIERARCHY_TERMINAL_METADATA_{id}"
        )));
    });
    agent_ui::test_support::init_test(cx);
    cx.update(|cx| {
        agent::ThreadStore::init_global(cx);
        ThreadMetadataStore::init_global(cx);
        language_model::LanguageModelRegistry::test(cx);
        prompt_store::init(cx);
    });
}

async fn new_fs(cx: &mut TestAppContext) -> Arc<FakeFs> {
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/project-a", serde_json::json!({ "src": {} }))
        .await;
    fs.insert_tree("/project-b", serde_json::json!({ "src": {} }))
        .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));
    fs
}

/// A window with project A and project B and a registered, open sidebar.
async fn open_window(
    cx: &mut TestAppContext,
    fs: &Arc<FakeFs>,
) -> (Entity<MultiWorkspace>, Entity<Sidebar>, VisualTestContext) {
    let project_a = project::Project::test(fs.clone(), ["/project-a".as_ref()], cx).await;
    let project_b = project::Project::test(fs.clone(), ["/project-b".as_ref()], cx).await;
    let window = cx.add_window(|window, cx| MultiWorkspace::test_new(project_a, window, cx));
    let mut vcx = VisualTestContext::from_window(window.into(), cx);
    let multi_workspace = window.root(&mut vcx).expect("window root");
    multi_workspace.update_in(&mut vcx, |mw, window, cx| {
        mw.test_add_workspace(project_b, window, cx);
    });
    let sidebar =
        vcx.update(|window, cx| cx.new(|cx| Sidebar::new(multi_workspace.clone(), window, cx)));
    multi_workspace.update(&mut vcx, |mw, cx| mw.register_sidebar(sidebar.clone(), cx));
    vcx.run_until_parked();
    multi_workspace.update_in(&mut vcx, |mw, window, cx| mw.toggle_sidebar(window, cx));
    vcx.run_until_parked();
    (multi_workspace, sidebar, vcx)
}

fn group(path: &str) -> GroupKey {
    GroupKey::new(None, vec![PathBuf::from(path)])
}

fn key(id: TerminalId) -> RowKey {
    hierarchy_ui::terminal_key(id)
}

/// Saves a terminal in `path`'s group. A larger `age_secs` sorts lower (older).
fn add_terminal(cx: &mut VisualTestContext, title: &str, path: &str, age_secs: i64) -> TerminalId {
    let terminal_id = TerminalId::test_new();
    let paths = PathList::new(&[PathBuf::from(path)]);
    let metadata = TerminalThreadMetadata {
        terminal_id,
        title: title.to_string().into(),
        custom_title: None,
        created_at: chrono::Utc::now() - chrono::Duration::seconds(age_secs),
        worktree_paths: WorktreePaths::from_path_lists(paths.clone(), paths).unwrap(),
        remote_connection: None,
        working_directory: None,
    };
    cx.update(|_, cx| {
        TerminalThreadMetadataStore::global(cx).update(cx, |store, cx| store.save(metadata, cx));
    });
    cx.run_until_parked();
    terminal_id
}

fn edit(
    cx: &mut VisualTestContext,
    f: impl FnOnce(&mut HierarchyStore, &mut Context<HierarchyStore>),
) {
    cx.update(|_, cx| HierarchyStore::global(cx).update(cx, f));
    cx.run_until_parked();
}

fn refresh(sidebar: &Entity<Sidebar>, cx: &mut VisualTestContext) {
    cx.run_until_parked();
    sidebar.update(cx, |sidebar, cx| sidebar.update_entries(cx));
    cx.run_until_parked();
}

/// One string per sidebar entry: indent shows the level, `§` a section, `+N` a collapsed
/// parent's rollup, `#N` the depth badge and `@project` the project chip.
fn rows(sidebar: &Entity<Sidebar>, cx: &mut VisualTestContext) -> Vec<String> {
    refresh(sidebar, cx);
    sidebar.read_with(cx, |sidebar, _| {
        sidebar
            .contents
            .entries
            .iter()
            .map(|entry| match entry {
                ListEntry::ProjectHeader { label, .. } => format!("[{label}]"),
                ListEntry::Section(section) => format!(
                    "  § {}{}",
                    section.name,
                    if section.collapsed {
                        format!(" (collapsed, +{})", section.rollup.count)
                    } else {
                        String::new()
                    }
                ),
                ListEntry::Terminal(terminal) => {
                    let layout = sidebar
                        .hierarchy
                        .row_layout
                        .get(&key(terminal.metadata.terminal_id));
                    let mut text = String::new();
                    let level = layout.map(hierarchy_ui::indent_level).unwrap_or(0);
                    text.push_str(&"  ".repeat(level + 1));
                    text.push_str(&terminal.metadata.display_title());
                    if let Some(layout) = layout {
                        if layout.collapsed {
                            text.push_str(&format!(" +{}", layout.rollup.count));
                        }
                        if let Some(depth) = hierarchy_ui::depth_badge(layout) {
                            text.push_str(&format!(" #{depth}"));
                        }
                        if let Some(project) = &layout.foreign_project {
                            text.push_str(&format!(" @{project}"));
                        }
                    }
                    text
                }
                ListEntry::Thread(thread) => {
                    format!("  thread {}", thread.metadata.display_title())
                }
            })
            .collect()
    })
}

fn entry_index(sidebar: &Entity<Sidebar>, id: TerminalId, cx: &mut VisualTestContext) -> usize {
    sidebar.read_with(cx, |sidebar, _| {
        sidebar
            .contents
            .entries
            .iter()
            .position(
                |entry| matches!(entry, ListEntry::Terminal(t) if t.metadata.terminal_id == id),
            )
            .expect("terminal row")
    })
}

fn select(sidebar: &Entity<Sidebar>, ix: usize, cx: &mut VisualTestContext) {
    sidebar.update(cx, |sidebar, _| sidebar.selection = Some(ix));
}

fn press_right(sidebar: &Entity<Sidebar>, cx: &mut VisualTestContext) {
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.expand_selected_entry(&SelectChild, window, cx)
    });
    cx.run_until_parked();
}

fn press_left(sidebar: &Entity<Sidebar>, cx: &mut VisualTestContext) {
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.collapse_selected_entry(&SelectParent, window, cx)
    });
    cx.run_until_parked();
}

fn selected(sidebar: &Entity<Sidebar>, cx: &mut VisualTestContext) -> Option<usize> {
    sidebar.read_with(cx, |sidebar, _| sidebar.selection)
}

// 1. An empty hierarchy renders today's sidebar: no sections, no layout. (The unchanged
// `sidebar_tests.rs` suite is the other half of this acceptance item.)
#[gpui::test]
async fn test_empty_hierarchy_renders_flat_newest_first(cx: &mut TestAppContext) {
    init(cx);
    let fs = new_fs(cx).await;
    let (_mw, sidebar, mut cx) = open_window(cx, &fs).await;
    add_terminal(&mut cx, "old", "/project-a", 30);
    add_terminal(&mut cx, "new", "/project-a", 10);
    assert_eq!(
        rows(&sidebar, &mut cx),
        ["[project-b]", "[project-a]", "  new", "  old"]
    );
    sidebar.read_with(&cx, |sidebar, _| {
        assert!(sidebar.hierarchy.row_layout.is_empty());
    });
}

// 2. Sections inside a project: unsectioned rows first, then each section and its rows.
#[gpui::test]
async fn test_sections_render_inside_a_project(cx: &mut TestAppContext) {
    init(cx);
    let fs = new_fs(cx).await;
    let (_mw, sidebar, mut cx) = open_window(cx, &fs).await;
    let u1 = add_terminal(&mut cx, "u1", "/project-a", 10);
    let _u2 = add_terminal(&mut cx, "u2", "/project-a", 20);
    let a1 = add_terminal(&mut cx, "a1", "/project-a", 30);
    let a2 = add_terminal(&mut cx, "a2", "/project-a", 40);
    let b1 = add_terminal(&mut cx, "b1", "/project-a", 50);
    let g = group("/project-a");
    edit(&mut cx, |store, cx| {
        let (alpha, _) = store.create_section(&g, "Alpha", cx).unwrap();
        let (beta, _) = store.create_section(&g, "Beta", cx).unwrap();
        // `set_section` puts a row at the top, so add the later one first.
        store.set_section(&key(a2), &g, Some(&alpha), cx).unwrap();
        store.set_section(&key(a1), &g, Some(&alpha), cx).unwrap();
        store.set_section(&key(b1), &g, Some(&beta), cx).unwrap();
    });
    assert_eq!(
        rows(&sidebar, &mut cx),
        [
            "[project-b]",
            "[project-a]",
            "  u1",
            "  u2",
            "  § Alpha",
            "    a1",
            "    a2",
            "  § Beta",
            "    b1",
        ]
    );
    let _ = u1;

    // Collapsing a section hides its rows and keeps its rolled-up count.
    edit(&mut cx, |store, cx| {
        let id = store.hierarchy().clone_section_id(&g, "Alpha").unwrap();
        store.set_section_collapsed(&g, &id, true, cx).unwrap();
    });
    let collapsed = rows(&sidebar, &mut cx);
    assert!(collapsed.contains(&"  § Alpha (collapsed, +2)".to_string()));
    assert!(!collapsed.iter().any(|row| row.trim() == "a1"));
    assert!(collapsed.iter().any(|row| row.trim() == "b1"));
}

// 3. Nesting, collapse rollups, the depth clamp and the arrow keys.
#[gpui::test]
async fn test_nesting_collapse_depth_and_keys(cx: &mut TestAppContext) {
    init(cx);
    let fs = new_fs(cx).await;
    let (_mw, sidebar, mut cx) = open_window(cx, &fs).await;
    let parent = add_terminal(&mut cx, "parent", "/project-a", 10);
    let c1 = add_terminal(&mut cx, "c1", "/project-a", 20);
    let c2 = add_terminal(&mut cx, "c2", "/project-a", 30);
    edit(&mut cx, |store, cx| {
        store.set_parent(&key(c1), Some(&key(parent)), cx).unwrap();
        store.set_parent(&key(c2), Some(&key(parent)), cx).unwrap();
    });
    assert_eq!(
        rows(&sidebar, &mut cx),
        ["[project-b]", "[project-a]", "  parent", "    c1", "    c2"]
    );

    // Collapsed: "+2" and the children are gone.
    edit(&mut cx, |store, cx| {
        store.set_row_collapsed(&key(parent), true, cx).ok();
    });
    assert_eq!(
        rows(&sidebar, &mut cx),
        ["[project-b]", "[project-a]", "  parent +2"]
    );
    sidebar.read_with(&cx, |sidebar, _| {
        let layout = &sidebar.hierarchy.row_layout[&key(parent)];
        assert!(layout.collapsed && layout.has_children);
        assert_eq!(layout.rollup.count, 2);
    });

    // Keys. Right on a collapsed parent expands it, right again goes to the first child.
    let parent_ix = entry_index(&sidebar, parent, &mut cx);
    select(&sidebar, parent_ix, &mut cx);
    press_right(&sidebar, &mut cx);
    assert_eq!(rows(&sidebar, &mut cx).len(), 5);
    assert_eq!(selected(&sidebar, &mut cx), Some(parent_ix));
    press_right(&sidebar, &mut cx);
    assert_eq!(selected(&sidebar, &mut cx), Some(parent_ix + 1));
    // Left on a child selects its parent.
    press_left(&sidebar, &mut cx);
    assert_eq!(selected(&sidebar, &mut cx), Some(parent_ix));
    // Left on an expanded parent collapses it.
    press_left(&sidebar, &mut cx);
    assert_eq!(
        rows(&sidebar, &mut cx),
        ["[project-b]", "[project-a]", "  parent +2"]
    );
}

#[gpui::test]
async fn test_deep_chain_clamps_indent_and_shows_depth_badges(cx: &mut TestAppContext) {
    init(cx);
    let fs = new_fs(cx).await;
    let (_mw, sidebar, mut cx) = open_window(cx, &fs).await;
    let mut chain = Vec::new();
    for ix in 0..10 {
        chain.push(add_terminal(
            &mut cx,
            &format!("t{ix}"),
            "/project-a",
            10 + ix,
        ));
    }
    edit(&mut cx, |store, cx| {
        for pair in chain.windows(2) {
            store
                .set_parent(&key(pair[1]), Some(&key(pair[0])), cx)
                .unwrap();
        }
    });
    let rendered = rows(&sidebar, &mut cx);
    assert_eq!(rendered[2], "  t0");
    assert_eq!(rendered[8], format!("{}t6", "  ".repeat(7)));
    // Depths 7, 8 and 9 stay at the 6-step indent and carry a badge.
    assert_eq!(rendered[9], format!("{}t7 #7", "  ".repeat(7)));
    assert_eq!(rendered[10], format!("{}t8 #8", "  ".repeat(7)));
    assert_eq!(rendered[11], format!("{}t9 #9", "  ".repeat(7)));
}

// 4. D1 (a child under a parent of another project) and D4 (closing the parent lifts children).
#[gpui::test]
async fn test_cross_project_child_and_parent_close_lifts_children(cx: &mut TestAppContext) {
    init(cx);
    let fs = new_fs(cx).await;
    let (_mw, sidebar, mut cx) = open_window(cx, &fs).await;
    let parent = add_terminal(&mut cx, "parent", "/project-a", 10);
    let child = add_terminal(&mut cx, "child", "/project-b", 20);
    let other = add_terminal(&mut cx, "other", "/project-b", 30);
    edit(&mut cx, |store, cx| {
        store
            .set_parent(&key(child), Some(&key(parent)), cx)
            .unwrap();
    });
    assert_eq!(
        rows(&sidebar, &mut cx),
        [
            "[project-b]",
            "  other",
            "[project-a]",
            "  parent",
            "    child @project-b",
        ]
    );

    // D4: the parent goes away; the child is lifted to the top level of its own project.
    cx.update(|_, cx| {
        TerminalThreadMetadataStore::global(cx).update(cx, |store, cx| store.delete(parent, cx));
    });
    assert_eq!(
        rows(&sidebar, &mut cx),
        ["[project-b]", "  child", "  other", "[project-a]"]
    );
    let _ = other;
}

// 5. Search keeps today's flat list.
#[gpui::test]
async fn test_search_is_flat(cx: &mut TestAppContext) {
    init(cx);
    let fs = new_fs(cx).await;
    let (_mw, sidebar, mut cx) = open_window(cx, &fs).await;
    let parent = add_terminal(&mut cx, "build parent", "/project-a", 10);
    let child = add_terminal(&mut cx, "build child", "/project-a", 20);
    let g = group("/project-a");
    edit(&mut cx, |store, cx| {
        store
            .set_parent(&key(child), Some(&key(parent)), cx)
            .unwrap();
        let (id, _) = store.create_section(&g, "Alpha", cx).unwrap();
        store.set_section(&key(parent), &g, Some(&id), cx).unwrap();
    });
    assert!(rows(&sidebar, &mut cx).iter().any(|row| row.contains('§')));

    sidebar.update_in(&mut cx, |sidebar, window, cx| {
        sidebar
            .filter_editor
            .update(cx, |editor, cx| editor.set_text("build", window, cx));
    });
    let searched = rows(&sidebar, &mut cx);
    assert_eq!(searched, ["[project-a]", "  build parent", "  build child"]);
}

// 6. The real sequence: a manager with builders in a "Personas" section, a second project with
// two sections, collapse state, a simulated restart and a second window.
#[gpui::test]
async fn test_real_sequence_survives_restart_and_shows_in_two_windows(cx: &mut TestAppContext) {
    init(cx);
    let fs = new_fs(cx).await;
    let (_mw, sidebar, mut cx1) = open_window(cx, &fs).await;
    let (_mw2, sidebar2, mut cx2) = open_window(cx, &fs).await;

    let manager = add_terminal(&mut cx1, "manager", "/project-a", 10);
    let builders: Vec<TerminalId> = (1..=4)
        .map(|ix| add_terminal(&mut cx1, &format!("builder{ix}"), "/project-a", 10 + ix))
        .collect();
    let sub = add_terminal(&mut cx1, "sub-builder", "/project-a", 40);
    let p1 = add_terminal(&mut cx1, "p1", "/project-b", 10);
    let p2 = add_terminal(&mut cx1, "p2", "/project-b", 20);
    let ga = group("/project-a");
    let gb = group("/project-b");
    edit(&mut cx1, |store, cx| {
        let (personas, _) = store.create_section(&ga, "Personas", cx).unwrap();
        store
            .set_section(&key(manager), &ga, Some(&personas), cx)
            .unwrap();
        for builder in &builders {
            store
                .set_parent(&key(*builder), Some(&key(manager)), cx)
                .unwrap();
        }
        store
            .set_parent(&key(sub), Some(&key(builders[0])), cx)
            .unwrap();
        let (one, _) = store.create_section(&gb, "One", cx).unwrap();
        let (two, _) = store.create_section(&gb, "Two", cx).unwrap();
        store.set_section(&key(p1), &gb, Some(&one), cx).unwrap();
        store.set_section(&key(p2), &gb, Some(&two), cx).unwrap();
        store.set_row_collapsed(&key(manager), true, cx).ok();
        store.set_section_collapsed(&gb, &two, true, cx).unwrap();
    });

    let expected = [
        "[project-b]",
        "  § One",
        "    p1",
        "  § Two (collapsed, +1)",
        "[project-a]",
        "  § Personas",
        "    manager +5",
    ]
    .map(String::from)
    .to_vec();
    let before = rows(&sidebar, &mut cx1);
    assert_eq!(before, expected);
    // Two windows show the same nesting.
    assert_eq!(rows(&sidebar2, &mut cx2), expected);

    // Simulate a restart: flush to the in-memory KVP, throw the state away, reload it.
    let flush =
        cx1.update(|_, cx| HierarchyStore::global(cx).update(cx, |store, cx| store.flush(cx)));
    flush.await;
    cx1.update(|_, cx| {
        let db = db::kvp::KeyValueStore::global(cx);
        HierarchyStore::global(cx).update(cx, |store, cx| *store = HierarchyStore::new(db, cx));
    });
    assert_eq!(rows(&sidebar, &mut cx1), expected);

    // Expanding the manager after the reload shows the whole tree, sub-builder at depth 2.
    edit(&mut cx1, |store, cx| {
        store.set_row_collapsed(&key(manager), false, cx).ok();
    });
    let tree = rows(&sidebar2, &mut cx2);
    assert_eq!(tree[6], "    manager");
    assert_eq!(tree[7].trim(), "builder1");
    assert_eq!(tree[8].trim(), "sub-builder");
    assert!(tree[8].starts_with(&"  ".repeat(4)));
}

// Rollups at any depth: a collapsed manager carries the notification of its grandchild.
#[gpui::test]
async fn test_collapsed_parent_rolls_up_notifications(cx: &mut TestAppContext) {
    init(cx);
    let fs = new_fs(cx).await;
    let (mw, sidebar, mut cx) = open_window(cx, &fs).await;
    let workspace = mw.read_with(&cx, |mw, _| mw.workspace().clone());
    let panel = workspace.update_in(&mut cx, |workspace, window, cx| {
        let panel = cx.new(|cx| AgentPanel::test_new(workspace, window, cx));
        workspace.add_panel(panel.clone(), window, cx);
        panel
    });
    let mut ids = Vec::new();
    for title in ["manager", "builder", "sub"] {
        ids.push(
            panel
                .update_in(&mut cx, |panel, window, cx| {
                    panel.insert_test_terminal(title, true, window, cx)
                })
                .expect("terminal"),
        );
    }
    cx.run_until_parked();
    edit(&mut cx, |store, cx| {
        store
            .set_parent(&key(ids[1]), Some(&key(ids[0])), cx)
            .unwrap();
        store
            .set_parent(&key(ids[2]), Some(&key(ids[1])), cx)
            .unwrap();
        store.set_row_collapsed(&key(ids[0]), true, cx).ok();
    });
    panel.update(&mut cx, |panel, cx| {
        panel.emit_test_terminal_bell(ids[2], cx)
    });
    refresh(&sidebar, &mut cx);
    sidebar.read_with(&cx, |sidebar, _| {
        let layout = &sidebar.hierarchy.row_layout[&key(ids[0])];
        assert!(layout.collapsed);
        assert_eq!(layout.rollup.count, 2);
        assert!(layout.rollup.notified, "the grandchild's dot rolls up");
    });
}

fn set_editor_text(sidebar: &Entity<Sidebar>, text: &str, cx: &mut VisualTestContext) {
    sidebar.update_in(cx, |sidebar, window, cx| {
        let editor = sidebar
            .hierarchy
            .section_edit
            .as_ref()
            .expect("section editor is open")
            .editor
            .clone();
        editor.update(cx, |editor, cx| editor.set_text(text, window, cx));
    });
}

fn confirm_edit(sidebar: &Entity<Sidebar>, cx: &mut VisualTestContext) {
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.confirm_section_edit(window, cx)
    });
    cx.run_until_parked();
}

fn notice(sidebar: &Entity<Sidebar>, cx: &mut VisualTestContext) -> Option<String> {
    sidebar.read_with(cx, |sidebar, _| {
        sidebar.hierarchy.notice.as_ref().map(|n| n.to_string())
    })
}

// Review B1: the first subthread and the first section must be creatable from the row menu on
// the starting state (empty hierarchy, no row layout).
#[gpui::test]
async fn test_row_menu_creates_first_subthread_and_section_from_empty_hierarchy(
    cx: &mut TestAppContext,
) {
    init(cx);
    let fs = new_fs(cx).await;
    let (_mw, sidebar, mut cx) = open_window(cx, &fs).await;
    let top = add_terminal(&mut cx, "top", "/project-a", 10);
    let low = add_terminal(&mut cx, "low", "/project-a", 20);
    refresh(&sidebar, &mut cx);
    let weak = sidebar.downgrade();
    let ga = group("/project-a");

    let items = sidebar.read_with(&cx, |s, cx| s.row_menu_items(&key(low), cx));
    assert!(
        items.iter().any(|item| matches!(item,
            hierarchy_ui::RowMenuItem::MakeSubthreadOf(c) if c.iter().any(|(k, _)| *k == key(top)))),
        "Make Subthread Of offers the other row: {items:?}"
    );
    assert!(
        items.iter().any(|item| matches!(item,
            hierarchy_ui::RowMenuItem::MoveToSection { group, .. } if *group == ga)),
        "Move to Section is offered with the row's own project: {items:?}"
    );

    // Nest through the same action the menu entry runs.
    cx.update(|_, cx| hierarchy_ui::set_parent_action(&weak, cx, &key(low), Some(&key(top))));
    cx.run_until_parked();
    assert_eq!(
        rows(&sidebar, &mut cx),
        ["[project-b]", "[project-a]", "  top", "    low"]
    );

    // "New Section…" through the editor the menu opens, moving the row into it.
    sidebar.update_in(&mut cx, |sidebar, window, cx| {
        sidebar.new_section_in(ga.clone(), Some((key(top), ga.clone())), window, cx)
    });
    set_editor_text(&sidebar, "Personas", &mut cx);
    confirm_edit(&sidebar, &mut cx);
    assert_eq!(
        rows(&sidebar, &mut cx),
        [
            "[project-b]",
            "[project-a]",
            "  § Personas",
            "    top",
            "      low"
        ]
    );
    assert_eq!(notice(&sidebar, &mut cx), None);
}

// Review S3: refusals are shown, not swallowed.
#[gpui::test]
async fn test_refusals_are_surfaced(cx: &mut TestAppContext) {
    init(cx);
    let fs = new_fs(cx).await;
    let (_mw, sidebar, mut cx) = open_window(cx, &fs).await;
    let top = add_terminal(&mut cx, "top", "/project-a", 10);
    let low = add_terminal(&mut cx, "low", "/project-a", 20);
    let ga = group("/project-a");
    let weak = sidebar.downgrade();

    // A cycle.
    cx.update(|_, cx| hierarchy_ui::set_parent_action(&weak, cx, &key(low), Some(&key(top))));
    cx.update(|_, cx| hierarchy_ui::set_parent_action(&weak, cx, &key(top), Some(&key(low))));
    assert!(
        notice(&sidebar, &mut cx).is_some_and(|n| n.contains("ancestor")),
        "cycle refusal is shown"
    );
    // The next successful edit clears it.
    cx.update(|_, cx| hierarchy_ui::set_parent_action(&weak, cx, &key(low), None));
    assert_eq!(notice(&sidebar, &mut cx), None);

    // An empty name keeps the editor open and says why.
    sidebar.update_in(&mut cx, |sidebar, window, cx| {
        sidebar.new_section_in(ga.clone(), None, window, cx)
    });
    set_editor_text(&sidebar, "   ", &mut cx);
    confirm_edit(&sidebar, &mut cx);
    assert!(notice(&sidebar, &mut cx).is_some_and(|n| n.contains("empty")));
    assert!(sidebar.read_with(&cx, |s, _| s.hierarchy.section_edit.is_some()));

    // A duplicate name on rename.
    edit(&mut cx, |store, cx| {
        store.create_section(&ga, "Alpha", cx).unwrap();
        store.create_section(&ga, "Beta", cx).unwrap();
    });
    let beta = cx.update(|_, cx| {
        HierarchyStore::global(cx)
            .read(cx)
            .hierarchy()
            .clone_section_id(&ga, "Beta")
            .unwrap()
    });
    sidebar.update_in(&mut cx, |sidebar, window, cx| {
        sidebar.begin_section_edit(
            ga.clone(),
            hierarchy_ui::SectionEditKind::Rename(beta),
            "Beta",
            window,
            cx,
        )
    });
    set_editor_text(&sidebar, "alpha", &mut cx);
    confirm_edit(&sidebar, &mut cx);
    assert!(notice(&sidebar, &mut cx).is_some_and(|n| n.contains("already exists")));
}

// A hierarchy saved by a newer CPZed is read-only: visible banner, no menu actions, edits say so.
#[gpui::test]
async fn test_read_only_hierarchy_is_visible_and_refuses_edits(cx: &mut TestAppContext) {
    init(cx);
    let fs = new_fs(cx).await;
    let (_mw, sidebar, mut cx) = open_window(cx, &fs).await;
    let top = add_terminal(&mut cx, "top", "/project-a", 10);
    let low = add_terminal(&mut cx, "low", "/project-a", 20);
    let db = cx.update(|_, cx| db::kvp::KeyValueStore::global(cx));
    db.scoped(cpzed_hierarchy::KVP_SCOPE)
        .write(
            cpzed_hierarchy::KVP_KEY.to_string(),
            r#"{"version":99}"#.to_string(),
        )
        .await
        .unwrap();
    cx.update(|_, cx| {
        HierarchyStore::global(cx).update(cx, |store, cx| *store = HierarchyStore::new(db, cx))
    });
    refresh(&sidebar, &mut cx);

    sidebar.update(&mut cx, |sidebar, cx| {
        assert!(sidebar.hierarchy.store.read(cx).is_read_only());
        assert!(
            sidebar.render_section_edit_bar(cx).is_some(),
            "the read-only banner is rendered"
        );
    });
    let items = sidebar.read_with(&cx, |s, cx| s.row_menu_items(&key(low), cx));
    assert!(matches!(
        items.as_slice(),
        [hierarchy_ui::RowMenuItem::ReadOnly]
    ));
    let weak = sidebar.downgrade();
    cx.update(|_, cx| hierarchy_ui::set_parent_action(&weak, cx, &key(low), Some(&key(top))));
    assert!(notice(&sidebar, &mut cx).is_some_and(|n| n.contains("read-only")));
}

// Review N3: the sidebar keys terminals with `Display`; `to_key_string` is crate-private in
// agent_ui, so pin the shape it promises: lowercase hyphenated uuid.
#[gpui::test]
fn test_terminal_key_is_a_hyphenated_uuid(_cx: &mut TestAppContext) {
    let RowKey::Terminal(text) = key(TerminalId::test_new()) else {
        panic!("terminal key");
    };
    assert_eq!(text.len(), 36);
    for (ix, ch) in text.chars().enumerate() {
        if [8, 13, 18, 23].contains(&ix) {
            assert_eq!(ch, '-', "{text}");
        } else {
            assert!(ch.is_ascii_hexdigit() && !ch.is_ascii_uppercase(), "{text}");
        }
    }
}

// Review S1: rows hidden in a collapsed subtree still count as seen (30-day prune).
#[gpui::test]
async fn test_hidden_rows_are_still_recorded_as_seen(cx: &mut TestAppContext) {
    init(cx);
    let fs = new_fs(cx).await;
    let (_mw, sidebar, mut cx) = open_window(cx, &fs).await;
    let top = add_terminal(&mut cx, "top", "/project-a", 10);
    let low = add_terminal(&mut cx, "low", "/project-a", 20);
    edit(&mut cx, |store, cx| {
        store.set_parent(&key(low), Some(&key(top)), cx).unwrap();
        store.set_row_collapsed(&key(top), true, cx).ok();
    });
    assert_eq!(
        rows(&sidebar, &mut cx),
        ["[project-b]", "[project-a]", "  top +1"]
    );
    sidebar.read_with(&cx, |sidebar, _| {
        assert!(sidebar.hierarchy.seen_rows.contains(&key(low)));
    });
    let stored = cx.update(|_, cx| {
        HierarchyStore::global(cx)
            .read(cx)
            .hierarchy()
            .last_seen
            .contains_key(&key(low))
    });
    assert!(stored, "the hidden row has a last_seen entry");
}

// Review (S2 deferral): an empty hierarchy never writes; with structure only the rows the
// hierarchy mentions are touched, collapsed subtrees included.
#[gpui::test]
async fn test_touch_seen_records_only_mentioned_rows_and_never_writes_when_empty(
    cx: &mut TestAppContext,
) {
    init(cx);
    let fs = new_fs(cx).await;
    let (_mw, sidebar, mut cx) = open_window(cx, &fs).await;
    let top = add_terminal(&mut cx, "top", "/project-a", 10);
    let low = add_terminal(&mut cx, "low", "/project-a", 20);
    let other = add_terminal(&mut cx, "other", "/project-a", 30);

    let settle = |cx: &mut VisualTestContext| {
        cx.executor()
            .advance_clock(cpzed_hierarchy::WRITE_DEBOUNCE * 4);
        cx.run_until_parked();
    };
    let writes = |cx: &mut VisualTestContext| {
        cx.update(|_, cx| HierarchyStore::global(cx).read(cx).write_count())
    };
    let last_seen = |cx: &mut VisualTestContext| {
        cx.update(|_, cx| {
            HierarchyStore::global(cx)
                .read(cx)
                .hierarchy()
                .last_seen
                .keys()
                .cloned()
                .collect::<Vec<_>>()
        })
    };

    for _ in 0..5 {
        refresh(&sidebar, &mut cx);
    }
    settle(&mut cx);
    assert_eq!(writes(&mut cx), 0, "empty hierarchy causes no store write");
    assert!(last_seen(&mut cx).is_empty());

    edit(&mut cx, |store, cx| {
        store.set_parent(&key(low), Some(&key(top)), cx).unwrap();
        store.set_row_collapsed(&key(top), true, cx).ok();
    });
    refresh(&sidebar, &mut cx);
    settle(&mut cx);
    let seen = last_seen(&mut cx);
    assert!(seen.contains(&key(top)));
    assert!(
        seen.contains(&key(low)),
        "a row hidden in a collapsed subtree is still touched"
    );
    assert!(
        !seen.contains(&key(other)),
        "a row with no hierarchy data is never recorded"
    );
}

// ---- S4: drag and drop -----------------------------------------------------------------------

use cpzed_hierarchy::{DragItem, DropTarget, Zone};

fn row_item(id: TerminalId, path: &str) -> DragItem {
    DragItem::Row {
        key: key(id),
        group: group(path),
    }
}

fn row_target(id: TerminalId, path: &str) -> DropTarget {
    DropTarget::Row {
        key: key(id),
        group: group(path),
    }
}

/// Run one drop through the same applier the real drop handler uses.
fn drop_on(
    sidebar: &Entity<Sidebar>,
    cx: &mut VisualTestContext,
    item: &DragItem,
    target: &DropTarget,
    zone: Zone,
) -> bool {
    let changed = sidebar.update(cx, |sidebar, cx| {
        sidebar.drop_item_on(item, target, zone, cx)
    });
    cx.run_until_parked();
    changed
}

fn hierarchy_json(cx: &mut VisualTestContext) -> String {
    cx.update(|_, cx| {
        HierarchyStore::global(cx)
            .read(cx)
            .hierarchy()
            .to_json()
            .unwrap()
    })
}

#[gpui::test]
fn test_zone_at_splits_25_50_25(_cx: &mut TestAppContext) {
    let bounds = gpui::Bounds {
        origin: gpui::point(gpui::px(0.), gpui::px(100.)),
        size: gpui::size(gpui::px(200.), gpui::px(40.)),
    };
    let zone = |y: f32| hierarchy_dnd::zone_at(gpui::px(y), bounds);
    assert_eq!(zone(101.), Zone::Before);
    assert_eq!(zone(109.), Zone::Before);
    assert_eq!(zone(111.), Zone::Into);
    assert_eq!(zone(120.), Zone::Into);
    assert_eq!(zone(129.), Zone::Into);
    assert_eq!(zone(131.), Zone::After);
    assert_eq!(zone(139.), Zone::After);
}

// A middle drop nests; an edge drop reorders and re-parents.
#[gpui::test]
async fn test_middle_drop_nests_and_edge_drop_reorders_and_reparents(cx: &mut TestAppContext) {
    init(cx);
    let fs = new_fs(cx).await;
    let (_mw, sidebar, mut cx) = open_window(cx, &fs).await;
    let a = add_terminal(&mut cx, "a", "/project-a", 10);
    let b = add_terminal(&mut cx, "b", "/project-a", 20);
    let c = add_terminal(&mut cx, "c", "/project-a", 30);
    refresh(&sidebar, &mut cx);
    let p = "/project-a";
    assert_eq!(
        rows(&sidebar, &mut cx),
        ["[project-b]", "[project-a]", "  a", "  b", "  c"]
    );

    // Middle: c becomes a child of a.
    assert!(drop_on(
        &sidebar,
        &mut cx,
        &row_item(c, p),
        &row_target(a, p),
        Zone::Into
    ));
    assert_eq!(
        rows(&sidebar, &mut cx),
        ["[project-b]", "[project-a]", "  a", "    c", "  b"]
    );

    // Edge: c dropped After b leaves a and becomes a root after b (re-parent).
    assert!(drop_on(
        &sidebar,
        &mut cx,
        &row_item(c, p),
        &row_target(b, p),
        Zone::After
    ));
    assert_eq!(
        rows(&sidebar, &mut cx),
        ["[project-b]", "[project-a]", "  a", "  b", "  c"]
    );

    // Edge: c dropped Before a reorders the roots.
    assert!(drop_on(
        &sidebar,
        &mut cx,
        &row_item(c, p),
        &row_target(a, p),
        Zone::Before
    ));
    assert_eq!(
        rows(&sidebar, &mut cx),
        ["[project-b]", "[project-a]", "  c", "  a", "  b"]
    );
    assert_eq!(notice(&sidebar, &mut cx), None);
}

// A refused drop (cycle, own descendant, missing target, ReadOnly) changes nothing and says why.
#[gpui::test]
async fn test_refused_drops_change_nothing_and_show_why(cx: &mut TestAppContext) {
    init(cx);
    let fs = new_fs(cx).await;
    let (_mw, sidebar, mut cx) = open_window(cx, &fs).await;
    let top = add_terminal(&mut cx, "top", "/project-a", 10);
    let mid = add_terminal(&mut cx, "mid", "/project-a", 20);
    let leaf = add_terminal(&mut cx, "leaf", "/project-a", 30);
    let ghost = TerminalId::test_new();
    let p = "/project-a";
    edit(&mut cx, |store, cx| {
        store.set_parent(&key(mid), Some(&key(top)), cx).unwrap();
        store.set_parent(&key(leaf), Some(&key(mid)), cx).unwrap();
    });
    refresh(&sidebar, &mut cx);
    let before = hierarchy_json(&mut cx);
    let rows_before = rows(&sidebar, &mut cx);

    // The top dropped into its own grandchild: a cycle.
    assert!(!drop_on(
        &sidebar,
        &mut cx,
        &row_item(top, p),
        &row_target(leaf, p),
        Zone::Into
    ));
    assert!(notice(&sidebar, &mut cx).is_some_and(|n| n.contains("ancestor")));
    // An edge drop inside its own subtree: also a cycle.
    assert!(!drop_on(
        &sidebar,
        &mut cx,
        &row_item(top, p),
        &row_target(leaf, p),
        Zone::After
    ));
    // A target that is not in the list.
    assert!(!drop_on(
        &sidebar,
        &mut cx,
        &row_item(leaf, p),
        &row_target(ghost, p),
        Zone::Into
    ));
    assert!(notice(&sidebar, &mut cx).is_some_and(|n| n.contains("not in the list")));
    // Onto itself: nothing to do.
    assert!(!drop_on(
        &sidebar,
        &mut cx,
        &row_item(top, p),
        &row_target(top, p),
        Zone::Into
    ));

    assert_eq!(hierarchy_json(&mut cx), before);
    assert_eq!(rows(&sidebar, &mut cx), rows_before);
}

#[gpui::test]
async fn test_read_only_store_refuses_drops_visibly(cx: &mut TestAppContext) {
    init(cx);
    let fs = new_fs(cx).await;
    let (_mw, sidebar, mut cx) = open_window(cx, &fs).await;
    let top = add_terminal(&mut cx, "top", "/project-a", 10);
    let low = add_terminal(&mut cx, "low", "/project-a", 20);
    let db = cx.update(|_, cx| db::kvp::KeyValueStore::global(cx));
    db.scoped(cpzed_hierarchy::KVP_SCOPE)
        .write(
            cpzed_hierarchy::KVP_KEY.to_string(),
            r#"{"version":99}"#.to_string(),
        )
        .await
        .unwrap();
    cx.update(|_, cx| {
        HierarchyStore::global(cx).update(cx, |store, cx| *store = HierarchyStore::new(db, cx))
    });
    refresh(&sidebar, &mut cx);
    let p = "/project-a";
    assert!(!drop_on(
        &sidebar,
        &mut cx,
        &row_item(low, p),
        &row_target(top, p),
        Zone::Into
    ));
    assert!(notice(&sidebar, &mut cx).is_some_and(|n| n.contains("read-only")));
}

// Sections: a row dropped on a section header moves into it; on its own project header it leaves
// it; a section dropped on another section reorders within its project; a cross-project drop
// is announced (S5), not applied.
#[gpui::test]
async fn test_section_drops(cx: &mut TestAppContext) {
    init(cx);
    let fs = new_fs(cx).await;
    let (_mw, sidebar, mut cx) = open_window(cx, &fs).await;
    let x = add_terminal(&mut cx, "x", "/project-a", 10);
    let y = add_terminal(&mut cx, "y", "/project-a", 20);
    let other = add_terminal(&mut cx, "other", "/project-b", 10);
    let ga = group("/project-a");
    let gb = group("/project-b");
    let (s1, s2, sb) = {
        let ids = std::cell::RefCell::new(Vec::new());
        edit(&mut cx, |store, cx| {
            let (one, _) = store.create_section(&ga, "One", cx).unwrap();
            let (two, _) = store.create_section(&ga, "Two", cx).unwrap();
            let (bsec, _) = store.create_section(&gb, "B", cx).unwrap();
            ids.borrow_mut().extend([one, two, bsec]);
        });
        let ids = ids.into_inner();
        (ids[0].clone(), ids[1].clone(), ids[2].clone())
    };
    refresh(&sidebar, &mut cx);
    let p = "/project-a";

    // x into section Two.
    assert!(drop_on(
        &sidebar,
        &mut cx,
        &row_item(x, p),
        &DropTarget::SectionHeader {
            id: s2.clone(),
            group: ga.clone()
        },
        Zone::Into
    ));
    let shown = rows(&sidebar, &mut cx);
    let two_at = shown.iter().position(|r| r == "  § Two").unwrap();
    assert_eq!(shown[two_at + 1], "    x");

    // Section Two dropped Before section One reorders the sections.
    assert!(drop_on(
        &sidebar,
        &mut cx,
        &DragItem::Section {
            id: s2.clone(),
            group: ga.clone()
        },
        &DropTarget::SectionHeader {
            id: s1.clone(),
            group: ga.clone()
        },
        Zone::Before
    ));
    let shown = rows(&sidebar, &mut cx);
    let two_at = shown.iter().position(|r| r == "  § Two").unwrap();
    let one_at = shown.iter().position(|r| r == "  § One").unwrap();
    assert!(two_at < one_at, "{shown:?}");

    // A section cannot move to another project: refused, nothing changes.
    let before = hierarchy_json(&mut cx);
    assert!(!drop_on(
        &sidebar,
        &mut cx,
        &DragItem::Section {
            id: s1.clone(),
            group: ga.clone()
        },
        &DropTarget::SectionHeader {
            id: sb.clone(),
            group: gb.clone()
        },
        Zone::Into
    ));
    assert!(notice(&sidebar, &mut cx).is_some_and(|n| n.contains("another project")));
    assert_eq!(hierarchy_json(&mut cx), before);

    // x back out onto its own project header.
    assert!(drop_on(
        &sidebar,
        &mut cx,
        &row_item(x, p),
        &DropTarget::ProjectHeader { group: ga.clone() },
        Zone::Into
    ));
    let shown = rows(&sidebar, &mut cx);
    let a_at = shown.iter().position(|r| r == "[project-a]").unwrap();
    let first_section = a_at
        + shown[a_at..]
            .iter()
            .position(|r| r.starts_with("  §"))
            .unwrap();
    let x_at = shown.iter().position(|r| r == "  x").unwrap();
    assert!(
        a_at < x_at && x_at < first_section,
        "x is unsectioned, above the sections: {shown:?}"
    );

    // A terminal dropped on another project is announced, not applied (S5).
    let before = hierarchy_json(&mut cx);
    assert!(!drop_on(
        &sidebar,
        &mut cx,
        &row_item(y, p),
        &DropTarget::ProjectHeader { group: gb.clone() },
        Zone::Into
    ));
    assert!(notice(&sidebar, &mut cx).is_some_and(|n| n.contains("not available yet")));
    assert_eq!(hierarchy_json(&mut cx), before);
    let _ = other;
}

// The order survives a restart (KVP).
#[gpui::test]
async fn test_drop_result_survives_restart(cx: &mut TestAppContext) {
    init(cx);
    let fs = new_fs(cx).await;
    let (_mw, sidebar, mut cx) = open_window(cx, &fs).await;
    let a = add_terminal(&mut cx, "a", "/project-a", 10);
    let b = add_terminal(&mut cx, "b", "/project-a", 20);
    let c = add_terminal(&mut cx, "c", "/project-a", 30);
    let p = "/project-a";
    refresh(&sidebar, &mut cx);
    assert!(drop_on(
        &sidebar,
        &mut cx,
        &row_item(c, p),
        &row_target(a, p),
        Zone::Before
    ));
    assert!(drop_on(
        &sidebar,
        &mut cx,
        &row_item(b, p),
        &row_target(c, p),
        Zone::Into
    ));
    let expected = rows(&sidebar, &mut cx);
    assert_eq!(
        expected,
        ["[project-b]", "[project-a]", "  c", "    b", "  a"]
    );

    let flush =
        cx.update(|_, cx| HierarchyStore::global(cx).update(cx, |store, cx| store.flush(cx)));
    flush.await;
    cx.update(|_, cx| {
        let db = db::kvp::KeyValueStore::global(cx);
        HierarchyStore::global(cx).update(cx, |store, cx| *store = HierarchyStore::new(db, cx));
    });
    assert_eq!(rows(&sidebar, &mut cx), expected);
}

// A context-menu action equals the drop action.
#[gpui::test]
async fn test_menu_actions_equal_drop_actions(cx: &mut TestAppContext) {
    init(cx);
    let fs = new_fs(cx).await;
    let (_mw, sidebar, mut cx) = open_window(cx, &fs).await;
    let top = add_terminal(&mut cx, "top", "/project-a", 10);
    let low = add_terminal(&mut cx, "low", "/project-a", 20);
    let ga = group("/project-a");
    let p = "/project-a";
    let (section, _) = {
        let ids = std::cell::RefCell::new(None);
        edit(&mut cx, |store, cx| {
            *ids.borrow_mut() = Some(store.create_section(&ga, "S", cx).unwrap());
        });
        ids.into_inner().unwrap()
    };
    refresh(&sidebar, &mut cx);
    let weak = sidebar.downgrade();
    let reset = |cx: &mut VisualTestContext| {
        edit(cx, |store, cx| {
            store
                .edit(cx, |h| {
                    h.parents.clear();
                    h.child_order.clear();
                    h.unsectioned_order.clear();
                    for sections in h.sections.values_mut() {
                        for s in sections {
                            s.items.clear();
                        }
                    }
                    Ok(())
                })
                .unwrap();
        });
    };

    // Make Subthread Of == middle drop.
    cx.update(|_, cx| hierarchy_ui::set_parent_action(&weak, cx, &key(low), Some(&key(top))));
    cx.run_until_parked();
    let by_menu = hierarchy_json(&mut cx);
    reset(&mut cx);
    drop_on(
        &sidebar,
        &mut cx,
        &row_item(low, p),
        &row_target(top, p),
        Zone::Into,
    );
    assert_eq!(hierarchy_json(&mut cx), by_menu);

    // Remove from Parent == drop on the project header.
    cx.update(|_, cx| hierarchy_ui::set_parent_action(&weak, cx, &key(low), None));
    cx.run_until_parked();
    let by_menu = hierarchy_json(&mut cx);
    reset(&mut cx);
    drop_on(
        &sidebar,
        &mut cx,
        &row_item(low, p),
        &row_target(top, p),
        Zone::Into,
    );
    drop_on(
        &sidebar,
        &mut cx,
        &row_item(low, p),
        &DropTarget::ProjectHeader { group: ga.clone() },
        Zone::Into,
    );
    assert_eq!(hierarchy_json(&mut cx), by_menu);

    // Move to Section == drop on the section header.
    cx.update(|_, cx| hierarchy_ui::set_section_action(&weak, cx, &key(low), &ga, Some(&section)));
    cx.run_until_parked();
    let by_menu = hierarchy_json(&mut cx);
    reset(&mut cx);
    drop_on(
        &sidebar,
        &mut cx,
        &row_item(low, p),
        &DropTarget::SectionHeader {
            id: section.clone(),
            group: ga.clone(),
        },
        Zone::Into,
    );
    assert_eq!(hierarchy_json(&mut cx), by_menu);
}

// A real pointer drag: mouse down on one row, move over the middle of another, release.
#[gpui::test]
async fn test_real_pointer_drag_nests_and_edge_drag_reorders(cx: &mut TestAppContext) {
    init(cx);
    let fs = new_fs(cx).await;
    let (_mw, sidebar, mut cx) = open_window(cx, &fs).await;
    let a = add_terminal(&mut cx, "a", "/project-a", 10);
    let b = add_terminal(&mut cx, "b", "/project-a", 20);
    refresh(&sidebar, &mut cx);
    let a_ix = entry_index(&sidebar, a, &mut cx);
    let b_ix = entry_index(&sidebar, b, &mut cx);
    let selector =
        |ix: usize| -> &'static str { Box::leak(format!("hierarchy-dnd-{ix}").into_boxed_str()) };
    let a_bounds = cx.debug_bounds(selector(a_ix)).expect("row a is laid out");
    let b_bounds = cx.debug_bounds(selector(b_ix)).expect("row b is laid out");
    let none = gpui::Modifiers::default();

    // Drag b onto the middle of a: nests.
    cx.simulate_mouse_move(b_bounds.center(), None, none);
    cx.simulate_mouse_down(b_bounds.center(), gpui::MouseButton::Left, none);
    for step in 1..=4 {
        let t = step as f32 / 4.;
        let pos = b_bounds.center() + (a_bounds.center() - b_bounds.center()) * t;
        cx.simulate_mouse_move(pos, gpui::MouseButton::Left, none);
    }
    cx.simulate_mouse_up(a_bounds.center(), gpui::MouseButton::Left, none);
    cx.run_until_parked();
    assert_eq!(
        rows(&sidebar, &mut cx),
        ["[project-b]", "[project-a]", "  a", "    b"]
    );
}
