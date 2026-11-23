//! CPZed fork: rendering of the H7 hierarchy (sections inside project groups, nested subthread
//! rows at any depth, rollups, keys and menus) in the Threads Sidebar.
//!
//! `sidebar.rs` only carries thin hooks; the arranging itself is `cpzed_hierarchy::arrange_rows`.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use cpzed_hierarchy::{
    ArrangeInput, GroupKey, Hierarchy, HierarchyStore, Rollup, RowInfo, RowKey, SectionId, Slot,
    arrange_rows,
};
use editor::Editor;
use gpui::{Entity, Subscription, WeakEntity};
use ui::{ContextMenu, prelude::*, right_click_menu};

use super::*;

/// Indent step in px per depth level, and the depth at which the indent stops growing.
const INDENT_STEP: f32 = 12.;
pub(crate) const MAX_INDENT_DEPTH: usize = 6;

/// What `render_*` needs to know about a nested row. Filled in `rebuild_contents`.
#[derive(Clone, Debug)]
pub(crate) struct RowLayout {
    pub depth: usize,
    pub in_section: bool,
    pub has_children: bool,
    pub collapsed: bool,
    pub rollup: Rollup,
    /// The row's own project name, when it is shown under a parent of another project (D1).
    pub foreign_project: Option<SharedString>,
    /// The row's own project.
    pub group: GroupKey,
}

#[derive(Clone)]
pub(crate) struct SectionEntry {
    pub group: GroupKey,
    pub id: SectionId,
    pub name: SharedString,
    pub collapsed: bool,
    pub rollup: Rollup,
}

pub(crate) enum SectionEditKind {
    /// Create a section; when `then_move` is set, put that row into it afterwards.
    New { then_move: Option<(RowKey, GroupKey)> },
    Rename(SectionId),
}

pub(crate) struct SectionEdit {
    pub group: GroupKey,
    pub kind: SectionEditKind,
    pub editor: Entity<Editor>,
}

/// Fork-owned sidebar state, one field on `Sidebar`.
pub(crate) struct HierarchyUi {
    pub store: Entity<HierarchyStore>,
    pub row_layout: HashMap<RowKey, RowLayout>,
    pub section_edit: Option<SectionEdit>,
    pub _store_subscription: Subscription,
}

pub(crate) fn group_key(key: &ProjectGroupKey) -> GroupKey {
    let host = key.host().map(|host| host.display_name());
    GroupKey::new(host, key.path_list().paths().to_vec())
}

pub(crate) fn terminal_key(id: TerminalId) -> RowKey {
    // `TerminalId::to_key_string` is `pub(crate)` in agent_ui; `Display` prints the same
    // hyphenated uuid.
    RowKey::Terminal(id.to_string())
}

pub(crate) fn thread_key(id: ThreadId) -> RowKey {
    RowKey::Thread(id.to_key_string())
}

fn entry_row_key(entry: &ListEntry) -> Option<RowKey> {
    match entry {
        ListEntry::Thread(thread) => Some(thread_key(thread.metadata.thread_id)),
        ListEntry::Terminal(terminal) => Some(terminal_key(terminal.metadata.terminal_id)),
        ListEntry::ProjectHeader { .. } | ListEntry::Section(_) => None,
    }
}

/// An empty hierarchy renders exactly today's sidebar, so the whole arranging path is skipped.
pub(crate) fn has_structure(hierarchy: &Hierarchy) -> bool {
    hierarchy.sections.values().any(|s| !s.is_empty())
        || !hierarchy.parents.is_empty()
        || !hierarchy.child_order.is_empty()
        || !hierarchy.unsectioned_order.is_empty()
        || !hierarchy.collapsed_rows.is_empty()
}

/// The rows of one project group, held back until every group is known (D1 needs that).
pub(crate) struct DeferredGroup {
    pub at: usize,
    pub group: GroupKey,
    pub terminals: Vec<TerminalEntry>,
    pub threads: Vec<Arc<ThreadEntry>>,
}

fn project_name(group: &GroupKey) -> SharedString {
    group
        .paths
        .last()
        .and_then(|path| path.file_name())
        .map(|name| SharedString::from(name.to_string_lossy().to_string()))
        .unwrap_or_else(|| SharedString::from("project"))
}

/// Arrange every deferred group and splice the rows in after their project headers.
pub(crate) fn arrange_deferred(
    deferred: Vec<DeferredGroup>,
    entries: Vec<ListEntry>,
    hierarchy: &Hierarchy,
    notified_threads: &HashSet<ThreadId>,
    notified_terminals: &HashSet<TerminalId>,
    current_session_ids: &mut HashSet<acp::SessionId>,
    current_thread_ids: &mut HashSet<ThreadId>,
    row_layout: &mut HashMap<RowKey, RowLayout>,
) -> Vec<ListEntry> {
    row_layout.clear();

    // Every row, its info and its entry.
    let mut infos: Vec<Vec<RowInfo>> = Vec::with_capacity(deferred.len());
    let mut entry_of: HashMap<RowKey, ListEntry> = HashMap::new();
    let mut key_group: HashMap<RowKey, usize> = HashMap::new();
    for (gi, group) in deferred.iter().enumerate() {
        let mut rows = Vec::new();
        for terminal in &group.terminals {
            let key = terminal_key(terminal.metadata.terminal_id);
            key_group.insert(key.clone(), gi);
            rows.push(RowInfo {
                key: key.clone(),
                group: group.group.clone(),
                sort_time: terminal.metadata.created_at.timestamp_millis(),
                running: false,
                waiting: false,
                notified: notified_terminals.contains(&terminal.metadata.terminal_id),
            });
            entry_of.insert(key, ListEntry::Terminal(terminal.clone()));
        }
        for thread in &group.threads {
            let key = thread_key(thread.metadata.thread_id);
            key_group.insert(key.clone(), gi);
            if let Some(session_id) = &thread.metadata.session_id {
                current_session_ids.insert(session_id.clone());
            }
            current_thread_ids.insert(thread.metadata.thread_id);
            let sort_time = if thread.draft == Some(DraftKind::Empty) {
                i64::MAX
            } else {
                Sidebar::thread_display_time(&thread.metadata).timestamp_millis()
            };
            rows.push(RowInfo {
                key: key.clone(),
                group: group.group.clone(),
                sort_time,
                running: thread.status == AgentThreadStatus::Running,
                waiting: thread.status == AgentThreadStatus::WaitingForConfirmation,
                notified: notified_threads.contains(&thread.metadata.thread_id),
            });
            entry_of.insert(key, ListEntry::Thread(thread.clone()));
        }
        infos.push(rows);
    }

    // D1: a row whose parent chain leads into another group is also handed to that group.
    let mut foreign: Vec<Vec<RowInfo>> = vec![Vec::new(); deferred.len()];
    for (gi, rows) in infos.iter().enumerate() {
        for row in rows {
            let mut reached: Vec<usize> = Vec::new();
            let mut seen: HashSet<&RowKey> = HashSet::new();
            let mut cursor = &row.key;
            while let Some(parent) = hierarchy.parents.get(cursor) {
                if !seen.insert(parent) {
                    break;
                }
                if let Some(&pg) = key_group.get(parent)
                    && pg != gi
                    && !reached.contains(&pg)
                {
                    reached.push(pg);
                }
                cursor = parent;
            }
            for pg in reached {
                foreign[pg].push(row.clone());
            }
        }
    }

    let all_keys: Vec<(usize, &RowKey)> = infos
        .iter()
        .enumerate()
        .flat_map(|(gi, rows)| rows.iter().map(move |row| (gi, &row.key)))
        .collect();

    let mut arranged: Vec<(usize, Vec<ListEntry>)> = Vec::with_capacity(deferred.len());
    for (gi, group) in deferred.iter().enumerate() {
        let elsewhere: std::collections::BTreeSet<RowKey> = all_keys
            .iter()
            .filter(|(other, _)| *other != gi)
            .map(|(_, key)| (*key).clone())
            .collect();
        let slots = arrange_rows(&ArrangeInput {
            group: &group.group,
            rows: &infos[gi],
            foreign: &foreign[gi],
            elsewhere: &elsewhere,
            hierarchy,
            query_mode: false,
        });
        let mut out = Vec::with_capacity(slots.len());
        let mut in_section = false;
        for slot in slots {
            match slot {
                Slot::SectionHeader {
                    id,
                    name,
                    collapsed,
                    rollup,
                } => {
                    in_section = true;
                    out.push(ListEntry::Section(SectionEntry {
                        group: group.group.clone(),
                        id,
                        name: name.into(),
                        collapsed,
                        rollup,
                    }));
                }
                Slot::Row {
                    key,
                    depth,
                    has_children,
                    collapsed,
                    rollup,
                    foreign_project,
                } => {
                    let Some(entry) = entry_of.get(&key) else {
                        continue;
                    };
                    let own_group = key_group
                        .get(&key)
                        .map(|&g| deferred[g].group.clone())
                        .unwrap_or_else(|| group.group.clone());
                    row_layout.insert(
                        key,
                        RowLayout {
                            depth,
                            in_section,
                            has_children,
                            collapsed,
                            rollup,
                            foreign_project: foreign_project
                                .then(|| project_name(&own_group)),
                            group: own_group,
                        },
                    );
                    out.push(entry.clone());
                }
            }
        }
        arranged.push((group.at, out));
    }

    // Splice each group's rows at the position recorded after its header.
    let mut arranged = arranged.into_iter().peekable();
    let mut result = Vec::with_capacity(entries.len());
    for (ix, entry) in entries.into_iter().enumerate() {
        while arranged.peek().is_some_and(|(at, _)| *at == ix) {
            if let Some((_, rows)) = arranged.next() {
                result.extend(rows);
            }
        }
        result.push(entry);
    }
    for (_, rows) in arranged {
        result.extend(rows);
    }
    result
}

/// Indent steps of a row: depth clamped at [`MAX_INDENT_DEPTH`], plus one inside a section.
pub(crate) fn indent_level(layout: &RowLayout) -> usize {
    layout.depth.min(MAX_INDENT_DEPTH) + layout.in_section as usize
}

/// The depth shown as a badge once the indent has stopped growing.
pub(crate) fn depth_badge(layout: &RowLayout) -> Option<usize> {
    (layout.depth > MAX_INDENT_DEPTH).then_some(layout.depth)
}

fn rollup_label(rollup: &Rollup, include_count: bool) -> String {
    let mut parts: Vec<String> = Vec::new();
    if include_count {
        parts.push(format!("+{}", rollup.count));
    }
    if rollup.running > 0 {
        parts.push(format!("{} running", rollup.running));
    }
    if rollup.waiting > 0 {
        parts.push(format!("{} waiting", rollup.waiting));
    }
    parts.join(" · ")
}

fn store_update(
    cx: &mut App,
    f: impl FnOnce(&mut HierarchyStore, &mut Context<HierarchyStore>),
) {
    if let Some(store) = HierarchyStore::try_global(cx) {
        store.update(cx, f);
    }
}

impl Sidebar {
    pub(crate) fn new_hierarchy_ui(cx: &mut Context<Self>) -> HierarchyUi {
        let store = HierarchyStore::init(cx);
        let subscription = cx.observe(&store, |this, _store, cx| {
            this.schedule_update_entries(false, cx);
        });
        HierarchyUi {
            store,
            row_layout: HashMap::new(),
            section_edit: None,
            _store_subscription: subscription,
        }
    }

    /// Quietly record that the visible rows and groups were seen (drives the 30-day prune).
    pub(crate) fn touch_hierarchy_seen(&mut self, cx: &mut Context<Self>) {
        let mut rows: Vec<RowKey> = Vec::new();
        let mut groups: Vec<GroupKey> = Vec::new();
        for entry in &self.contents.entries {
            match entry {
                ListEntry::ProjectHeader { key, .. } => groups.push(group_key(key)),
                other => rows.extend(entry_row_key(other)),
            }
        }
        let now = chrono::Utc::now().timestamp();
        self.hierarchy.store.update(cx, |store, cx| {
            store.touch_seen(&rows, &groups, now, cx);
        });
    }

    /// Wrap a rendered thread or terminal row with its indent, chevron, depth badge, rollup and
    /// project chip.
    pub(crate) fn wrap_hierarchy_row(
        &self,
        ix: usize,
        key: RowKey,
        row: AnyElement,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(layout) = self.hierarchy.row_layout.get(&key) else {
            return row;
        };
        if layout.depth == 0
            && !layout.in_section
            && !layout.has_children
            && layout.foreign_project.is_none()
        {
            return row;
        }
        let level = indent_level(layout);
        let has_children = layout.has_children;
        let collapsed = layout.collapsed;
        let toggle_key = key.clone();

        let chevron = if has_children {
            IconButton::new(
                ("hierarchy-chevron", ix),
                if collapsed {
                    IconName::ChevronRight
                } else {
                    IconName::ChevronDown
                },
            )
            .icon_size(IconSize::XSmall)
            .on_click(cx.listener(move |this, _, _window, cx| {
                this.set_row_subtree_collapsed(&toggle_key, !collapsed, cx);
            }))
            .into_any_element()
        } else {
            div().w(px(INDENT_STEP + 4.)).into_any_element()
        };

        let rollup_text = if collapsed {
            rollup_label(&layout.rollup, true)
        } else {
            String::new()
        };

        h_flex()
            .w_full()
            .pl(px(INDENT_STEP * level as f32))
            .child(chevron)
            .child(div().flex_1().min_w_0().child(row))
            .when_some(depth_badge(layout), |this, depth| {
                this.child(
                    Label::new(format!("{depth}"))
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
            })
            .when(!rollup_text.is_empty(), |this| {
                this.child(
                    Label::new(rollup_text)
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
            })
            .when(collapsed && layout.rollup.notified, |this| {
                this.child(
                    Icon::new(IconName::Circle)
                        .size(IconSize::XSmall)
                        .color(Color::Accent),
                )
            })
            .when_some(layout.foreign_project.clone(), |this, project| {
                this.child(
                    Label::new(project)
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
            })
            .into_any_element()
    }

    fn set_row_subtree_collapsed(&mut self, key: &RowKey, collapsed: bool, cx: &mut Context<Self>) {
        self.hierarchy
            .store
            .update(cx, |store, cx| store.set_row_collapsed(key, collapsed, cx));
    }

    fn set_section_collapsed(
        &mut self,
        group: &GroupKey,
        id: &SectionId,
        collapsed: bool,
        cx: &mut Context<Self>,
    ) {
        self.hierarchy.store.update(cx, |store, cx| {
            store.set_section_collapsed(group, id, collapsed, cx).ok();
        });
    }

    pub(crate) fn render_section_header(
        &self,
        ix: usize,
        section: &SectionEntry,
        is_selected: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = cx.theme().colors();
        let group = section.group.clone();
        let id = section.id.clone();
        let collapsed = section.collapsed;
        let rollup_text = rollup_label(&section.rollup, collapsed);

        let header = h_flex()
            .id(("section-header", ix))
            .w_full()
            .h(px(28.))
            .pl(px(INDENT_STEP))
            .pr_2()
            .gap_1()
            .bg(colors.panel_background)
            .when(is_selected, |this| this.bg(colors.element_selected))
            .hover(|this| this.bg(colors.element_hover))
            .child(
                Icon::new(if collapsed {
                    IconName::ChevronRight
                } else {
                    IconName::ChevronDown
                })
                .size(IconSize::XSmall)
                .color(Color::Muted),
            )
            .child(
                Label::new(section.name.clone())
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .when(!rollup_text.is_empty(), |this| {
                this.child(
                    Label::new(rollup_text)
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
            })
            .when(section.rollup.notified, |this| {
                this.child(
                    Icon::new(IconName::Circle)
                        .size(IconSize::XSmall)
                        .color(Color::Accent),
                )
            })
            .on_click({
                let group = group.clone();
                let id = id.clone();
                cx.listener(move |this, _, _window, cx| {
                    this.selection = Some(ix);
                    this.set_section_collapsed(&group, &id, !collapsed, cx);
                })
            });

        let sidebar = cx.weak_entity();
        let name = section.name.clone();
        right_click_menu(SharedString::from(format!("section-menu-{ix}")))
            .trigger(move |_, _, _| header)
            .menu(move |window, cx| {
                let sidebar = sidebar.clone();
                let group = group.clone();
                let id = id.clone();
                let name = name.clone();
                ContextMenu::build(window, cx, move |menu, _window, _cx| {
                    menu.entry("Rename", None, {
                        let sidebar = sidebar.clone();
                        let group = group.clone();
                        let id = id.clone();
                        let name = name.clone();
                        move |window, cx| {
                            sidebar
                                .update(cx, |sidebar, cx| {
                                    sidebar.begin_section_edit(
                                        group.clone(),
                                        SectionEditKind::Rename(id.clone()),
                                        &name,
                                        window,
                                        cx,
                                    );
                                })
                                .ok();
                        }
                    })
                    .entry("Delete Section", None, {
                        let group = group.clone();
                        let id = id.clone();
                        move |_window, cx| {
                            store_update(cx, |store, cx| {
                                store.delete_section(&group, &id, cx).ok();
                            });
                        }
                    })
                    .entry(if collapsed { "Expand" } else { "Collapse" }, None, {
                        let group = group.clone();
                        let id = id.clone();
                        move |_window, cx| {
                            store_update(cx, |store, cx| {
                                store
                                    .set_section_collapsed(&group, &id, !collapsed, cx)
                                    .ok();
                            });
                        }
                    })
                })
            })
            .into_any_element()
    }

    // ---- keys -----------------------------------------------------------------------------

    /// ← and → on hierarchy rows. Returns false to fall through to today's behavior.
    pub(crate) fn handle_hierarchy_key(&mut self, expand: bool, cx: &mut Context<Self>) -> bool {
        let Some(ix) = self.selection else {
            return false;
        };
        let Some(entry) = self.contents.entries.get(ix).cloned() else {
            return false;
        };
        match entry {
            ListEntry::Section(section) => {
                if expand {
                    if section.collapsed {
                        self.set_section_collapsed(&section.group, &section.id, false, cx);
                    } else if ix + 1 < self.contents.entries.len() {
                        self.select_entry(ix + 1, cx);
                    }
                    true
                } else if !section.collapsed {
                    self.set_section_collapsed(&section.group, &section.id, true, cx);
                    true
                } else {
                    false
                }
            }
            ListEntry::Thread(_) | ListEntry::Terminal(_) => {
                let Some(key) = entry_row_key(&entry) else {
                    return false;
                };
                let Some(layout) = self.hierarchy.row_layout.get(&key).cloned() else {
                    return false;
                };
                if expand {
                    if !layout.has_children {
                        return false;
                    }
                    if layout.collapsed {
                        self.set_row_subtree_collapsed(&key, false, cx);
                    } else if ix + 1 < self.contents.entries.len() {
                        self.select_entry(ix + 1, cx);
                    }
                    return true;
                }
                if layout.has_children && !layout.collapsed {
                    self.set_row_subtree_collapsed(&key, true, cx);
                    return true;
                }
                // A child selects its parent: the nearest earlier row with a smaller depth. A
                // root in a section selects its section header.
                for i in (0..ix).rev() {
                    match self.contents.entries.get(i) {
                        Some(ListEntry::Section(_)) if layout.depth == 0 => {
                            self.select_entry(i, cx);
                            return true;
                        }
                        Some(other) if layout.depth > 0 => {
                            if let Some(other_key) = entry_row_key(other)
                                && self
                                    .hierarchy
                                    .row_layout
                                    .get(&other_key)
                                    .is_some_and(|l| l.depth < layout.depth)
                            {
                                self.select_entry(i, cx);
                                return true;
                            }
                        }
                        Some(ListEntry::ProjectHeader { .. }) | None => break,
                        _ => {}
                    }
                }
                false
            }
            ListEntry::ProjectHeader { .. } => false,
        }
    }

    fn select_entry(&mut self, ix: usize, cx: &mut Context<Self>) {
        self.selection = Some(ix);
        self.list_state.scroll_to_reveal_item(ix);
        cx.notify();
    }

    /// Enter on a section header toggles it.
    pub(crate) fn confirm_section(&mut self, section: &SectionEntry, cx: &mut Context<Self>) {
        self.set_section_collapsed(&section.group, &section.id, !section.collapsed, cx);
    }

    // ---- section name editing ---------------------------------------------------------------

    pub(crate) fn begin_section_edit(
        &mut self,
        group: GroupKey,
        kind: SectionEditKind,
        initial: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Section name", window, cx);
            editor.set_text(initial, window, cx);
            editor
        });
        window.focus(&editor.focus_handle(cx), cx);
        self.hierarchy.section_edit = Some(SectionEdit {
            group,
            kind,
            editor,
        });
        cx.notify();
    }

    fn confirm_section_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(edit) = self.hierarchy.section_edit.take() else {
            return;
        };
        let name = edit.editor.read(cx).text(cx).trim().to_string();
        if !name.is_empty() {
            let group = edit.group.clone();
            self.hierarchy.store.update(cx, |store, cx| match edit.kind {
                SectionEditKind::New { then_move } => {
                    if let Ok((id, _)) = store.create_section(&group, &name, cx)
                        && let Some((row, row_group)) = then_move
                    {
                        store.set_section(&row, &row_group, Some(&id), cx).ok();
                    }
                }
                SectionEditKind::Rename(id) => {
                    store.rename_section(&group, &id, &name, cx).ok();
                }
            });
        }
        window.focus(&self.focus_handle, cx);
        cx.notify();
    }

    fn cancel_section_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.hierarchy.section_edit = None;
        window.focus(&self.focus_handle, cx);
        cx.notify();
    }

    pub(crate) fn render_section_edit_bar(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let edit = self.hierarchy.section_edit.as_ref()?;
        let colors = cx.theme().colors();
        Some(
            h_flex()
                .w_full()
                .px_2()
                .py_1()
                .gap_2()
                .border_t_1()
                .border_color(colors.border)
                .child(
                    Label::new(match edit.kind {
                        SectionEditKind::New { .. } => "New section",
                        SectionEditKind::Rename(_) => "Rename section",
                    })
                    .size(LabelSize::Small)
                    .color(Color::Muted),
                )
                .child(div().flex_1().min_w_0().child(edit.editor.clone()))
                .capture_action(cx.listener(
                    |this, _: &editor::actions::Newline, window, cx| {
                        this.confirm_section_edit(window, cx);
                    },
                ))
                .on_action(cx.listener(|this, _: &menu::Cancel, window, cx| {
                    this.cancel_section_edit(window, cx);
                }))
                .into_any_element(),
        )
    }

    // ---- menus ----------------------------------------------------------------------------

    /// Titles of the rows visible in this window, for "Make Subthread Of".
    fn visible_row_titles(&self) -> Vec<(RowKey, SharedString)> {
        self.contents
            .entries
            .iter()
            .filter_map(|entry| {
                let key = entry_row_key(entry)?;
                let title = match entry {
                    ListEntry::Thread(thread) => thread.metadata.display_title(),
                    ListEntry::Terminal(terminal) => terminal.metadata.display_title(),
                    _ => return None,
                };
                Some((key, title))
            })
            .collect()
    }

    /// Append the hierarchy block to a row's context menu.
    pub(crate) fn append_row_menu(
        menu: ContextMenu,
        sidebar: &WeakEntity<Sidebar>,
        key: &RowKey,
        cx: &mut App,
    ) -> ContextMenu {
        let Some(store) = HierarchyStore::try_global(cx) else {
            return menu;
        };
        let snapshot = sidebar
            .read_with(cx, |sidebar, _| {
                let layout = sidebar.hierarchy.row_layout.get(key)?.clone();
                Some((layout, sidebar.visible_row_titles()))
            })
            .ok()
            .flatten();
        let Some((layout, visible)) = snapshot else {
            return menu;
        };
        let hierarchy = store.read(cx).hierarchy().clone();
        let has_parent = hierarchy.parents.contains_key(key);
        let current_section = hierarchy
            .section_of(key)
            .map(|(_, id)| id.clone());
        let sections: Vec<(SectionId, String)> = hierarchy
            .sections
            .get(&layout.group)
            .map(|sections| {
                sections
                    .iter()
                    .map(|s| (s.id.clone(), s.name.clone()))
                    .collect()
            })
            .unwrap_or_default();
        let candidates: Vec<(RowKey, SharedString)> = visible
            .into_iter()
            .filter(|(candidate, _)| !hierarchy.would_cycle(key, candidate))
            .collect();

        let mut menu = menu.separator();

        if !candidates.is_empty() {
            let key = key.clone();
            menu = menu.submenu("Make Subthread Of", move |mut submenu, _window, _cx| {
                for (candidate, title) in candidates.clone() {
                    let key = key.clone();
                    submenu = submenu.entry(title, None, move |_window, cx| {
                        store_update(cx, |store, cx| {
                            store.set_parent(&key, Some(&candidate), cx).ok();
                        });
                    });
                }
                submenu
            });
        }
        if has_parent {
            let key = key.clone();
            menu = menu.entry("Remove from Parent", None, move |_window, cx| {
                store_update(cx, |store, cx| {
                    store.set_parent(&key, None, cx).ok();
                });
            });
        } else {
            let remove_key = key.clone();
            let key = key.clone();
            let group = layout.group.clone();
            let sidebar = sidebar.clone();
            let sections = sections.clone();
            menu = menu.submenu("Move to Section", move |mut submenu, _window, _cx| {
                for (id, name) in sections.clone() {
                    let key = key.clone();
                    let group = group.clone();
                    submenu = submenu.entry(name, None, move |_window, cx| {
                        store_update(cx, |store, cx| {
                            store.set_section(&key, &group, Some(&id), cx).ok();
                        });
                    });
                }
                let key = key.clone();
                let group = group.clone();
                let sidebar = sidebar.clone();
                submenu.entry("New Section…", None, move |window, cx| {
                    sidebar
                        .update(cx, |sidebar, cx| {
                            sidebar.begin_section_edit(
                                group.clone(),
                                SectionEditKind::New {
                                    then_move: Some((key.clone(), group.clone())),
                                },
                                "",
                                window,
                                cx,
                            );
                        })
                        .ok();
                })
            });
            if current_section.is_some() {
                let key = remove_key;
                let group = layout.group.clone();
                menu = menu.entry("Remove from Section", None, move |_window, cx| {
                    store_update(cx, |store, cx| {
                        store.set_section(&key, &group, None, cx).ok();
                    });
                });
            }
        }
        if layout.has_children {
            let key = key.clone();
            let collapsed = layout.collapsed;
            menu = menu.entry(
                if collapsed {
                    "Expand Subthreads"
                } else {
                    "Collapse Subthreads"
                },
                None,
                move |_window, cx| {
                    store_update(cx, |store, cx| {
                        store.set_row_collapsed(&key, !collapsed, cx);
                    });
                },
            );
        }
        menu
    }

    /// Append "New Section…" to a project header's menu.
    pub(crate) fn append_project_menu(
        menu: ContextMenu,
        sidebar: &WeakEntity<Sidebar>,
        project: &ProjectGroupKey,
    ) -> ContextMenu {
        let sidebar = sidebar.clone();
        let group = group_key(project);
        menu.separator()
            .entry("New Section…", None, move |window, cx| {
                sidebar
                    .update(cx, |sidebar, cx| {
                        sidebar.begin_section_edit(
                            group.clone(),
                            SectionEditKind::New { then_move: None },
                            "",
                            window,
                            cx,
                        );
                    })
                    .ok();
            })
    }
}
