extern crate kprint_platform;
pub mod environment;
pub mod test_helpers;
pub use kprint_platform::utils;
mod plugins {
  pub use kprint_host_api::compiler::CompilationResult;
  pub use kprint_host_api::compiler::CompileControl as WasmCompileControl;
  #[cfg(feature = "plugins")]
  pub use kprint_plugin_host::compile_wasm;
}
