//! # cowt-core
//!
//! Platform-independent core engine for **co-worktree**: manifest snapshots,
//! structural/content diff and three-way merge. It contains no platform VFS
//! code and no async runtime; platform backends live in the CLI crate.

pub mod diff;
pub mod error;
pub mod manifest;
pub mod merge;
pub mod overlay;

pub use diff::{Change, ChangeKind, ContentDiff, KeyChange};
pub use error::{Error, Result};
pub use manifest::{Entry, EntryKind, Manifest, ScanOutcome};
pub use merge::{Conflict, ConflictKind, MergePlan, Operation};
