use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use dprint_configuration::ConfigKeyMap;
use dprint_configuration::ConfigurationDiagnostic;
use dprint_configuration::GlobalConfiguration;
use dprint_plugin_types::CancellationToken;
use dprint_plugin_types::CheckConfigUpdatesMessage;
use dprint_plugin_types::ConfigChange;
use dprint_plugin_types::CriticalFormatError;
use dprint_plugin_types::FileMatchingInfo;
use dprint_plugin_types::FormatConfigId;
use dprint_plugin_types::FormatError;
use dprint_plugin_types::FormatRange;
use dprint_plugin_types::FormatResult;
use dprint_plugin_types::HostFormatRequest;
use dprint_plugin_types::NullCancellationToken;
use dprint_plugin_types::PluginInfo;
use serde::Serialize;

use crate::plugins::FormatConfig;
use crate::plugins::implementations::wasm::WasmHostFormatSender;

use super::InitializedWasmPluginInstance;
use super::Linker;
use super::PluginExports;
use super::checked_range;

enum WasmFormatResult {
  NoChange,
  Change,
  Error,
}

#[derive(Clone, Serialize, serde::Deserialize, Debug, PartialEq, Eq)]
struct SyncPluginInfo {
  #[serde(flatten)]
  info: PluginInfo,
  #[serde(flatten)]
  file_matching: FileMatchingInfo,
}

#[derive(Default)]
struct SharedBytes {
  data: Vec<u8>,
  index: usize,
}

impl SharedBytes {
  pub fn with_size(size: usize) -> Self {
    Self::from_bytes(vec![0; size])
  }

  pub fn from_bytes(data: Vec<u8>) -> Self {
    Self { data, index: 0 }
  }
}

/// The host state for a v3 plugin, kept in the store of the engine that
/// runs it (see `WasmHostState`).
pub struct ImportObjectEnvironmentV3 {
  pub token: Arc<dyn CancellationToken>,
  override_config: Option<ConfigKeyMap>,
  file_path: Option<PathBuf>,
  formatted_text_store: Vec<u8>,
  shared_bytes: SharedBytes,
  error_text_store: String,
  host_format_sender: WasmHostFormatSender,
}

impl ImportObjectEnvironmentV3 {
  pub fn new(host_format_sender: WasmHostFormatSender) -> Self {
    Self {
      token: Arc::new(NullCancellationToken),
      override_config: None,
      file_path: None,
      formatted_text_store: Default::default(),
      shared_bytes: SharedBytes::default(),
      error_text_store: Default::default(),
      host_format_sender,
    }
  }

  fn take_shared_bytes(&mut self) -> Vec<u8> {
    let data = std::mem::take(&mut self.shared_bytes.data);
    self.shared_bytes.index = 0;
    data
  }
}

pub fn add_identity_imports(linker: &mut Linker) -> Result<()> {
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

pub fn host_clear_bytes(state: &mut ImportObjectEnvironmentV3, length: u32) {
  state.shared_bytes = SharedBytes::with_size(length as usize);
}

pub fn host_read_buffer(memory: &[u8], state: &mut ImportObjectEnvironmentV3, buffer_pointer: u32, length: u32) -> Result<(), String> {
  let source = checked_range(memory, buffer_pointer, length as usize)?;
  let index = state.shared_bytes.index;
  let Some(target) = state.shared_bytes.data.get_mut(index..index + length as usize) else {
    return Err(format!("Reading {} bytes past the end of the shared bytes.", length));
  };
  target.copy_from_slice(&memory[source]);
  state.shared_bytes.index += length as usize;
  Ok(())
}

pub fn host_write_buffer(memory: &mut [u8], state: &mut ImportObjectEnvironmentV3, buffer_pointer: u32, offset: u32, length: u32) -> Result<(), String> {
  let (offset, length) = (offset as usize, length as usize);
  let Some(chunk) = state.shared_bytes.data.get(offset..offset + length) else {
    return Err(format!("Writing {} bytes from past the end of the shared bytes.", length));
  };
  let target = checked_range(memory, buffer_pointer, length)?;
  memory[target].copy_from_slice(chunk);
  Ok(())
}

pub fn host_take_override_config(state: &mut ImportObjectEnvironmentV3) {
  let bytes = state.take_shared_bytes();
  let config_key_map: ConfigKeyMap = serde_json::from_slice(&bytes).unwrap_or_default();
  state.override_config.replace(config_key_map);
}

pub fn host_take_file_path(state: &mut ImportObjectEnvironmentV3) -> Result<(), String> {
  let bytes = state.take_shared_bytes();
  let file_path_str = String::from_utf8(bytes).map_err(|err| format!("Invalid file path: {err}"))?;
  state.file_path.replace(PathBuf::from(file_path_str));
  Ok(())
}

pub fn host_format(state: &mut ImportObjectEnvironmentV3) -> Result<u32, String> {
  let override_config = state.override_config.take().unwrap_or_default();
  let Some(file_path) = state.file_path.take() else {
    return Err("Expected to have file path.".to_string());
  };
  let request = HostFormatRequest {
    file_path,
    file_bytes: state.take_shared_bytes(),
    range: None,
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

pub fn host_get_formatted_text(state: &mut ImportObjectEnvironmentV3) -> u32 {
  let formatted_bytes = std::mem::take(&mut state.formatted_text_store);
  let len = formatted_bytes.len();
  state.shared_bytes = SharedBytes::from_bytes(formatted_bytes);
  len as u32
}

pub fn host_get_error_text(state: &mut ImportObjectEnvironmentV3) -> u32 {
  let error_text = std::mem::take(&mut state.error_text_store);
  let len = error_text.len();
  state.shared_bytes = SharedBytes::from_bytes(error_text.into_bytes());
  len as u32
}

pub struct InitializedWasmPluginInstanceV3<TExports: PluginExports> {
  wasm_functions: WasmFunctions<TExports>,
  buffer_size: usize,
  current_config_id: FormatConfigId,
}

impl<TExports: PluginExports> InitializedWasmPluginInstanceV3<TExports> {
  pub fn new(exports: TExports) -> Result<Self> {
    let mut wasm_functions = WasmFunctions { exports };
    let buffer_size = wasm_functions.get_wasm_memory_buffer_size()?;
    Ok(Self {
      wasm_functions,
      buffer_size,
      current_config_id: FormatConfigId::uninitialized(),
    })
  }

  fn set_global_config(&mut self, global_config: &GlobalConfiguration) -> Result<()> {
    let json = serde_json::to_string(global_config)?;
    self.send_string(&json)?;
    self.wasm_functions.set_global_config()?;
    Ok(())
  }

  fn set_plugin_config(&mut self, plugin_config: &ConfigKeyMap) -> Result<()> {
    let json = serde_json::to_string(plugin_config)?;
    self.send_string(&json)?;
    self.wasm_functions.set_plugin_config()?;
    Ok(())
  }

  fn sync_plugin_info(&mut self) -> Result<SyncPluginInfo> {
    let len = self.wasm_functions.get_plugin_info()?;
    let json_text = self.receive_string(len)?;
    Ok(serde_json::from_str(&json_text)?)
  }

  fn inner_format_text(&mut self, file_path: &Path, file_bytes: &[u8], override_config: &ConfigKeyMap) -> Result<FormatResult> {
    // send override config if necessary
    if !override_config.is_empty() {
      self.send_string(&match serde_json::to_string(override_config) {
        Ok(text) => text,
        Err(err) => return Ok(Err(err.into())),
      })?;
      self.wasm_functions.set_override_config()?;
    }

    // send file path
    self.send_string(&file_path.to_string_lossy())?;
    self.wasm_functions.set_file_path()?;

    // send file text and format
    self.send_bytes(file_bytes)?;
    let response_code = self.wasm_functions.format()?;

    // handle the response
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
    if self.current_config_id != config.id {
      // set this to uninitialized in case it errors below
      self.current_config_id = FormatConfigId::uninitialized();
      // update the plugin
      self.set_global_config(&config.global)?;
      self.set_plugin_config(&config.plugin)?;
      // now mark this as successfully set
      self.current_config_id = config.id;
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
    let mut index = 0;
    let len = bytes.len();
    self.wasm_functions.clear_shared_bytes(len)?;
    while index < len {
      let write_count = std::cmp::min(len - index, self.buffer_size);
      self.write_bytes_to_memory_buffer(&bytes[index..(index + write_count)])?;
      self.wasm_functions.add_to_shared_bytes_from_buffer(write_count)?;
      index += write_count;
    }
    Ok(())
  }

  fn write_bytes_to_memory_buffer(&mut self, bytes: &[u8]) -> Result<()> {
    let wasm_buffer_pointer = self.wasm_functions.get_wasm_memory_buffer_ptr()?;
    self.wasm_functions.write_memory(wasm_buffer_pointer as usize, bytes)?;
    Ok(())
  }

  fn receive_string(&mut self, len: usize) -> Result<String> {
    let bytes = self.receive_bytes(len)?;
    Ok(String::from_utf8(bytes)?)
  }

  fn receive_bytes(&mut self, len: usize) -> Result<Vec<u8>> {
    let mut index = 0;
    let mut bytes: Vec<u8> = vec![0; len];
    while index < len {
      let read_count = std::cmp::min(len - index, self.buffer_size);
      self.wasm_functions.set_buffer_with_shared_bytes(index, read_count)?;
      self.read_bytes_from_memory_buffer(&mut bytes[index..(index + read_count)])?;
      index += read_count;
    }
    Ok(bytes)
  }

  fn read_bytes_from_memory_buffer(&mut self, bytes: &mut [u8]) -> Result<()> {
    let wasm_buffer_pointer = self.wasm_functions.get_wasm_memory_buffer_ptr()?;
    self.wasm_functions.read_memory(wasm_buffer_pointer as usize, bytes)?;
    Ok(())
  }
}

impl<TExports: PluginExports> InitializedWasmPluginInstance for InitializedWasmPluginInstanceV3<TExports> {
  fn plugin_info(&mut self) -> Result<PluginInfo> {
    self.sync_plugin_info().map(|i| i.info)
  }

  fn license_text(&mut self) -> Result<String> {
    let len = self.wasm_functions.get_license_text()?;
    self.receive_string(len)
  }

  fn check_config_updates(&mut self, _message: &CheckConfigUpdatesMessage) -> Result<Vec<ConfigChange>> {
    Ok(Vec::new())
  }

  fn resolved_config(&mut self, config: &FormatConfig) -> Result<String> {
    self.ensure_config(config)?;
    let len = self.wasm_functions.get_resolved_config()?;
    self.receive_string(len)
  }

  fn config_diagnostics(&mut self, config: &FormatConfig) -> Result<Vec<ConfigurationDiagnostic>> {
    self.ensure_config(config)?;
    let len = self.wasm_functions.get_config_diagnostics()?;
    let json_text = self.receive_string(len)?;
    Ok(serde_json::from_str(&json_text)?)
  }

  fn file_matching_info(&mut self, _config: &FormatConfig) -> Result<FileMatchingInfo> {
    self.sync_plugin_info().map(|i| i.file_matching)
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
    if range.is_some() && range != Some(0..file_bytes.len()) {
      return Ok(None); // not supported for v3
    }
    self.wasm_functions.exports.set_token(token);
    self.ensure_config(config).map_err(FormatError::new)?;
    match self.inner_format_text(file_path, file_bytes, override_config) {
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
  pub fn set_global_config(&mut self) -> Result<()> {
    self.exports.call("set_global_config", &[])
  }

  #[inline]
  pub fn set_plugin_config(&mut self) -> Result<()> {
    self.exports.call("set_plugin_config", &[])
  }

  #[inline]
  pub fn get_plugin_info(&mut self) -> Result<usize> {
    self.call_len("get_plugin_info")
  }

  #[inline]
  pub fn get_license_text(&mut self) -> Result<usize> {
    self.call_len("get_license_text")
  }

  #[inline]
  pub fn get_resolved_config(&mut self) -> Result<usize> {
    self.call_len("get_resolved_config")
  }

  #[inline]
  pub fn get_config_diagnostics(&mut self) -> Result<usize> {
    self.call_len("get_config_diagnostics")
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
  pub fn format(&mut self) -> Result<WasmFormatResult> {
    let value = self.exports.call_u32("format", &[])?;
    Ok(u8_to_format_result(value as u8))
  }

  #[inline]
  pub fn get_formatted_text(&mut self) -> Result<usize> {
    self.call_len("get_formatted_text")
  }

  #[inline]
  pub fn get_error_text(&mut self) -> Result<usize> {
    self.call_len("get_error_text")
  }

  #[inline]
  pub fn clear_shared_bytes(&mut self, capacity: usize) -> Result<()> {
    self.exports.call("clear_shared_bytes", &[capacity as u32])
  }

  #[inline]
  pub fn get_wasm_memory_buffer_size(&mut self) -> Result<usize> {
    self.call_len("get_wasm_memory_buffer_size")
  }

  #[inline]
  pub fn get_wasm_memory_buffer_ptr(&mut self) -> Result<u32> {
    self.exports.call_u32("get_wasm_memory_buffer", &[])
  }

  #[inline]
  pub fn set_buffer_with_shared_bytes(&mut self, offset: usize, length: usize) -> Result<()> {
    self.exports.call("set_buffer_with_shared_bytes", &[offset as u32, length as u32])
  }

  #[inline]
  pub fn add_to_shared_bytes_from_buffer(&mut self, length: usize) -> Result<()> {
    self.exports.call("add_to_shared_bytes_from_buffer", &[length as u32])
  }

  #[inline]
  fn write_memory(&mut self, offset: usize, bytes: &[u8]) -> Result<()> {
    self.exports.write_memory(offset, bytes)
  }

  #[inline]
  fn read_memory(&mut self, offset: usize, bytes: &mut [u8]) -> Result<()> {
    self.exports.read_memory(offset, bytes)
  }

  fn call_len(&mut self, name: &str) -> Result<usize> {
    Ok(self.exports.call_u32(name, &[])? as usize)
  }
}

fn u8_to_format_result(orig: u8) -> WasmFormatResult {
  match orig {
    0 => WasmFormatResult::NoChange,
    1 => WasmFormatResult::Change,
    2 => WasmFormatResult::Error,
    _ => unreachable!(),
  }
}
