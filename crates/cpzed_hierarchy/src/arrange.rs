//! `arrange_rows`: turns the rows of one project group plus the [`Hierarchy`] into the ordered
//! list of slots the sidebar draws. Pure, iterative (explicit stacks, no recursion), O(rows log).
//!
//! The empty draft thread is pinned first by the caller; it is not part of the hierarchy.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::model::{GroupKey, Hierarchy, RowKey, SectionId};

/// One row as the sidebar knows it.
#[derive(Clone, Debug)]
pub struct RowInfo {
    pub key: RowKey,
    /// The row's own project.
    pub group: GroupKey,
    /// Newest-first sort time (unix millis or secs; only the order matters).
    pub sort_time: i64,
    pub running: bool,
    pub waiting: bool,
    pub notified: bool,
}

/// Rolled-up state of a subtree or a section.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rollup {
    pub running: u32,
    pub waiting: u32,
    pub notified: bool,
    /// Number of rows rolled up (the "+N" of a collapsed parent).
    pub count: u32,
}

impl Rollup {
    fn add(&mut self, other: Rollup) {
        self.running += other.running;
        self.waiting += other.waiting;
        self.notified |= other.notified;
        self.count += other.count;
    }

    fn of(info: &RowInfo) -> Rollup {
        Rollup {
            running: info.running as u32,
            waiting: info.waiting as u32,
            notified: info.notified,
            count: 1,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Slot {
    SectionHeader {
        id: SectionId,
        name: String,
        collapsed: bool,
        rollup: Rollup,
    },
    Row {
        key: RowKey,
        depth: usize,
        has_children: bool,
        /// DIRECT children only (not descendants), shown as the child-count chip.
        child_count: usize,
        collapsed: bool,
        /// Rolled up over the row's descendants (not the row itself).
        rollup: Rollup,
        /// The row belongs to another project and is shown here under its parent (D1).
        foreign_project: bool,
    },
}

pub struct ArrangeInput<'a> {
    pub group: &'a GroupKey,
    /// The rows present in this group.
    pub rows: &'a [RowInfo],
    /// Rows of OTHER groups whose parent chain leads into this group (D1).
    pub foreign: &'a [RowInfo],
    /// Keys of rows shown in other open groups. An in-group row whose parent is shown there is
    /// rendered under it and omitted here (D1); without this it would be listed twice.
    pub elsewhere: &'a BTreeSet<RowKey>,
    pub hierarchy: &'a Hierarchy,
    /// Search active: a flat newest-first list, exactly today's order.
    pub query_mode: bool,
}

#[derive(Clone, Copy)]
enum Eff {
    Root,
    Parent(usize),
    Omit,
}

fn newest_first(a: &RowInfo, b: &RowInfo) -> std::cmp::Ordering {
    (Reverse(a.sort_time), &a.key).cmp(&(Reverse(b.sort_time), &b.key))
}

/// Unordered rows first (newest first), then listed rows in the stored order.
fn order_members(
    infos: &[&RowInfo],
    members: Vec<usize>,
    order: Option<&Vec<RowKey>>,
) -> Vec<usize> {
    let position: BTreeMap<&RowKey, usize> = order
        .map(|o| o.iter().enumerate().map(|(i, k)| (k, i)).collect())
        .unwrap_or_default();
    let (mut ordered, mut unordered): (Vec<usize>, Vec<usize>) = members
        .into_iter()
        .partition(|&i| position.contains_key(&infos[i].key));
    unordered.sort_by(|&a, &b| newest_first(infos[a], infos[b]));
    ordered.sort_by_key(|&i| position.get(&infos[i].key).copied().unwrap_or(usize::MAX));
    unordered.extend(ordered);
    unordered
}

pub fn arrange_rows(input: &ArrangeInput) -> Vec<Slot> {
    let ArrangeInput {
        group,
        rows,
        foreign,
        elsewhere,
        hierarchy,
        query_mode,
    } = *input;

    if query_mode {
        let mut flat: Vec<&RowInfo> = rows.iter().collect();
        flat.sort_by(|a, b| newest_first(a, b));
        return flat
            .into_iter()
            .map(|info| Slot::Row {
                key: info.key.clone(),
                depth: 0,
                has_children: false,
                child_count: 0,
                collapsed: false,
                rollup: Rollup::default(),
                foreign_project: false,
            })
            .collect();
    }

    // Universe of nodes: in-group rows first, then foreign ones. First occurrence of a key wins.
    let mut infos: Vec<&RowInfo> = Vec::with_capacity(rows.len() + foreign.len());
    let mut index: HashMap<&RowKey, usize> = HashMap::with_capacity(rows.len() + foreign.len());
    for info in rows.iter().chain(foreign.iter()) {
        if !index.contains_key(&info.key) {
            index.insert(&info.key, infos.len());
            infos.push(info);
        }
    }
    let in_group = |i: usize| infos[i].group == *group;
    let n = infos.len();

    // Effective parent: the nearest present ancestor. Absent parents are skipped, which lifts a
    // child one level per absent parent (D4) and restores it when the parent returns.
    let mut eff: Vec<Eff> = Vec::with_capacity(n);
    for (i, info) in infos.iter().enumerate() {
        let mut cursor = &info.key;
        let mut visited: Option<BTreeSet<&RowKey>> = None;
        let result = loop {
            let Some(parent) = hierarchy.parents.get(cursor) else {
                break Eff::Root;
            };
            if let Some(&j) = index.get(parent) {
                break if j == i { Eff::Root } else { Eff::Parent(j) };
            }
            if elsewhere.contains(parent) {
                break Eff::Omit;
            }
            let seen = visited.get_or_insert_with(BTreeSet::new);
            if !seen.insert(parent) {
                break Eff::Root;
            }
            cursor = parent;
        };
        eff.push(result);
    }

    let mut children: Vec<Vec<usize>> = vec![Vec::new(); n];
    let mut par: Vec<Option<usize>> = vec![None; n];
    let mut roots: Vec<usize> = Vec::new();
    let mut omitted: Vec<usize> = Vec::new();
    for i in 0..n {
        match eff[i] {
            Eff::Parent(p) => {
                children[p].push(i);
                par[i] = Some(p);
            }
            Eff::Root if in_group(i) => roots.push(i),
            Eff::Root => {}
            Eff::Omit => omitted.push(i),
        }
    }

    // Mark what is reachable. Whatever hangs below an omitted row is rendered elsewhere. Any
    // in-group row left over is part of a corrupt parent cycle: promote it to a root so nothing
    // is ever hidden.
    let mut state = vec![0u8; n]; // 1 = reached, 2 = rendered elsewhere
    let mark =
        |starts: &[usize], mark_with: u8, state: &mut Vec<u8>, children: &Vec<Vec<usize>>| {
            let mut stack: Vec<usize> = starts.to_vec();
            while let Some(i) = stack.pop() {
                if state[i] != 0 {
                    continue;
                }
                state[i] = mark_with;
                stack.extend(children[i].iter().copied());
            }
        };
    mark(&roots, 1, &mut state, &children);
    mark(&omitted, 2, &mut state, &children);
    let mut promoted: Vec<usize> = Vec::new();
    let mut leftover: Vec<usize> = (0..n).filter(|&i| state[i] == 0 && in_group(i)).collect();
    leftover.sort_by(|&a, &b| newest_first(infos[a], infos[b]));
    for i in leftover {
        if state[i] != 0 {
            continue;
        }
        if let Some(p) = par[i].take() {
            children[p].retain(|&c| c != i);
        }
        promoted.push(i);
        mark(&[i], 1, &mut state, &children);
    }

    // Partition roots into the unsectioned area and sections of this group.
    let sections = hierarchy
        .sections
        .get(group)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let mut section_of: BTreeMap<&RowKey, usize> = BTreeMap::new();
    for (si, section) in sections.iter().enumerate() {
        for item in &section.items {
            section_of.entry(item).or_insert(si);
        }
    }
    let mut unsectioned: Vec<usize> = Vec::new();
    let mut per_section: Vec<Vec<usize>> = vec![Vec::new(); sections.len()];
    for &i in &roots {
        match section_of.get(&infos[i].key) {
            Some(&si) => per_section[si].push(i),
            None => unsectioned.push(i),
        }
    }
    let mut unsectioned =
        order_members(&infos, unsectioned, hierarchy.unsectioned_order.get(group));
    unsectioned.extend(promoted.iter().copied());
    let per_section: Vec<Vec<usize>> = per_section
        .into_iter()
        .zip(sections)
        .map(|(members, section)| order_members(&infos, members, Some(&section.items)))
        .collect();

    // Pass 1: full pre-order over everything (collapse ignored) to sort children and roll up.
    let mut preorder: Vec<usize> = Vec::with_capacity(n);
    let mut stack: Vec<usize> = Vec::new();
    let all_roots = unsectioned.iter().chain(per_section.iter().flatten());
    for &root in all_roots.collect::<Vec<_>>().into_iter().rev() {
        stack.push(root);
    }
    while let Some(i) = stack.pop() {
        preorder.push(i);
        let members = std::mem::take(&mut children[i]);
        let sorted = order_members(&infos, members, hierarchy.child_order.get(&infos[i].key));
        stack.extend(sorted.iter().rev().copied());
        children[i] = sorted;
    }
    let mut agg: Vec<Rollup> = vec![Rollup::default(); n];
    for &i in preorder.iter().rev() {
        if let Some(p) = par[i] {
            let mut total = agg[i];
            total.add(Rollup::of(infos[i]));
            agg[p].add(total);
        }
    }
    let subtree = |i: usize, agg: &Vec<Rollup>| {
        let mut total = agg[i];
        total.add(Rollup::of(infos[i]));
        total
    };

    // Pass 2: emit, skipping collapsed subtrees and collapsed sections.
    let mut out: Vec<Slot> = Vec::with_capacity(preorder.len() + sections.len());
    let emit = |roots: &[usize], out: &mut Vec<Slot>| {
        let mut stack: Vec<(usize, usize)> = roots.iter().rev().map(|&i| (i, 0)).collect();
        while let Some((i, depth)) = stack.pop() {
            let has_children = !children[i].is_empty();
            let collapsed = has_children && hierarchy.collapsed_rows.contains(&infos[i].key);
            out.push(Slot::Row {
                key: infos[i].key.clone(),
                depth,
                has_children,
                child_count: children[i].len(),
                collapsed,
                rollup: agg[i],
                foreign_project: !in_group(i),
            });
            if !collapsed {
                stack.extend(children[i].iter().rev().map(|&c| (c, depth + 1)));
            }
        }
    };
    emit(&unsectioned, &mut out);
    for (section, items) in sections.iter().zip(&per_section) {
        let mut rollup = Rollup::default();
        for &i in items {
            rollup.add(subtree(i, &agg));
        }
        out.push(Slot::SectionHeader {
            id: section.id.clone(),
            name: section.name.clone(),
            collapsed: section.collapsed,
            rollup,
        });
        if !section.collapsed {
            emit(items, &mut out);
        }
    }
    out
}
