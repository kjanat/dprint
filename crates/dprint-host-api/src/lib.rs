//! Contracts for embedding a host. Implementations live in dprint-platform
//! and frontends supply filesystem, output, interaction and compiler services.
#[macro_use]
pub mod environment;
pub mod compiler;
pub mod options;
pub mod selection;
pub mod ui;
