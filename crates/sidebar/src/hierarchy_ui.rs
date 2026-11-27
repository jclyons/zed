//! CPZed fork: rendering of the H7 hierarchy (sections inside project groups, nested subthread
//! rows at any depth, rollups, keys and menus) in the Threads Sidebar.
//!
//! `sidebar.rs` only carries thin hooks; the arranging itself is `cpzed_hierarchy::arrange_rows`.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use cpzed_hierarchy::{
    ArrangeInput, DragItem, DropTarget, GroupKey, Hierarchy, HierarchyStore, Refusal, Rollup,
    RowInfo, RowKey, SectionId, Slot, Zone, arrange_rows,
};
use editor::Editor;
use gpui::{Entity, Subscription, WeakEntity};
use ui::{ContextMenu, prelude::*, right_click_menu};

use super::*;

/// Indent step in px per depth level, and the depth at which the indent stops growing.
const INDENT_STEP: f32 = 12.;
pub(crate) const MAX_INDENT_DEPTH: usize = 6;
/// Shown when the stored hierarchy was written by a newer CPZed: edits are not saved.
const READ_ONLY_NOTICE: &str =
    "Sections and nesting were saved by a newer CPZed: edits are disabled and not saved.";

/// What `render_*` needs to know about a nested row. Filled in `rebuild_contents`.
#[derive(Clone, Debug)]
pub(crate) struct RowLayout {
    pub depth: usize,
    pub in_section: bool,
    pub has_children: bool,
    /// Direct children only (child-count chip).
    pub child_count: usize,
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
    New {
        then_move: Option<(RowKey, GroupKey)>,
    },
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
    /// The last refused edit, shown in the bar until the next edit succeeds.
    pub notice: Option<SharedString>,
    /// Every row the last rebuild saw (including collapsed subtrees), for the 30-day prune.
    pub seen_rows: Vec<RowKey>,
    /// The drop indicator while a drag is over a row (S4).
    pub drop_preview: Option<super::hierarchy_dnd::DropPreview>,
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

pub(crate) fn entry_row_key(entry: &ListEntry) -> Option<RowKey> {
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

/// Every row the hierarchy has any data about.
fn mentioned_rows(hierarchy: &Hierarchy) -> HashSet<RowKey> {
    let mut mentioned: HashSet<RowKey> = HashSet::new();
    for (child, parent) in &hierarchy.parents {
        mentioned.insert(child.clone());
        mentioned.insert(parent.clone());
    }
    for (parent, children) in &hierarchy.child_order {
        mentioned.insert(parent.clone());
        mentioned.extend(children.iter().cloned());
    }
    for rows in hierarchy.unsectioned_order.values() {
        mentioned.extend(rows.iter().cloned());
    }
    mentioned.extend(hierarchy.collapsed_rows.iter().cloned());
    for sections in hierarchy.sections.values() {
        for section in sections {
            mentioned.extend(section.items.iter().cloned());
        }
    }
    mentioned
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
                    child_count,
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
                            child_count,
                            collapsed,
                            rollup,
                            foreign_project: foreign_project.then(|| project_name(&own_group)),
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

/// Run a store edit and surface a refusal in the sidebar (never swallow it).
pub(crate) fn edit_store<T>(
    sidebar: &WeakEntity<Sidebar>,
    cx: &mut App,
    edit: impl FnOnce(&mut HierarchyStore, &mut Context<HierarchyStore>) -> Result<T, Refusal>,
) -> Option<T> {
    let store = HierarchyStore::try_global(cx)?;
    let result = store.update(cx, edit);
    sidebar
        .update(cx, |sidebar, cx| sidebar.note_refusal(result, cx))
        .ok()
        .flatten()
}

/// Make `child` a subthread of `parent` (or un-nest it with `None`). This is the drop path: the
/// menu entry and the drag-and-drop equivalent (middle drop / drop on the project header) run the
/// same `plan_drop` and applier.
pub(crate) fn set_parent_action(
    sidebar: &WeakEntity<Sidebar>,
    cx: &mut App,
    child: &RowKey,
    parent: Option<&RowKey>,
) {
    sidebar
        .update(cx, |sidebar, cx| {
            let Some(group) = sidebar.row_group(child) else {
                return;
            };
            let target = match parent {
                Some(parent) => {
                    let Some(parent_group) = sidebar.row_group(parent) else {
                        return;
                    };
                    DropTarget::Row {
                        key: parent.clone(),
                        group: parent_group,
                    }
                }
                None => DropTarget::ProjectHeader {
                    group: group.clone(),
                },
            };
            let item = DragItem::Row {
                key: child.clone(),
                group,
            };
            sidebar.drop_item_on(&item, &target, Zone::Into, cx);
        })
        .ok();
}

/// Put a row in a section of its own project, or in the unsectioned area with `None` (the same
/// path as dropping it on the section header or its own project header).
pub(crate) fn set_section_action(
    sidebar: &WeakEntity<Sidebar>,
    cx: &mut App,
    row: &RowKey,
    group: &GroupKey,
    section: Option<&SectionId>,
) {
    sidebar
        .update(cx, |sidebar, cx| {
            let target = match section {
                Some(id) => DropTarget::SectionHeader {
                    id: id.clone(),
                    group: group.clone(),
                },
                None => DropTarget::ProjectHeader {
                    group: group.clone(),
                },
            };
            let item = DragItem::Row {
                key: row.clone(),
                group: group.clone(),
            };
            sidebar.drop_item_on(&item, &target, Zone::Into, cx);
        })
        .ok();
}

/// What the row context menu offers. Built from the live sidebar, so it also works on the
/// starting state (no hierarchy, no `row_layout`).
#[derive(Clone, Debug)]
pub(crate) enum RowMenuItem {
    MakeSubthreadOf(Vec<(RowKey, SharedString)>),
    RemoveFromParent,
    MoveToSection {
        group: GroupKey,
        sections: Vec<(SectionId, String)>,
    },
    RemoveFromSection {
        group: GroupKey,
    },
    ToggleSubtree {
        collapsed: bool,
    },
    ReadOnly,
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
            notice: None,
            seen_rows: Vec::new(),
            drop_preview: None,
            _store_subscription: subscription,
        }
    }

    /// Quietly record the rows and groups the hierarchy actually mentions (a parent link, a
    /// section membership, an order or collapse entry) that the last rebuild saw, hidden ones
    /// included, so a long-collapsed subtree is not pruned. Rows with no hierarchy data are never
    /// recorded: a user with an empty hierarchy causes no store write at all. Rows of collapsed
    /// PROJECT groups are only known for terminals: do not wire `prune` before that is solved.
    pub(crate) fn touch_hierarchy_seen(&mut self, cx: &mut Context<Self>) {
        let (rows, groups) = {
            let hierarchy = self.hierarchy.store.read(cx).hierarchy();
            if !has_structure(hierarchy) {
                return;
            }
            let mentioned = mentioned_rows(hierarchy);
            let mut rows: Vec<RowKey> = self.hierarchy.seen_rows.clone();
            let mut groups: Vec<GroupKey> = Vec::new();
            for entry in &self.contents.entries {
                match entry {
                    ListEntry::ProjectHeader { key, .. } => groups.push(group_key(key)),
                    other => rows.extend(entry_row_key(other)),
                }
            }
            rows.retain(|row| mentioned.contains(row));
            groups.retain(|group| {
                hierarchy.sections.get(group).is_some_and(|s| !s.is_empty())
                    || hierarchy.unsectioned_order.contains_key(group)
            });
            (rows, groups)
        };
        if rows.is_empty() && groups.is_empty() {
            return;
        }
        let now = chrono::Utc::now().timestamp();
        self.hierarchy.store.update(cx, |store, cx| {
            store.touch_seen(&rows, &groups, now, cx);
        });
    }

    /// Show a refusal (or clear the notice after a success). Returns the success value.
    pub(crate) fn note_refusal<T>(
        &mut self,
        result: Result<T, Refusal>,
        cx: &mut Context<Self>,
    ) -> Option<T> {
        match result {
            Ok(value) => {
                if self.hierarchy.notice.take().is_some() {
                    cx.notify();
                }
                Some(value)
            }
            Err(refusal) => {
                self.hierarchy.notice = Some(SharedString::from(format!("{refusal}")));
                cx.notify();
                None
            }
        }
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
        let result = self
            .hierarchy
            .store
            .update(cx, |store, cx| store.set_row_collapsed(key, collapsed, cx));
        self.note_refusal(result, cx);
    }

    fn set_section_collapsed(
        &mut self,
        group: &GroupKey,
        id: &SectionId,
        collapsed: bool,
        cx: &mut Context<Self>,
    ) {
        let result = self.hierarchy.store.update(cx, |store, cx| {
            store.set_section_collapsed(group, id, collapsed, cx)
        });
        self.note_refusal(result, cx);
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
                        let sidebar = sidebar.clone();
                        move |_window, cx| {
                            edit_store(&sidebar, cx, |store, cx| {
                                store.delete_section(&group, &id, cx)
                            });
                        }
                    })
                    .entry(
                        if collapsed { "Expand" } else { "Collapse" },
                        None,
                        {
                            let group = group.clone();
                            let id = id.clone();
                            let sidebar = sidebar.clone();
                            move |_window, cx| {
                                edit_store(&sidebar, cx, |store, cx| {
                                    store.set_section_collapsed(&group, &id, !collapsed, cx)
                                });
                            }
                        },
                    )
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

    pub(crate) fn confirm_section_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(edit) = self.hierarchy.section_edit.as_ref() else {
            return;
        };
        let name = edit.editor.read(cx).text(cx).trim().to_string();
        let group = edit.group.clone();
        let result = match &edit.kind {
            SectionEditKind::New { then_move } => {
                let then_move = then_move.clone();
                self.hierarchy.store.update(cx, |store, cx| {
                    let (id, _) = store.create_section(&group, &name, cx)?;
                    if let Some((row, row_group)) = then_move {
                        store.set_section(&row, &row_group, Some(&id), cx)?;
                    }
                    Ok(())
                })
            }
            SectionEditKind::Rename(id) => {
                let id = id.clone();
                self.hierarchy
                    .store
                    .update(cx, |store, cx| store.rename_section(&group, &id, &name, cx))
            }
        };
        // A refused edit keeps the bar open with the reason; a successful one closes it.
        if self.note_refusal(result, cx).is_some() {
            self.hierarchy.section_edit = None;
            window.focus(&self.focus_handle, cx);
        }
        cx.notify();
    }

    fn cancel_section_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.hierarchy.section_edit = None;
        window.focus(&self.focus_handle, cx);
        cx.notify();
    }

    pub(crate) fn render_section_edit_bar(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let colors = cx.theme().colors();
        if self.hierarchy.store.read(cx).is_read_only() {
            return Some(
                h_flex()
                    .w_full()
                    .px_2()
                    .py_1()
                    .child(
                        Label::new(READ_ONLY_NOTICE)
                            .size(LabelSize::XSmall)
                            .color(Color::Warning),
                    )
                    .into_any_element(),
            );
        }
        let notice = self.hierarchy.notice.clone();
        let edit = self.hierarchy.section_edit.as_ref();
        if notice.is_none() && edit.is_none() {
            return None;
        }
        Some(
            v_flex()
                .w_full()
                .border_t_1()
                .border_color(colors.border)
                .when_some(notice, |this, notice| {
                    this.child(
                        div()
                            .id("hierarchy-notice")
                            .px_2()
                            .py_1()
                            .cursor_pointer()
                            .on_click(cx.listener(|this, _, _window, cx| {
                                this.hierarchy.notice = None;
                                cx.notify();
                            }))
                            .child(
                                Label::new(notice)
                                    .size(LabelSize::XSmall)
                                    .color(Color::Warning),
                            ),
                    )
                })
                .when_some(edit, |this, edit| {
                    this.child(
                        h_flex()
                            .w_full()
                            .px_2()
                            .py_1()
                            .gap_2()
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
                            })),
                    )
                })
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

    /// The group a row belongs to: its layout when arranged, else the nearest project header
    /// above it (the starting state has no layout).
    pub(crate) fn row_group(&self, key: &RowKey) -> Option<GroupKey> {
        if let Some(layout) = self.hierarchy.row_layout.get(key) {
            return Some(layout.group.clone());
        }
        let ix = self
            .contents
            .entries
            .iter()
            .position(|entry| entry_row_key(entry).as_ref() == Some(key))?;
        self.contents.entries[..ix]
            .iter()
            .rev()
            .find_map(|entry| match entry {
                ListEntry::ProjectHeader { key, .. } => Some(group_key(key)),
                _ => None,
            })
    }

    /// What the row context menu offers for `key`.
    pub(crate) fn row_menu_items(&self, key: &RowKey, cx: &App) -> Vec<RowMenuItem> {
        if self.hierarchy.store.read(cx).is_read_only() {
            return vec![RowMenuItem::ReadOnly];
        }
        let Some(group) = self.row_group(key) else {
            return Vec::new();
        };
        let hierarchy = self.hierarchy.store.read(cx).hierarchy();
        let layout = self.hierarchy.row_layout.get(key);
        let mut items = Vec::new();

        let candidates: Vec<(RowKey, SharedString)> = self
            .visible_row_titles()
            .into_iter()
            .filter(|(candidate, _)| !hierarchy.would_cycle(key, candidate))
            .collect();
        if !candidates.is_empty() {
            items.push(RowMenuItem::MakeSubthreadOf(candidates));
        }
        if hierarchy.parents.contains_key(key) {
            items.push(RowMenuItem::RemoveFromParent);
        } else {
            items.push(RowMenuItem::MoveToSection {
                sections: hierarchy
                    .sections
                    .get(&group)
                    .map(|sections| {
                        sections
                            .iter()
                            .map(|s| (s.id.clone(), s.name.clone()))
                            .collect()
                    })
                    .unwrap_or_default(),
                group: group.clone(),
            });
            if hierarchy.section_of(key).is_some() {
                items.push(RowMenuItem::RemoveFromSection { group });
            }
        }
        if let Some(layout) = layout.filter(|layout| layout.has_children) {
            items.push(RowMenuItem::ToggleSubtree {
                collapsed: layout.collapsed,
            });
        }
        items
    }

    /// Open the "New Section…" editor, optionally moving `then_move` into it afterwards.
    pub(crate) fn new_section_in(
        &mut self,
        group: GroupKey,
        then_move: Option<(RowKey, GroupKey)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.begin_section_edit(group, SectionEditKind::New { then_move }, "", window, cx);
    }

    /// Append the hierarchy block to a row's context menu.
    pub(crate) fn append_row_menu(
        menu: ContextMenu,
        sidebar: &WeakEntity<Sidebar>,
        key: &RowKey,
        cx: &mut App,
    ) -> ContextMenu {
        let items = sidebar
            .read_with(cx, |sidebar, cx| sidebar.row_menu_items(key, cx))
            .unwrap_or_default();
        if items.is_empty() {
            return menu;
        }
        let mut menu = menu.separator();
        for item in items {
            let key = key.clone();
            let sidebar = sidebar.clone();
            menu = match item {
                RowMenuItem::ReadOnly => menu.label(READ_ONLY_NOTICE),
                RowMenuItem::MakeSubthreadOf(candidates) => {
                    menu.submenu("Make Subthread Of", move |mut submenu, _window, _cx| {
                        for (candidate, title) in candidates.clone() {
                            let key = key.clone();
                            let sidebar = sidebar.clone();
                            submenu = submenu.entry(title, None, move |_window, cx| {
                                set_parent_action(&sidebar, cx, &key, Some(&candidate));
                            });
                        }
                        submenu
                    })
                }
                RowMenuItem::RemoveFromParent => {
                    menu.entry("Remove from Parent", None, move |_window, cx| {
                        set_parent_action(&sidebar, cx, &key, None);
                    })
                }
                RowMenuItem::MoveToSection { group, sections } => {
                    menu.submenu("Move to Section", move |mut submenu, _window, _cx| {
                        for (id, name) in sections.clone() {
                            let key = key.clone();
                            let group = group.clone();
                            let sidebar = sidebar.clone();
                            submenu = submenu.entry(name, None, move |_window, cx| {
                                set_section_action(&sidebar, cx, &key, &group, Some(&id));
                            });
                        }
                        let key = key.clone();
                        let group = group.clone();
                        let sidebar = sidebar.clone();
                        submenu.entry("New Section…", None, move |window, cx| {
                            sidebar
                                .update(cx, |sidebar, cx| {
                                    sidebar.new_section_in(
                                        group.clone(),
                                        Some((key.clone(), group.clone())),
                                        window,
                                        cx,
                                    );
                                })
                                .ok();
                        })
                    })
                }
                RowMenuItem::RemoveFromSection { group } => {
                    menu.entry("Remove from Section", None, move |_window, cx| {
                        set_section_action(&sidebar, cx, &key, &group, None);
                    })
                }
                RowMenuItem::ToggleSubtree { collapsed } => menu.entry(
                    if collapsed {
                        "Expand Subthreads"
                    } else {
                        "Collapse Subthreads"
                    },
                    None,
                    move |_window, cx| {
                        edit_store(&sidebar, cx, |store, cx| {
                            store.set_row_collapsed(&key, !collapsed, cx)
                        });
                    },
                ),
            };
        }
        menu
    }

    /// Append "New Section…" to a project header's menu.
    pub(crate) fn append_project_menu(
        menu: ContextMenu,
        sidebar: &WeakEntity<Sidebar>,
        project: &ProjectGroupKey,
        cx: &App,
    ) -> ContextMenu {
        let sidebar = sidebar.clone();
        let group = group_key(project);
        if HierarchyStore::try_global(cx).is_some_and(|store| store.read(cx).is_read_only()) {
            return menu.separator().label(READ_ONLY_NOTICE);
        }
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
