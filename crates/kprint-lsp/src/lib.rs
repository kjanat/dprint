#[macro_use]
extern crate kprint_platform;
pub mod environment {
  pub use kprint_platform::environment::*;
  #[cfg(test)]
  pub use kprint_test_support::environment::*;
}
pub use kprint_config::resolution as configuration;
pub use kprint_host::resolution;
pub use kprint_plugin_host as plugins;
#[cfg(test)]
pub use kprint_test_support::test_helpers;
mod lsp;
pub use lsp::*;
#[cfg(test)]
extern crate kprint_test_support;
