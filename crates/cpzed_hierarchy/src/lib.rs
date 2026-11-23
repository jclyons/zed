//! Fork-owned hierarchy model for the CPZed Threads Sidebar (H7 revision 2).
//!
//! Layout: project group > its unsectioned threads, then its sections in the
//! user's order > threads > subthreads (any depth).
//!
//! The pure modules (`model`, `arrange`, `drop`, `validate`) have no gpui
//! dependency. `store` (feature `store`, on by default) wraps a [`Hierarchy`]
//! in a gpui `Entity` with a debounced single-writer KVP persistence task.

mod arrange;
mod drop;
mod model;
mod validate;

#[cfg(feature = "store")]
mod store;

pub use arrange::{ArrangeInput, Rollup, RowInfo, Slot, arrange_rows};
pub use drop::{DragItem, DropAction, DropCtx, DropTarget, Zone, plan_drop};
pub use model::{
    Container, GroupKey, Hierarchy, LoadOutcome, RETENTION_SECS, Refusal, RowKey, Section,
    SectionId, Side, VERSION,
};
pub use validate::Repair;

#[cfg(feature = "store")]
pub use store::{HierarchyStore, KVP_KEY, KVP_SCOPE, WRITE_DEBOUNCE};

#[cfg(test)]
mod tests;
