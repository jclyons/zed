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
        cx.set_global(agent_ui::thread_metadata_store::TestMetadataDbName(format!(
            "HIERARCHY_THREAD_METADATA_{id}"
        )));
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
    let sidebar = vcx.update(|window, cx| {
        cx.new(|cx| Sidebar::new(multi_workspace.clone(), window, cx))
    });
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
                ListEntry::Thread(thread) => format!("  thread {}", thread.metadata.display_title()),
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
            .position(|entry| {
                matches!(entry, ListEntry::Terminal(t) if t.metadata.terminal_id == id)
            })
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
    edit(&mut cx, |store, cx| store.set_row_collapsed(&key(parent), true, cx));
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
        chain.push(add_terminal(&mut cx, &format!("t{ix}"), "/project-a", 10 + ix));
    }
    edit(&mut cx, |store, cx| {
        for pair in chain.windows(2) {
            store.set_parent(&key(pair[1]), Some(&key(pair[0])), cx).unwrap();
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
        store.set_parent(&key(child), Some(&key(parent)), cx).unwrap();
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
        store.set_parent(&key(child), Some(&key(parent)), cx).unwrap();
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
    assert_eq!(
        searched,
        ["[project-a]", "  build parent", "  build child"]
    );
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
        store.set_section(&key(manager), &ga, Some(&personas), cx).unwrap();
        for builder in &builders {
            store.set_parent(&key(*builder), Some(&key(manager)), cx).unwrap();
        }
        store.set_parent(&key(sub), Some(&key(builders[0])), cx).unwrap();
        let (one, _) = store.create_section(&gb, "One", cx).unwrap();
        let (two, _) = store.create_section(&gb, "Two", cx).unwrap();
        store.set_section(&key(p1), &gb, Some(&one), cx).unwrap();
        store.set_section(&key(p2), &gb, Some(&two), cx).unwrap();
        store.set_row_collapsed(&key(manager), true, cx);
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
    let flush = cx1.update(|_, cx| {
        HierarchyStore::global(cx).update(cx, |store, cx| store.flush(cx))
    });
    flush.await;
    cx1.update(|_, cx| {
        let db = db::kvp::KeyValueStore::global(cx);
        HierarchyStore::global(cx).update(cx, |store, cx| *store = HierarchyStore::new(db, cx));
    });
    assert_eq!(rows(&sidebar, &mut cx1), expected);

    // Expanding the manager after the reload shows the whole tree, sub-builder at depth 2.
    edit(&mut cx1, |store, cx| store.set_row_collapsed(&key(manager), false, cx));
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
        store.set_parent(&key(ids[1]), Some(&key(ids[0])), cx).unwrap();
        store.set_parent(&key(ids[2]), Some(&key(ids[1])), cx).unwrap();
        store.set_row_collapsed(&key(ids[0]), true, cx);
    });
    panel.update(&mut cx, |panel, cx| panel.emit_test_terminal_bell(ids[2], cx));
    refresh(&sidebar, &mut cx);
    sidebar.read_with(&cx, |sidebar, _| {
        let layout = &sidebar.hierarchy.row_layout[&key(ids[0])];
        assert!(layout.collapsed);
        assert_eq!(layout.rollup.count, 2);
        assert!(layout.rollup.notified, "the grandchild's dot rolls up");
    });
}
