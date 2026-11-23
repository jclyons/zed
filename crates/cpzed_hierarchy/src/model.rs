//! Data model and the pure edit operations.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// On-disk schema version. A blob with any other version loads as empty.
pub const VERSION: u32 = 2;
/// Rows and groups unseen for this long are pruned (decision H6-5).
pub const RETENTION_SECS: i64 = 30 * 24 * 60 * 60;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum RowKey {
    /// `TerminalId::to_key_string()`.
    Terminal(String),
    /// `ThreadId` string.
    Thread(String),
}

/// A project group. Same meaning as `ProjectGroupKey` (S3 converts). `paths` must be sorted.
#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct GroupKey {
    pub host: Option<String>,
    pub paths: Vec<PathBuf>,
}

impl GroupKey {
    pub fn new(host: Option<String>, mut paths: Vec<PathBuf>) -> Self {
        paths.sort();
        Self { host, paths }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct SectionId(pub String);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Section {
    pub id: SectionId,
    pub name: String,
    pub collapsed: bool,
    /// Display order. Flat: no sub-sections.
    pub items: Vec<RowKey>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    Before,
    After,
}

/// Where an ordered list of rows lives.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Container {
    /// Children of a parent row (`child_order`).
    Children(RowKey),
    /// Items of a section (`Section::items`).
    Section(GroupKey, SectionId),
    /// A project's unsectioned roots (`unsectioned_order`).
    Unsectioned(GroupKey),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The parent is the child, or a descendant of it.
    Cycle,
    /// An agent thread cannot change project (H6-3).
    AgentThreadAcrossProjects,
    /// A section cannot move to another project.
    SectionAcrossProjects,
    SectionIntoItself,
    /// The section does not exist in the given project.
    UnknownSection,
    /// The section is not in the row's current project.
    SectionNotInGroup,
    /// A reorder target that is not in the visible order.
    TargetNotFound,
    /// A drop on itself.
    SameTarget,
    /// Section names must be non-empty.
    EmptyName,
    /// Section names are unique per project, case-insensitive.
    DuplicateName,
    /// This drag/target pairing has no meaning.
    InvalidTarget,
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let text = match self {
            Refusal::Cycle => "that would make a thread its own ancestor",
            Refusal::AgentThreadAcrossProjects => "agent threads cannot move to another project",
            Refusal::SectionAcrossProjects => "a section cannot move to another project",
            Refusal::SectionIntoItself => "a section cannot be dropped onto itself",
            Refusal::UnknownSection => "no such section in this project",
            Refusal::SectionNotInGroup => "the section belongs to a different project",
            Refusal::TargetNotFound => "the drop target is not in the list",
            Refusal::SameTarget => "dropped onto itself",
            Refusal::EmptyName => "section names cannot be empty",
            Refusal::DuplicateName => "a section with that name already exists in this project",
            Refusal::InvalidTarget => "cannot drop here",
        };
        f.write_str(text)
    }
}

impl std::error::Error for Refusal {}

/// Serialize maps with non-string keys as a list of pairs (JSON object keys must be strings).
mod pairs {
    use std::collections::BTreeMap;

    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<K, V, S>(map: &BTreeMap<K, V>, serializer: S) -> Result<S::Ok, S::Error>
    where
        K: Serialize,
        V: Serialize,
        S: Serializer,
    {
        serializer.collect_seq(map.iter())
    }

    pub fn deserialize<'de, K, V, D>(deserializer: D) -> Result<BTreeMap<K, V>, D::Error>
    where
        K: Deserialize<'de> + Ord,
        V: Deserialize<'de>,
        D: Deserializer<'de>,
    {
        Ok(Vec::<(K, V)>::deserialize(deserializer)?
            .into_iter()
            .collect())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hierarchy {
    pub version: u32,
    #[serde(default, with = "pairs")]
    pub sections: BTreeMap<GroupKey, Vec<Section>>,
    /// Manual order of unsectioned roots, per project.
    #[serde(default, with = "pairs")]
    pub unsectioned_order: BTreeMap<GroupKey, Vec<RowKey>>,
    /// child -> parent. Any depth; may cross projects (D1).
    #[serde(default, with = "pairs")]
    pub parents: BTreeMap<RowKey, RowKey>,
    /// Manual order of children under a parent.
    #[serde(default, with = "pairs")]
    pub child_order: BTreeMap<RowKey, Vec<RowKey>>,
    #[serde(default)]
    pub collapsed_rows: BTreeSet<RowKey>,
    /// Unix seconds; drives the 30-day prune.
    #[serde(default, with = "pairs")]
    pub last_seen: BTreeMap<RowKey, i64>,
    #[serde(default, with = "pairs")]
    pub group_last_seen: BTreeMap<GroupKey, i64>,
}

impl Default for Hierarchy {
    fn default() -> Self {
        Self {
            version: VERSION,
            sections: BTreeMap::new(),
            unsectioned_order: BTreeMap::new(),
            parents: BTreeMap::new(),
            child_order: BTreeMap::new(),
            collapsed_rows: BTreeSet::new(),
            last_seen: BTreeMap::new(),
            group_last_seen: BTreeMap::new(),
        }
    }
}

fn name_eq(a: &str, b: &str) -> bool {
    a.trim().to_lowercase() == b.trim().to_lowercase()
}

impl Hierarchy {
    pub fn is_empty(&self) -> bool {
        *self == Hierarchy::default()
    }

    /// Parse a stored blob. A v1 blob, any other version, or malformed JSON loads as an empty
    /// hierarchy (with a warning). Never panics.
    pub fn from_json(text: &str) -> Self {
        match serde_json::from_str::<Hierarchy>(text) {
            Ok(h) if h.version == VERSION => h,
            Ok(h) => {
                log::warn!(
                    "cpzed_hierarchy: ignoring stored version {} (want {VERSION})",
                    h.version
                );
                Hierarchy::default()
            }
            Err(error) => {
                log::warn!("cpzed_hierarchy: ignoring unreadable stored hierarchy: {error}");
                Hierarchy::default()
            }
        }
    }

    pub fn to_json(&self) -> anyhow::Result<String> {
        Ok(serde_json::to_string(self)?)
    }

    // ---- queries ------------------------------------------------------------------------

    /// The id of the section named `name` (case-insensitive) in `group`, if any.
    pub fn clone_section_id(&self, group: &GroupKey, name: &str) -> Option<SectionId> {
        self.sections
            .get(group)?
            .iter()
            .find(|s| name_eq(&s.name, name))
            .map(|s| s.id.clone())
    }

    pub fn section(&self, group: &GroupKey, id: &SectionId) -> Option<&Section> {
        self.sections.get(group)?.iter().find(|s| &s.id == id)
    }

    /// The section (in any project) that lists `row`, if any.
    pub fn section_of(&self, row: &RowKey) -> Option<(&GroupKey, &SectionId)> {
        self.sections.iter().find_map(|(group, sections)| {
            sections
                .iter()
                .find(|s| s.items.contains(row))
                .map(|s| (group, &s.id))
        })
    }

    /// True if `node` is a strict descendant of `ancestor`. O(depth) with a visited guard.
    pub fn is_descendant(&self, node: &RowKey, ancestor: &RowKey) -> bool {
        let mut seen = BTreeSet::new();
        let mut cursor = node;
        while let Some(parent) = self.parents.get(cursor) {
            if parent == ancestor {
                return true;
            }
            if !seen.insert(parent) {
                return false;
            }
            cursor = parent;
        }
        false
    }

    /// Would `child` becoming a child of `parent` create a cycle?
    pub fn would_cycle(&self, child: &RowKey, parent: &RowKey) -> bool {
        child == parent || self.is_descendant(parent, child)
    }

    pub fn children_of(&self, parent: &RowKey) -> Vec<&RowKey> {
        self.parents
            .iter()
            .filter(|(_, p)| *p == parent)
            .map(|(c, _)| c)
            .collect()
    }

    // ---- sections -----------------------------------------------------------------------

    fn next_section_id(&self) -> SectionId {
        let used: BTreeSet<&str> = self
            .sections
            .values()
            .flatten()
            .map(|s| s.id.0.as_str())
            .collect();
        let mut n = 1u64;
        loop {
            let candidate = format!("s{n}");
            if !used.contains(candidate.as_str()) {
                return SectionId(candidate);
            }
            n += 1;
        }
    }

    /// Returns the existing id when the name already exists (case-insensitive), with
    /// `created == false`. New sections go last.
    pub fn create_section(
        &mut self,
        group: &GroupKey,
        name: &str,
    ) -> Result<(SectionId, bool), Refusal> {
        let name = name.trim();
        if name.is_empty() {
            return Err(Refusal::EmptyName);
        }
        if let Some(existing) = self
            .sections
            .get(group)
            .and_then(|s| s.iter().find(|s| name_eq(&s.name, name)))
        {
            return Ok((existing.id.clone(), false));
        }
        let id = self.next_section_id();
        self.sections
            .entry(group.clone())
            .or_default()
            .push(Section {
                id: id.clone(),
                name: name.to_string(),
                collapsed: false,
                items: Vec::new(),
            });
        Ok((id, true))
    }

    pub fn rename_section(
        &mut self,
        group: &GroupKey,
        id: &SectionId,
        name: &str,
    ) -> Result<(), Refusal> {
        let name = name.trim();
        if name.is_empty() {
            return Err(Refusal::EmptyName);
        }
        let sections = self
            .sections
            .get_mut(group)
            .ok_or(Refusal::UnknownSection)?;
        if sections
            .iter()
            .any(|s| &s.id != id && name_eq(&s.name, name))
        {
            return Err(Refusal::DuplicateName);
        }
        let section = sections
            .iter_mut()
            .find(|s| &s.id == id)
            .ok_or(Refusal::UnknownSection)?;
        section.name = name.to_string();
        Ok(())
    }

    /// The section's items become unsectioned roots, appended to the unsectioned order (the
    /// unsectioned area sits directly above the sections, so this is where the section was).
    /// No thread is touched. Returns the freed items.
    pub fn delete_section(
        &mut self,
        group: &GroupKey,
        id: &SectionId,
    ) -> Result<Vec<RowKey>, Refusal> {
        let sections = self
            .sections
            .get_mut(group)
            .ok_or(Refusal::UnknownSection)?;
        let index = sections
            .iter()
            .position(|s| &s.id == id)
            .ok_or(Refusal::UnknownSection)?;
        let section = sections.remove(index);
        if sections.is_empty() {
            self.sections.remove(group);
        }
        let order = self.unsectioned_order.entry(group.clone()).or_default();
        for item in &section.items {
            if !order.contains(item) {
                order.push(item.clone());
            }
        }
        Ok(section.items)
    }

    pub fn move_section(
        &mut self,
        group: &GroupKey,
        id: &SectionId,
        target_group: &GroupKey,
        target: &SectionId,
        side: Side,
    ) -> Result<(), Refusal> {
        if group != target_group {
            return Err(Refusal::SectionAcrossProjects);
        }
        if id == target {
            return Err(Refusal::SectionIntoItself);
        }
        let sections = self
            .sections
            .get_mut(group)
            .ok_or(Refusal::UnknownSection)?;
        if !sections.iter().any(|s| &s.id == target) {
            return Err(Refusal::TargetNotFound);
        }
        let from = sections
            .iter()
            .position(|s| &s.id == id)
            .ok_or(Refusal::UnknownSection)?;
        let section = sections.remove(from);
        let to = sections
            .iter()
            .position(|s| &s.id == target)
            .ok_or(Refusal::TargetNotFound)?;
        sections.insert(
            match side {
                Side::Before => to,
                Side::After => to + 1,
            },
            section,
        );
        Ok(())
    }

    pub fn set_section_collapsed(
        &mut self,
        group: &GroupKey,
        id: &SectionId,
        collapsed: bool,
    ) -> Result<(), Refusal> {
        let section = self
            .sections
            .get_mut(group)
            .and_then(|s| s.iter_mut().find(|s| &s.id == id))
            .ok_or(Refusal::UnknownSection)?;
        section.collapsed = collapsed;
        Ok(())
    }

    // ---- membership ---------------------------------------------------------------------

    /// Remove `row` from every section, every unsectioned order and every sibling order.
    /// Does not touch `parents`.
    fn remove_from_lists(&mut self, row: &RowKey) {
        for sections in self.sections.values_mut() {
            for section in sections {
                section.items.retain(|r| r != row);
            }
        }
        for order in self.unsectioned_order.values_mut() {
            order.retain(|r| r != row);
        }
        for order in self.child_order.values_mut() {
            order.retain(|r| r != row);
        }
        self.unsectioned_order.retain(|_, v| !v.is_empty());
        self.child_order.retain(|_, v| !v.is_empty());
    }

    /// `Some(section)` puts the row, un-nested, at the top of the section; `None` at the top of
    /// the project's unsectioned area. The section must be in `row_group`.
    pub fn set_section(
        &mut self,
        row: &RowKey,
        row_group: &GroupKey,
        section: Option<&SectionId>,
    ) -> Result<(), Refusal> {
        if let Some(id) = section
            && self.section(row_group, id).is_none()
        {
            return Err(if self.sections.values().flatten().any(|s| &s.id == id) {
                Refusal::SectionNotInGroup
            } else {
                Refusal::UnknownSection
            });
        }
        self.parents.remove(row);
        self.remove_from_lists(row);
        match section {
            Some(id) => {
                if let Some(section) = self
                    .sections
                    .get_mut(row_group)
                    .and_then(|s| s.iter_mut().find(|s| &s.id == id))
                {
                    section.items.insert(0, row.clone());
                }
            }
            None => {
                self.unsectioned_order
                    .entry(row_group.clone())
                    .or_default()
                    .insert(0, row.clone());
            }
        }
        Ok(())
    }

    /// Nest `child` under `parent` (appended last), or un-nest it with `None`. Refused with
    /// [`Refusal::Cycle`] when `parent` is `child` or one of its descendants. There is no depth
    /// limit. The child leaves any section.
    pub fn set_parent(&mut self, child: &RowKey, parent: Option<&RowKey>) -> Result<(), Refusal> {
        if let Some(parent) = parent
            && self.would_cycle(child, parent)
        {
            return Err(Refusal::Cycle);
        }
        self.remove_from_lists(child);
        match parent {
            Some(parent) => {
                self.parents.insert(child.clone(), parent.clone());
                self.child_order
                    .entry(parent.clone())
                    .or_default()
                    .push(child.clone());
            }
            None => {
                self.parents.remove(child);
            }
        }
        Ok(())
    }

    /// Move `row` next to `target` inside `container`.
    ///
    /// `visible` is the container's current visible order; it is materialized first (H6), so the
    /// stored order is exactly what the user saw. Entries stored but not visible keep their
    /// relative order after the visible ones. If `row` is not yet in the container it is moved in
    /// (re-parented, or re-sectioned: the caller guarantees the section is in the row's project).
    pub fn move_row(
        &mut self,
        row: &RowKey,
        target: &RowKey,
        side: Side,
        container: &Container,
        visible: &[RowKey],
    ) -> Result<(), Refusal> {
        if row == target {
            return Err(Refusal::SameTarget);
        }
        if !visible.contains(target) {
            return Err(Refusal::TargetNotFound);
        }
        match container {
            Container::Children(parent) => {
                if self.would_cycle(row, parent) {
                    return Err(Refusal::Cycle);
                }
            }
            Container::Section(group, id) => {
                if self.section(group, id).is_none() {
                    return Err(Refusal::UnknownSection);
                }
            }
            Container::Unsectioned(_) => {}
        }

        // Detach from wherever the row is now, then record membership in the container.
        self.remove_from_lists(row);
        match container {
            Container::Children(parent) => {
                self.parents.insert(row.clone(), parent.clone());
            }
            _ => {
                self.parents.remove(row);
            }
        }

        let stored: Vec<RowKey> = match container {
            Container::Children(parent) => {
                self.child_order.get(parent).cloned().unwrap_or_default()
            }
            Container::Section(group, id) => self
                .section(group, id)
                .map(|s| s.items.clone())
                .unwrap_or_default(),
            Container::Unsectioned(group) => self
                .unsectioned_order
                .get(group)
                .cloned()
                .unwrap_or_default(),
        };

        let mut list: Vec<RowKey> = Vec::with_capacity(visible.len() + stored.len() + 1);
        for key in visible {
            if key != row && !list.contains(key) {
                list.push(key.clone());
            }
        }
        for key in stored {
            if key != *row && !list.contains(&key) {
                list.push(key);
            }
        }
        let at = list
            .iter()
            .position(|k| k == target)
            .ok_or(Refusal::TargetNotFound)?;
        list.insert(
            match side {
                Side::Before => at,
                Side::After => at + 1,
            },
            row.clone(),
        );

        match container {
            Container::Children(parent) => {
                self.child_order.insert(parent.clone(), list);
            }
            Container::Section(group, id) => {
                if let Some(section) = self
                    .sections
                    .get_mut(group)
                    .and_then(|s| s.iter_mut().find(|s| &s.id == id))
                {
                    section.items = list;
                }
            }
            Container::Unsectioned(group) => {
                self.unsectioned_order.insert(group.clone(), list);
            }
        }
        Ok(())
    }

    /// H6 cross-project move: the row leaves its section and, if it is a root, lands at the top of
    /// `to`'s unsectioned area. Links to and from other rows are untouched, so its own children
    /// stay with it and a child keeps its parent (D1).
    pub fn cross_project_move(&mut self, row: &RowKey, from: &GroupKey, to: &GroupKey) {
        if let Some(sections) = self.sections.get_mut(from) {
            for section in sections {
                section.items.retain(|r| r != row);
            }
        }
        if let Some(order) = self.unsectioned_order.get_mut(from) {
            order.retain(|r| r != row);
            if order.is_empty() {
                self.unsectioned_order.remove(from);
            }
        }
        if !self.parents.contains_key(row) {
            let order = self.unsectioned_order.entry(to.clone()).or_default();
            order.retain(|r| r != row);
            order.insert(0, row.clone());
        }
    }

    pub fn set_row_collapsed(&mut self, row: &RowKey, collapsed: bool) {
        if collapsed {
            self.collapsed_rows.insert(row.clone());
        } else {
            self.collapsed_rows.remove(row);
        }
    }

    // ---- retention ----------------------------------------------------------------------

    pub fn touch_seen(&mut self, rows: &[RowKey], groups: &[GroupKey], now: i64) {
        for row in rows {
            self.last_seen.insert(row.clone(), now);
        }
        for group in groups {
            self.group_last_seen.insert(group.clone(), now);
        }
    }

    /// Drop rows unseen for [`RETENTION_SECS`], and the sections of groups unseen that long.
    /// Empty sections of a group seen recently persist. Children of a pruned row are lifted to its
    /// nearest surviving ancestor (D4), permanently. Rows or groups that are referenced but have
    /// no `last_seen` yet start their clock at `now`. Returns the number of pruned rows + groups.
    pub fn prune(&mut self, now: i64) -> usize {
        let cutoff = now - RETENTION_SECS;

        // Everything the structures mention.
        let mut mentioned: BTreeSet<RowKey> = BTreeSet::new();
        mentioned.extend(self.parents.keys().cloned());
        mentioned.extend(self.parents.values().cloned());
        mentioned.extend(self.collapsed_rows.iter().cloned());
        mentioned.extend(self.child_order.keys().cloned());
        mentioned.extend(self.child_order.values().flatten().cloned());
        mentioned.extend(self.unsectioned_order.values().flatten().cloned());
        mentioned.extend(
            self.sections
                .values()
                .flatten()
                .flat_map(|s| s.items.clone()),
        );
        for row in &mentioned {
            self.last_seen.entry(row.clone()).or_insert(now);
        }
        for group in self.sections.keys().chain(self.unsectioned_order.keys()) {
            self.group_last_seen.entry(group.clone()).or_insert(now);
        }

        let stale_rows: BTreeSet<RowKey> = self
            .last_seen
            .iter()
            .filter(|(_, seen)| **seen < cutoff)
            .map(|(row, _)| row.clone())
            .collect();
        let stale_groups: Vec<GroupKey> = self
            .group_last_seen
            .iter()
            .filter(|(_, seen)| **seen < cutoff)
            .map(|(group, _)| group.clone())
            .collect();

        // Lift children of stale rows to the nearest surviving ancestor.
        let relinks: Vec<(RowKey, Option<RowKey>)> = self
            .parents
            .iter()
            .filter(|(child, parent)| !stale_rows.contains(*child) && stale_rows.contains(*parent))
            .map(|(child, parent)| {
                let mut seen = BTreeSet::new();
                let mut cursor = parent;
                let survivor = loop {
                    if !stale_rows.contains(cursor) {
                        break Some(cursor.clone());
                    }
                    if !seen.insert(cursor) {
                        break None;
                    }
                    match self.parents.get(cursor) {
                        Some(next) => cursor = next,
                        None => break None,
                    }
                };
                (child.clone(), survivor)
            })
            .collect();
        for (child, survivor) in relinks {
            match survivor {
                Some(parent) if parent != child => {
                    self.parents.insert(child.clone(), parent.clone());
                    let order = self.child_order.entry(parent).or_default();
                    if !order.contains(&child) {
                        order.push(child);
                    }
                }
                _ => {
                    self.parents.remove(&child);
                }
            }
        }

        for row in &stale_rows {
            self.parents.remove(row);
            self.child_order.remove(row);
            self.collapsed_rows.remove(row);
            self.last_seen.remove(row);
        }
        self.parents
            .retain(|_, parent| !stale_rows.contains(parent));
        for order in self.child_order.values_mut() {
            order.retain(|r| !stale_rows.contains(r));
        }
        for order in self.unsectioned_order.values_mut() {
            order.retain(|r| !stale_rows.contains(r));
        }
        for sections in self.sections.values_mut() {
            for section in sections {
                section.items.retain(|r| !stale_rows.contains(r));
            }
        }
        for group in &stale_groups {
            self.sections.remove(group);
            self.unsectioned_order.remove(group);
            self.group_last_seen.remove(group);
        }
        self.child_order.retain(|_, v| !v.is_empty());
        self.unsectioned_order.retain(|_, v| !v.is_empty());

        stale_rows.len() + stale_groups.len()
    }
}
