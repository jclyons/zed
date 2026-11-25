//! CPZed fork: drag and drop for the H7 hierarchy in the Threads Sidebar (S4).
//!
//! One payload ([`DraggedSidebarItem`]), one zone function ([`zone_at`]), one validator
//! (`cpzed_hierarchy::plan_drop`) and one applier ([`Sidebar::drop_item_on`]). The context menu
//! goes through the same `drop_item_on`, so a menu action and the equivalent drop are identical.
//!
//! A middle drop on a row NESTS, an edge drop REORDERS (and re-parents or re-sections into the
//! target's container), a drop on a section header moves the row into it, a drop on its own
//! project header un-nests / un-sections it. A refused drop changes nothing and shows why.
//! Moving a terminal to another project is S5: it is announced, not applied.

use cpzed_hierarchy::{
    Container, DragItem, DropAction, DropCtx, DropTarget, Refusal, RowKey, Zone, plan_drop,
};
use gpui::{Bounds, DragMoveEvent, Pixels};

use super::hierarchy_ui::{group_key, thread_key};
use super::*;

/// What a drag carries: the item (a row or a section) and its title for the ghost.
#[derive(Clone)]
pub(crate) struct DraggedSidebarItem {
    pub item: DragItem,
    pub title: SharedString,
}

impl Render for DraggedSidebarItem {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        h_flex()
            .px_2()
            .py_1()
            .gap_1()
            .rounded_sm()
            .border_1()
            .border_color(colors.border)
            .bg(colors.elevated_surface_background)
            .child(
                Icon::new(match self.item {
                    DragItem::Section { .. } => IconName::Folder,
                    DragItem::Row { .. } => IconName::Thread,
                })
                .size(IconSize::XSmall)
                .color(Color::Muted),
            )
            .child(Label::new(self.title.clone()).size(LabelSize::Small))
    }
}

/// Where the pointer currently is over a row, for the drop indicator.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DropPreview {
    /// Keyed on the target, not the render-time row index: `update_entries` can rebuild the
    /// list between a drag event and the next paint.
    pub target: DropTarget,
    pub zone: Zone,
    /// Set when dropping here would be refused: the indicator turns red.
    pub refusal: Option<SharedString>,
}

/// Top 25% = Before, bottom 25% = After, the middle 50% = Into (H7 section 4).
pub(crate) fn zone_at(y: Pixels, bounds: Bounds<Pixels>) -> Zone {
    let height = f32::from(bounds.size.height);
    if height <= 0. {
        return Zone::Into;
    }
    let fraction = (f32::from(y) - f32::from(bounds.origin.y)) / height;
    if fraction < 0.25 {
        Zone::Before
    } else if fraction > 0.75 {
        Zone::After
    } else {
        Zone::Into
    }
}

impl Sidebar {
    /// The drag payload for the entry at `ix`: rows and section headers drag, project headers do
    /// not (project order stays upstream's, H7 revision 2).
    fn dragged_item_at(&self, ix: usize) -> Option<DraggedSidebarItem> {
        match self.contents.entries.get(ix)? {
            ListEntry::Thread(thread) => {
                let key = thread_key(thread.metadata.thread_id);
                Some(DraggedSidebarItem {
                    title: thread.metadata.display_title(),
                    item: DragItem::Row {
                        group: self.row_group(&key)?,
                        key,
                    },
                })
            }
            ListEntry::Terminal(terminal) => {
                let key = hierarchy_ui::terminal_key(terminal.metadata.terminal_id);
                Some(DraggedSidebarItem {
                    title: terminal.metadata.display_title(),
                    item: DragItem::Row {
                        group: self.row_group(&key)?,
                        key,
                    },
                })
            }
            ListEntry::Section(section) => Some(DraggedSidebarItem {
                title: section.name.clone(),
                item: DragItem::Section {
                    id: section.id.clone(),
                    group: section.group.clone(),
                },
            }),
            ListEntry::ProjectHeader { .. } => None,
        }
    }

    fn drop_target_at(&self, ix: usize) -> Option<DropTarget> {
        match self.contents.entries.get(ix)? {
            ListEntry::Thread(thread) => {
                let key = thread_key(thread.metadata.thread_id);
                Some(DropTarget::Row {
                    group: self.row_group(&key)?,
                    key,
                })
            }
            ListEntry::Terminal(terminal) => {
                let key = hierarchy_ui::terminal_key(terminal.metadata.terminal_id);
                Some(DropTarget::Row {
                    group: self.row_group(&key)?,
                    key,
                })
            }
            ListEntry::Section(section) => Some(DropTarget::SectionHeader {
                id: section.id.clone(),
                group: section.group.clone(),
            }),
            ListEntry::ProjectHeader { key, .. } => Some(DropTarget::ProjectHeader {
                group: group_key(key),
            }),
        }
    }

    fn entry_depth(&self, key: &RowKey) -> usize {
        self.hierarchy
            .row_layout
            .get(key)
            .map_or(0, |layout| layout.depth)
    }

    /// The order of a container as the user currently sees it (collapsed rows are not in it; the
    /// store keeps them after the visible ones).
    pub(crate) fn visible_in(&self, container: &Container) -> Vec<RowKey> {
        let entries = &self.contents.entries;
        let start = match container {
            Container::Children(parent) => entries
                .iter()
                .position(|e| hierarchy_ui::entry_row_key(e).as_ref() == Some(parent)),
            Container::Section(group, id) => entries.iter().position(
                |e| matches!(e, ListEntry::Section(s) if &s.group == group && &s.id == id),
            ),
            Container::Unsectioned(group) => entries.iter().position(
                |e| matches!(e, ListEntry::ProjectHeader { key, .. } if &group_key(key) == group),
            ),
        };
        let Some(start) = start else {
            return Vec::new();
        };
        let parent_depth = match container {
            Container::Children(parent) => Some(self.entry_depth(parent)),
            _ => None,
        };
        let mut out = Vec::new();
        for entry in &entries[start + 1..] {
            let Some(key) = hierarchy_ui::entry_row_key(entry) else {
                break; // the next section or project header ends every container
            };
            let depth = self.entry_depth(&key);
            match parent_depth {
                Some(parent_depth) => {
                    if depth <= parent_depth {
                        break;
                    }
                    if depth == parent_depth + 1 {
                        out.push(key);
                    }
                }
                None => {
                    if depth == 0 {
                        out.push(key);
                    }
                }
            }
        }
        out
    }

    fn plan_drop_on(
        &self,
        item: &DragItem,
        target: &DropTarget,
        zone: Zone,
        cx: &App,
    ) -> Result<DropAction, Refusal> {
        let hierarchy = self.hierarchy.store.read(cx).hierarchy();
        let row_exists = |key: &RowKey| self.row_group(key).is_some();
        let visible = |container: &Container| self.visible_in(container);
        plan_drop(
            item,
            target,
            zone,
            hierarchy,
            &DropCtx {
                row_exists: &row_exists,
                visible: &visible,
            },
        )
    }

    /// Plan and apply one drop (or the equivalent menu action). A refusal changes nothing and is
    /// shown in the sidebar's notice bar. Returns whether the hierarchy changed.
    pub(crate) fn drop_item_on(
        &mut self,
        item: &DragItem,
        target: &DropTarget,
        zone: Zone,
        cx: &mut Context<Self>,
    ) -> bool {
        let action = match self.plan_drop_on(item, target, zone, cx) {
            Ok(action) => action,
            Err(Refusal::SameTarget) => return false,
            Err(refusal) => {
                self.note_refusal::<()>(Err(refusal), cx);
                return false;
            }
        };
        let row_group = match item {
            DragItem::Row { group, .. } | DragItem::Section { group, .. } => group.clone(),
        };
        // S5 owns the cross-project terminal move: say so instead of failing silently.
        if matches!(action, DropAction::MoveTerminalToGroup { .. }) {
            self.hierarchy.notice = Some(SharedString::from(
                "Moving a terminal to another project is not available yet",
            ));
            cx.notify();
            return false;
        }
        let result = self.hierarchy.store.update(cx, |store, cx| match action {
            DropAction::Reorder {
                container,
                key,
                target,
                side,
                visible,
            } => store.move_row(&key, &row_group, &target, side, &container, &visible, cx),
            DropAction::Nest { child, parent } => store.set_parent(&child, Some(&parent), cx),
            DropAction::Unnest { key, group } | DropAction::ToUnsectioned { key, group } => {
                store.set_section(&key, &group, None, cx)
            }
            DropAction::IntoSection {
                key,
                group,
                section,
            } => store.set_section(&key, &group, Some(&section), cx),
            DropAction::ReorderSection {
                group,
                id,
                target,
                side,
            } => store.move_section(&group, &id, &group, &target, side, cx),
            DropAction::MoveTerminalToGroup { .. } => Ok(()), // announced above, never applied
        });
        self.note_refusal(result, cx).is_some()
    }

    pub(crate) fn update_drop_preview(
        &mut self,
        target: DropTarget,
        item: &DragItem,
        zone: Zone,
        cx: &mut Context<Self>,
    ) {
        let zone = Self::effective_zone(item, &target, zone);
        let planned = if self.hierarchy.store.read(cx).is_read_only() {
            Err(Refusal::ReadOnly)
        } else {
            self.plan_drop_on(item, &target, zone, cx)
        };
        let preview = match planned {
            Ok(_) => Some(DropPreview {
                target,
                zone,
                refusal: None,
            }),
            Err(Refusal::SameTarget) => None,
            Err(refusal) => Some(DropPreview {
                target,
                zone,
                refusal: Some(SharedString::from(format!("{refusal}"))),
            }),
        };
        if self.hierarchy.drop_preview != preview {
            self.hierarchy.drop_preview = preview;
            cx.notify();
        }
    }

    /// A row dropped on a section or project header always goes into / out to that header, so
    /// the indicator always shows the whole header (never an edge line that looks like the end
    /// of the container above).
    fn effective_zone(item: &DragItem, target: &DropTarget, zone: Zone) -> Zone {
        match (item, target) {
            (
                DragItem::Row { .. },
                DropTarget::SectionHeader { .. } | DropTarget::ProjectHeader { .. },
            ) => Zone::Into,
            _ => zone,
        }
    }

    pub(crate) fn finish_drop(
        &mut self,
        target: DropTarget,
        dragged: &DraggedSidebarItem,
        cx: &mut Context<Self>,
    ) {
        let zone = match self.hierarchy.drop_preview.take() {
            Some(preview) if preview.target == target => preview.zone,
            _ => Zone::Into,
        };
        let zone = Self::effective_zone(&dragged.item, &target, zone);
        self.drop_item_on(&dragged.item, &target, zone, cx);
        cx.notify();
    }

    /// Make the rendered entry at `ix` draggable and a drop target. The one hook `sidebar.rs`
    /// calls from `render_list_entry`.
    pub(crate) fn wrap_drag_drop(
        &self,
        ix: usize,
        element: AnyElement,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if self.has_filter_query(cx) {
            return element;
        }
        let colors = cx.theme().colors();
        let error = cx.theme().status().error;
        // The drop target is captured by KEY now, at render time: the handlers below must not
        // re-resolve `ix` against entries that `update_entries` may have rebuilt since.
        let target = self.drop_target_at(ix);
        let Some(target) = target else {
            return element;
        };
        let preview = if cx.has_active_drag() {
            self.hierarchy
                .drop_preview
                .clone()
                .filter(|preview| preview.target == target)
        } else {
            None
        };
        let dragged = self.dragged_item_at(ix);
        let sidebar = cx.entity().downgrade();

        let (move_target, drop_target) = (target.clone(), target);
        let wrapper = div()
            .id(("hierarchy-dnd", ix))
            .relative()
            .w_full()
            .child(element)
            .when_some(preview, |this, preview| {
                let color = if preview.refusal.is_some() {
                    error
                } else {
                    colors.border_focused
                };
                match preview.zone {
                    Zone::Before => this.child(
                        div()
                            .absolute()
                            .top_0()
                            .left_0()
                            .right_0()
                            .h(px(2.))
                            .bg(color),
                    ),
                    Zone::After => this.child(
                        div()
                            .absolute()
                            .bottom_0()
                            .left_0()
                            .right_0()
                            .h(px(2.))
                            .bg(color),
                    ),
                    Zone::Into => this.child(div().absolute().inset_0().bg(color.opacity(0.2))),
                }
            })
            .when_some(dragged, |this, dragged| {
                this.on_drag(dragged, move |dragged, _offset, _window, cx| {
                    // A new drag starts with no indicator (a previous drag may have ended off-row).
                    let sidebar = sidebar.clone();
                    cx.defer(move |cx| {
                        sidebar
                            .update(cx, |this, cx| {
                                if this.hierarchy.drop_preview.take().is_some() {
                                    cx.notify();
                                }
                            })
                            .ok();
                    });
                    cx.new(|_| dragged.clone())
                })
            })
            .on_drag_move(cx.listener(
                move |this, event: &DragMoveEvent<DraggedSidebarItem>, _window, cx| {
                    if !event.bounds.contains(&event.event.position) {
                        if this
                            .hierarchy
                            .drop_preview
                            .as_ref()
                            .is_some_and(|preview| preview.target == move_target)
                        {
                            this.hierarchy.drop_preview = None;
                            cx.notify();
                        }
                        return;
                    }
                    let zone = zone_at(event.event.position.y, event.bounds);
                    let dragged = event.drag(cx).clone();
                    this.update_drop_preview(move_target.clone(), &dragged.item, zone, cx);
                },
            ))
            .on_drop(
                cx.listener(move |this, dragged: &DraggedSidebarItem, _window, cx| {
                    this.finish_drop(drop_target.clone(), dragged, cx);
                }),
            );

        #[cfg(test)]
        let wrapper = wrapper.debug_selector(|| format!("hierarchy-dnd-{ix}"));

        wrapper.into_any_element()
    }
}
