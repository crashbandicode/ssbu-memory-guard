//! memory_guard: at known Smash Ultimate 13.0.5 crash sites, repairs a pointer
//! that gained one stray bit before the game reads through it, and records the
//! page-release failure behind a startup abort. It changes only values the
//! game would fault on; everything else passes through untouched.
pub mod chain;
pub mod guards;
pub mod model;
#[cfg(target_os = "switch")]
mod runtime;
