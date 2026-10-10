use anyhow::Result;
use anyhow::anyhow;
use kprint_async_runtime::FutureExt;
use kprint_async_runtime::LocalBoxFuture;
use kprint_async_runtime::async_trait;
use kprint_plugin_types::CancellationToken;
use kprint_plugin_types::CheckConfigUpdatesMessage;
use kprint_plugin_types::ConfigChange;
use kprint_plugin_types::CriticalFormatError;
use kprint_plugin_types::FileMatchingInfo;
use kprint_plugin_types::FormatConfigId;
use kprint_plugin_types::FormatError;
use kprint_plugin_types::FormatRange;
use kprint_plugin_types::FormatResult;
use kprint_plugin_types::HostFormatCallback;
use kprint_plugin_types::HostFormatRequest;
use std::cell::Cell;
use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::rc::Weak;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;
use sys_traits::FsMetadata;
use sys_traits::FsMetadataValue;

use kprint_configuration::ConfigKeyMap;
use kprint_configuration::ConfigurationDiagnostic;
use kprint_plugin_types::PluginInfo;

use super::WasmHostFormatSender;
use super::create_pools_import_object;
use super::engine_choice;
use super::engine_choice::FormatEngine;
use super::engine_choice::FormatRate;
use super::instance::InitializedWasmPluginInstance;
use super::instance::LogFn;
use super::instance::create_host_state;
use super::interpreter::InterpretedModule;
use super::load_instance;
use super::load_instance::WasmModule;
use crate::environment::PluginEnvironment as Environment;
use crate::plugins::FormatConfig;
use crate::plugins::InitializedPlugin;
use crate::plugins::InitializedPluginFormatRequest;
use crate::plugins::Plugin;
use crate::plugins::PluginResolutionCache;
use crate::plugins::implementations::wasm::create_wasm_plugin_instance;

/// Loads a module of a plugin.
pub type LoadModule<T> = Box<dyn Fn() -> LocalBoxFuture<'static, Result<T>>>;

/// How a Wasm plugin loads its modules.
pub struct WasmPluginModules {
  /// Loads the module the interpreter runs, for the calls that come before
  /// formatting (its resolved configuration, the files it formats, ...) and
  /// to format when compiling doesn't pay off.
  pub load_interpreted: LoadModule<InterpretedModule>,
  /// Loads the native module, compiling it first when needed.
  pub load_native: LoadModule<WasmModule>,
  /// Loads existing native code without compiling a missing or unusable cache.
  pub load_cached_native: LoadModule<Option<WasmModule>>,
  /// Where the plugin's module is kept.
  pub wasm_module_path: PathBuf,
  /// Where the native module is kept once it's compiled.
  pub native_module_path: PathBuf,
  /// Where the plugin's formatting rate in the interpreter is kept.
  pub format_rate_path: PathBuf,
}

/// How long a module that failed to load isn't tried again, as loading it
/// again can mean downloading and compiling the plugin. A CLI run is over
/// well before then, so it doesn't try again, while a long running process
/// such as `dprint lsp` gets over a failure that was temporary (ex. no
/// network while the plugin needed downloading).
const LOAD_FAILURE_RETRY_AFTER: Duration = Duration::from_secs(30);

enum ModuleLoad<T> {
  Loaded(T),
  Failed { message: String, at: Instant },
}

/// A module that's loaded the first time it's needed.
struct LazyModule<T: Clone> {
  load: LoadModule<T>,
  module: tokio::sync::Mutex<Option<ModuleLoad<T>>>,
  load_failure_retry_after: Duration,
}

impl<T: Clone> LazyModule<T> {
  fn new(load: LoadModule<T>) -> Self {
    Self {
      load,
      module: Default::default(),
      load_failure_retry_after: LOAD_FAILURE_RETRY_AFTER,
    }
  }

  fn is_loaded(&self) -> bool {
    self.module.try_lock().is_ok_and(|module| matches!(&*module, Some(ModuleLoad::Loaded(_))))
  }

  async fn set(&self, loaded: T) {
    *self.module.lock().await = Some(ModuleLoad::Loaded(loaded));
  }

  /// The module, loaded the first time it's needed. A failure is returned
  /// again until `load_failure_retry_after` passed.
  async fn get(&self) -> Result<T> {
    let mut module = self.module.lock().await;
    match &*module {
      Some(ModuleLoad::Loaded(module)) => return Ok(module.clone()),
      Some(ModuleLoad::Failed { message, at }) if at.elapsed() < self.load_failure_retry_after => return Err(anyhow!("{}", message)),
      _ => {}
    }
    match (self.load)().await {
      Ok(loaded) => {
        *module = Some(ModuleLoad::Loaded(loaded.clone()));
        Ok(loaded)
      }
      Err(err) => {
        let message = format!("{:#}", err);
        *module = Some(ModuleLoad::Failed {
          message: message.clone(),
          at: Instant::now(),
        });
        Err(anyhow!(message))
      }
    }
  }
}

pub struct WasmPlugin<TEnvironment: Environment> {
  interpreted: LazyModule<InterpretedModule>,
  formatting: Rc<Formatting<TEnvironment>>,
  wasm_module_path: PathBuf,
  resolution_cache: PluginResolutionCache,
  environment: TEnvironment,
  plugin_info: PluginInfo,
}

/// How a Wasm plugin formats, shared by the plugin and its initialized
/// plugin.
struct Formatting<TEnvironment: Environment> {
  native: Rc<LazyModule<WasmModule>>,
  load_cached_native: LoadModule<Option<WasmModule>>,
  /// A cached module failed preflight and needs compiling again.
  invalid_native: Cell<bool>,
  native_module_path: PathBuf,
  native_exists: Cell<Option<bool>>,
  format_rate_path: PathBuf,
  /// What `choose_format_engine` chose for this run. A process that formats
  /// without choosing first (ex. `dprint lsp`) has none.
  chosen: Cell<Option<FormatEngine>>,
  environment: TEnvironment,
}

impl<TEnvironment: Environment> Formatting<TEnvironment> {
  fn has_native_code(&self) -> bool {
    self.native.is_loaded()
      || (!self.invalid_native.get()
        && self.native_exists.get().unwrap_or_else(|| {
          let exists = self.environment.path_exists(&self.native_module_path);
          self.native_exists.set(Some(exists));
          exists
        }))
  }

  /// How the plugin's next instance formats. Native code that exists is
  /// always used, as loading it takes milliseconds.
  fn engine(&self) -> FormatEngine {
    if self.has_native_code() {
      FormatEngine::Native
    } else {
      self
        .chosen
        .get()
        .or_else(|| engine_choice::forced(&self.environment))
        .unwrap_or(FormatEngine::Interpreter)
    }
  }
}

impl<TEnvironment: Environment> WasmPlugin<TEnvironment> {
  /// Creates the plugin, which loads its modules once it's used.
  pub fn new(plugin_info: PluginInfo, modules: WasmPluginModules, resolution_cache: PluginResolutionCache, environment: TEnvironment) -> Self {
    WasmPlugin {
      interpreted: LazyModule::new(modules.load_interpreted),
      formatting: Rc::new(Formatting {
        native: Rc::new(LazyModule::new(modules.load_native)),
        load_cached_native: modules.load_cached_native,
        invalid_native: Cell::new(false),
        native_module_path: modules.native_module_path,
        native_exists: Cell::new(None),
        format_rate_path: modules.format_rate_path,
        chosen: Cell::new(None),
        environment: environment.clone(),
      }),
      wasm_module_path: modules.wasm_module_path,
      resolution_cache,
      environment,
      plugin_info,
    }
  }
}

#[async_trait(?Send)]
impl<TEnvironment: Environment> Plugin for WasmPlugin<TEnvironment> {
  fn info(&self) -> &PluginInfo {
    &self.plugin_info
  }

  fn is_process_plugin(&self) -> bool {
    false
  }

  fn resolution_cache(&self) -> Option<&PluginResolutionCache> {
    Some(&self.resolution_cache)
  }

  async fn prepare_format_engine(&self) -> Result<()> {
    if self.formatting.native.is_loaded() {
      return Ok(());
    }
    match (self.formatting.load_cached_native)().await {
      Ok(Some(module)) => {
        self.formatting.native.set(module).await;
        self.formatting.invalid_native.set(false);
      }
      Ok(None) => {
        self.formatting.native_exists.set(Some(false));
      }
      Err(err) => {
        log_debug!(self.environment, "Error loading cached native code for {}: {:#}", self.plugin_info.name, err);
        self.formatting.invalid_native.set(true);
        return Err(err);
      }
    }
    Ok(())
  }

  fn chooses_format_engine(&self) -> bool {
    !self.formatting.has_native_code()
  }

  fn choose_format_engine(&self, bytes_to_format: u64) {
    let engine = if self.formatting.invalid_native.get() {
      // Preserve native cache recovery, but include it in the compile budget.
      if bytes_to_format == 0 {
        FormatEngine::Interpreter
      } else {
        FormatEngine::Native
      }
    } else {
      engine_choice::forced(&self.environment).unwrap_or_else(|| {
        let module_len = match self.environment.fs_metadata(&self.wasm_module_path) {
          Ok(metadata) => metadata.len(),
          // it's set up again when it's loaded, so this is a guess
          Err(_) => 0,
        };
        let rate = engine_choice::read_rate(&self.environment, &self.formatting.format_rate_path);
        engine_choice::choose(module_len, bytes_to_format, rate.as_ref())
      })
    };
    log_debug!(self.environment, "{} formats {} bytes: {:?}", self.plugin_info.name, bytes_to_format, engine);
    self.formatting.chosen.set(Some(engine));
  }

  fn compiles_to_format(&self) -> bool {
    self.formatting.chosen.get() == Some(FormatEngine::Native) && !self.formatting.has_native_code()
  }

  async fn initialize(&self) -> Result<Rc<dyn InitializedPlugin>> {
    // the calls before formatting need the interpreted module, so a plugin
    // whose module doesn't load fails here
    let interpreted = self.interpreted.get().await?;
    let environment = self.environment.clone();
    let plugin_name = self.info().name.clone();
    let log: LogFn = {
      let environment = environment.clone();
      let plugin_name = plugin_name.clone();
      Arc::new(move |text: &str| environment.log_stderr_with_context(text, &plugin_name))
    };
    let plugin: Rc<dyn InitializedPlugin> = Rc::new(InitializedWasmPlugin::new(
      plugin_name,
      Arc::new(Interpreter::new(interpreted, log)),
      self.formatting.clone(),
      environment,
    ));

    Ok(plugin)
  }
}

/// Runs the calls that come before formatting in an interpreted instance of
/// the plugin.
struct Interpreter {
  module: InterpretedModule,
  /// Gets what the plugin prints.
  log: LogFn,
  instance: parking_lot::Mutex<Option<Box<dyn InitializedWasmPluginInstance + Send>>>,
}

impl Interpreter {
  fn new(module: InterpretedModule, log: LogFn) -> Self {
    Self {
      module,
      log,
      instance: Default::default(),
    }
  }

  fn run<T>(&self, call: impl FnOnce(&mut dyn InitializedWasmPluginInstance) -> Result<T>) -> Result<T> {
    let mut instance = self.instance.lock();
    let result = match &mut *instance {
      Some(instance) => call(instance.as_mut()),
      None => call(instance.insert(self.module.instantiate(self.log.clone())?).as_mut()),
    };
    if result.is_err() {
      // a call that failed (ex. the plugin panicked) can leave the instance
      // broken, so the next call gets a new one
      *instance = None;
    }
    result
  }
}

struct WasmPluginFormatMessage {
  file_path: PathBuf,
  file_bytes: Vec<u8>,
  range: FormatRange,
  config: Arc<FormatConfig>,
  override_config: ConfigKeyMap,
  token: Arc<dyn CancellationToken>,
}

type WasmResponseSender<T> = tokio::sync::oneshot::Sender<T>;

enum WasmPluginRequest {
  Format(Arc<WasmPluginFormatMessage>, WasmResponseSender<FormatResult>),
  ReleaseConfig(FormatConfigId),
}

type WasmPluginSender = std::sync::mpsc::Sender<WasmPluginRequest>;

#[derive(Clone)]
struct InstanceState {
  host_format_callback: HostFormatCallback,
}

struct WasmPluginSenderWithState {
  sender: Rc<WasmPluginSender>,
  instance_state_cell: Rc<RefCell<Option<InstanceState>>>,
  engine: FormatEngine,
}

/// Creates an instance that formats, on the thread it formats on.
type CreateFormatInstance = Box<dyn FnOnce(WasmHostFormatSender) -> Result<Box<dyn InitializedWasmPluginInstance + Send>> + Send>;

pub struct InitializedWasmPlugin<TEnvironment: Environment> {
  name: String,
  interpreter: Arc<Interpreter>,
  pending_instances: RefCell<Vec<WasmPluginSenderWithState>>,
  /// Every instance that formats, including the ones formatting right now.
  instance_senders: RefCell<Vec<Weak<WasmPluginSender>>>,
  formatting: Rc<Formatting<TEnvironment>>,
  /// What the plugin formatted in the interpreter in this process.
  interpreted: Arc<parking_lot::Mutex<FormatRate>>,
  /// Whether the plugin started compiling in the background because it
  /// formatted enough in the interpreter (see `compile_once_it_pays_off`).
  compiling_in_background: Cell<bool>,
  /// How long the plugin formats in the interpreter before it compiles in
  /// the background: about as long as compiling it takes.
  compile_after: Duration,
  environment: TEnvironment,
}

impl<TEnvironment: Environment> Drop for InitializedWasmPlugin<TEnvironment> {
  fn drop(&mut self) {
    let start = Instant::now();
    let len = {
      let instances = {
        let mut instances = self.pending_instances.borrow_mut();
        std::mem::take(&mut *instances)
      };

      instances.len()
    };
    let interpreted = *self.interpreted.lock();
    if interpreted.bytes > 0 {
      engine_choice::add_to_rate(&self.environment, &self.formatting.format_rate_path, interpreted);
    }
    log_debug!(
      self.environment,
      "Dropped {} ({} instances) in {}ms",
      self.name,
      len,
      start.elapsed().as_millis()
    );
  }
}

impl<TEnvironment: Environment> InitializedWasmPlugin<TEnvironment> {
  fn new(name: String, interpreter: Arc<Interpreter>, formatting: Rc<Formatting<TEnvironment>>, environment: TEnvironment) -> Self {
    let compile_after = engine_choice::compile_time(interpreter.module.wasm_len() as u64);
    Self {
      name,
      interpreter,
      pending_instances: Default::default(),
      instance_senders: Default::default(),
      formatting,
      interpreted: Default::default(),
      compiling_in_background: Cell::new(false),
      compile_after,
      environment,
    }
  }

  /// Runs a call that comes before formatting in the interpreter, on a
  /// blocking thread so several plugins run their calls at once.
  async fn interpret<T: Send + 'static>(&self, call: impl FnOnce(&mut dyn InitializedWasmPluginInstance) -> Result<T> + Send + 'static) -> Result<T> {
    let interpreter = self.interpreter.clone();
    kprint_async_runtime::spawn_blocking(move || interpreter.run(call)).await?
  }

  async fn with_instance<T>(
    &self,
    instance_state: Option<InstanceState>,
    action: impl Fn(Rc<WasmPluginSender>) -> LocalBoxFuture<'static, Result<T>>,
  ) -> Result<T> {
    let plugin = match self.get_or_create_instance(instance_state.clone()).await {
      Ok(instance) => instance,
      Err(err) => return Err(CriticalFormatError(FormatError::new(err)).into()),
    };
    let result = action(plugin.sender.clone()).await;
    match result {
      Ok(result) => {
        self.release_instance(plugin);
        Ok(result)
      }
      Err(original_err) if crate::plugins::maybe_critical_format_error(&original_err).is_some() => {
        let plugin = match self.get_or_create_instance(instance_state).await {
          Ok(plugin) => plugin,
          Err(err) => return Err(CriticalFormatError(FormatError::new(err)).into()),
        };

        // try again
        let result = action(plugin.sender.clone()).await;
        match result {
          Ok(result) => {
            self.release_instance(plugin);
            Ok(result)
          }
          Err(reinitialize_err) if crate::plugins::maybe_critical_format_error(&original_err).is_some() => Err(
            CriticalFormatError(FormatError::new(anyhow!(
              concat!(
                "Originally panicked in {}, then failed reinitialize. ",
                "This may be a bug in the plugin, the dprint cli is out of date, or the ",
                "plugin is out of date.\nOriginal error: {}\nReinitialize error: {}",
              ),
              self.name,
              original_err,
              reinitialize_err,
            )))
            .into(),
          ),
          Err(err) => {
            self.release_instance(plugin);
            Err(err)
          }
        }
      }
      Err(err) => {
        self.release_instance(plugin);
        Err(err)
      }
    }
  }

  async fn get_or_create_instance(&self, instance_state: Option<InstanceState>) -> Result<WasmPluginSenderWithState> {
    let engine = self.formatting.engine();
    let maybe_instance = {
      let mut instances = self.pending_instances.borrow_mut();
      // an instance of the other engine isn't used again (ex. interpreted
      // ones once the native code is there)
      instances.retain(|instance| instance.engine == engine);
      instances.pop()
    };
    let plugin_sender = match maybe_instance {
      Some(instance) => instance,
      None => self.create_instance(engine).await?,
    };
    *plugin_sender.instance_state_cell.borrow_mut() = instance_state;
    Ok(plugin_sender)
  }

  fn release_instance(&self, plugin_sender: WasmPluginSenderWithState) {
    *plugin_sender.instance_state_cell.borrow_mut() = None;
    self.pending_instances.borrow_mut().push(plugin_sender);
  }

  async fn create_instance(&self, engine: FormatEngine) -> Result<WasmPluginSenderWithState> {
    let create: CreateFormatInstance = match engine {
      FormatEngine::Native => {
        // compiled the first time the plugin formats natively
        let module = self.formatting.native.get().await?;
        let log = self.interpreter.log.clone();
        Box::new(move |host_format_sender| {
          let (linker, host_state) = create_pools_import_object(log, module.version(), module.engine(), host_format_sender)?;
          let mut store = module.new_store(host_state);
          let instance = load_instance(&mut store, &module, &linker)?;
          create_wasm_plugin_instance(store, instance)
        })
      }
      FormatEngine::Interpreter => {
        let module = self.interpreter.module.clone();
        let log = self.interpreter.log.clone();
        Box::new(move |host_format_sender| module.instantiate_to_format(create_host_state(module.version(), log, host_format_sender)))
      }
    };
    let start_instant = Instant::now();
    log_debug!(self.environment, "Creating instance of {} ({:?})", self.name, engine);

    let (host_format_tx, mut host_format_rx) = tokio::sync::mpsc::unbounded_channel::<(HostFormatRequest, std::sync::mpsc::Sender<FormatResult>)>();
    let instance_state_cell: Rc<RefCell<Option<InstanceState>>> = Default::default();
    // the time the instance waited for other plugins to format for it (ex.
    // markdown's code blocks), which isn't its own formatting
    let host_format_nanos = Arc::new(AtomicU64::new(0));

    kprint_async_runtime::spawn({
      let instance_state_cell = instance_state_cell.clone();
      let host_format_nanos = host_format_nanos.clone();
      async move {
        while let Some((request, sender)) = host_format_rx.recv().await {
          let instance_state = instance_state_cell.borrow().clone();
          match instance_state {
            Some(instance_state) => {
              let start = Instant::now();
              let message = (instance_state.host_format_callback)(request).await;
              host_format_nanos.fetch_add(start.elapsed().as_nanos() as u64, Ordering::Relaxed);
              if sender.send(message).is_err() {
                return; // disconnected
              }
            }
            None => {
              if sender.send(Err(FormatError::new("Host format callback was not set."))).is_err() {
                return; // disconnected
              }
            }
          }
        }
      }
    });

    let (tx, rx) = std::sync::mpsc::channel::<WasmPluginRequest>();
    let (initialize_tx, initialize_rx) = tokio::sync::oneshot::channel::<Result<(), anyhow::Error>>();
    // what the instance formats is timed when it's interpreted, for the
    // plugin's rate in the interpreter
    let interpreted = (engine == FormatEngine::Interpreter).then(|| self.interpreted.clone());
    let log = self.interpreter.log.clone();

    // spawn the wasm instance on a dedicated blocking thread to reduce issues.
    // the runtime gives its blocking threads a large stack (see
    // WASM_PLUGIN_THREAD_STACK_SIZE) so wasmtime can run wasm on this native
    // stack, up to MAX_WASM_STACK_SIZE.
    kprint_async_runtime::spawn_blocking(move || {
      let mut instance = match create(host_format_tx) {
        Ok(instance) => {
          if initialize_tx.send(Ok(())).is_err() {
            return; // disconnected
          }
          instance
        }
        Err(err) => {
          let _ = initialize_tx.send(Err(err));
          return; // quit
        }
      };
      while let Ok(message) = rx.recv() {
        let (request, response) = match message {
          WasmPluginRequest::Format(request, response) => (request, response),
          WasmPluginRequest::ReleaseConfig(config_id) => {
            if let Err(err) = instance.release_config(config_id) {
              log(&format!("Error releasing config {:?}: {:#}", config_id, err));
            }
            continue;
          }
        };
        let start = Instant::now();
        let host_format_start = host_format_nanos.load(Ordering::Relaxed);
        let result = instance.format_text(
          &request.file_path,
          &request.file_bytes,
          request.range.clone(),
          &request.config,
          &request.override_config,
          request.token.clone(),
        );
        if let Some(interpreted) = &interpreted {
          let host_format = host_format_nanos.load(Ordering::Relaxed) - host_format_start;
          interpreted.lock().add(FormatRate {
            bytes: request.file_bytes.len() as u64,
            nanos: (start.elapsed().as_nanos() as u64).saturating_sub(host_format),
          });
        }
        if response.send(result).is_err() {
          break; // disconnected
        }
      }
    });

    // wait for initialization
    initialize_rx.await??;

    log_debug!(
      self.environment,
      "Created instance of {} in {}ms",
      self.name,
      start_instant.elapsed().as_millis() as u64
    );
    let sender = Rc::new(tx);
    self.instance_senders.borrow_mut().push(Rc::downgrade(&sender));
    Ok(WasmPluginSenderWithState {
      sender,
      instance_state_cell,
      engine,
    })
  }

  /// Without a choice for the run (ex. in `dprint lsp`), a plugin formats in
  /// the interpreter until that took as long as compiling it would, then
  /// compiles in the background and formats natively once that's done. That
  /// costs at most about twice what knowing the future would.
  fn compile_once_it_pays_off(&self) {
    if self.formatting.chosen.get().is_some() || self.compiling_in_background.get() || self.formatting.has_native_code() {
      return;
    }
    if engine_choice::forced(&self.environment).is_some() {
      return;
    }
    let interpreted = Duration::from_nanos(self.interpreted.lock().nanos);
    if interpreted < self.compile_after {
      return;
    }
    self.compiling_in_background.set(true);
    log_debug!(
      self.environment,
      "Compiling {} in the background after {}ms in the interpreter.",
      self.name,
      interpreted.as_millis()
    );
    let native = self.formatting.native.clone();
    let environment = self.environment.clone();
    let name = self.name.clone();
    kprint_async_runtime::spawn(async move {
      if let Err(err) = native.get().await {
        log_debug!(environment, "Error compiling {} in the background: {:#}", name, err);
      }
    });
  }
}

#[async_trait(?Send)]
impl<TEnvironment: Environment> InitializedPlugin for InitializedWasmPlugin<TEnvironment> {
  async fn license_text(&self) -> Result<String> {
    self.interpret(|instance| instance.license_text()).await
  }

  async fn resolved_config(&self, config: Arc<FormatConfig>) -> Result<String> {
    self.interpret(move |instance| instance.resolved_config(&config)).await
  }

  async fn file_matching_info(&self, config: Arc<FormatConfig>) -> Result<FileMatchingInfo> {
    self.interpret(move |instance| instance.file_matching_info(&config)).await
  }

  async fn config_diagnostics(&self, config: Arc<FormatConfig>) -> Result<Vec<ConfigurationDiagnostic>> {
    self.interpret(move |instance| instance.config_diagnostics(&config)).await
  }

  async fn check_config_updates(&self, message: CheckConfigUpdatesMessage) -> Result<Vec<ConfigChange>> {
    self.interpret(move |instance| instance.check_config_updates(&message)).await
  }

  async fn format_text(&self, request: InitializedPluginFormatRequest) -> FormatResult {
    if request.token.is_cancelled() {
      return Ok(None);
    }
    let message = Arc::new(WasmPluginFormatMessage {
      file_path: request.file_path,
      file_bytes: request.file_text,
      range: request.range,
      config: request.config,
      override_config: request.override_config,
      token: request.token,
    });
    let instance_state = InstanceState {
      host_format_callback: request.on_host_format,
    };
    let result = self
      .with_instance(Some(instance_state), move |plugin_sender| {
        let message = message.clone();
        async move {
          let (tx, rx) = tokio::sync::oneshot::channel();
          plugin_sender.send(WasmPluginRequest::Format(message, tx))?;
          rx.await?.map_err(anyhow::Error::from)
        }
        .boxed_local()
      })
      .await
      .map_err(crate::plugins::anyhow_to_format_error);
    self.compile_once_it_pays_off();
    result
  }

  async fn release_config(&self, config_id: FormatConfigId) -> Result<()> {
    self.interpret(move |instance| instance.release_config(config_id)).await?;
    let mut senders = self.instance_senders.borrow_mut();
    senders.retain(|sender| match sender.upgrade() {
      Some(sender) => sender.send(WasmPluginRequest::ReleaseConfig(config_id)).is_ok(),
      None => false,
    });
    Ok(())
  }

  async fn shutdown(&self) {
    // do nothing
  }
}

#[cfg(test)]
mod test {

  use std::cell::Cell;
  use std::path::Path;

  use kprint_async_runtime::FutureExt;

  use super::*;
  use crate::environment::TestEnvironment;
  use crate::test_helpers::WASM_PLUGIN_BYTES;

  async fn format_text(plugin: &InitializedWasmPlugin<TestEnvironment>) -> FormatResult {
    plugin
      .format_text(InitializedPluginFormatRequest {
        file_path: PathBuf::from("/file.txt"),
        file_text: b"text".to_vec(),
        range: None,
        config: Arc::new(FormatConfig {
          id: kprint_plugin_types::FormatConfigId::from_raw(1),
          global: Default::default(),
          plugin: Default::default(),
        }),
        override_config: Default::default(),
        on_host_format: Rc::new(|_| async { Ok(None) }.boxed_local()),
        token: Arc::new(kprint_plugin_types::NullCancellationToken),
      })
      .await
  }

  #[tokio::test]
  async fn without_a_choice_compiles_in_the_background_once_interpreting_took_as_long() {
    let environment = TestEnvironment::new();
    let native_loads = Rc::new(Cell::new(0));
    let load_native: LoadModule<WasmModule> = Box::new({
      let native_loads = native_loads.clone();
      move || {
        native_loads.set(native_loads.get() + 1);
        async move {
          let compiled = crate::plugins::compile_wasm(WASM_PLUGIN_BYTES)?;
          super::super::WasmModuleCreator::default().create_from_serialized(&compiled.bytes)
        }
        .boxed_local()
      }
    });
    let formatting = Rc::new(Formatting {
      native: Rc::new(LazyModule::new(load_native)),
      load_cached_native: Box::new(|| async { Ok(None) }.boxed_local()),
      invalid_native: Cell::new(false),
      native_module_path: PathBuf::from("/plugin.cwasm"),
      native_exists: Cell::new(None),
      format_rate_path: PathBuf::from("/plugin.rate.json"),
      chosen: Cell::new(None),
      environment: environment.clone(),
    });
    let log: LogFn = Arc::new(|_| {});
    let interpreter = Arc::new(Interpreter::new(InterpretedModule::new(WASM_PLUGIN_BYTES).unwrap(), log));
    let mut plugin = InitializedWasmPlugin::new("test-plugin".to_string(), interpreter, formatting.clone(), environment.clone());
    plugin.compile_after = Duration::from_secs(3600);

    // it interprets while that took less long than compiling would
    assert_eq!(format_text(&plugin).await.unwrap(), Some(b"text_formatted".to_vec()));
    tokio::task::yield_now().await;
    assert_eq!(native_loads.get(), 0);
    assert_eq!(formatting.engine(), FormatEngine::Interpreter);

    // then compiles in the background, once
    plugin.compile_after = Duration::ZERO;
    assert_eq!(format_text(&plugin).await.unwrap(), Some(b"text_formatted".to_vec()));
    assert_eq!(format_text(&plugin).await.unwrap(), Some(b"text_formatted".to_vec()));
    while !formatting.native.is_loaded() {
      tokio::task::yield_now().await;
    }
    assert_eq!(native_loads.get(), 1);

    // and formats natively from then on
    assert_eq!(formatting.engine(), FormatEngine::Native);
    assert_eq!(format_text(&plugin).await.unwrap(), Some(b"text_formatted".to_vec()));
    assert_eq!(native_loads.get(), 1);
    assert!(plugin.pending_instances.borrow().iter().all(|instance| instance.engine == FormatEngine::Native));
  }

  #[tokio::test]
  async fn a_chosen_engine_is_kept_for_the_run() {
    let environment = TestEnvironment::new();
    let load_native: LoadModule<WasmModule> = Box::new(|| async { Err(anyhow!("not compiled in this test")) }.boxed_local());
    let formatting = Rc::new(Formatting {
      native: Rc::new(LazyModule::new(load_native)),
      load_cached_native: Box::new(|| async { Ok(None) }.boxed_local()),
      invalid_native: Cell::new(false),
      native_module_path: PathBuf::from("/plugin.cwasm"),
      native_exists: Cell::new(None),
      format_rate_path: PathBuf::from("/plugin.rate.json"),
      chosen: Cell::new(Some(FormatEngine::Interpreter)),
      environment: environment.clone(),
    });
    let log: LogFn = Arc::new(|_| {});
    let interpreter = Arc::new(Interpreter::new(InterpretedModule::new(WASM_PLUGIN_BYTES).unwrap(), log));
    let mut plugin = InitializedWasmPlugin::new("test-plugin".to_string(), interpreter, formatting.clone(), environment.clone());
    plugin.compile_after = Duration::ZERO;
    assert_eq!(format_text(&plugin).await.unwrap(), Some(b"text_formatted".to_vec()));
    tokio::task::yield_now().await;
    // the run chose to interpret, so it doesn't compile in the background
    assert!(!plugin.compiling_in_background.get());
    assert_eq!(formatting.engine(), FormatEngine::Interpreter);
    drop(plugin);
    // and it keeps how fast the plugin formatted
    let rate = engine_choice::read_rate(&environment, Path::new("/plugin.rate.json")).unwrap();
    assert_eq!(rate.bytes, 4);
  }

  #[tokio::test]
  async fn a_released_config_is_registered_again_in_every_instance() {
    let environment = TestEnvironment::new();
    let load_native: LoadModule<WasmModule> = Box::new(|| async { Err(anyhow!("not compiled in this test")) }.boxed_local());
    let formatting = Rc::new(Formatting {
      native: Rc::new(LazyModule::new(load_native)),
      load_cached_native: Box::new(|| async { Ok(None) }.boxed_local()),
      invalid_native: Cell::new(false),
      native_module_path: PathBuf::from("/plugin.cwasm"),
      native_exists: Cell::new(None),
      format_rate_path: PathBuf::from("/plugin.rate.json"),
      chosen: Cell::new(Some(FormatEngine::Interpreter)),
      environment: environment.clone(),
    });
    let log: LogFn = Arc::new(|_| {});
    let interpreter = Arc::new(Interpreter::new(InterpretedModule::new(WASM_PLUGIN_BYTES).unwrap(), log));
    let plugin = InitializedWasmPlugin::new("test-plugin".to_string(), interpreter, formatting.clone(), environment.clone());
    let config = Arc::new(FormatConfig {
      id: FormatConfigId::from_raw(1),
      global: Default::default(),
      plugin: Default::default(),
    });
    assert_eq!(
      plugin.resolved_config(config.clone()).await.unwrap(),
      r#"{"ending":"formatted","lineWidth":120}"#
    );
    assert_eq!(format_text(&plugin).await.unwrap(), Some(b"text_formatted".to_vec()));
    assert_eq!(plugin.pending_instances.borrow().len(), 1);
    assert_eq!(plugin.instance_senders.borrow().len(), 1);
    plugin.release_config(config.id).await.unwrap();
    assert_eq!(
      plugin.resolved_config(config.clone()).await.unwrap(),
      r#"{"ending":"formatted","lineWidth":120}"#
    );
    assert_eq!(format_text(&plugin).await.unwrap(), Some(b"text_formatted".to_vec()));
    assert_eq!(plugin.pending_instances.borrow().len(), 1);
    plugin.pending_instances.borrow_mut().clear();
    plugin.release_config(config.id).await.unwrap();
    assert_eq!(plugin.instance_senders.borrow().len(), 0);
  }

  /// A module that fails to load the first time, and the count of times it
  /// was loaded.
  fn module_failing_to_load_once(load_failure_retry_after: Duration) -> (LazyModule<u32>, Rc<Cell<usize>>) {
    let loads = Rc::new(Cell::new(0));
    let load: LoadModule<u32> = Box::new({
      let loads = loads.clone();
      move || {
        loads.set(loads.get() + 1);
        let result = if loads.get() == 1 { Err(anyhow!("no network")) } else { Ok(1) };
        async move { result }.boxed_local()
      }
    });
    let mut module = LazyModule::new(load);
    module.load_failure_retry_after = load_failure_retry_after;
    (module, loads)
  }

  #[tokio::test]
  async fn returns_a_load_failure_again_until_it_may_be_retried() {
    let (module, loads) = module_failing_to_load_once(LOAD_FAILURE_RETRY_AFTER);
    assert_eq!(module.get().await.err().unwrap().to_string(), "no network");
    assert_eq!(module.get().await.err().unwrap().to_string(), "no network");
    assert!(!module.is_loaded());
    assert_eq!(loads.get(), 1);

    // once it may be retried, ex. in a long running `dprint lsp`
    let (module, loads) = module_failing_to_load_once(Duration::ZERO);
    assert!(module.get().await.is_err());
    assert!(module.get().await.is_ok());
    assert!(module.get().await.is_ok());
    assert!(module.is_loaded());
    assert_eq!(loads.get(), 2);
  }
}
