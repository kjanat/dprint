use serde::Deserialize;
use serde::Serialize;

#[cfg(feature = "async_runtime")]
use dprint_async_runtime::FutureExt;
#[cfg(feature = "async_runtime")]
use dprint_async_runtime::LocalBoxFuture;

use dprint_configuration::ConfigKeyMap;
use dprint_configuration::ConfigKeyValue;
use dprint_configuration::ConfigurationDiagnostic;
use dprint_configuration::GlobalConfiguration;

use super::FileMatchingInfo;

pub trait CancellationToken: Send + Sync + std::fmt::Debug {
  fn is_cancelled(&self) -> bool;
  #[cfg(feature = "async_runtime")]
  fn wait_cancellation(&self) -> LocalBoxFuture<'static, ()>;
}

#[cfg(feature = "async_runtime")]
impl CancellationToken for tokio_util::sync::CancellationToken {
  fn is_cancelled(&self) -> bool {
    self.is_cancelled()
  }

  fn wait_cancellation(&self) -> LocalBoxFuture<'static, ()> {
    let token = self.clone();
    async move { token.cancelled().await }.boxed_local()
  }
}

/// A cancellation token that always says it's not cancelled.
#[derive(Debug)]
pub struct NullCancellationToken;

impl CancellationToken for NullCancellationToken {
  fn is_cancelled(&self) -> bool {
    false
  }

  #[cfg(feature = "async_runtime")]
  fn wait_cancellation(&self) -> LocalBoxFuture<'static, ()> {
    // never resolves
    Box::pin(std::future::pending())
  }
}

pub type FormatRange = Option<std::ops::Range<usize>>;

/// An error returned by formatting operations.
///
/// This can hold any error, allowing plugins to return their own error types,
/// while still implementing [`std::error::Error`] so that consumers can convert
/// it into their own error type.
#[derive(Debug, thiserror::Error)]
#[error(transparent)]
pub struct FormatError(Box<dyn std::error::Error + Send + Sync + 'static>);

impl FormatError {
  /// Creates a new error from anything that can be turned into a boxed error
  /// (for example a `String`, `&str`, or any [`std::error::Error`]).
  pub fn new(error: impl Into<Box<dyn std::error::Error + Send + Sync + 'static>>) -> Self {
    FormatError(error.into())
  }

  /// Attempts to downcast the underlying error to a concrete type
  /// (ex. to check for a [`CriticalFormatError`]).
  pub fn downcast_ref<E: std::error::Error + 'static>(&self) -> Option<&E> {
    self.0.downcast_ref::<E>()
  }
}

/// Formats an error and its source chain into a single string,
/// joining each level with `: ` (equivalent to formatting an
/// `anyhow` error with the alternate `{:#}` specifier).
pub fn error_to_string(err: &(dyn std::error::Error + 'static)) -> String {
  // cap the depth so a pathological error with a cyclic `source()` chain
  // can't make this loop forever
  const MAX_DEPTH: usize = 100;
  let mut result = err.to_string();
  let mut source = err.source();
  for _ in 0..MAX_DEPTH {
    let Some(err) = source else { break };
    result.push_str(": ");
    result.push_str(&err.to_string());
    source = err.source();
  }
  result
}

macro_rules! impl_format_error_from {
  ($($t:ty),* $(,)?) => {
    $(
      impl From<$t> for FormatError {
        fn from(error: $t) -> Self {
          FormatError(error.into())
        }
      }
    )*
  };
}

impl_format_error_from!(
  String,
  &str,
  Box<dyn std::error::Error + Send + Sync + 'static>,
  std::io::Error,
  std::str::Utf8Error,
  std::string::FromUtf8Error,
  CriticalFormatError,
);

#[cfg(test)]
mod tests {
  use super::FormatError;

  #[test]
  fn should_convert_utf8_error_to_format_error() {
    let bytes = [u8::MAX];
    let utf8_error = std::str::from_utf8(&bytes).unwrap_err();
    let format_error: FormatError = utf8_error.into();

    assert!(format_error.downcast_ref::<std::str::Utf8Error>().is_some());
  }
}

impl_format_error_from!(serde_json::Error);

#[cfg(feature = "async_runtime")]
impl_format_error_from!(tokio::task::JoinError, tokio::sync::oneshot::error::RecvError);

/// A formatting error where the plugin cannot recover.
///
/// Return one of these to signal to the dprint CLI that
/// it should recreate the plugin.
#[derive(Debug, thiserror::Error)]
#[error(transparent)]
pub struct CriticalFormatError(pub FormatError);

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckConfigUpdatesMessage {
  /// dprint versions < 0.47 won't have this set
  #[serde(default)]
  pub old_version: Option<String>,
  pub config: ConfigKeyMap,
}

/// `Ok(Some(text))` - Changes due to the format.
/// `Ok(None)` - No changes.
/// `Err(err)` - Error formatting. Use a `CriticalError` to signal that the plugin can't recover.
pub type FormatResult = Result<Option<Vec<u8>>, FormatError>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawFormatConfig {
  pub plugin: ConfigKeyMap,
  pub global: GlobalConfiguration,
}

/// A unique configuration id used for formatting.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FormatConfigId(u32);

impl std::fmt::Display for FormatConfigId {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    write!(f, "${}", self.0)
  }
}

impl FormatConfigId {
  pub fn from_raw(raw: u32) -> FormatConfigId {
    FormatConfigId(raw)
  }

  pub fn uninitialized() -> FormatConfigId {
    FormatConfigId(0)
  }

  pub fn as_raw(&self) -> u32 {
    self.0
  }
}

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ConfigChangePathItem {
  /// String property name.
  String(String),
  /// Number if an index in an array.
  Number(usize),
}

impl From<String> for ConfigChangePathItem {
  fn from(value: String) -> Self {
    Self::String(value)
  }
}

impl From<usize> for ConfigChangePathItem {
  fn from(value: usize) -> Self {
    Self::Number(value)
  }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigChange {
  /// The path to make modifications at.
  pub path: Vec<ConfigChangePathItem>,
  #[serde(flatten)]
  pub kind: ConfigChangeKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value")]
pub enum ConfigChangeKind {
  /// Adds an object property or array element.
  Add(ConfigKeyValue),
  /// Overwrites an existing value at the provided path.
  Set(ConfigKeyValue),
  /// Removes the value at the path.
  Remove,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginResolveConfigurationResult<T>
where
  T: Clone + Serialize,
{
  /// Information about what files are matched for the provided configuration.
  pub file_matching: FileMatchingInfo,

  /// The configuration diagnostics.
  pub diagnostics: Vec<ConfigurationDiagnostic>,

  /// The configuration derived from the unresolved configuration
  /// that can be used to format a file.
  pub config: T,
}
