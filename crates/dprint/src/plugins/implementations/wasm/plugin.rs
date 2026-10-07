use anyhow::Result;
use anyhow::anyhow;
use dprint_core::async_runtime::FutureExt;
use dprint_core::async_runtime::LocalBoxFuture;
use dprint_core::async_runtime::async_trait;
use dprint_core::plugins::CancellationToken;
use dprint_core::plugins::CheckConfigUpdatesMessage;
use dprint_core::plugins::ConfigChange;
use dprint_core::plugins::CriticalFormatError;
use dprint_core::plugins::FileMatchingInfo;
use dprint_core::plugins::FormatError;
use dprint_core::plugins::FormatRange;
use dprint_core::plugins::FormatResult;
use dprint_core::plugins::HostFormatRequest;
use dprint_core::plugins::process::HostFormatCallback;
use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use dprint_core::configuration::ConfigKeyMap;
use dprint_core::configuration::ConfigurationDiagnostic;
use dprint_core::plugins::PluginInfo;

use super::WasmHostFormatSender;
use super::create_pools_import_object;
use super::instance::InitializedWasmPluginInstance;
use super::instance::LogFn;
use super::instance::Store;
use super::interpreter::InterpretedModule;
use super::load_instance;
use super::load_instance::WasmInstance;
use super::load_instance::WasmModule;
use crate::environment::Environment;
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
  /// formatting (its resolved configuration, the files it formats, ...).
  pub load_interpreted: LoadModule<InterpretedModule>,
  /// Loads the native module that formats, compiling it first when needed.
  pub load_native: LoadModule<WasmModule>,
  /// Where the native module is kept once it's compiled.
  pub native_module_path: PathBuf,
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
  native: Rc<LazyModule<WasmModule>>,
  native_module_path: PathBuf,
  resolution_cache: PluginResolutionCache,
  environment: TEnvironment,
  plugin_info: PluginInfo,
}

impl<TEnvironment: Environment> WasmPlugin<TEnvironment> {
  /// Creates the plugin, which loads its modules once it's used.
  pub fn new(plugin_info: PluginInfo, modules: WasmPluginModules, resolution_cache: PluginResolutionCache, environment: TEnvironment) -> Self {
    WasmPlugin {
      interpreted: LazyModule::new(modules.load_interpreted),
      native: Rc::new(LazyModule::new(modules.load_native)),
      native_module_path: modules.native_module_path,
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

  fn compiles_to_format(&self) -> bool {
    !self.native.is_loaded() && !self.environment.path_exists(&self.native_module_path)
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
      plugin_name.clone(),
      Arc::new(Interpreter::new(interpreted, log)),
      self.native.clone(),
      Arc::new({
        move |module: &WasmModule, host_format_sender| {
          let (linker, host_state) = create_pools_import_object(environment.clone(), &plugin_name, module.version(), module.engine(), host_format_sender)?;
          let mut store = module.new_store(host_state);
          let instance = load_instance(&mut store, module, &linker)?;
          Ok((store, instance))
        }
      }),
      self.environment.clone(),
    ));

    Ok(plugin)
  }
}

/// Runs the calls that come before formatting in an interpreted instance of
/// the plugin.
struct Interpreter {
  module: InterpretedModule,
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

struct WasmPluginFormatRequest(Arc<WasmPluginFormatMessage>, WasmResponseSender<FormatResult>);

type WasmPluginSender = std::sync::mpsc::Sender<WasmPluginFormatRequest>;

#[derive(Clone)]
struct InstanceState {
  host_format_callback: HostFormatCallback,
}

struct WasmPluginSenderWithState {
  sender: Rc<WasmPluginSender>,
  instance_state_cell: Rc<RefCell<Option<InstanceState>>>,
}

type LoadInstanceFn = dyn Fn(&WasmModule, WasmHostFormatSender) -> Result<(Store, WasmInstance)> + Send + Sync;

pub struct InitializedWasmPlugin<TEnvironment: Environment> {
  name: String,
  interpreter: Arc<Interpreter>,
  pending_instances: RefCell<Vec<WasmPluginSenderWithState>>,
  native: Rc<LazyModule<WasmModule>>,
  load_instance: Arc<LoadInstanceFn>,
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
  fn new(
    name: String,
    interpreter: Arc<Interpreter>,
    native: Rc<LazyModule<WasmModule>>,
    load_instance: Arc<LoadInstanceFn>,
    environment: TEnvironment,
  ) -> Self {
    Self {
      name,
      interpreter,
      pending_instances: Default::default(),
      native,
      load_instance,
      environment,
    }
  }

  /// Runs a call that comes before formatting in the interpreter, on a
  /// blocking thread so several plugins run their calls at once.
  async fn interpret<T: Send + 'static>(&self, call: impl FnOnce(&mut dyn InitializedWasmPluginInstance) -> Result<T> + Send + 'static) -> Result<T> {
    let interpreter = self.interpreter.clone();
    dprint_core::async_runtime::spawn_blocking(move || interpreter.run(call)).await?
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
    let maybe_instance = self.pending_instances.borrow_mut().pop(); // needs to be on a separate line
    let plugin_sender = match maybe_instance {
      Some(instance) => instance,
      None => self.create_instance().await?,
    };
    *plugin_sender.instance_state_cell.borrow_mut() = instance_state;
    Ok(plugin_sender)
  }

  fn release_instance(&self, plugin_sender: WasmPluginSenderWithState) {
    *plugin_sender.instance_state_cell.borrow_mut() = None;
    self.pending_instances.borrow_mut().push(plugin_sender);
  }

  async fn create_instance(&self) -> Result<WasmPluginSenderWithState> {
    // compiled the first time the plugin formats
    let module = self.native.get().await?;
    let start_instant = Instant::now();
    log_debug!(self.environment, "Creating instance of {}", self.name);

    let (host_format_tx, mut host_format_rx) = tokio::sync::mpsc::unbounded_channel::<(HostFormatRequest, std::sync::mpsc::Sender<FormatResult>)>();
    let instance_state_cell: Rc<RefCell<Option<InstanceState>>> = Default::default();

    dprint_core::async_runtime::spawn({
      let instance_state_cell = instance_state_cell.clone();
      async move {
        while let Some((request, sender)) = host_format_rx.recv().await {
          let instance_state = instance_state_cell.borrow().clone();
          match instance_state {
            Some(instance_state) => {
              let message = (instance_state.host_format_callback)(request).await;
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

    let (tx, rx) = std::sync::mpsc::channel::<WasmPluginFormatRequest>();
    let (initialize_tx, initialize_rx) = tokio::sync::oneshot::channel::<Result<(), anyhow::Error>>();

    // spawn the wasm instance on a dedicated blocking thread to reduce issues.
    // the runtime gives its blocking threads a large stack (see
    // WASM_PLUGIN_THREAD_STACK_SIZE) so wasmtime can run wasm on this native
    // stack, up to MAX_WASM_STACK_SIZE.
    dprint_core::async_runtime::spawn_blocking({
      let load_instance = self.load_instance.clone();
      move || {
        let initialize = || {
          let (store, instance) = (load_instance)(&module, host_format_tx)?;
          let instance = create_wasm_plugin_instance(store, instance)?;
          Ok(instance)
        };
        let mut instance = match initialize() {
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
        while let Ok(WasmPluginFormatRequest(request, response)) = rx.recv() {
          let result = instance.format_text(
            &request.file_path,
            &request.file_bytes,
            request.range.clone(),
            &request.config,
            &request.override_config,
            request.token.clone(),
          );
          if response.send(result).is_err() {
            break; // disconnected
          }
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
    Ok(WasmPluginSenderWithState {
      sender: Rc::new(tx),
      instance_state_cell,
    })
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
    self
      .with_instance(Some(instance_state), move |plugin_sender| {
        let message = message.clone();
        async move {
          let (tx, rx) = tokio::sync::oneshot::channel();
          plugin_sender.send(WasmPluginFormatRequest(message, tx))?;
          rx.await?.map_err(anyhow::Error::from)
        }
        .boxed_local()
      })
      .await
      .map_err(crate::plugins::anyhow_to_format_error)
  }

  async fn shutdown(&self) {
    // do nothing
  }
}

#[cfg(test)]
mod test {
  use std::cell::Cell;

  use super::*;

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
