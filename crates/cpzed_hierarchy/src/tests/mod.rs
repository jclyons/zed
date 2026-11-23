//! All tests are in-memory: no filesystem, no real HOME, keychain or data dirs.

mod arrange_tests;
mod drop_tests;
mod model_tests;
mod validate_tests;

#[cfg(feature = "test-support")]
mod store_tests;

use std::collections::BTreeSet;
use std::path::PathBuf;

use crate::{ArrangeInput, GroupKey, Hierarchy, RowInfo, RowKey, Slot, arrange_rows};

pub fn g(name: &str) -> GroupKey {
    GroupKey::new(None, vec![PathBuf::from(format!("/tmp/{name}"))])
}

pub fn term(id: &str) -> RowKey {
    RowKey::Terminal(id.to_string())
}

pub fn thr(id: &str) -> RowKey {
    RowKey::Thread(id.to_string())
}

/// A row in `group`; larger `time` is newer.
pub fn info(key: &RowKey, group: &GroupKey, time: i64) -> RowInfo {
    RowInfo {
        key: key.clone(),
        group: group.clone(),
        sort_time: time,
        running: false,
        waiting: false,
        notified: false,
    }
}

pub fn arrange(
    group: &GroupKey,
    rows: &[RowInfo],
    foreign: &[RowInfo],
    h: &Hierarchy,
) -> Vec<Slot> {
    arrange_rows(&ArrangeInput {
        group,
        rows,
        foreign,
        elsewhere: &BTreeSet::new(),
        hierarchy: h,
        query_mode: false,
    })
}

/// Compact rendering for assertions: `name:depth`, section headers as `[name]`.
pub fn shape(slots: &[Slot]) -> Vec<String> {
    slots
        .iter()
        .map(|slot| match slot {
            Slot::SectionHeader { name, .. } => format!("[{name}]"),
            Slot::Row { key, depth, .. } => {
                let id = match key {
                    RowKey::Terminal(s) | RowKey::Thread(s) => s,
                };
                format!("{id}:{depth}")
            }
        })
        .collect()
}
