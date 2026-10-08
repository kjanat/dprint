#![deny(clippy::print_stderr)]
#![deny(clippy::print_stdout)]
#![deny(clippy::unused_async)]

#[macro_use]
extern crate dprint_platform;
pub mod arg_parser;
pub mod commands;
pub mod environment;
pub mod run_cli;
pub use dprint_host::format;
pub use dprint_host::incremental;
pub use dprint_host::paths;
pub use dprint_host::patterns;
pub use dprint_host::resolution;
pub use dprint_platform::cache;
pub use dprint_plugin_host as plugins;
pub use run_cli::AppError;
pub mod terminal;
pub mod utils {
  pub use crate::terminal::*;
  pub use dprint_discovery::*;
  pub use dprint_platform::utils::*;
}
mod get_init_config_file_text;
pub mod configuration {
  pub use crate::get_init_config_file_text::*;
  pub use dprint_config::resolution::*;
  pub use dprint_host::get_plugin_config_map;
}
#[cfg(test)]
pub mod test_helpers;

#[cfg(test)]
mod backend_integration_tests;

#[cfg(test)]
pub use dprint_test_support::assert_contains;
