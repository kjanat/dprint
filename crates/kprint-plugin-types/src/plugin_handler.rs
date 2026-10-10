use serde::Deserialize;
use serde::Serialize;

#[cfg(feature = "async_runtime")]
use kprint_async_runtime::FutureExt;
#[cfg(feature = "async_runtime")]
use kprint_async_runtime::LocalBoxFuture;

use kprint_configuration::ConfigKeyMap;
use kprint_configuration::ConfigKeyValue;
use kprint_configuration::ConfigurationDiagnostic;
use kprint_configuration::GlobalConfiguration;

use super::FileMatchingInfo;

pub trait CancellationToken: Send + Sync + std::fmt::Debug {
  fn is_cancelled(&self) -> bool;
  /// Resolves once the token is cancelled. The default polls `is_cancelled`
  /// every 10ms; a token that can wake its waiters overrides it.
  #[cfg(feature = "async_runtime")]
  fn wait_cancellation(&self) -> LocalBoxFuture<'_, ()> {
    async move {
      while !self.is_cancelled() {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
      }
    }
    .boxed_local()
  }
}

#[cfg(feature = "async_runtime")]
impl CancellationToken for tokio_util::sync::CancellationToken {
  fn is_cancelled(&self) -> bool {
    self.is_cancelled()
  }

  fn wait_cancellation(&self) -> LocalBoxFuture<'_, ()> {
    self.cancelled().boxed_local()
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
  fn wait_cancellation(&self) -> LocalBoxFuture<'_, ()> {
    // never resolves
    Box::pin(std::future::pending())
  }
}

#[cfg(all(test, feature = "async_runtime"))]
mod cancellation_test {
  use std::sync::Arc;
  use std::sync::atomic::AtomicBool;
  use std::sync::atomic::Ordering;
  use std::time::Duration;

  use super::CancellationToken;

  #[derive(Debug, Default)]
  struct FlagToken(AtomicBool);

  impl CancellationToken for FlagToken {
    fn is_cancelled(&self) -> bool {
      self.0.load(Ordering::SeqCst)
    }
  }

  #[tokio::test]
  async fn a_token_with_only_is_cancelled_waits_until_cancelled() {
    let token = Arc::new(FlagToken::default());
    let dyn_token: Arc<dyn CancellationToken> = token.clone();
    let waiting = dyn_token.wait_cancellation();
    let canceller = std::thread::spawn({
      let token = token.clone();
      move || {
        std::thread::sleep(Duration::from_millis(30));
        token.0.store(true, Ordering::SeqCst);
      }
    });
    tokio::time::timeout(Duration::from_secs(5), waiting).await.unwrap();
    assert!(dyn_token.is_cancelled());
    canceller.join().unwrap();
  }

  #[tokio::test]
  async fn an_uncancelled_token_keeps_waiting() {
    let token: Arc<dyn CancellationToken> = Arc::new(FlagToken::default());
    assert!(tokio::time::timeout(Duration::from_millis(50), token.wait_cancellation()).await.is_err());
  }

  #[derive(Default)]
  struct WokenFlag(AtomicBool);

  impl std::task::Wake for WokenFlag {
    fn wake(self: Arc<Self>) {
      self.0.store(true, Ordering::SeqCst);
    }
  }

  #[tokio::test]
  async fn a_tokio_token_wakes_its_waiter_when_cancelled() {
    let token = tokio_util::sync::CancellationToken::new();
    let dyn_token: Arc<dyn CancellationToken> = Arc::new(token.clone());
    let mut waiting = dyn_token.wait_cancellation();
    let woken = Arc::new(WokenFlag::default());
    let waker = std::task::Waker::from(woken.clone());
    let mut context = std::task::Context::from_waker(&waker);
    assert!(waiting.as_mut().poll(&mut context).is_pending());
    token.cancel();
    assert!(woken.0.load(Ordering::SeqCst));
    assert!(waiting.as_mut().poll(&mut context).is_ready());
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
