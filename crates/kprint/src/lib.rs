#![deny(clippy::print_stderr)]
#![deny(clippy::print_stdout)]
#![deny(clippy::unused_async)]

#[macro_use]
extern crate kprint_platform;
pub mod arg_parser;
pub mod commands;
pub mod environment;
pub mod run_cli;
pub use kprint_host::format;
pub use kprint_host::incremental;
pub use kprint_host::paths;
pub use kprint_host::patterns;
pub use kprint_host::resolution;
pub use kprint_platform::cache;
pub use kprint_plugin_host as plugins;
pub use run_cli::AppError;
pub mod terminal;
pub mod utils {
  pub use crate::terminal::*;
  pub use kprint_discovery::*;
  pub use kprint_platform::utils::*;
}
mod get_init_config_file_text;
pub mod configuration {
  pub use crate::get_init_config_file_text::*;
  pub use kprint_config::resolution::*;
  pub use kprint_host::get_plugin_config_map;
}
#[cfg(test)]
pub mod test_helpers;

#[cfg(test)]
mod backend_integration_tests;

#[cfg(test)]
pub use kprint_test_support::assert_contains;
