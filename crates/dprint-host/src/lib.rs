#[macro_use]
extern crate dprint_platform;
pub mod environment {
  pub use dprint_platform::environment::*;
  #[cfg(test)]
  pub use dprint_test_support::environment::*;
}
pub use dprint_plugin_host as plugins;
mod get_plugin_config_map;
pub use get_plugin_config_map::*;
pub mod configuration {
  pub use crate::get_plugin_config_map::*;
  pub use dprint_config::resolution::*;
}
pub mod format;
pub mod incremental;
pub mod paths;
pub mod patterns;
pub mod resolution;
mod utils {
  pub use dprint_discovery::*;
  pub use dprint_platform::utils::*;
}
#[cfg(test)]
pub use dprint_test_support::arg_parser;
#[cfg(test)]
pub use dprint_test_support::test_helpers;

mod name_resolution;
pub use name_resolution::PluginNameResolutionMaps;
pub mod session;
pub use dprint_config::options::ConfigOptions;
pub use dprint_config::options::SessionOptions;
pub use session::HostSession;

#[cfg(test)]
mod backend_tests;
