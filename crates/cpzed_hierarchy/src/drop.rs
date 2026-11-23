//! `plan_drop`: the one pure validator behind both drag-and-drop and the context menu
//! ("H7 revision 2: plan_drop"). It decides; the sidebar applies the returned [`DropAction`].

use crate::model::{Container, GroupKey, Hierarchy, Refusal, RowKey, SectionId, Side};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Zone {
    Before,
    Into,
    After,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DragItem {
    /// A thread row. `RowKey::Terminal` is a terminal thread, `RowKey::Thread` an agent thread.
    Row {
        key: RowKey,
        group: GroupKey,
    },
    Section {
        id: SectionId,
        group: GroupKey,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DropTarget {
    Row { key: RowKey, group: GroupKey },
    SectionHeader { id: SectionId, group: GroupKey },
    ProjectHeader { group: GroupKey },
}

pub struct DropCtx<'a> {
    /// Is this row currently present (open thread, live terminal)? A drop onto a row that is not
    /// is refused with `TargetNotFound`, so no link to a phantom parent is ever stored.
    pub row_exists: &'a dyn Fn(&RowKey) -> bool,
    /// The visible order of a container, as the user currently sees it. Embedded in
    /// [`DropAction::Reorder`] so the applier can materialize it (H6).
    pub visible: &'a dyn Fn(&Container) -> Vec<RowKey>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DropAction {
    /// Move `key` next to `target` in `container` (same parent, same section, or the unsectioned
    /// area; may re-parent or re-section, as the container dictates).
    Reorder {
        container: Container,
        key: RowKey,
        target: RowKey,
        side: Side,
        visible: Vec<RowKey>,
    },
    /// Make `child` the last child of `parent`.
    Nest { child: RowKey, parent: RowKey },
    /// A child dropped on its own project header: a root at the top of the unsectioned area.
    Unnest { key: RowKey, group: GroupKey },
    /// A root dropped on a section header of its own project: top of that section, with subtree.
    IntoSection {
        key: RowKey,
        group: GroupKey,
        section: SectionId,
    },
    /// A root dropped on its own project header: top of the unsectioned area.
    ToUnsectioned { key: RowKey, group: GroupKey },
    /// Terminal only (H6). `section` is applied after the move (it is then in the row's project).
    MoveTerminalToGroup {
        key: RowKey,
        from: GroupKey,
        to: GroupKey,
        section: Option<SectionId>,
    },
    ReorderSection {
        group: GroupKey,
        id: SectionId,
        target: SectionId,
        side: Side,
    },
}

/// The container a row currently lives in, judged from the hierarchy.
fn container_of(h: &Hierarchy, key: &RowKey, group: &GroupKey) -> Container {
    if let Some(parent) = h.parents.get(key) {
        return Container::Children(parent.clone());
    }
    if let Some(sections) = h.sections.get(group) {
        if let Some(section) = sections.iter().find(|s| s.items.contains(key)) {
            return Container::Section(group.clone(), section.id.clone());
        }
    }
    Container::Unsectioned(group.clone())
}

fn cross_project(
    key: &RowKey,
    from: &GroupKey,
    to: &GroupKey,
    section: Option<SectionId>,
) -> Result<DropAction, Refusal> {
    match key {
        RowKey::Thread(_) => Err(Refusal::AgentThreadAcrossProjects),
        RowKey::Terminal(_) => Ok(DropAction::MoveTerminalToGroup {
            key: key.clone(),
            from: from.clone(),
            to: to.clone(),
            section,
        }),
    }
}

pub fn plan_drop(
    item: &DragItem,
    target: &DropTarget,
    zone: Zone,
    h: &Hierarchy,
    ctx: &DropCtx,
) -> Result<DropAction, Refusal> {
    match item {
        DragItem::Section { id, group } => plan_section_drop(id, group, target, zone, h),
        DragItem::Row { key, group } => plan_row_drop(key, group, target, zone, h, ctx),
    }
}

fn plan_section_drop(
    id: &SectionId,
    group: &GroupKey,
    target: &DropTarget,
    zone: Zone,
    h: &Hierarchy,
) -> Result<DropAction, Refusal> {
    if h.section(group, id).is_none() {
        return Err(Refusal::UnknownSection);
    }
    match target {
        DropTarget::SectionHeader {
            id: target_id,
            group: target_group,
        } => {
            if target_group != group {
                return Err(Refusal::SectionAcrossProjects);
            }
            if target_id == id {
                return Err(Refusal::SectionIntoItself);
            }
            if h.section(group, target_id).is_none() {
                return Err(Refusal::UnknownSection);
            }
            Ok(DropAction::ReorderSection {
                group: group.clone(),
                id: id.clone(),
                target: target_id.clone(),
                // A section cannot nest, so "into" means "just after".
                side: if zone == Zone::Before {
                    Side::Before
                } else {
                    Side::After
                },
            })
        }
        DropTarget::Row { group: g, .. } | DropTarget::ProjectHeader { group: g } => {
            if g != group {
                Err(Refusal::SectionAcrossProjects)
            } else {
                Err(Refusal::InvalidTarget)
            }
        }
    }
}

fn plan_row_drop(
    key: &RowKey,
    group: &GroupKey,
    target: &DropTarget,
    zone: Zone,
    h: &Hierarchy,
    ctx: &DropCtx,
) -> Result<DropAction, Refusal> {
    match target {
        DropTarget::Row {
            key: target_key,
            group: target_group,
        } => {
            if target_key == key {
                return Err(Refusal::SameTarget);
            }
            if !(ctx.row_exists)(target_key) {
                return Err(Refusal::TargetNotFound);
            }
            match zone {
                // Nesting across projects is allowed (D1): the child renders under its parent.
                Zone::Into => {
                    if h.would_cycle(key, target_key) {
                        return Err(Refusal::Cycle);
                    }
                    Ok(DropAction::Nest {
                        child: key.clone(),
                        parent: target_key.clone(),
                    })
                }
                // Before/After a row of another project counts as a drop onto that project.
                Zone::Before | Zone::After if target_group != group => {
                    cross_project(key, group, target_group, None)
                }
                Zone::Before | Zone::After => {
                    let container = container_of(h, target_key, group);
                    if let Container::Children(parent) = &container
                        && h.would_cycle(key, parent)
                    {
                        return Err(Refusal::Cycle);
                    }
                    let visible = (ctx.visible)(&container);
                    if !visible.contains(target_key) {
                        return Err(Refusal::TargetNotFound);
                    }
                    Ok(DropAction::Reorder {
                        container,
                        key: key.clone(),
                        target: target_key.clone(),
                        side: if zone == Zone::Before {
                            Side::Before
                        } else {
                            Side::After
                        },
                        visible,
                    })
                }
            }
        }
        DropTarget::SectionHeader {
            id,
            group: target_group,
        } => {
            if h.section(target_group, id).is_none() {
                return Err(Refusal::UnknownSection);
            }
            if target_group != group {
                return cross_project(key, group, target_group, Some(id.clone()));
            }
            Ok(DropAction::IntoSection {
                key: key.clone(),
                group: group.clone(),
                section: id.clone(),
            })
        }
        DropTarget::ProjectHeader {
            group: target_group,
        } => {
            if target_group != group {
                return cross_project(key, group, target_group, None);
            }
            if h.parents.contains_key(key) {
                Ok(DropAction::Unnest {
                    key: key.clone(),
                    group: group.clone(),
                })
            } else {
                Ok(DropAction::ToUnsectioned {
                    key: key.clone(),
                    group: group.clone(),
                })
            }
        }
    }
}
