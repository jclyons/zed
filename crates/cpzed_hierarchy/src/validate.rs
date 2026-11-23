//! Invariant repair. Runs on load and on demand (after re-homing); repairs and reports, never
//! panics.

use std::collections::BTreeSet;

use crate::model::{GroupKey, Hierarchy, RowKey, SectionId};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Repair {
    /// A cycle in `parents` was broken by dropping this node's link.
    CycleBroken { node: RowKey },
    /// The row was listed again (in another section, or twice); the first listing was kept.
    DuplicateMembership { row: RowKey },
    /// A parented row was listed in a section or in an unsectioned order; removed.
    ParentedInContainer { row: RowKey },
    /// The row's section is in a project other than the row's current one; membership dropped.
    SectionInWrongGroup { row: RowKey, section: SectionId },
    /// Two sections shared an id; the later one got a fresh id.
    DuplicateSectionId { old: SectionId, new: SectionId },
    /// Two sections of a project shared a name (case-insensitive); the later one was renamed.
    DuplicateSectionName { section: SectionId, renamed: String },
}

impl Hierarchy {
    /// `group_of` returns a row's current project, or `None` when unknown (membership is then
    /// kept: nothing is dropped on missing information).
    pub fn validate(&mut self, group_of: &dyn Fn(&RowKey) -> Option<GroupKey>) -> Vec<Repair> {
        let mut repairs = Vec::new();
        self.break_cycles(&mut repairs);
        self.repair_sections(&mut repairs);
        self.repair_memberships(group_of, &mut repairs);
        repairs
    }

    fn repair_sections(&mut self, repairs: &mut Vec<Repair>) {
        // Duplicate ids (across all projects): the first keeps its id.
        let mut seen_ids: BTreeSet<SectionId> = BTreeSet::new();
        let mut duplicates: Vec<(GroupKey, usize)> = Vec::new();
        for (group, sections) in &self.sections {
            for (index, section) in sections.iter().enumerate() {
                if !seen_ids.insert(section.id.clone()) {
                    duplicates.push((group.clone(), index));
                }
            }
        }
        for (group, index) in duplicates {
            let new = self.next_section_id();
            if let Some(section) = self.sections.get_mut(&group).and_then(|s| s.get_mut(index)) {
                repairs.push(Repair::DuplicateSectionId {
                    old: std::mem::replace(&mut section.id, new.clone()),
                    new,
                });
            }
        }
        // Duplicate names within a project.
        for sections in self.sections.values_mut() {
            let mut names: BTreeSet<String> = BTreeSet::new();
            for section in sections.iter_mut() {
                if names.insert(section.name.trim().to_lowercase()) {
                    continue;
                }
                let base = section.name.trim().to_string();
                let mut n = 2;
                let renamed = loop {
                    let candidate = format!("{base} ({n})");
                    if !names.contains(&candidate.to_lowercase()) {
                        break candidate;
                    }
                    n += 1;
                };
                names.insert(renamed.to_lowercase());
                section.name = renamed.clone();
                repairs.push(Repair::DuplicateSectionName {
                    section: section.id.clone(),
                    renamed,
                });
            }
        }
    }

    fn break_cycles(&mut self, repairs: &mut Vec<Repair>) {
        let starts: Vec<RowKey> = self.parents.keys().cloned().collect();
        let mut done: BTreeSet<RowKey> = BTreeSet::new();
        for start in starts {
            if done.contains(&start) {
                continue;
            }
            let mut path: Vec<RowKey> = Vec::new();
            let mut on_path: BTreeSet<RowKey> = BTreeSet::new();
            let mut cursor = start;
            loop {
                if done.contains(&cursor) {
                    break;
                }
                if !on_path.insert(cursor.clone()) {
                    // `cursor` is the first node seen twice: drop its link.
                    self.parents.remove(&cursor);
                    for order in self.child_order.values_mut() {
                        order.retain(|r| r != &cursor);
                    }
                    self.child_order.retain(|_, v| !v.is_empty());
                    repairs.push(Repair::CycleBroken { node: cursor });
                    break;
                }
                path.push(cursor.clone());
                match self.parents.get(&cursor) {
                    Some(parent) => cursor = parent.clone(),
                    None => break,
                }
            }
            done.extend(path);
        }
    }

    fn repair_memberships(
        &mut self,
        group_of: &dyn Fn(&RowKey) -> Option<GroupKey>,
        repairs: &mut Vec<Repair>,
    ) {
        let mut seen_in_section: BTreeSet<RowKey> = BTreeSet::new();
        let parents = &self.parents;
        for (group, sections) in self.sections.iter_mut() {
            for section in sections.iter_mut() {
                let id = section.id.clone();
                section.items.retain(|row| {
                    if parents.contains_key(row) {
                        repairs.push(Repair::ParentedInContainer { row: row.clone() });
                        return false;
                    }
                    if let Some(current) = group_of(row)
                        && &current != group
                    {
                        repairs.push(Repair::SectionInWrongGroup {
                            row: row.clone(),
                            section: id.clone(),
                        });
                        return false;
                    }
                    if !seen_in_section.insert(row.clone()) {
                        repairs.push(Repair::DuplicateMembership { row: row.clone() });
                        return false;
                    }
                    true
                });
            }
        }
        // A row in a section is not also in an unsectioned order, and orders hold no duplicates
        // or parented rows.
        let mut seen_unsectioned: BTreeSet<RowKey> = BTreeSet::new();
        for order in self.unsectioned_order.values_mut() {
            order.retain(|row| {
                if parents.contains_key(row) {
                    repairs.push(Repair::ParentedInContainer { row: row.clone() });
                    return false;
                }
                if seen_in_section.contains(row) || !seen_unsectioned.insert(row.clone()) {
                    repairs.push(Repair::DuplicateMembership { row: row.clone() });
                    return false;
                }
                true
            });
        }
        self.unsectioned_order.retain(|_, v| !v.is_empty());
    }
}
