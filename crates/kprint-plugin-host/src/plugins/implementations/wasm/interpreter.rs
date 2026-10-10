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
//!
//! A plugin that formats little also formats here, as compiling it would
//! take longer than interpreting what it formats (see `engine_choice.rs`).

use std::sync::Arc;
use std::sync::OnceLock;

use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use kprint_plugin_types::CancellationToken;
use parking_lot::Mutex;
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
use super::instance::WasmHostState;
use super::instance::add_interpreted_host_functions;
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
  wasm_bytes: Arc<[u8]>,
  /// The module for formatting, read from `wasm_bytes` the first time the
  /// plugin formats.
  format_module: Arc<Mutex<Option<Module>>>,
}

impl InterpretedModule {
  /// Reads the module. A function is checked and translated when it first
  /// runs, so this is quick however large the plugin is.
  pub fn new(wasm_bytes: &[u8]) -> Result<Self> {
    let module = Module::new(engine(), wasm_bytes)?;
    let version = plugin_schema_version_from_exports(module.exports().map(|export| export.name()))?;
    Ok(Self {
      module,
      version,
      wasm_bytes: Arc::from(wasm_bytes),
      format_module: Default::default(),
    })
  }

  pub fn version(&self) -> PluginSchemaVersion {
    self.version
  }

  /// The size of the plugin's module in bytes.
  pub fn wasm_len(&self) -> usize {
    self.wasm_bytes.len()
  }

  /// Creates an instance of the plugin for the calls before formatting.
  /// `log` gets what the plugin prints.
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
    create_plugin_instance(
      self.version,
      InterpretedExports {
        store,
        instance,
        memory,
        fuel_per_call: Some(FUEL_PER_CALL),
      },
    )
  }

  /// Creates an instance of the plugin that formats. It runs without a fuel
  /// limit, like native code.
  pub fn instantiate_to_format(&self, host_state: WasmHostState) -> Result<Box<dyn InitializedWasmPluginInstance + Send>> {
    let module = self.format_module()?;
    let mut store = Store::new(format_engine(), host_state);
    let mut linker = Linker::new(format_engine());
    add_interpreted_host_functions(&mut linker, self.version)?;
    let instance = linker
      .instantiate_and_start(&mut store, &module)
      .map_err(|err| anyhow!("Error instantiating module: {:#}", err))?;
    let Some(memory) = instance.get_memory(&store, "memory") else {
      bail!("Could not find memory export in plugin.");
    };
    create_plugin_instance(
      self.version,
      InterpretedExports {
        store,
        instance,
        memory,
        fuel_per_call: None,
      },
    )
  }

  fn format_module(&self) -> Result<Module> {
    let mut format_module = self.format_module.lock();
    if let Some(module) = &*format_module {
      return Ok(module.clone());
    }
    let module = Module::new(format_engine(), &self.wasm_bytes)?;
    *format_module = Some(module.clone());
    Ok(module)
  }
}

/// The engine for the calls before formatting, which stops a call that runs
/// out of fuel.
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

/// The engine for formatting, which doesn't count fuel.
fn format_engine() -> &'static Engine {
  static ENGINE: OnceLock<Engine> = OnceLock::new();
  ENGINE.get_or_init(|| {
    let mut config = wasmi::Config::default();
    // Formatting traverses source trees recursively. Wasmi's default 1,000
    // calls rejects files the native engine handles (ex. 100 nested blocks).
    // Interpreter frames differ from native frames, so give both its call
    // and value stacks bounded headroom beyond the native stack allowance.
    config.set_max_recursion_depth(16 * 1024);
    config.set_max_stack_height(16 * super::load_instance::MAX_WASM_STACK_SIZE);
    config.compilation_mode(CompilationMode::Lazy);
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

/// The data in the store of an interpreted instance.
trait InterpretedHostData: Send + 'static {
  fn set_token(&mut self, token: Arc<dyn CancellationToken>);
}

impl InterpretedHostData for LogFn {
  fn set_token(&mut self, _token: Arc<dyn CancellationToken>) {
    // the instance doesn't format, so nothing checks the token
  }
}

impl InterpretedHostData for WasmHostState {
  fn set_token(&mut self, token: Arc<dyn CancellationToken>) {
    WasmHostState::set_token(self, token);
  }
}

/// An interpreted plugin instance's exports.
struct InterpretedExports<T: InterpretedHostData> {
  store: Store<T>,
  instance: wasmi::Instance,
  memory: Memory,
  /// The fuel every call gets, when the engine counts fuel.
  fuel_per_call: Option<u64>,
}

impl<T: InterpretedHostData> InterpretedExports<T> {
  fn call_with_results(&mut self, name: &str, params: &[u32], results: &mut [Val]) -> Result<()> {
    let Some(func) = self.instance.get_func(&self.store, name) else {
      bail!("Could not find export '{}' in plugin.", name);
    };
    let mut values = [Val::I32(0), Val::I32(0), Val::I32(0)];
    debug_assert_eq!(values.len(), MAX_EXPORT_PARAMS);
    for (value, param) in values.iter_mut().zip(params) {
      *value = Val::I32(*param as i32);
    }
    if let Some(fuel) = self.fuel_per_call {
      // every call gets the same fuel, whatever the calls before it used
      self.store.set_fuel(fuel)?;
    }
    func
      .call(&mut self.store, &values[..params.len()], results)
      .map_err(|err| call_error(&format!("'{}'", name), err))
  }
}

impl<T: InterpretedHostData> PluginExports for InterpretedExports<T> {
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

  fn memory_size(&mut self) -> usize {
    self.memory.data_size(&self.store)
  }

  fn set_token(&mut self, token: Arc<dyn CancellationToken>) {
    self.store.data_mut().set_token(token);
  }
}

#[cfg(test)]
mod test {
  use super::*;

  #[test]
  fn formatting_supports_deeper_call_stacks_than_the_default_interpreter() {
    // (func $recurse (export "recurse") (param i32)
    //   local.get 0 if local.get 0 i32.const 1 i32.sub call $recurse end)
    let bytes = b"\x00\x61\x73\x6d\x01\x00\x00\x00\x01\x05\x01\x60\x01\x7f\x00\x03\x02\x01\x00\x07\x0b\x01\x07\x72\x65\x63\x75\x72\x73\x65\x00\x00\x0a\x10\x01\x0e\x00\x20\x00\x04\x40\x20\x00\x41\x01\x6b\x10\x00\x0b\x0b";
    let engine = format_engine();
    let module = Module::new(engine, &bytes[..]).unwrap();
    let mut store = Store::new(engine, ());
    let instance = Linker::new(engine).instantiate_and_start(&mut store, &module).unwrap();
    let recurse = instance.get_typed_func::<u32, ()>(&store, "recurse").unwrap();
    recurse.call(&mut store, 2_000).unwrap();
    // The larger stack still has a limit and traps rather than growing forever.
    assert_eq!(recurse.call(&mut store, 20_000).unwrap_err().as_trap_code(), Some(TrapCode::StackOverflow));
  }
}
