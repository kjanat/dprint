use anyhow::Result;
use std::hash::Hash;
use std::io;
use std::sync::Arc;
use url::Url;

use kprint_async_runtime::async_trait;

use super::DownloadedFile;
use super::UrlDownloader;
use crate::plugins::CompilationResult;
use crate::utils::FastInsecureHasher;
use crate::utils::LogLevel;
use crate::utils::Logger;
use crate::utils::LoggerOptions;
use crate::utils::MultiSelectItem;
use crate::utils::NoProxy;
use crate::utils::ProgressReporter;
use crate::utils::RealUrlDownloader;
use crate::utils::ShowConfirmStrategy;
use crate::utils::UnsafelyIgnoreCertificates;
use crate::utils::is_terminal_interactive;
use crate::utils::log_action_with_progress;
use crate::utils::show_confirm;
use crate::utils::show_multi_select;
use crate::utils::show_select;
use kprint_platform::environment::*;

pub type RealEnvironment = NativeEnvironment<CliServices>;
pub struct RealEnvironmentOptions {
  pub log_level: LogLevel,
  pub is_stdout_machine_readable: bool,
}
#[derive(Clone)]
pub struct CliServices {
  progress_bars: Option<Arc<dyn ProgressReporter>>,
  url_downloader: Arc<RealUrlDownloader>,
  logger: Arc<Logger>,
}
impl std::fmt::Debug for CliServices {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("CliServices").finish()
  }
}
pub fn create_real_environment(options: RealEnvironmentOptions) -> Result<RealEnvironment> {
  let logger = Arc::new(Logger::new(&LoggerOptions {
    initial_context_name: "dprint".to_string(),
    is_stdout_machine_readable: options.is_stdout_machine_readable,
    log_level: options.log_level,
  }));
  let progress_bars = crate::terminal::ProgressBars::new(&logger).map(|bars| Arc::new(bars) as Arc<dyn ProgressReporter>);
  let no_proxy = NoProxy::from_env();
  let url_downloader = Arc::new(RealUrlDownloader::new(
    progress_bars.clone(),
    logger.clone(),
    no_proxy,
    UnsafelyIgnoreCertificates::from_env(),
  )?);
  NativeEnvironment::new(
    CliServices {
      logger,
      progress_bars,
      url_downloader,
    },
    env!("CARGO_PKG_VERSION"),
  )
}
impl kprint_platform::environment::OutputEnvironment for CliServices {
  fn __log__(&self, text: &str) {
    self.logger.log(text, "dprint");
  }

  fn log_machine_readable(&self, bytes: &[u8]) {
    self.logger.log_machine_readable(bytes);
  }

  fn log_stderr_with_context(&self, text: &str, context_name: &str) {
    self.logger.log_stderr_with_context(text, context_name);
  }

  fn log_action_with_progress<TResult: Send + Sync, TCreate: FnOnce(Box<dyn Fn(usize)>) -> TResult + Send + Sync>(
    &self,
    message: &str,
    action: TCreate,
    total_size: usize,
  ) -> TResult {
    log_action_with_progress(self.progress_bars.as_deref(), message, action, total_size)
  }

  #[inline]
  fn log_level(&self) -> LogLevel {
    self.logger.log_level()
  }

  fn progress_bars(&self) -> Option<&Arc<dyn ProgressReporter>> {
    self.progress_bars.as_ref()
  }
}
impl kprint_platform::environment::InteractionEnvironment for CliServices {
  fn get_selection(&self, prompt_message: &str, item_indent_width: u16, items: &[String]) -> Result<usize> {
    show_select(&self.logger, "dprint", prompt_message, item_indent_width, items)
  }

  fn get_multi_selection(&self, prompt_message: &str, item_indent_width: u16, items: Vec<MultiSelectItem>) -> Result<Vec<usize>> {
    show_multi_select(&self.logger, "dprint", prompt_message, item_indent_width, items)
  }

  fn stdout(&self) -> Box<dyn io::Write + Send> {
    Box::new(io::stdout())
  }

  fn stdin(&self) -> Box<dyn io::Read + Send> {
    Box::new(io::stdin())
  }
}
#[async_trait(?Send)]
impl UrlDownloader for CliServices {
  async fn download_file_no_redirects(&self, url: &Url, auth: Option<&str>, max_len: Option<usize>) -> Result<Option<DownloadedFile>> {
    log_debug!(self, "Downloading url: {}", url);

    let downloader = self.url_downloader.clone();
    let url = url.clone();
    let auth = auth.map(|s| s.to_string());
    // the download gives up at the deadline itself, so it doesn't keep going
    // once what it's for has given up
    let deadline = crate::utils::current_deadline();
    kprint_async_runtime::spawn_blocking(move || downloader.download_with_auth(&url, auth.as_deref(), deadline, max_len)).await?
  }
}
impl NativeServices for CliServices {
  fn compile_wasm<T: PluginEnvironment + ConcurrencyEnvironment + ProcessEnvironment>(
    &self,
    environment: &T,
    plugin_display: &str,
    wasm_bytes: &[u8],
    control: &crate::plugins::WasmCompileControl,
  ) -> Result<CompilationResult> {
    crate::plugins::compile_wasm_supervised(environment, plugin_display, wasm_bytes, control)
  }

  fn wasm_cache_key(&self, environment: &impl SystemEnvironment) -> String {
    let cpu = environment.cpu_arch();
    let mut hash = FastInsecureHasher::default();
    // wasmtime tunes native code to the host CPU features and refuses to
    // deserialize an artifact compiled for incompatible ones (the caller then
    // recompiles), so include what it checks in the key. that way artifacts for
    // different CPUs get distinct cache entries and coexist in a cache directory
    // shared across machines, such as one restored from a CI cache, rather than
    // each machine overwriting the other's. the rustc version is hashed too
    // because deserialization can break across a rust upgrade.
    // https://github.com/dprint/dprint/issues/735
    env!("RUSTC_VERSION_TEXT").hash(&mut hash);
    crate::plugins::wasm_precompile_compatibility_hash().hash(&mut hash);
    format!("{}-{}", cpu, hash.finish())
  }
}

impl kprint_platform::environment::ConsentEnvironment for CliServices {
  fn confirm_with_strategy(&self, strategy: &dyn ShowConfirmStrategy) -> Result<bool> {
    show_confirm(&self.logger, "dprint", strategy)
  }
  fn is_terminal_interactive(&self) -> bool {
    is_terminal_interactive()
  }
}
