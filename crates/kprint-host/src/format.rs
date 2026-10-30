use anyhow::Result;
use anyhow::bail;
use kprint_async_runtime::FutureExt;
use kprint_async_runtime::future;
use kprint_plugin_types::HostFormatCallback;
use kprint_plugin_types::HostFormatRequest;
use kprint_plugin_types::NullCancellationToken;
use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;
use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::environment::HostEnvironment as Environment;
use crate::incremental::FileMetadata;
use crate::incremental::IncrementalFile;
use crate::plugins::FormatEngine;
use crate::resolution::GetPluginResult;
use crate::resolution::InitializedPluginWithConfig;
use crate::resolution::InitializedPluginWithConfigFormatRequest;
use crate::resolution::PluginWithConfig;
use crate::resolution::PluginsScope;
use crate::resolution::PluginsScopeAndPaths;
use crate::utils::ErrorCountLogger;
use crate::utils::Semaphore;
use sys_traits::FsMetadata;
use sys_traits::FsMetadataValue;

struct TaskWork {
  semaphore: Rc<Semaphore>,
  plugins: Vec<Rc<PluginWithConfig>>,
  file_paths: Vec<PathBuf>,
}

type InitializedPlugins = Rc<Vec<(Rc<PluginWithConfig>, InitializedPluginWithConfig)>>;

/// How long formatting a file takes before it's reported as slow.
const SLOW_FORMAT: Duration = Duration::from_secs(10);

/// Where formatting a file is, to say what's slow about it.
#[derive(Clone)]
enum FormatStage {
  /// Waiting for the plugins of the file's group, which start before their
  /// first file is formatted.
  StartingPlugins,
  Reading,
  /// `plugin` (an index into the group's plugins) formats the file, in pass
  /// `pass` (more than 1 when ensuring a stable format).
  Formatting {
    plugin: usize,
    pass: usize,
  },
  /// Like `Formatting`, while the plugin waits for the host to format code it
  /// embeds (ex. a code block in markdown).
  FormattingEmbedded {
    plugin: usize,
    pass: usize,
    embedded: EmbeddedFormat,
  },
  /// Doing what the command does with the formatted text, ex. writing it.
  Finishing,
}

impl FormatStage {
  /// Ex. "dprint-plugin-typescript 0.95.0 (interpreted Wasm) is formatting it".
  fn describe(&self, plugins: &[Rc<PluginWithConfig>]) -> String {
    let describe_index = |plugin: usize| {
      plugins
        .get(plugin)
        .map(|plugin| describe_plugin(plugin))
        .unwrap_or_else(|| "a plugin".to_string())
    };
    let pass_text = |pass: usize| {
      if pass > 1 {
        format!(" again (pass {}) to ensure a stable format", pass)
      } else {
        String::new()
      }
    };
    match self {
      FormatStage::StartingPlugins => format!(
        "it's waiting for {} to start",
        plugins.iter().map(|plugin| describe_plugin(plugin)).collect::<Vec<_>>().join(", ")
      ),
      FormatStage::Reading => "it's being read".to_string(),
      FormatStage::Formatting { plugin, pass } => format!("{} is formatting it{}", describe_index(*plugin), pass_text(*pass)),
      FormatStage::FormattingEmbedded { plugin, pass, embedded } => format!(
        "{} is formatting it{}, waiting for {} to format its embedded {}",
        describe_index(*plugin),
        pass_text(*pass),
        if embedded.plugin_names.is_empty() {
          "the host".to_string()
        } else {
          embedded.plugin_names.join(", ")
        },
        embedded.file_path.display()
      ),
      FormatStage::Finishing => "the formatted text is being handled (ex. written)".to_string(),
    }
  }
}

/// Code a plugin asks the host to format, ex. a markdown code block.
#[derive(Clone)]
struct EmbeddedFormat {
  /// The made-up path that gives the code's language, ex. `file.toml`.
  file_path: PathBuf,
  plugin_names: Vec<String>,
}

/// Ex. "dprint-plugin-typescript 0.95.0 (interpreted Wasm)": an interpreted
/// plugin formats several times slower than its native code.
fn describe_plugin(plugin: &PluginWithConfig) -> String {
  let engine = match plugin.plugin.format_engine() {
    Some(FormatEngine::Interpreter) => " (interpreted Wasm)",
    Some(FormatEngine::Native) if plugin.plugin.compiles_to_format() => " (Wasm, compiling to native code)",
    Some(FormatEngine::Native) => " (Wasm as native code)",
    None => "",
  };
  format!("{} {}{}", plugin.name(), plugin.info().version, engine)
}

/// The errors formatting files, reported once per message: the first file
/// with a message gets it in full, and the other files with the same message
/// are listed after formatting (ex. 39 files whose formatter isn't
/// installed). Every file still counts as an error.
struct FileErrors<TEnvironment: Environment> {
  logger: ErrorCountLogger<TEnvironment>,
  environment: TEnvironment,
  /// The messages in the order they were first reported, with the files
  /// after the first that had them.
  repeated: RefCell<Vec<(String, PathBuf, Vec<PathBuf>)>>,
  /// Where each message is in `repeated`.
  indexes: RefCell<HashMap<String, usize>>,
}

impl<TEnvironment: Environment> FileErrors<TEnvironment> {
  /// How many of the other files with a message are named.
  const NAMED_FILES: usize = 5;

  fn new(logger: ErrorCountLogger<TEnvironment>, environment: &TEnvironment) -> Self {
    FileErrors {
      logger,
      environment: environment.clone(),
      repeated: Default::default(),
      indexes: Default::default(),
    }
  }

  fn log(&self, file_path: &Path, message: String) {
    let index = self.indexes.borrow().get(&message).copied();
    match index {
      Some(index) => {
        self.repeated.borrow_mut()[index].2.push(file_path.to_path_buf());
        self.logger.add_error_count(1);
      }
      None => {
        self
          .logger
          .log_error(&format!("Error formatting {}. Message: {}", file_path.display(), message));
        let mut repeated = self.repeated.borrow_mut();
        self.indexes.borrow_mut().insert(message.clone(), repeated.len());
        repeated.push((message, file_path.to_path_buf(), Vec::new()));
      }
    }
  }

  /// Lists the other files of each message reported more than once.
  fn log_repeated(&self) {
    let cwd = self.environment.cwd();
    let display = |file_path: &PathBuf| file_path.strip_prefix(&cwd).unwrap_or(file_path).display().to_string();
    for (_, first, others) in self.repeated.borrow().iter().filter(|(_, _, others)| !others.is_empty()) {
      let mut named = others.iter().take(Self::NAMED_FILES).map(display).collect::<Vec<_>>().join(", ");
      if others.len() > Self::NAMED_FILES {
        named.push_str(&format!(" and {} more", others.len() - Self::NAMED_FILES));
      }
      log_error!(
        self.environment,
        "The same error as for {} for {} more {}: {}",
        display(first),
        others.len(),
        if others.len() == 1 { "file" } else { "files" },
        named
      );
    }
  }
}

/// The plugins that format a group of files, which are initialized when the
/// first of the files needs formatting. When every file is unchanged since the
/// last run, they aren't loaded at all.
struct GroupPlugins {
  plugins: Vec<Rc<PluginWithConfig>>,
  /// `None` when they failed to initialize, which was already reported.
  initialized: tokio::sync::OnceCell<Option<InitializedPlugins>>,
}

impl GroupPlugins {
  async fn get<TEnvironment: Environment>(&self, environment: &TEnvironment, error_logger: &ErrorCountLogger<TEnvironment>) -> Option<InitializedPlugins> {
    self
      .initialized
      .get_or_init(|| async {
        let mut plugins = Vec::with_capacity(self.plugins.len());
        for plugin in &self.plugins {
          match plugin.get_or_create_checking_config_diagnostics(environment).await {
            Ok(GetPluginResult::Success(initialized_plugin)) => plugins.push((plugin.clone(), initialized_plugin)),
            Ok(GetPluginResult::HadDiagnostics(count)) => {
              error_logger.add_error_count(count);
              return None;
            }
            Err(err) => {
              error_logger.log_error(&format!("Error creating plugin {}. Message: {}", plugin.name(), err));
              return None;
            }
          }
        }
        Some(Rc::new(plugins))
      })
      .await
      .clone()
  }
}

#[derive(Copy, Clone, PartialEq, Eq)]
pub struct EnsureStableFormat(pub bool);

#[derive(Debug, Error)]
pub enum RunForFilePathError {
  #[error("")]
  Stop,
  #[error(transparent)]
  TokioJoin(#[from] tokio::task::JoinError),
  #[error(transparent)]
  Any(#[from] anyhow::Error),
  #[error(transparent)]
  Io(#[from] std::io::Error),
}

#[derive(Debug, Error)]
#[error("Had {} error{} formatting.", error_count, if *error_count == 1 { "" } else { "s" })]
pub struct RunParallelizedFailedError {
  pub error_count: usize,
}

#[derive(Debug, Error)]
pub enum RunParallelizedError {
  #[error(transparent)]
  Any(#[from] anyhow::Error),
  #[error(transparent)]
  Failed(#[from] RunParallelizedFailedError),
  #[error(transparent)]
  Io(#[from] std::io::Error),
}

/// How many files are checked against the incremental file at a time.
const INCREMENTAL_CHECK_CHUNK_LEN: usize = if cfg!(test) { 3 } else { 4096 };

pub async fn run_parallelized<F, TEnvironment: Environment>(
  scope_and_paths: PluginsScopeAndPaths<TEnvironment>,
  environment: &TEnvironment,
  incremental_file: Option<Arc<IncrementalFile<TEnvironment>>>,
  ensure_stable_format: EnsureStableFormat,
  f: F,
) -> Result<(), RunParallelizedError>
where
  F: Fn(PathBuf, Vec<u8>, Vec<u8>, Instant, TEnvironment) -> Result<(), RunForFilePathError> + 'static + Clone + Send + Sync,
{
  if let Some(config) = &scope_and_paths.scope.config {
    log_debug!(environment, "Running for config: {}", config.origin.source.display());
  }

  let max_threads = environment.max_threads();
  let number_process_plugins = scope_and_paths.scope.process_plugin_count();
  let reduction_count = number_process_plugins + 1; // + 1 for each process plugin's possible runtime thread and this runtime's thread
  let number_threads = if max_threads > reduction_count { max_threads - reduction_count } else { 1 };
  log_debug!(environment, "Max threads: {}\nThread count: {}", max_threads, number_threads,);

  let error_logger = ErrorCountLogger::from_environment(environment);
  let file_errors = Rc::new(FileErrors::new(error_logger.clone(), environment));

  let scope = Rc::new(scope_and_paths.scope);
  let mut file_paths_by_plugins = scope_and_paths.file_paths_by_plugins.into_vec();
  // favour giving semaphore permits to ones with more items at the start
  file_paths_by_plugins.sort_by_key(|(_, file_paths)| 0i32 - file_paths.len() as i32);
  let collection_count = file_paths_by_plugins.len();
  let mut semaphores = Vec::with_capacity(collection_count);
  let mut task_works = Vec::with_capacity(collection_count);
  for (i, (plugin_names, file_paths)) in file_paths_by_plugins.into_iter().enumerate() {
    let plugins = plugin_names.names().map(|plugin_name| scope.get_plugin(plugin_name)).collect();
    let additional_thread = i < number_threads % collection_count;
    let permits = number_threads / collection_count + if additional_thread { 1 } else { 0 };
    let semaphore = Rc::new(Semaphore::new(permits));
    semaphores.push(semaphore.clone());
    task_works.push(TaskWork {
      semaphore,
      plugins,
      file_paths,
    });
  }

  let semaphores = Rc::new(semaphores);
  // one incremental check at a time across the groups, as each one uses up
  // to max_threads threads
  let incremental_check_lock = Rc::new(Semaphore::new(1));
  let cpu_task_token = CancellationToken::new();

  kprint_async_runtime::spawn({
    let semaphores = semaphores.clone();
    let environment = environment.clone();
    let cpu_task_token = cpu_task_token.clone();
    async move { run_cpu_throttling_task(&environment, number_threads, &semaphores, cpu_task_token).await }
  });

  let handles = task_works.into_iter().enumerate().map(|(index, task_work)| {
    kprint_async_runtime::spawn({
      let error_logger = error_logger.clone();
      let file_errors = file_errors.clone();
      let environment = environment.clone();
      let incremental_file = incremental_file.clone();
      let f = f.clone();
      let semaphores = semaphores.clone();
      let scope = scope.clone();
      let incremental_check_lock = incremental_check_lock.clone();
      async move {
        let _semaphore_permits = SemaphorePermitReleaser { index, semaphores };
        let plugins = Rc::new(GroupPlugins {
          plugins: task_work.plugins,
          initialized: Default::default(),
        });
        let mut format_handles = Vec::with_capacity(task_work.file_paths.len());
        let mut unchecked_file_paths = task_work.file_paths;
        // The files are checked against the incremental file a chunk at a
        // time, so formatting starts after the first chunk and a run that
        // stops early (ex. --fail-fast) doesn't check every file first.
        while !unchecked_file_paths.is_empty() && !task_work.semaphore.closed() {
          let rest = unchecked_file_paths.split_off(unchecked_file_paths.len().min(INCREMENTAL_CHECK_CHUNK_LEN));
          let chunk = std::mem::replace(&mut unchecked_file_paths, rest);
          let file_paths = match &incremental_file {
            Some(incremental_file) => {
              let _check_permit = incremental_check_lock.acquire().await;
              match remove_known_formatted_files(&environment, incremental_file, chunk, max_threads).await {
                Ok(file_paths) => file_paths,
                Err(err) => {
                  error_logger.log_error(&format!("Error checking files against the incremental cache. Message: {:#}", err));
                  return;
                }
              }
            }
            None => chunk,
          };
          for file_path in file_paths.into_iter() {
            let permit = match task_work.semaphore.acquire().await {
              Ok(permit) => permit,
              Err(_) => return, // semaphore was closed, so stop working
            };
            let semaphore = task_work.semaphore.clone();
            let environment = environment.clone();
            let f = f.clone();
            let plugins = plugins.clone();
            let error_logger = error_logger.clone();
            let file_errors = file_errors.clone();
            let scope = scope.clone();
            format_handles.push(kprint_async_runtime::spawn(async move {
              let long_format_token = CancellationToken::new();
              let stage = Rc::new(RefCell::new(FormatStage::StartingPlugins));
              kprint_async_runtime::spawn({
                let long_format_token = long_format_token.clone();
                let environment = environment.clone();
                let file_path = file_path.clone();
                let plugins = plugins.clone();
                let stage = stage.clone();
                async move {
                  let start = Instant::now();
                  tokio::select! {
                    _ = long_format_token.cancelled() => return,
                    _ = tokio::time::sleep(SLOW_FORMAT) => {}
                  }
                  let display_path = file_path.strip_prefix(environment.cwd()).unwrap_or(&file_path).display().to_string();
                  log_warn!(
                    environment,
                    "WARNING: Formatting {} is slow: after {}s, {}.",
                    display_path,
                    SLOW_FORMAT.as_secs(),
                    stage.borrow().describe(&plugins.plugins)
                  );
                  long_format_token.cancelled().await;
                  log_warn!(environment, "Finished formatting {} after {:.1}s.", display_path, start.elapsed().as_secs_f64());
                }
              });
              let result = run_for_file_path(environment, scope, plugins, &error_logger, file_path.clone(), ensure_stable_format, &stage, f).await;
              long_format_token.cancel();
              if let Err(err) = result {
                match err {
                  RunForFilePathError::Stop => {
                    semaphore.close(); // stop formatting
                  }
                  RunForFilePathError::Any(err) => {
                    if let Some(err) = crate::plugins::maybe_critical_format_error(&err) {
                      error_logger.log_error(&format!(
                        "Critical error formatting {}. Cannot continue. Message: {}",
                        file_path.display(),
                        kprint_plugin_types::error_to_string(err)
                      ));
                      semaphore.close(); // stop formatting
                    } else {
                      file_errors.log(&file_path, format!("{:#}", err));
                    }
                  }
                  RunForFilePathError::TokioJoin(_) | RunForFilePathError::Io(_) => {
                    file_errors.log(&file_path, format!("{:#}", err));
                  }
                }
              }
              // drop the semaphore permit when we're all done
              drop(permit);
            }));
          }
        }
        future::join_all(format_handles).await;
      }
    })
  });
  future::join_all(handles).await;
  file_errors.log_repeated();

  cpu_task_token.cancel();

  let error_count = error_logger.get_error_count();
  return if error_count == 0 {
    Ok(())
  } else {
    Err(RunParallelizedError::Failed(RunParallelizedFailedError { error_count }))
  };

  /// Removes the files the incremental file knows are formatted. This checks
  /// them in bulk on several threads because sending an unchanged file through
  /// the formatting pipeline (permits, tasks and a blocking thread per file)
  /// costs far more than checking it.
  async fn remove_known_formatted_files<TEnvironment: Environment>(
    environment: &TEnvironment,
    incremental_file: &Arc<IncrementalFile<TEnvironment>>,
    file_paths: Vec<PathBuf>,
    thread_count: usize,
  ) -> Result<Vec<PathBuf>, tokio::task::JoinError> {
    if !incremental_file.has_known_files() {
      // nothing is known, so checking would only read every file twice
      return Ok(file_paths);
    }
    let start = Instant::now();
    let total_count = file_paths.len();
    let thread_environment = environment.clone();
    let incremental_file = incremental_file.clone();
    // starting threads costs more than checking a few hundred files
    const FILES_PER_THREAD: usize = 500;
    let thread_count = thread_count.min(file_paths.len().div_ceil(FILES_PER_THREAD)).max(1);
    let result = kprint_async_runtime::spawn_blocking(move || {
      if thread_count == 1 {
        return file_paths
          .into_iter()
          .filter(|file_path| !is_known_formatted(&thread_environment, &incremental_file, file_path))
          .collect::<Vec<_>>();
      }
      let chunk_size = file_paths.len().div_ceil(thread_count);
      std::thread::scope(|scope| {
        let handles = file_paths
          .chunks(chunk_size)
          .map(|chunk| {
            let environment = &thread_environment;
            let incremental_file = &incremental_file;
            scope.spawn(move || {
              chunk
                .iter()
                .filter(|file_path| !is_known_formatted(environment, incremental_file, file_path))
                .cloned()
                .collect::<Vec<_>>()
            })
          })
          .collect::<Vec<_>>();
        handles.into_iter().flat_map(|handle| handle.join().unwrap()).collect::<Vec<_>>()
      })
    })
    .await;
    // a failed check (ex. a thread couldn't start) is an error rather than
    // every file counting as unchanged
    let file_paths = result?;
    log_debug!(
      environment,
      "{} of {} file(s) unchanged since formatted ({}ms)",
      total_count - file_paths.len(),
      total_count,
      start.elapsed().as_millis()
    );
    Ok(file_paths)
  }

  fn is_known_formatted<TEnvironment: Environment>(environment: &TEnvironment, incremental_file: &IncrementalFile<TEnvironment>, file_path: &Path) -> bool {
    let metadata = environment.fs_metadata(file_path).ok().and_then(|metadata| {
      Some(FileMetadata {
        len: metadata.len(),
        modified: metadata.modified().ok()?,
      })
    });
    if let Some(metadata) = &metadata
      && incremental_file.is_file_known_formatted_by_metadata(file_path, metadata)
    {
      log_debug!(environment, "No change: {} (unmodified, so not read)", file_path.display());
      return true;
    }
    // an error reading it is reported when formatting it
    let Ok(file_text) = environment.read_file_bytes(file_path) else {
      return false;
    };
    if incremental_file.is_file_known_formatted(file_path, &file_text, metadata.as_ref()) {
      log_debug!(environment, "No change: {}", file_path.display());
      true
    } else {
      false
    }
  }

  #[inline]
  #[allow(clippy::too_many_arguments)]
  async fn run_for_file_path<F, TEnvironment: Environment>(
    environment: TEnvironment,
    scope: Rc<PluginsScope<TEnvironment>>,
    group_plugins: Rc<GroupPlugins>,
    error_logger: &ErrorCountLogger<TEnvironment>,
    file_path: PathBuf,
    ensure_stable_format: EnsureStableFormat,
    stage: &Rc<RefCell<FormatStage>>,
    f: F,
  ) -> Result<(), RunForFilePathError>
  where
    F: Fn(PathBuf, Vec<u8>, Vec<u8>, Instant, TEnvironment) -> Result<(), RunForFilePathError> + 'static + Clone + Send + Sync,
  {
    // the plugins that failed to initialize were reported, and as no file of
    // the group can be formatted, stopping the group skips reading the rest
    let Some(plugins) = group_plugins.get(&environment, error_logger).await else {
      return Err(RunForFilePathError::Stop);
    };

    // it's a big perf improvement to do this work on a blocking thread
    stage.replace(FormatStage::Reading);
    let (file_path, file_text, environment) = kprint_async_runtime::spawn_blocking(move || {
      let file_text = environment.read_file_bytes(&file_path)?;
      Ok::<_, std::io::Error>((file_path, file_text, environment))
    })
    .await??;

    let (start_instant, formatted_text, _) =
      run_single_pass_for_file_path(environment.clone(), scope.clone(), plugins.clone(), file_path.clone(), &file_text, stage, 1).await?;

    let formatted_text = if ensure_stable_format.0 && formatted_text != file_text {
      get_stabilized_format_text(environment.clone(), scope, plugins, file_path.clone(), formatted_text, stage).await?
    } else {
      formatted_text
    };

    stage.replace(FormatStage::Finishing);
    kprint_async_runtime::spawn_blocking(move || f(file_path, file_text, formatted_text, start_instant, environment)).await??;

    Ok(())
  }

  async fn get_stabilized_format_text<TEnvironment: Environment>(
    environment: TEnvironment,
    scope: Rc<PluginsScope<TEnvironment>>,
    plugins: Rc<Vec<(Rc<PluginWithConfig>, InitializedPluginWithConfig)>>,
    file_path: PathBuf,
    mut formatted_text: Vec<u8>,
    stage: &Rc<RefCell<FormatStage>>,
  ) -> Result<Vec<u8>> {
    log_debug!(environment, "Ensuring stable format: {}", file_path.display());
    let mut count = 0;
    let mut changed_by: Vec<String>;
    loop {
      match run_single_pass_for_file_path(
        environment.clone(),
        scope.clone(),
        plugins.clone(),
        file_path.clone(),
        &formatted_text,
        stage,
        count + 2,
      )
      .await
      {
        Ok((_, next_pass_text, next_changed_by)) => {
          if next_pass_text == formatted_text {
            return Ok(formatted_text);
          } else {
            formatted_text = next_pass_text;
            changed_by = next_changed_by;
            log_debug!(environment, "Ensuring stable format failed on try {}: {}", count + 1, file_path.display());
          }
        }
        Err(err) => {
          bail!(
            concat!(
              "Formatting succeeded initially, but failed when ensuring a stable format. ",
              "This is most likely a bug in the plugin where the text it produces is not syntatically correct. ",
              "Please report this as a bug to the plugin that formatted this file.\n\n{:#}"
            ),
            err,
          )
        }
      }
      count += 1;
      if count == 5 {
        bail!("Formatting not stable. Bailed after {} tries. {}", count, unstable_reason(&changed_by));
      }
    }
  }

  async fn run_single_pass_for_file_path<TEnvironment: Environment>(
    environment: TEnvironment,
    scope: Rc<PluginsScope<TEnvironment>>,
    plugins: Rc<Vec<(Rc<PluginWithConfig>, InitializedPluginWithConfig)>>,
    file_path: PathBuf,
    file_text: &[u8],
    stage: &Rc<RefCell<FormatStage>>,
    pass: usize,
  ) -> Result<(Instant, Vec<u8>, Vec<String>)> {
    let start_instant = Instant::now();
    let original_text = file_text;
    let mut file_text = Cow::Borrowed(file_text);
    let mut changed_by = Vec::new();
    let plugins_len = plugins.len();
    for (i, (plugin, initialized_plugin)) in plugins.iter().enumerate() {
      stage.replace(FormatStage::Formatting { plugin: i, pass });
      let start_instant = Instant::now();
      let format_text_result = initialized_plugin
        .format_text(InitializedPluginWithConfigFormatRequest {
          file_path: file_path.to_path_buf(),
          file_bytes: file_text.to_vec(),
          range: None,
          override_config: plugin.get_config_file_overrides_for_path(&file_path),
          on_host_format: observe_host_format(
            scope.create_host_format_callback(plugin.clone(), file_path.clone()),
            &scope,
            plugin,
            &file_path,
            stage,
          ),
          token: Arc::new(NullCancellationToken),
        })
        .await;
      log_debug!(
        environment,
        "Formatted file: {} in {}ms{}",
        file_path.display(),
        start_instant.elapsed().as_millis(),
        if plugins_len > 1 {
          format!(" (Plugin {}/{})", i + 1, plugins_len)
        } else {
          String::new()
        },
      );
      if let Some(text) = format_text_result? {
        if text != *file_text {
          changed_by.push(plugin.info().config_key.clone());
        }
        file_text = Cow::Owned(text)
      }
    }

    // some heuristic to stop plugins accidentally formatting a file to empty
    const MIN_CHARS_TO_EMPTY: usize = 300;
    if file_text.len() < 100 && original_text.len() > MIN_CHARS_TO_EMPTY {
      let original_text = String::from_utf8_lossy(original_text);
      let new_text = String::from_utf8_lossy(&file_text);
      if original_text.trim().len() > MIN_CHARS_TO_EMPTY && new_text.trim().is_empty() {
        bail!(
          concat!(
            "The original file text was greater than {} characters, but the formatted text was empty. ",
            "Most likely this is a bug in the plugin and the dprint CLI has prevented the plugin from ",
            "formatting the file to an empty file. Please report this scenario.",
          ),
          MIN_CHARS_TO_EMPTY
        )
      }
    }

    Ok((start_instant, file_text.into_owned(), changed_by))
  }
}

/// Notes in `stage` which code `plugin` embeds in the file at `file_path` the
/// host is formatting, and with which plugins, while it does. Only the code
/// the file's own plugins embed is noted, not code embedded in that code.
fn observe_host_format<TEnvironment: Environment>(
  on_host_format: HostFormatCallback,
  scope: &Rc<PluginsScope<TEnvironment>>,
  plugin: &Rc<PluginWithConfig>,
  file_path: &Path,
  stage: &Rc<RefCell<FormatStage>>,
) -> HostFormatCallback {
  let scope = scope.clone();
  let plugin = plugin.clone();
  let file_path = file_path.to_path_buf();
  let stage = stage.clone();
  Rc::new(move |request: HostFormatRequest| {
    let embedded = EmbeddedFormat {
      file_path: request.file_path.clone(),
      plugin_names: scope.embedded_format_plugin_names(&plugin, &file_path, &request),
    };
    let outer = stage.borrow().clone();
    if let FormatStage::Formatting { plugin, pass } = outer {
      stage.replace(FormatStage::FormattingEmbedded { plugin, pass, embedded });
    }
    let formatted = on_host_format(request);
    let stage = stage.clone();
    async move {
      let result = formatted.await;
      stage.replace(outer);
      result
    }
    .boxed_local()
  })
}

fn unstable_reason(changed_by: &[String]) -> String {
  match changed_by {
    [plugin] => format!("The {} plugin formats the file differently each time, which is a bug in it.", plugin),
    [first @ .., last] => format!("{} and {} keep changing each other's output.", first.join(", "), last),
    [] => "No plugin changed the file in the last try.".to_string(),
  }
}

fn target_cpu_decrease_bound(number_threads: usize) -> u8 {
  if number_threads < 3 {
    100 // never decrease
  } else if number_threads >= 50 {
    97
  } else {
    std::cmp::max((100f64 - 100f64 / (number_threads as f64)) as u8, 50)
  }
}

fn target_cpu_increase_bound(number_threads: usize) -> u8 {
  if number_threads < 3 {
    0 // never increase
  } else if number_threads >= 50 {
    95
  } else {
    let target_cpu = target_cpu_decrease_bound(number_threads);
    let ratio = number_threads as f64 / 60f64;
    let target_cpu = target_cpu - std::cmp::min((5f64 * (1f64 - ratio)) as u8, target_cpu);
    target_cpu - std::cmp::min(target_cpu, (100f64 / number_threads as f64) as u8)
  }
}

async fn run_cpu_throttling_task(environment: &impl Environment, number_threads: usize, semaphores: &[Rc<Semaphore>], cpu_task_token: CancellationToken) {
  if environment.is_ci() {
    // don't bother doing this on the CI as we should be the only thing running
    return;
  }

  // It's ok to go full out for a few seconds on the person's machine
  // when they initially start formatting, but as they take a few seconds
  // to switch to do something else, we should then start throttling the CPU
  tokio::select! {
    _ = cpu_task_token.cancelled() => {
      return; // exit
    }
    _ = tokio::time::sleep(Duration::from_secs(5)) => {
    }
  }

  let mut throttled_times = 0;
  let decrease_bound = target_cpu_decrease_bound(number_threads);
  let increase_bound = target_cpu_increase_bound(number_threads);
  let mut last_cpu_usage = 0;

  // now check the CPU usage every few seconds and throttle
  // the amount of work being done so that we don't completely
  // takeover someone's computer
  loop {
    let cpu_usage = environment.cpu_usage().await;
    log_debug!(environment, "CPU usage: {}%", cpu_usage);
    if cpu_usage > decrease_bound {
      if throttle_cpu(semaphores) {
        log_debug!(environment, "High CPU. Reducing parallelism.");
        throttled_times += 1;
      }
    } else if throttled_times > 0 && last_cpu_usage < increase_bound && cpu_usage < increase_bound {
      // Whatever was running in the background might
      // not be using as much CPU at this point, so increase
      // the permits
      add_permits(semaphores, 1);
      throttled_times -= 1;
      log_debug!(environment, "Low CPU. Increasing parallelism.");
    }
    last_cpu_usage = cpu_usage;

    // wait a couple seconds before re-checking cpu usage
    tokio::select! {
      _ = cpu_task_token.cancelled() => {
        return; // exit
      }
      _ = tokio::time::sleep(Duration::from_secs(2)) => {
      }
    }
  }
}

fn throttle_cpu(semaphores: &[Rc<Semaphore>]) -> bool {
  let mut best_match: Option<&Rc<Semaphore>> = None;
  let mut total_max_permits = 0;
  for semaphore in semaphores.iter() {
    if semaphore.closed() || semaphore.max_permits() == 0 {
      continue;
    }
    if semaphore.acquired_permits() > semaphore.max_permits() {
      // The previous adjustment hasn't yet been applied. Wait for that
      // to complete so we don't over scale down.
      best_match = None;
      break;
    }
    total_max_permits += semaphore.max_permits();

    match &best_match {
      Some(current_best_match) => {
        if current_best_match.max_permits() < semaphore.max_permits() {
          best_match = Some(semaphore);
        }
      }
      None => {
        best_match = Some(semaphore);
      }
    }
  }

  // always ensure there will be at least 1 permit running
  if total_max_permits <= 1 {
    return false;
  }

  match best_match {
    Some(best_match) => {
      best_match.remove_permits(1);
      true
    }
    None => false,
  }
}

/// Ensures all semaphores are released on drop
/// so that other threads can do more work.
struct SemaphorePermitReleaser {
  index: usize,
  semaphores: Rc<Vec<Rc<Semaphore>>>,
}

impl Drop for SemaphorePermitReleaser {
  fn drop(&mut self) {
    // release the permits to other semaphores so other tasks start doing more work
    self.semaphores[self.index].close();
    let amount = self.semaphores[self.index].max_permits();
    add_permits(&self.semaphores, amount)
  }
}

fn add_permits(semaphores: &[Rc<Semaphore>], amount: usize) {
  let mut remaining_semaphores = semaphores.iter().filter(|s| !s.closed()).collect::<Vec<_>>();
  // favour giving permits to tasks with less permits... this should more ideally
  // give permits to batches that look like they will take the longest to complete
  remaining_semaphores.sort_by_key(|s| s.max_permits());
  let remaining_len = remaining_semaphores.len();
  for (i, semaphore) in remaining_semaphores.iter_mut().enumerate() {
    let additional_permit = i < amount % remaining_len;
    let new_permits = amount / remaining_len + if additional_permit { 1 } else { 0 };
    if new_permits > 0 {
      semaphore.add_permits(new_permits);
    }
  }
}

#[cfg(test)]
mod test {
  use std::rc::Rc;

  use super::*;
  use crate::utils::Semaphore;

  #[test]
  fn target_cpu_calc() {
    run_test(0, 0..100);
    run_test(1, 0..100);
    run_test(2, 0..100);
    run_test(3, 29..66);
    run_test(4, 46..75);
    run_test(5, 56..80);
    run_test(8, 71..87);
    run_test(10, 76..90);
    run_test(20, 87..95);
    run_test(30, 91..96);
    run_test(40, 94..97);
    run_test(49, 95..97);
    run_test(50, 95..97);
    run_test(100, 95..97);
    run_test(200, 95..97);

    #[track_caller]
    fn run_test(input: usize, bound: std::ops::Range<u8>) {
      let increase_bound = target_cpu_increase_bound(input);
      let decrease_bound = target_cpu_decrease_bound(input);
      assert_eq!(increase_bound..decrease_bound, bound);
    }
  }

  #[tokio::test]
  async fn test_throttle_cpu() {
    let semaphore1 = Rc::new(Semaphore::new(1));
    let semaphore2 = Rc::new(Semaphore::new(2));
    let permit1 = semaphore1.acquire().await;
    let permit2 = semaphore2.acquire().await;
    let permit3 = semaphore2.acquire().await;
    let semaphores = vec![semaphore1, semaphore2];
    assert!(throttle_cpu(&semaphores));
    assert_eq!(semaphores[1].max_permits(), 1);
    // still a pending removal
    assert!(!throttle_cpu(&semaphores));
    drop(permit2);
    assert!(throttle_cpu(&semaphores));
    assert_eq!(semaphores[0].max_permits(), 0);
    assert_eq!(semaphores[1].max_permits(), 1);
    // still a pending removal
    assert!(!throttle_cpu(&semaphores));
    drop(permit3);
    // only one permit remaining
    assert!(!throttle_cpu(&semaphores));
    drop(permit1);
    assert!(!throttle_cpu(&semaphores));
  }
}
