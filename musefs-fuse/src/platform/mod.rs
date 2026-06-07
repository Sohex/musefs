//! Per-OS behavior for the FUSE adapter. Every `#[cfg(target_os = ...)]`
//! branch in this crate lives under this module, so the `Filesystem` handlers
//! in `lib.rs` stay platform-agnostic: they call functions whose stubs compile
//! to no-ops or `None` on the wrong OS.

pub mod mount;
pub mod passthrough;
pub mod spotlight;
