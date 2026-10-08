use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use anyhow::bail;
use dprint_configuration::ConfigKeyMap;
use dprint_configuration::ConfigurationDiagnostic;
use dprint_plugin_types::CancellationToken;
use dprint_plugin_types::CheckConfigUpdatesMessage;
use dprint_plugin_types::ConfigChange;
use dprint_plugin_types::FileMatchingInfo;
use dprint_plugin_types::FormatRange;
use dprint_plugin_types::FormatResult;
use dprint_plugin_types::HostFormatRequest;
use dprint_plugin_types::PluginInfo;
use dprint_wasm_plugin::PLUGIN_SYSTEM_SCHEMA_VERSION;
use wasmtime::Engine;
use wasmtime::Memory;

use crate::plugins::FormatConfig;

use super::WasmInstance;

mod v3;
mod v4;

pub type WasmHostFormatSender = tokio::sync::mpsc::UnboundedSender<(HostFormatRequest, std::sync::mpsc::Sender<FormatResult>)>;

/// The data in the store of an instance that formats, on either engine.
/// The functions the plugin imports reach it through their `Caller`, and
/// the instance's memory through its "memory" export. A store only ever
/// runs one schema version, so the functions match on their variant.
pub enum WasmHostState {
  /// Used for the identity import object (compilation / plugin-info probing)
  /// where the host functions are no-ops, so no state is needed.
  Empty,
  V3(v3::ImportObjectEnvironmentV3),
  V4(v4::ImportObjectEnvironmentV4),
}

impl WasmHostState {
  pub fn set_token(&mut self, token: Arc<dyn CancellationToken>) {
    match self {
      WasmHostState::Empty => {}
      WasmHostState::V3(state) => state.token = token,
      WasmHostState::V4(state) => state.token = token,
    }
  }
}

/// The host state for an instance that formats with the plugins in the
/// plugin pool.
pub fn create_host_state(version: PluginSchemaVersion, log: LogFn, host_format_sender: WasmHostFormatSender) -> WasmHostState {
  match version {
    PluginSchemaVersion::V3 => WasmHostState::V3(v3::ImportObjectEnvironmentV3::new(host_format_sender)),
    PluginSchemaVersion::V4 => WasmHostState::V4(v4::ImportObjectEnvironmentV4::new(log, host_format_sender)),
  }
}

/// The range of `len` bytes at `offset`, if it's in the memory.
fn memory_range(memory: &[u8], offset: usize, len: usize) -> Option<std::ops::Range<usize>> {
  let end = offset.checked_add(len)?;
  (end <= memory.len()).then_some(offset..end)
}

/// Like `memory_range`, with an error for a range outside the memory.
fn checked_range(memory: &[u8], offset: u32, len: usize) -> Result<std::ops::Range<usize>, String> {
  memory_range(memory, offset as usize, len).ok_or_else(|| format!("{} bytes at {} are outside the plugin's memory.", len, offset))
}

/// Defines `$name`, which adds the functions a plugin imports to format to
/// a linker of `$engine` (wasmtime or wasmi). `$error` makes the engine's
/// error, which traps the plugin, from a message.
macro_rules! define_add_host_functions {
  ($name:ident, $engine:ident, $error:expr) => {
    pub fn $name(linker: &mut $engine::Linker<WasmHostState>, version: PluginSchemaVersion) -> Result<()> {
      use $engine::Caller;
      type HostResult<T> = std::result::Result<T, $engine::Error>;

      fn trap(message: String) -> $engine::Error {
        $error(message)
      }

      fn memory_and_state<'a>(caller: &'a mut Caller<'_, WasmHostState>) -> HostResult<(&'a mut [u8], &'a mut WasmHostState)> {
        match caller.get_export("memory").and_then(|export| export.into_memory()) {
          Some(memory) => Ok(memory.data_and_store_mut(caller)),
          None => Err(trap("Could not find memory export in plugin.".to_string())),
        }
      }

      fn v3_state(state: &mut WasmHostState) -> HostResult<&mut v3::ImportObjectEnvironmentV3> {
        match state {
          WasmHostState::V3(state) => Ok(state),
          _ => Err(trap("Expected v3 host state.".to_string())),
        }
      }

      fn v4_state(state: &mut WasmHostState) -> HostResult<&mut v4::ImportObjectEnvironmentV4> {
        match state {
          WasmHostState::V4(state) => Ok(state),
          _ => Err(trap("Expected v4 host state.".to_string())),
        }
      }

      match version {
        PluginSchemaVersion::V3 => {
          linker.func_wrap(
            "dprint",
            "host_clear_bytes",
            |mut caller: Caller<'_, WasmHostState>, length: u32| -> HostResult<()> {
              v3::host_clear_bytes(v3_state(caller.data_mut())?, length);
              Ok(())
            },
          )?;
          linker.func_wrap(
            "dprint",
            "host_read_buffer",
            |mut caller: Caller<'_, WasmHostState>, buffer_pointer: u32, length: u32| -> HostResult<()> {
              let (memory, state) = memory_and_state(&mut caller)?;
              v3::host_read_buffer(memory, v3_state(state)?, buffer_pointer, length).map_err(trap)
            },
          )?;
          linker.func_wrap(
            "dprint",
            "host_write_buffer",
            |mut caller: Caller<'_, WasmHostState>, buffer_pointer: u32, offset: u32, length: u32| -> HostResult<()> {
              let (memory, state) = memory_and_state(&mut caller)?;
              v3::host_write_buffer(memory, v3_state(state)?, buffer_pointer, offset, length).map_err(trap)
            },
          )?;
          linker.func_wrap(
            "dprint",
            "host_take_override_config",
            |mut caller: Caller<'_, WasmHostState>| -> HostResult<()> {
              v3::host_take_override_config(v3_state(caller.data_mut())?);
              Ok(())
            },
          )?;
          linker.func_wrap("dprint", "host_take_file_path", |mut caller: Caller<'_, WasmHostState>| -> HostResult<()> {
            v3::host_take_file_path(v3_state(caller.data_mut())?).map_err(trap)
          })?;
          linker.func_wrap("dprint", "host_format", |mut caller: Caller<'_, WasmHostState>| -> HostResult<u32> {
            v3::host_format(v3_state(caller.data_mut())?).map_err(trap)
          })?;
          linker.func_wrap(
            "dprint",
            "host_get_formatted_text",
            |mut caller: Caller<'_, WasmHostState>| -> HostResult<u32> { Ok(v3::host_get_formatted_text(v3_state(caller.data_mut())?)) },
          )?;
          linker.func_wrap("dprint", "host_get_error_text", |mut caller: Caller<'_, WasmHostState>| -> HostResult<u32> {
            Ok(v3::host_get_error_text(v3_state(caller.data_mut())?))
          })?;
        }
        PluginSchemaVersion::V4 => {
          linker.func_wrap(
            "env",
            "fd_write",
            |mut caller: Caller<'_, WasmHostState>, fd: u32, iovs_ptr: u32, iovs_len: u32, nwritten_ptr: u32| -> HostResult<u32> {
              let (memory, state) = memory_and_state(&mut caller)?;
              Ok(v4::fd_write(memory, v4_state(state)?, fd, iovs_ptr, iovs_len, nwritten_ptr))
            },
          )?;
          linker.func_wrap(
            "dprint",
            "host_write_buffer",
            |mut caller: Caller<'_, WasmHostState>, buffer_pointer: u32| -> HostResult<()> {
              let (memory, state) = memory_and_state(&mut caller)?;
              v4::host_write_buffer(memory, v4_state(state)?, buffer_pointer).map_err(trap)
            },
          )?;
          linker.func_wrap(
            "dprint",
            "host_format",
            |mut caller: Caller<'_, WasmHostState>,
             file_path_ptr: u32,
             file_path_len: u32,
             range_start: u32,
             range_end: u32,
             override_cfg_ptr: u32,
             override_cfg_len: u32,
             file_bytes_ptr: u32,
             file_bytes_len: u32|
             -> HostResult<u32> {
              let (memory, state) = memory_and_state(&mut caller)?;
              v4::host_format(
                memory,
                v4_state(state)?,
                file_path_ptr,
                file_path_len,
                range_start,
                range_end,
                override_cfg_ptr,
                override_cfg_len,
                file_bytes_ptr,
                file_bytes_len,
              )
              .map_err(trap)
            },
          )?;
          linker.func_wrap(
            "dprint",
            "host_get_formatted_text",
            |mut caller: Caller<'_, WasmHostState>| -> HostResult<u32> { Ok(v4::host_get_formatted_text(v4_state(caller.data_mut())?)) },
          )?;
          linker.func_wrap("dprint", "host_get_error_text", |mut caller: Caller<'_, WasmHostState>| -> HostResult<u32> {
            Ok(v4::host_get_error_text(v4_state(caller.data_mut())?))
          })?;
          linker.func_wrap("dprint", "host_has_cancelled", |mut caller: Caller<'_, WasmHostState>| -> HostResult<i32> {
            Ok(v4::host_has_cancelled(v4_state(caller.data_mut())?))
          })?;
        }
      }
      Ok(())
    }
  };
}

define_add_host_functions!(add_native_host_functions, wasmtime, wasmtime::Error::msg);
define_add_host_functions!(add_interpreted_host_functions, wasmi, wasmi::Error::new);

pub type Store = wasmtime::Store<WasmHostState>;
pub type Linker = wasmtime::Linker<WasmHostState>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PluginSchemaVersion {
  V3,
  V4,
}

pub trait InitializedWasmPluginInstance {
  fn plugin_info(&mut self) -> Result<PluginInfo>;
  fn license_text(&mut self) -> Result<String>;
  fn resolved_config(&mut self, config: &FormatConfig) -> Result<String>;
  fn config_diagnostics(&mut self, config: &FormatConfig) -> Result<Vec<ConfigurationDiagnostic>>;
  fn file_matching_info(&mut self, config: &FormatConfig) -> Result<FileMatchingInfo>;
  fn check_config_updates(&mut self, message: &CheckConfigUpdatesMessage) -> Result<Vec<ConfigChange>>;
  fn format_text(
    &mut self,
    file_path: &Path,
    file_bytes: &[u8],
    range: FormatRange,
    config: &FormatConfig,
    override_config: &ConfigKeyMap,
    token: Arc<dyn CancellationToken>,
  ) -> FormatResult;
}

/// A plugin instance's exported functions and memory. The plugin protocol
/// (see `v3.rs` and `v4.rs`) runs on top of this, so it works the same on
/// every engine that runs plugins: wasmtime for native code, and wasmi to
/// interpret a plugin that isn't compiled (see `interpreter.rs`).
///
/// The protocol's exported functions only take and return `u32`s.
pub trait PluginExports {
  fn has_function(&mut self, name: &str) -> bool;
  /// Calls an exported function that returns nothing.
  fn call(&mut self, name: &str, params: &[u32]) -> Result<()>;
  /// Calls an exported function that returns a `u32`.
  fn call_u32(&mut self, name: &str, params: &[u32]) -> Result<u32>;
  fn read_memory(&mut self, offset: usize, bytes: &mut [u8]) -> Result<()>;
  fn write_memory(&mut self, offset: usize, bytes: &[u8]) -> Result<()>;
  /// The token the host functions check while the plugin formats.
  fn set_token(&mut self, token: Arc<dyn CancellationToken>);
}

/// The most parameters an exported function of the protocol takes.
pub const MAX_EXPORT_PARAMS: usize = 3;

/// Creates the protocol for a plugin's schema version on top of its exports.
pub fn create_plugin_instance<TExports: PluginExports + Send + 'static>(
  version: PluginSchemaVersion,
  exports: TExports,
) -> Result<Box<dyn InitializedWasmPluginInstance + Send>> {
  match version {
    PluginSchemaVersion::V3 => Ok(Box::new(v3::InitializedWasmPluginInstanceV3::new(exports)?)),
    PluginSchemaVersion::V4 => Ok(Box::new(v4::InitializedWasmPluginInstanceV4::new(exports))),
  }
}

/// A native plugin instance's exports, run by wasmtime.
struct NativeExports {
  store: Store,
  instance: WasmInstance,
  memory: Memory,
}

impl NativeExports {
  fn function(&mut self, name: &str) -> Result<wasmtime::Func> {
    match self.instance.get_function(&mut self.store, name) {
      Some(func) => Ok(func),
      None => bail!("Could not find export '{}' in plugin.", name),
    }
  }

  fn call_with_results(&mut self, name: &str, params: &[u32], results: &mut [wasmtime::Val]) -> Result<()> {
    let func = self.function(name)?;
    let mut values = [wasmtime::Val::I32(0); MAX_EXPORT_PARAMS];
    for (value, param) in values.iter_mut().zip(params) {
      *value = wasmtime::Val::I32(*param as i32);
    }
    Ok(func.call(&mut self.store, &values[..params.len()], results)?)
  }
}

impl PluginExports for NativeExports {
  fn has_function(&mut self, name: &str) -> bool {
    self.instance.get_function(&mut self.store, name).is_some()
  }

  fn call(&mut self, name: &str, params: &[u32]) -> Result<()> {
    self.call_with_results(name, params, &mut [])
  }

  fn call_u32(&mut self, name: &str, params: &[u32]) -> Result<u32> {
    let mut results = [wasmtime::Val::I32(0)];
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

  fn set_token(&mut self, token: Arc<dyn CancellationToken>) {
    self.store.data_mut().set_token(token);
  }
}

pub fn create_wasm_plugin_instance(mut store: Store, instance: WasmInstance) -> Result<Box<dyn InitializedWasmPluginInstance + Send>> {
  let memory = instance
    .get_memory(&mut store, "memory")
    .ok_or_else(|| anyhow::anyhow!("Could not find memory export in plugin."))?;
  let version = instance.version();
  create_plugin_instance(version, NativeExports { store, instance, memory })
}

/// Builds a linker whose host functions are no-ops. Used when the plugin doesn't
/// need to format via a plugin pool (compilation and plugin-info probing).
pub fn create_identity_import_object(version: PluginSchemaVersion, engine: &Engine) -> Result<Linker> {
  let mut linker = Linker::new(engine);
  match version {
    PluginSchemaVersion::V3 => v3::add_identity_imports(&mut linker)?,
    PluginSchemaVersion::V4 => v4::add_identity_imports(&mut linker)?,
  }
  Ok(linker)
}

/// Builds a wasmtime linker plus the initial store state for an instance
/// that formats text using plugins from the plugin pool.
pub fn create_pools_import_object(
  log: LogFn,
  version: PluginSchemaVersion,
  engine: &Engine,
  host_format_sender: WasmHostFormatSender,
) -> Result<(Linker, WasmHostState)> {
  let mut linker = Linker::new(engine);
  add_native_host_functions(&mut linker, version)?;
  Ok((linker, create_host_state(version, log, host_format_sender)))
}

pub use v4::LogFn;
pub use v4::write_output;

pub fn get_current_plugin_schema_version(module: &wasmtime::Module) -> Result<PluginSchemaVersion> {
  plugin_schema_version_from_exports(module.exports().map(|export| export.name()))
}

/// The plugin schema version a module's exports say it has.
pub fn plugin_schema_version_from_exports<'a>(export_names: impl Iterator<Item = &'a str>) -> Result<PluginSchemaVersion> {
  fn from_exports<'a>(export_names: impl Iterator<Item = &'a str>) -> Result<u32> {
    for name in export_names {
      if matches!(name, "get_plugin_schema_version") {
        // not exactly correct, but practically ok because this has been returning v3 for many years
        return Ok(3);
      } else if let Some(version) = name.strip_prefix("dprint_plugin_version_") {
        // this is what dprint will use in the future
        if let Ok(version) = version.parse() {
          return Ok(version);
        }
      }
    }
    bail!("Error determining plugin schema version. Are you sure this is a dprint plugin? If so, maybe try upgrading dprint.");
  }

  let plugin_schema_version = from_exports(export_names)?;
  match plugin_schema_version {
    3 => Ok(PluginSchemaVersion::V3),
    4 => Ok(PluginSchemaVersion::V4),
    version if version > 4 => {
      bail!(
        "Invalid schema version: {} -- Expected: {}. Upgrade your dprint CLI ({}).",
        plugin_schema_version,
        PLUGIN_SYSTEM_SCHEMA_VERSION,
        get_current_exe_display(),
      );
    }
    plugin_schema_version => {
      bail!(
        "Invalid schema version: {} -- Expected: {}. This plugin is too old for your version of dprint ({}). Please update the plugin manually.",
        plugin_schema_version,
        PLUGIN_SYSTEM_SCHEMA_VERSION,
        get_current_exe_display(),
      );
    }
  }
}

fn get_current_exe_display() -> String {
  std::env::current_exe()
    .ok()
    .map(|p| p.display().to_string())
    .unwrap_or_else(|| "<unknown path>".to_string())
}
