//! Agent setup inventory (issue #331): every instruction file, skill, rule,
//! hook and MCP entry the coding agents running in a workspace actually read,
//! project-scoped and global.
//!
//! GPU-free and never links GPUI: the app crate calls [`scan`] from a
//! background thread and renders the returned [`Inventory`]. The scan is
//! read-only - it never creates, edits or deletes an artifact - and never
//! makes a network call. It walks a fixed catalog ([`catalog`]), not the file
//! tree, so the Files sidebar's hidden / gitignored filter is untouched.
//!
//! Layout:
//! - [`catalog`]: the path catalog, per harness and scope.
//! - [`scan`]: `scan(&Roots) -> Inventory`, roots injected, no `dirs` call.
//! - [`classify`]: `ArtifactType`, `Scope`, `Harness`.
//! - [`title`]: frontmatter name / first heading / file stem, head-only read.

pub mod catalog;
pub mod classify;
pub mod scan;
pub mod title;

pub use classify::{ArtifactType, Harness, Scope};
pub use scan::{
    scan, Inventory, Roots, SetupRow, MAX_ARTIFACT_BYTES, MAX_ROWS, MAX_SKILLS_PER_DIR,
};
