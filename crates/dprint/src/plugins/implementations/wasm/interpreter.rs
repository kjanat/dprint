//! Runs a Wasm plugin with the wasmi interpreter, without compiling it to
//! native code.
//!
//! Before dprint formats anything, it asks each plugin for its plugin info,
//! its resolved configuration, the files it formats, its configuration
//! diagnostics and its license. Commands like `dprint output-file-paths` never
//! format at all. These calls do little work, so interpreting them takes a
//! few milliseconds. Compiling a plugin to native code takes up to seconds of
//! every core, so dprint only does it once the plugin formats files (see
//! `public.rs`).

use std::sync::Arc;
use std::sync::OnceLock;

use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use dprint_core::plugins::CancellationToken;
use wasmi::CompilationMode;
use wasmi::Engine;
use wasmi::Linker;
use wasmi::Memory;
use wasmi::Module;
use wasmi::Store;
use wasmi::TrapCode;
use wasmi::Val;

use super::instance::InitializedWasmPluginInstance;
use super::instance::LogFn;
use super::instance::MAX_EXPORT_PARAMS;
use super::instance::PluginExports;
use super::instance::PluginSchemaVersion;
use super::instance::create_plugin_instance;
use super::instance::plugin_schema_version_from_exports;
use super::instance::write_output;

/// The most fuel a plugin gets for one call. A Wasm instruction uses about
/// one. Measured with the typescript, json, markdown, toml, dockerfile, yaml,
/// malva and markup plugins, a call uses under 2 million (typescript resolving
/// its configuration uses the most). So this only stops a plugin stuck in a
/// loop, after about a second.
const FUEL_PER_CALL: u64 = 1_000_000_000;

/// A plugin's module, ready to run in the interpreter.
#[derive(Clone)]
pub struct InterpretedModule {
  module: Module,
  version: PluginSchemaVersion,
}

impl InterpretedModule {
  /// Reads the module. A function is checked and translated when it first
  /// runs, so this is quick however large the plugin is.
  pub fn new(wasm_bytes: &[u8]) -> Result<Self> {
    let module = Module::new(engine(), wasm_bytes)?;
    let version = plugin_schema_version_from_exports(module.exports().map(|export| export.name()))?;
    Ok(Self { module, version })
  }

  /// Creates an instance of the plugin. `log` gets what the plugin prints.
  ///
  /// The instance can't format: the functions the plugin imports to format
  /// with other plugins do nothing.
  pub fn instantiate(&self, log: LogFn) -> Result<Box<dyn InitializedWasmPluginInstance + Send>> {
    let mut store = Store::new(engine(), log);
    let mut linker = Linker::new(engine());
    match self.version {
      PluginSchemaVersion::V3 => add_v3_imports(&mut linker)?,
      PluginSchemaVersion::V4 => add_v4_imports(&mut linker)?,
    }
    // instantiating runs the module's start function
    store.set_fuel(FUEL_PER_CALL)?;
    let instance = linker
      .instantiate_and_start(&mut store, &self.module)
      .map_err(|err| anyhow!("Error instantiating module: {:#}", call_error("its start function", err)))?;
    let Some(memory) = instance.get_memory(&store, "memory") else {
      bail!("Could not find memory export in plugin.");
    };
    create_plugin_instance(self.version, InterpretedExports { store, instance, memory })
  }
}

fn engine() -> &'static Engine {
  static ENGINE: OnceLock<Engine> = OnceLock::new();
  ENGINE.get_or_init(|| {
    let mut config = wasmi::Config::default();
    // the calls run a small part of a plugin, so only that part is checked
    // and translated
    config.compilation_mode(CompilationMode::Lazy);
    config.consume_fuel(true);
    Engine::new(&config)
  })
}

/// The error for a call that failed, which says when the plugin ran out of fuel.
fn call_error(name: &str, err: wasmi::Error) -> anyhow::Error {
  match err.as_trap_code() {
    Some(TrapCode::OutOfFuel) => anyhow!("Stopped the plugin's {}, which ran {} instructions without finishing.", name, FUEL_PER_CALL),
    _ => anyhow!(err),
  }
}

fn add_v3_imports(linker: &mut Linker<LogFn>) -> Result<()> {
  linker.func_wrap("dprint", "host_clear_bytes", |_: u32| {})?;
  linker.func_wrap("dprint", "host_read_buffer", |_: u32, _: u32| {})?;
  linker.func_wrap("dprint", "host_write_buffer", |_: u32, _: u32, _: u32| {})?;
  linker.func_wrap("dprint", "host_take_override_config", || {})?;
  linker.func_wrap("dprint", "host_take_file_path", || {})?;
  linker.func_wrap("dprint", "host_format", || -> u32 { 0 })?; // no change
  linker.func_wrap("dprint", "host_get_formatted_text", || -> u32 { 0 })?; // zero length
  linker.func_wrap("dprint", "host_get_error_text", || -> u32 { 0 })?; // zero length
  Ok(())
}

fn add_v4_imports(linker: &mut Linker<LogFn>) -> Result<()> {
  linker.func_wrap(
    "env",
    "fd_write",
    |mut caller: wasmi::Caller<'_, LogFn>, fd: u32, iovs_ptr: u32, iovs_len: u32, nwritten_ptr: u32| -> u32 {
      let Some(memory) = caller.get_export("memory").and_then(|export| export.into_memory()) else {
        return 1;
      };
      let (memory, log) = memory.data_and_store_mut(&mut caller);
      write_output(memory, log, fd, iovs_ptr, iovs_len, nwritten_ptr)
    },
  )?;
  linker.func_wrap("dprint", "host_write_buffer", |_: u32| {})?;
  linker.func_wrap(
    "dprint",
    "host_format",
    |_: u32, _: u32, _: u32, _: u32, _: u32, _: u32, _: u32, _: u32| -> u32 { 0 },
  )?; // no change
  linker.func_wrap("dprint", "host_get_formatted_text", || -> u32 { 0 })?; // zero length
  linker.func_wrap("dprint", "host_get_error_text", || -> u32 { 0 })?; // zero length
  linker.func_wrap("dprint", "host_has_cancelled", || -> i32 { 0 })?; // false
  Ok(())
}

/// An interpreted plugin instance's exports.
struct InterpretedExports {
  store: Store<LogFn>,
  instance: wasmi::Instance,
  memory: Memory,
}

impl InterpretedExports {
  fn call_with_results(&mut self, name: &str, params: &[u32], results: &mut [Val]) -> Result<()> {
    let Some(func) = self.instance.get_func(&self.store, name) else {
      bail!("Could not find export '{}' in plugin.", name);
    };
    let mut values = [Val::I32(0), Val::I32(0), Val::I32(0)];
    debug_assert_eq!(values.len(), MAX_EXPORT_PARAMS);
    for (value, param) in values.iter_mut().zip(params) {
      *value = Val::I32(*param as i32);
    }
    // every call gets the same fuel, whatever the calls before it used
    self.store.set_fuel(FUEL_PER_CALL)?;
    func
      .call(&mut self.store, &values[..params.len()], results)
      .map_err(|err| call_error(&format!("'{}'", name), err))
  }
}

impl PluginExports for InterpretedExports {
  fn has_function(&mut self, name: &str) -> bool {
    self.instance.get_func(&self.store, name).is_some()
  }

  fn call(&mut self, name: &str, params: &[u32]) -> Result<()> {
    self.call_with_results(name, params, &mut [])
  }

  fn call_u32(&mut self, name: &str, params: &[u32]) -> Result<u32> {
    let mut results = [Val::I32(0)];
    self.call_with_results(name, params, &mut results)?;
    match results[0].i32() {
      Some(value) => Ok(value as u32),
      None => bail!("Expected export '{}' to return an i32.", name),
    }
  }

  fn read_memory(&mut self, offset: usize, bytes: &mut [u8]) -> Result<()> {
    Ok(self.memory.read(&self.store, offset, bytes)?)
  }

  fn write_memory(&mut self, offset: usize, bytes: &[u8]) -> Result<()> {
    Ok(self.memory.write(&mut self.store, offset, bytes)?)
  }

  fn set_token(&mut self, _token: Arc<dyn CancellationToken>) {
    // the instance doesn't format, so nothing checks the token
  }
}
