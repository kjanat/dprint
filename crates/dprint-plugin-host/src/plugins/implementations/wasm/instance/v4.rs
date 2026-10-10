use std::collections::HashSet;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use dprint_configuration::ConfigKeyMap;
use dprint_configuration::ConfigurationDiagnostic;
use dprint_configuration::GlobalConfiguration;
use dprint_plugin_types::CancellationToken;
use dprint_plugin_types::CheckConfigUpdatesMessage;
use dprint_plugin_types::ConfigChange;
use dprint_plugin_types::CriticalFormatError;
use dprint_plugin_types::FILE_MATCHING_INFO_ERROR_MESSAGE;
use dprint_plugin_types::FileMatchingInfo;
use dprint_plugin_types::FormatConfigId;
use dprint_plugin_types::FormatError;
use dprint_plugin_types::FormatRange;
use dprint_plugin_types::FormatResult;
use dprint_plugin_types::HostFormatRequest;
use dprint_plugin_types::NullCancellationToken;
use dprint_plugin_types::PluginInfo;
use dprint_wasm_plugin::JsonResponse;

use crate::plugins::FormatConfig;
use crate::plugins::implementations::wasm::WasmHostFormatSender;

use super::InitializedWasmPluginInstance;
use super::Linker;
use super::PluginExports;
use super::checked_range;
use super::memory_range;

enum WasmFormatResult {
  NoChange,
  Change,
  Error,
}

impl TryFrom<u32> for WasmFormatResult {
  type Error = anyhow::Error;

  fn try_from(value: u32) -> Result<Self> {
    match value {
      0 => Ok(WasmFormatResult::NoChange),
      1 => Ok(WasmFormatResult::Change),
      2 => Ok(WasmFormatResult::Error),
      _ => Err(anyhow!("Plugin returned the format result {}. Expected 0, 1 or 2.", value)),
    }
  }
}

/// A callback that logs plugin stderr output. Kept as a boxed callback rather
/// than storing the whole `Environment` + plugin name in the host state, which
/// would make the store data generic over the environment type.
pub type LogFn = Arc<dyn Fn(&str) + Send + Sync>;

/// The host state for a v4 plugin, kept in the store of the engine that
/// runs it (see `WasmHostState`).
pub struct ImportObjectEnvironmentV4 {
  pub token: Arc<dyn CancellationToken>,
  log: LogFn,
  formatted_text_store: Vec<u8>,
  shared_bytes: Vec<u8>,
  error_text_store: String,
  host_format_sender: WasmHostFormatSender,
}

impl ImportObjectEnvironmentV4 {
  pub fn new(log: LogFn, host_format_sender: WasmHostFormatSender) -> Self {
    Self {
      token: Arc::new(NullCancellationToken),
      log,
      formatted_text_store: Default::default(),
      shared_bytes: Default::default(),
      error_text_store: Default::default(),
      host_format_sender,
    }
  }
}

pub fn add_identity_imports(linker: &mut Linker) -> Result<()> {
  linker.func_wrap("env", "fd_write", |_: u32, _: u32, _: u32, _: u32| -> u32 { 0 })?; // ignore
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

/// The `fd_write` import: logs what the plugin writes to stdout or stderr.
/// Returns the WASI error code, which is 0 on success.
pub fn write_output(memory: &mut [u8], log: &LogFn, fd: u32, iovs_ptr: u32, iovs_len: u32, nwritten_ptr: u32) -> u32 {
  if !matches!(fd, 1 | 2) {
    return 1; // unsupported fd
  }
  let mut total_written: u32 = 0;
  for i in 0..iovs_len as usize {
    let Some(iovec) = memory_range(memory, (iovs_ptr as usize).saturating_add(i.saturating_mul(8)), 8) else {
      return 1;
    };
    let iovec = &memory[iovec];
    let buf_addr = u32::from_le_bytes(iovec[0..4].try_into().unwrap());
    let buf_len = u32::from_le_bytes(iovec[4..8].try_into().unwrap());
    let Some(buf) = memory_range(memory, buf_addr as usize, buf_len as usize) else {
      return 1;
    };
    log(&String::from_utf8_lossy(&memory[buf]));
    total_written = total_written.saturating_add(buf_len);
  }

  let Some(nwritten) = memory_range(memory, nwritten_ptr as usize, 4) else {
    return 1;
  };
  memory[nwritten].copy_from_slice(&total_written.to_le_bytes());
  0
}

pub fn fd_write(memory: &mut [u8], state: &mut ImportObjectEnvironmentV4, fd: u32, iovs_ptr: u32, iovs_len: u32, nwritten_ptr: u32) -> u32 {
  write_output(memory, &state.log, fd, iovs_ptr, iovs_len, nwritten_ptr)
}

pub fn host_write_buffer(memory: &mut [u8], state: &mut ImportObjectEnvironmentV4, buffer_pointer: u32) -> Result<(), String> {
  let range = checked_range(memory, buffer_pointer, state.shared_bytes.len())?;
  memory[range].copy_from_slice(&state.shared_bytes);
  Ok(())
}

#[allow(clippy::too_many_arguments)]
pub fn host_format(
  memory: &[u8],
  state: &mut ImportObjectEnvironmentV4,
  file_path_ptr: u32,
  file_path_len: u32,
  range_start: u32,
  range_end: u32,
  override_cfg_ptr: u32,
  override_cfg_len: u32,
  file_bytes_ptr: u32,
  file_bytes_len: u32,
) -> Result<u32, String> {
  let override_config = if override_cfg_len == 0 {
    ConfigKeyMap::default()
  } else {
    let bytes = &memory[checked_range(memory, override_cfg_ptr, override_cfg_len as usize)?];
    serde_json::from_slice::<ConfigKeyMap>(bytes).map_err(|err| format!("Invalid override configuration: {err}"))?
  };
  let file_path = {
    let bytes = &memory[checked_range(memory, file_path_ptr, file_path_len as usize)?];
    PathBuf::from(String::from_utf8(bytes.to_vec()).map_err(|err| format!("Invalid file path: {err}"))?)
  };
  let file_bytes = memory[checked_range(memory, file_bytes_ptr, file_bytes_len as usize)?].to_vec();
  let range = if range_start == 0 && range_end == file_bytes_len {
    None
  } else {
    Some(range_start as usize..range_end as usize)
  };
  let request = HostFormatRequest {
    file_path,
    file_bytes,
    range,
    override_config,
    token: state.token.clone(),
  };
  // todo: worth it to use a oneshot channel library here?
  let (tx, rx) = std::sync::mpsc::channel();
  let result = match state.host_format_sender.send((request, tx)) {
    Ok(()) => match rx.recv() {
      Ok(result) => result,
      Err(_) => Ok(None), // receive error
    },
    Err(_) => Ok(None), // send error
  };

  Ok(match result {
    Ok(Some(formatted_text)) => {
      state.formatted_text_store = formatted_text;
      1 // change
    }
    Ok(None) => {
      0 // no change
    }
    // ignore critical error as we can just continue formatting
    Err(err) => {
      state.error_text_store = err.to_string();
      2 // error
    }
  })
}

pub fn host_get_formatted_text(state: &mut ImportObjectEnvironmentV4) -> u32 {
  let formatted_bytes = std::mem::take(&mut state.formatted_text_store);
  let len = formatted_bytes.len();
  state.shared_bytes = formatted_bytes;
  len as u32
}

pub fn host_get_error_text(state: &mut ImportObjectEnvironmentV4) -> u32 {
  let error_text = std::mem::take(&mut state.error_text_store);
  let len = error_text.len();
  state.shared_bytes = error_text.into_bytes();
  len as u32
}

pub fn host_has_cancelled(state: &mut ImportObjectEnvironmentV4) -> i32 {
  if state.token.as_ref().is_cancelled() { 1 } else { 0 }
}

pub struct InitializedWasmPluginInstanceV4<TExports: PluginExports> {
  wasm_functions: WasmFunctions<TExports>,
  registered_config_ids: HashSet<FormatConfigId>,
}

impl<TExports: PluginExports> InitializedWasmPluginInstanceV4<TExports> {
  pub fn new(exports: TExports) -> Self {
    Self {
      wasm_functions: WasmFunctions { exports },
      registered_config_ids: HashSet::new(),
    }
  }

  fn register_config(&mut self, config: &FormatConfig) -> Result<()> {
    #[derive(serde::Serialize)]
    struct RawFormatConfig<'a> {
      pub plugin: &'a ConfigKeyMap,
      pub global: &'a GlobalConfiguration,
    }

    let json = serde_json::to_string(&RawFormatConfig {
      plugin: &config.plugin,
      global: &config.global,
    })?;
    self.send_string(&json)?;
    self.wasm_functions.register_config(config.id)?;
    Ok(())
  }

  fn inner_format_text(
    &mut self,
    file_path: &Path,
    file_bytes: &[u8],
    range: FormatRange,
    config: &FormatConfig,
    override_config: Option<&str>,
  ) -> Result<FormatResult> {
    self.inner_setup_formatting(file_path, file_bytes, override_config)?;
    let response_code = match range {
      Some(range) => self.wasm_functions.format_range(config.id, range)?,
      None => self.wasm_functions.format(config.id)?,
    };
    self.inner_handle_response(response_code)
  }

  fn inner_setup_formatting(&mut self, file_path: &Path, file_bytes: &[u8], override_config: Option<&str>) -> Result<()> {
    // send override config if necessary
    if let Some(override_config) = override_config {
      self.send_string(override_config)?;
      self.wasm_functions.set_override_config()?;
    }

    // send file path
    self.send_string(&file_path.to_string_lossy())?;
    self.wasm_functions.set_file_path()?;

    // send file text
    self.send_bytes(file_bytes)
  }

  fn inner_handle_response(&mut self, response_code: WasmFormatResult) -> Result<FormatResult> {
    match response_code {
      WasmFormatResult::NoChange => Ok(Ok(None)),
      WasmFormatResult::Change => {
        let len = self.wasm_functions.get_formatted_text()?;
        let text_bytes = self.receive_bytes(len)?;
        Ok(Ok(Some(text_bytes)))
      }
      WasmFormatResult::Error => {
        let len = self.wasm_functions.get_error_text()?;
        let text = self.receive_string(len)?;
        Ok(Err(FormatError::new(text)))
      }
    }
  }

  fn ensure_config(&mut self, config: &FormatConfig) -> Result<()> {
    if !self.registered_config_ids.contains(&config.id) {
      // update the plugin
      self.register_config(config)?;
      // now mark this as successfully set
      self.registered_config_ids.insert(config.id);
    }
    Ok(())
  }

  /* LOW LEVEL SENDING AND RECEIVING */

  // These methods should panic when failing because that may indicate
  // a major problem where the CLI is out of sync with the plugin.

  fn send_string(&mut self, text: &str) -> Result<()> {
    self.send_bytes(text.as_bytes())
  }

  fn send_bytes(&mut self, bytes: &[u8]) -> Result<()> {
    let shared_bytes_ptr = self.wasm_functions.clear_shared_bytes(bytes.len())?;
    self.wasm_functions.write_memory(shared_bytes_ptr as usize, bytes)?;
    Ok(())
  }

  fn receive_string(&mut self, len: usize) -> Result<String> {
    let bytes = self.receive_bytes(len)?;
    Ok(String::from_utf8(bytes)?)
  }

  fn receive_bytes(&mut self, len: usize) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(len)?;
    bytes.resize(len, 0);
    self.read_bytes_from_shared_bytes(&mut bytes)?;
    Ok(bytes)
  }

  fn read_bytes_from_shared_bytes(&mut self, bytes: &mut [u8]) -> Result<()> {
    let wasm_buffer_pointer = self.wasm_functions.get_shared_bytes_ptr()?;
    self.wasm_functions.read_memory(wasm_buffer_pointer as usize, bytes)?;
    Ok(())
  }
}

impl<TExports: PluginExports> InitializedWasmPluginInstance for InitializedWasmPluginInstanceV4<TExports> {
  fn plugin_info(&mut self) -> Result<PluginInfo> {
    let len = self.wasm_functions.get_plugin_info()?;
    let json_bytes = self.receive_bytes(len)?;
    Ok(serde_json::from_slice(&json_bytes)?)
  }

  fn license_text(&mut self) -> Result<String> {
    let len = self.wasm_functions.get_license_text()?;
    self.receive_string(len)
  }

  fn check_config_updates(&mut self, message: &CheckConfigUpdatesMessage) -> Result<Vec<ConfigChange>> {
    let bytes = serde_json::to_vec(&message)?;
    self.send_bytes(&bytes)?;
    let Some(len) = self.wasm_functions.check_config_updates()? else {
      return Ok(Vec::new());
    };
    let bytes = self.receive_bytes(len)?;
    let result: JsonResponse = serde_json::from_slice(&bytes)?;
    match result {
      JsonResponse::Ok(value) => Ok(serde_json::from_value(value)?),
      JsonResponse::Err(err) => Err(anyhow!("{}", err)),
    }
  }

  fn resolved_config(&mut self, config: &FormatConfig) -> Result<String> {
    self.ensure_config(config)?;
    let len = self.wasm_functions.get_resolved_config(config.id)?;
    self.receive_string(len)
  }

  fn config_diagnostics(&mut self, config: &FormatConfig) -> Result<Vec<ConfigurationDiagnostic>> {
    self.ensure_config(config)?;
    let len = self.wasm_functions.get_config_diagnostics(config.id)?;
    let json_text = self.receive_string(len)?;
    Ok(serde_json::from_str(&json_text)?)
  }

  fn file_matching_info(&mut self, config: &FormatConfig) -> Result<FileMatchingInfo> {
    self.ensure_config(config)?;
    let len = self.wasm_functions.get_config_file_matching(config.id)?;
    let json_text = self.receive_string(len)?;
    serde_json::from_str(&json_text).with_context(|| FILE_MATCHING_INFO_ERROR_MESSAGE)
  }

  fn format_text(
    &mut self,
    file_path: &Path,
    file_bytes: &[u8],
    range: FormatRange,
    config: &FormatConfig,
    override_config: &ConfigKeyMap,
    token: Arc<dyn CancellationToken>,
  ) -> FormatResult {
    let override_config = if !override_config.is_empty() {
      Some(serde_json::to_string(override_config)?)
    } else {
      None
    };
    self.wasm_functions.exports.set_token(token);
    self.ensure_config(config).map_err(FormatError::new)?;
    match self.inner_format_text(file_path, file_bytes, range, config, override_config.as_deref()) {
      Ok(inner) => inner,
      Err(err) => Err(CriticalFormatError(FormatError::new(err)).into()),
    }
  }
}

struct WasmFunctions<TExports: PluginExports> {
  exports: TExports,
}

impl<TExports: PluginExports> WasmFunctions<TExports> {
  #[inline]
  pub fn register_config(&mut self, config_id: FormatConfigId) -> Result<()> {
    self.exports.call("register_config", &[config_id.as_raw()])
  }

  #[inline]
  pub fn get_plugin_info(&mut self) -> Result<usize> {
    self.call_len("get_plugin_info", &[])
  }

  #[inline]
  pub fn get_license_text(&mut self) -> Result<usize> {
    self.call_len("get_license_text", &[])
  }

  #[inline]
  pub fn check_config_updates(&mut self) -> Result<Option<usize>> {
    if !self.exports.has_function("check_config_updates") {
      return Ok(None); // ignore, the plugin doesn't have this defined
    }
    self.call_len("check_config_updates", &[]).map(Some)
  }

  #[inline]
  pub fn get_resolved_config(&mut self, config_id: FormatConfigId) -> Result<usize> {
    self.call_len("get_resolved_config", &[config_id.as_raw()])
  }

  #[inline]
  pub fn get_config_diagnostics(&mut self, config_id: FormatConfigId) -> Result<usize> {
    self.call_len("get_config_diagnostics", &[config_id.as_raw()])
  }

  #[inline]
  pub fn get_config_file_matching(&mut self, config_id: FormatConfigId) -> Result<usize> {
    self.call_len("get_config_file_matching", &[config_id.as_raw()])
  }

  #[inline]
  pub fn set_override_config(&mut self) -> Result<()> {
    self.exports.call("set_override_config", &[])
  }

  #[inline]
  pub fn set_file_path(&mut self) -> Result<()> {
    self.exports.call("set_file_path", &[])
  }

  #[inline]
  pub fn format(&mut self, config_id: FormatConfigId) -> Result<WasmFormatResult> {
    let value = self.exports.call_u32("format", &[config_id.as_raw()])?;
    WasmFormatResult::try_from(value)
  }

  #[inline]
  pub fn format_range(&mut self, config_id: FormatConfigId, range: std::ops::Range<usize>) -> Result<WasmFormatResult> {
    if !self.exports.has_function("format_range") {
      return Ok(WasmFormatResult::NoChange); // not supported
    }
    let value = self
      .exports
      .call_u32("format_range", &[config_id.as_raw(), range.start as u32, range.end as u32])?;
    WasmFormatResult::try_from(value)
  }

  #[inline]
  pub fn get_formatted_text(&mut self) -> Result<usize> {
    self.call_len("get_formatted_text", &[])
  }

  #[inline]
  pub fn get_error_text(&mut self) -> Result<usize> {
    self.call_len("get_error_text", &[])
  }

  #[inline]
  pub fn clear_shared_bytes(&mut self, capacity: usize) -> Result<u32> {
    self.exports.call_u32("clear_shared_bytes", &[capacity as u32])
  }

  #[inline]
  pub fn get_shared_bytes_ptr(&mut self) -> Result<u32> {
    self.exports.call_u32("get_shared_bytes_ptr", &[])
  }

  #[inline]
  fn write_memory(&mut self, offset: usize, bytes: &[u8]) -> Result<()> {
    self.exports.write_memory(offset, bytes)
  }

  #[inline]
  fn read_memory(&mut self, offset: usize, bytes: &mut [u8]) -> Result<()> {
    self.exports.read_memory(offset, bytes)
  }

  fn call_len(&mut self, name: &str, params: &[u32]) -> Result<usize> {
    Ok(self.exports.call_u32(name, params)? as usize)
  }
}
