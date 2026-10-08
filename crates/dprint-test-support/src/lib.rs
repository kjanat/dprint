extern crate dprint_platform;
pub mod environment;
pub mod test_helpers;
pub use dprint_platform::utils;
mod plugins {
  pub use dprint_host_api::compiler::CompilationResult;
  pub use dprint_host_api::compiler::CompileControl as WasmCompileControl;
  #[cfg(feature = "plugins")]
  pub use dprint_plugin_host::compile_wasm;
}
