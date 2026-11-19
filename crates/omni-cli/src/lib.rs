//! Shared code for the `omni` command and the `omniup` updater.
//!
//! The two binaries differ only in what they edit by default: `omni update`
//! replaces the running engine binary, and `omniup` replaces the `omni` beside
//! it. Fetching, checksum verification and the atomic replacement itself live
//! here so the two paths cannot drift.

pub mod serve;
pub mod updater;
