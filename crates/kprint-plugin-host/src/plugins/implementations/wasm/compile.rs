use anyhow::Result;

use super::create_identity_import_object;
use super::create_wasm_plugin_instance;
use super::instance::WasmHostState;
use super::load_instance::WasmModuleCreator;
use super::load_instance::load_instance;
use crate::plugins::CompilationResult;

/// A step of setting up a wasm plugin. Reported as each one starts so a setup
/// that stalls can say where it stalled (compiling, or running plugin code).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WasmSetupStep {
  /// Compiling the wasm module to native code (Cranelift).
  Compile,
  /// Serializing the native code for the plugin cache.
  Serialize,
  /// Instantiating the module, which runs its start function (plugin code).
  Instantiate,
  /// Calling the plugin to get its plugin info (plugin code).
  PluginInfo,
}

impl WasmSetupStep {
  pub fn as_u8(self) -> u8 {
    match self {
      WasmSetupStep::Compile => 0,
      WasmSetupStep::Serialize => 1,
      WasmSetupStep::Instantiate => 2,
      WasmSetupStep::PluginInfo => 3,
    }
  }

  pub fn from_u8(value: u8) -> Option<Self> {
    match value {
      0 => Some(WasmSetupStep::Compile),
      1 => Some(WasmSetupStep::Serialize),
      2 => Some(WasmSetupStep::Instantiate),
      3 => Some(WasmSetupStep::PluginInfo),
      _ => None,
    }
  }

  pub fn description(self) -> &'static str {
    match self {
      WasmSetupStep::Compile => "compiling",
      WasmSetupStep::Serialize => "serializing the compiled module",
      WasmSetupStep::Instantiate => "instantiating the module",
      WasmSetupStep::PluginInfo => "getting the plugin info",
    }
  }
}

/// Compiles a Wasm module.
pub fn compile(wasm_bytes: &[u8]) -> Result<CompilationResult> {
  compile_with_steps(wasm_bytes, true, &mut |_| {})
}

/// Compiles a Wasm module, calling `on_step` as each step of the setup starts.
/// `optimize: false` compiles without Cranelift's optimizations, which is a
/// fallback for when an optimized compile stalls.
pub fn compile_with_steps(wasm_bytes: &[u8], optimize: bool, on_step: &mut dyn FnMut(WasmSetupStep)) -> Result<CompilationResult> {
  let wasm_module_creator = if optimize {
    WasmModuleCreator::default()
  } else {
    WasmModuleCreator::new_unoptimized()
  };
  on_step(WasmSetupStep::Compile);
  let module = wasm_module_creator.create_from_wasm_bytes(wasm_bytes)?;

  // cache the serialized native artifact so it can be loaded without recompiling
  on_step(WasmSetupStep::Serialize);
  let bytes: Vec<u8> = match module.inner().serialize() {
    Ok(bytes) => bytes,
    Err(err) => anyhow::bail!("Error serializing wasm module: {:#}", err),
  };

  // load the plugin and get the info
  on_step(WasmSetupStep::Instantiate);
  let linker = create_identity_import_object(module.version(), module.engine())?;
  let mut store = module.new_store(WasmHostState::Empty);
  let instance = load_instance(&mut store, &module, &linker)?;
  let mut instance = create_wasm_plugin_instance(store, instance)?;

  on_step(WasmSetupStep::PluginInfo);
  Ok(CompilationResult {
    bytes,
    plugin_info: instance.plugin_info()?,
  })
}
