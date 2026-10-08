use std::cell::RefCell;
use std::collections::BTreeMap;
use std::collections::HashSet;
use std::hash::Hasher;
use std::path::Path;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use anyhow::Result;
use anyhow::bail;
use dprint_async_runtime::FutureExt;
use dprint_async_runtime::LocalBoxFuture;
use dprint_async_runtime::future;
use dprint_configuration::ConfigKeyMap;
use dprint_configuration::ConfigurationDiagnostic;
use dprint_plugin_types::CancellationToken;
use dprint_plugin_types::CheckConfigUpdatesMessage;
use dprint_plugin_types::ConfigChange;
use dprint_plugin_types::CriticalFormatError;
use dprint_plugin_types::FileMatchingInfo;
use dprint_plugin_types::FormatConfigId;
use dprint_plugin_types::FormatError;
use dprint_plugin_types::FormatRange;
use dprint_plugin_types::FormatResult;
use dprint_plugin_types::HostFormatCallback;
use dprint_plugin_types::HostFormatRequest;
use dprint_plugin_types::PluginInfo;
use indexmap::IndexMap;
use sys_traits::FsMetadata;
use sys_traits::FsMetadataValue;
use thiserror::Error;

use crate::PluginNameResolutionMaps;
use crate::configuration::GlobalConfigDiagnostic;
use crate::configuration::RawPluginConfigOverride;
use crate::configuration::ResolveConfigError;
use crate::configuration::ResolvedConfig;
use crate::configuration::ResolvedConfigPathWithText;
use crate::configuration::get_default_config_file_in_ancestor_directories;
use crate::configuration::get_global_config;
use crate::configuration::get_plugin_config_map;
use crate::configuration::resolve_config_from_args;
use crate::configuration::resolve_config_from_path_with_bytes;
use crate::configuration::resolve_descendant_config_from_path_with_bytes;
use crate::configuration::resolve_global_config_path_and_text;
use crate::environment::CanonicalizedPathBuf;
use crate::environment::HostEnvironment as Environment;
use crate::incremental::FileMetadata;
use crate::incremental::IncrementalFile;
use crate::paths::FilesPathsByPlugins;
use crate::paths::NoFilesFoundError;
use crate::paths::get_and_resolve_file_paths;
use crate::paths::get_file_paths_by_plugins;
use crate::paths::get_plugin_names_for_file_on_disk;
use crate::patterns::FileMatcher;
use crate::patterns::FileMatcherOptions;
use crate::patterns::get_patterns_as_glob_matcher;
use crate::plugins::FormatConfig;
use crate::plugins::InitializedPlugin;
use crate::plugins::InitializedPluginFormatRequest;
use crate::plugins::OutputPluginConfigDiagnosticsError;
use crate::plugins::PluginResolution;
use crate::plugins::PluginResolver;
use crate::plugins::PluginWrapper;
use crate::plugins::describe_config_diagnostic;
use crate::plugins::output_plugin_config_diagnostics;
use crate::utils::FastInsecureHasher;
use crate::utils::GlobMatcher;
use crate::utils::GlobOutput;
use crate::utils::OutsideBasePath;
use crate::utils::PathSource;
use crate::utils::escape_glob_text_for_cli;
use crate::utils::is_negated_glob;
use dprint_config::options::ConfigOptions;
use dprint_discovery::ConfigDiscovery;
use dprint_discovery::FilePatternArgs;

pub enum GetPluginResult {
  HadDiagnostics(usize),
  Success(InitializedPluginWithConfig),
}

pub struct PluginConfigOverride {
  files: Vec<String>,
  properties: ConfigKeyMap,
  config_id: FormatConfigId,
  matcher: GlobMatcher,
  /// Like `PluginWithConfig::property_origins`, with this override's
  /// properties from the file the override is from.
  property_origins: IndexMap<String, PathSource>,
}

pub struct PluginWithConfig {
  pub plugin: Rc<PluginWrapper>,
  pub associations: Option<Vec<String>>,
  pub overrides: Vec<PluginConfigOverride>,
  pub format_config: Arc<FormatConfig>,
  pub file_matching: FileMatchingInfo,
  /// The plugin's resolved configuration serialized as JSON. This is used by
  /// the incremental hash so that values the plugin derives at resolution time
  /// (ex. the exec plugin's `cacheKeyFiles` hash) invalidate the cache.
  serialized_resolved_config: String,
  property_origins: IndexMap<String, PathSource>,
  config_diagnostic_count: tokio::sync::Mutex<Option<usize>>,
}

pub struct PluginWithConfigOptions {
  pub associations: Option<Vec<String>>,
  pub format_config: Arc<FormatConfig>,
  pub file_matching: FileMatchingInfo,
  pub overrides: Vec<PluginConfigOverride>,
  /// The plugin's resolved configuration serialized as JSON.
  pub serialized_resolved_config: String,
  /// The configuration files the properties of its configuration (and the
  /// global configuration) are from, when that's not the configuration file
  /// being resolved, for diagnostics.
  pub property_origins: IndexMap<String, PathSource>,
}

impl PluginWithConfig {
  pub fn new(plugin: Rc<PluginWrapper>, options: PluginWithConfigOptions) -> Self {
    Self {
      plugin,
      associations: options.associations,
      overrides: options.overrides,
      format_config: options.format_config,
      config_diagnostic_count: Default::default(),
      file_matching: options.file_matching,
      serialized_resolved_config: options.serialized_resolved_config,
      property_origins: options.property_origins,
    }
  }

  /// Gets a hash that represents the current state of the plugin.
  /// This is used for the "incremental" feature to tell if a plugin has changed state.
  pub fn incremental_hash(&self, hasher: &mut impl Hasher) {
    use std::hash::Hash;
    // list everything in here that would affect formatting, with the strings'
    // `Hash`, which ends them, so where one ends is part of the hash
    match self.plugin.built_in() {
      // released with dprint, so its cache revision says how it formats, not
      // dprint's version (which changes for unrelated reasons)
      Some(built_in) => {
        "dprint built-in".hash(hasher);
        built_in.name.hash(hasher);
        built_in.cache_revision.hash(hasher);
      }
      None => {
        self.info().name.hash(hasher);
        self.info().version.hash(hasher);
      }
    }

    // serialize the config keys in order to prevent the hash from changing
    let sorted_config = self.format_config.plugin.iter().collect::<BTreeMap<_, _>>();
    sorted_config.len().hash(hasher);
    for (key, value) in sorted_config {
      key.hash(hasher);
      value.hash(hasher);
    }

    // include the plugin's resolved config so that anything it derives at
    // resolution time but isn't present in the raw config map (ex. the exec
    // plugin folding `cacheKeyFiles` contents into its `cacheKey`) busts the cache
    self.serialized_resolved_config.hash(hasher);

    if let Some(associations) = &self.associations {
      associations.len().hash(hasher);
      for association in associations {
        association.hash(hasher);
      }
    }
    self.overrides.len().hash(hasher);
    for override_config in &self.overrides {
      override_config.files.len().hash(hasher);
      for file in &override_config.files {
        file.hash(hasher);
      }
      let sorted_config = override_config.properties.iter().collect::<BTreeMap<_, _>>();
      sorted_config.len().hash(hasher);
      for (key, value) in sorted_config {
        key.hash(hasher);
        value.hash(hasher);
      }
    }
    self.format_config.global.hash(hasher);
  }

  pub fn get_config_file_overrides_for_path(&self, file_path: &Path) -> ConfigKeyMap {
    let mut result = ConfigKeyMap::new();
    for override_config in &self.overrides {
      if override_config.matcher.matches(file_path) {
        for (key, value) in override_config.properties.iter() {
          result.insert(key.clone(), value.clone());
        }
      }
    }
    result
  }

  pub fn get_merged_overrides_for_path(&self, file_path: &Path, request_override_config: &ConfigKeyMap) -> ConfigKeyMap {
    let mut result = self.get_config_file_overrides_for_path(file_path);
    for (key, value) in request_override_config.iter() {
      result.insert(key.clone(), value.clone());
    }
    result
  }

  pub fn name(&self) -> &str {
    &self.info().name
  }

  pub fn info(&self) -> &PluginInfo {
    self.plugin.info()
  }

  pub async fn initialize(self: &Rc<Self>) -> Result<InitializedPluginWithConfig> {
    let instance = self.plugin.initialize().await?;
    Ok(InitializedPluginWithConfig {
      instance,
      plugin: self.clone(),
    })
  }

  pub async fn get_or_create_checking_config_diagnostics<TEnvironment: Environment>(self: &Rc<Self>, environment: &TEnvironment) -> Result<GetPluginResult> {
    // only allow one thread to initialize and output the diagnostics (we don't want the messages being spammed)
    let instance = self.initialize().await?;
    let mut config_diagnostic_count = self.config_diagnostic_count.lock().await;
    match *config_diagnostic_count {
      Some(count) => {
        if count > 0 {
          return Ok(GetPluginResult::HadDiagnostics(count));
        }
        Ok(GetPluginResult::Success(instance))
      }
      None => {
        let result = instance.output_config_diagnostics(environment).await?;
        if let Err(err) = result {
          log_error!(environment, &err.to_string());
          *config_diagnostic_count = Some(err.diagnostic_count);
          Ok(GetPluginResult::HadDiagnostics(err.diagnostic_count))
        } else {
          let result = instance.output_override_config_diagnostics(environment).await?;
          if let Err(err) = result {
            log_error!(environment, &err.to_string());
            *config_diagnostic_count = Some(err.diagnostic_count);
            Ok(GetPluginResult::HadDiagnostics(err.diagnostic_count))
          } else {
            *config_diagnostic_count = Some(0);
            Ok(GetPluginResult::Success(instance))
          }
        }
      }
    }
  }
}

pub struct InitializedPluginWithConfigFormatRequest {
  pub file_path: PathBuf,
  pub file_bytes: Vec<u8>,
  pub range: FormatRange,
  pub override_config: ConfigKeyMap,
  pub on_host_format: HostFormatCallback,
  pub token: Arc<dyn CancellationToken>,
}

#[derive(Clone)]
pub struct InitializedPluginWithConfig {
  plugin: Rc<PluginWithConfig>,
  instance: Rc<dyn InitializedPlugin>,
}

impl InitializedPluginWithConfig {
  pub fn info(&self) -> &PluginInfo {
    self.plugin.info()
  }

  pub async fn resolved_config(&self) -> Result<String> {
    self.instance.resolved_config(self.plugin.format_config.clone()).await
  }

  pub async fn file_matching_info(&self) -> Result<FileMatchingInfo> {
    self.instance.file_matching_info(self.plugin.format_config.clone()).await
  }

  pub async fn license_text(&self) -> Result<String> {
    self.instance.license_text().await
  }

  pub async fn output_config_diagnostics<TEnvironment: Environment>(
    &self,
    environment: &TEnvironment,
  ) -> Result<Result<(), OutputPluginConfigDiagnosticsError>> {
    output_plugin_config_diagnostics(
      &self.info().name,
      &*self.instance,
      self.plugin.format_config.clone(),
      &self.plugin.property_origins,
      environment,
    )
    .await
  }

  pub async fn output_override_config_diagnostics<TEnvironment: Environment>(
    &self,
    environment: &TEnvironment,
  ) -> Result<Result<(), OutputPluginConfigDiagnosticsError>> {
    let mut diagnostic_count = 0;
    for override_config in &self.plugin.overrides {
      let mut plugin_config = self.plugin.format_config.plugin.clone();
      for (key, value) in override_config.properties.iter() {
        plugin_config.insert(key.clone(), value.clone());
      }
      let format_config = Arc::new(FormatConfig {
        id: override_config.config_id,
        plugin: plugin_config,
        global: self.plugin.format_config.global.clone(),
      });
      for diagnostic in self.instance.config_diagnostics(format_config).await? {
        log_warn!(
          environment,
          "[{}]: {}",
          self.info().name,
          describe_config_diagnostic(&diagnostic, &override_config.property_origins)
        );
        diagnostic_count += 1;
      }
    }

    if diagnostic_count > 0 {
      Ok(Err(OutputPluginConfigDiagnosticsError {
        plugin_name: self.info().name.to_string(),
        diagnostic_count,
      }))
    } else {
      Ok(Ok(()))
    }
  }

  pub async fn check_config_updates(&self, message: CheckConfigUpdatesMessage) -> Result<Vec<ConfigChange>> {
    self.instance.check_config_updates(message).await
  }

  pub async fn format_text(&self, request: InitializedPluginWithConfigFormatRequest) -> FormatResult {
    self
      .instance
      .format_text(InitializedPluginFormatRequest {
        file_path: request.file_path,
        file_text: request.file_bytes,
        range: request.range,
        config: self.plugin.format_config.clone(),
        override_config: request.override_config,
        on_host_format: request.on_host_format,
        token: request.token,
      })
      .await
  }
}

pub struct PluginsScope<TEnvironment: Environment> {
  environment: TEnvironment,
  pub config: Option<Rc<ResolvedConfig>>,
  pub plugins: IndexMap<String, Rc<PluginWithConfig>>,
  pub plugin_name_maps: PluginNameResolutionMaps,
  global_config_diagnostics: Vec<GlobalConfigDiagnostic>,
  cached_editor_file_matcher: RefCell<Option<FileMatcher<TEnvironment>>>,
}

impl<TEnvironment: Environment> PluginsScope<TEnvironment> {
  pub fn new(
    environment: TEnvironment,
    plugins: Vec<Rc<PluginWithConfig>>,
    config: Rc<ResolvedConfig>,
    global_config_diagnostics: Vec<GlobalConfigDiagnostic>,
  ) -> Result<Self> {
    let plugin_name_maps =
      PluginNameResolutionMaps::from_plugins(plugins.iter().map(|p| p.as_ref()), &config.origin.base_path, config.routing.shebangs.as_ref())?;

    Ok(PluginsScope {
      environment,
      config: Some(config),
      plugin_name_maps,
      plugins: plugins.into_iter().map(|p| (p.name().to_string(), p)).collect(),
      global_config_diagnostics,
      cached_editor_file_matcher: Default::default(),
    })
  }

  pub fn ensure_valid_for_cli_args(&self, cli_args: &dyn ConfigOptions) -> Result<()> {
    self.ensure_no_global_config_diagnostics()?;
    self.ensure_plugins_found()?;
    // Skip checking these diagnostics when the user provides
    // plugins from the CLI args. They may be doing this to filter
    // to only specific plugins.
    if cli_args.plugins().is_empty() {
      self.ensure_no_unknown_config_property_diagnostics()?;
    }
    Ok(())
  }

  pub fn ensure_plugins_found(&self) -> Result<(), NoPluginsFoundError> {
    if self.plugins.is_empty() { Err(NoPluginsFoundError) } else { Ok(()) }
  }

  /// A global configuration diagnostic, followed by the configuration file
  /// its property is from when that's not the configuration file being
  /// resolved (ex. a file it extends).
  fn describe_global_config_diagnostic(&self, diagnostic: &ConfigurationDiagnostic) -> String {
    let source = self
      .config
      .as_ref()
      .and_then(|config| config.plugins.origins.root_elsewhere(&diagnostic.property_name, &config.origin.source));
    match source {
      Some(source) => format!("{}\n    at {}", diagnostic, source.display()),
      None => diagnostic.to_string(),
    }
  }

  pub fn ensure_no_global_config_diagnostics(&self) -> Result<(), ResolveConfigError> {
    if self.global_config_diagnostics.is_empty() {
      return Ok(());
    }
    let diagnostics = self
      .global_config_diagnostics
      .iter()
      .filter_map(|d| match d {
        GlobalConfigDiagnostic::UnknownProperty(_) => None,
        GlobalConfigDiagnostic::Other(d) => Some(self.describe_global_config_diagnostic(d)),
      })
      .collect::<Vec<_>>();
    self.error_for_diagnostics(&diagnostics)
  }

  pub fn ensure_no_unknown_config_property_diagnostics(&self) -> Result<(), ResolveConfigError> {
    if self.global_config_diagnostics.is_empty() {
      return Ok(());
    }
    let diagnostics = self
      .global_config_diagnostics
      .iter()
      .filter_map(|d| match d {
        GlobalConfigDiagnostic::UnknownProperty(d) => Some(self.describe_global_config_diagnostic(d)),
        GlobalConfigDiagnostic::Other(_) => None,
      })
      .collect::<Vec<_>>();
    self.error_for_diagnostics(&diagnostics)
  }

  fn error_for_diagnostics(&self, diagnostics: &[String]) -> Result<(), ResolveConfigError> {
    if diagnostics.is_empty() {
      return Ok(());
    }
    let diagnostics_len = diagnostics.len();
    let mut output_text = String::new();
    for diagnostic in diagnostics {
      output_text.push_str("* ");
      output_text.push_str(diagnostic);
      output_text.push('\n');
    }
    output_text.push_str(&format!("\nHad {} config diagnostic(s)", diagnostics_len));
    if let Some(config) = &self.config {
      output_text.push_str(&format!(" in {}", config.origin.source));
    }
    Err(ResolveConfigError::Other(anyhow::anyhow!("{}", output_text)))
  }

  pub fn process_plugin_count(&self) -> usize {
    self.plugins.values().filter(|p| p.plugin.is_process_plugin()).count()
  }

  pub fn get_plugin(&self, name: &str) -> Rc<PluginWithConfig> {
    self
      .plugins
      .get(name)
      .cloned()
      .unwrap_or_else(|| panic!("Expected to find plugin in collection: {}", name))
  }

  pub fn plugins_hash(&self) -> u64 {
    use std::hash::Hash;
    let mut hasher = FastInsecureHasher::default();
    for plugin in self.plugins.values() {
      plugin.incremental_hash(&mut hasher);
    }
    // the shebang mappings affect which plugin formats a file
    if let Some(shebangs) = self.config.as_ref().and_then(|c| c.routing.shebangs.as_ref()) {
      shebangs.len().hash(&mut hasher);
      for (shebang, extension) in shebangs {
        shebang.hash(&mut hasher);
        extension.hash(&mut hasher);
      }
    }
    hasher.finish()
  }

  pub fn create_host_format_callback(self: &Rc<Self>) -> HostFormatCallback {
    let scope = self.clone();
    Rc::new(move |host_request| scope.format(host_request))
  }

  /// Whether the editor should ask this scope to format the file.
  ///
  /// `file_bytes_start` is the start of the editor's in-memory text when it has
  /// it (ex. the LSP), which is what an extensionless file's shebang needs to be
  /// resolved from so an unsaved shebang still matches. When it's `None` (ex. the
  /// editor service, which only receives a path) the file on disk is used.
  pub fn can_format_for_editor(&self, file_path: &Path, file_bytes_start: Option<&[u8]>) -> bool {
    if !self.matches_editor_file_patterns(file_path) {
      return false;
    }

    // Extensionless files match the includes patterns whenever any shebangs are
    // configured because their shebang isn't known up front, so ensure one
    // actually resolves to a plugin rather than claiming every extensionless file.
    if self.plugin_name_maps.may_match_shebang(file_path) {
      return match file_bytes_start {
        Some(file_bytes_start) => !self
          .plugin_name_maps
          .get_plugin_names_from_file_path_and_bytes(file_path, file_bytes_start)
          .is_empty(),
        None => !get_plugin_names_for_file_on_disk(&self.plugin_name_maps, file_path, &self.environment).is_empty(),
      };
    }

    true
  }

  fn matches_editor_file_patterns(&self, file_path: &Path) -> bool {
    let mut file_matcher_borrow = self.cached_editor_file_matcher.borrow_mut();
    if file_matcher_borrow.is_none() {
      let Some(config) = &self.config else {
        return false;
      };
      let matcher = match FileMatcher::new(
        self.environment.clone(),
        FileMatcherOptions {
          config,
          args: &FilePatternArgs::default(),
          root_dir: &config.origin.base_path,
          specified_file_path: None,
        },
      ) {
        Ok(matcher) => matcher,
        Err(err) => {
          log_warn!(self.environment, "Error creating file matcher: {}", err);
          return false;
        }
      };
      file_matcher_borrow.replace(matcher);
    }
    match file_matcher_borrow.as_mut() {
      Some(file_matcher) => file_matcher.matches_and_dir_not_ignored(file_path),
      None => false, // should never happen
    }
  }

  pub fn format(self: &Rc<Self>, request: HostFormatRequest) -> LocalBoxFuture<'static, FormatResult> {
    // owned because they outlive this scope by moving into the future below
    let plugin_names = self
      .plugin_name_maps
      .get_plugin_names_from_file_path_and_bytes(&request.file_path, &request.file_bytes)
      .into_iter()
      .map(ToOwned::to_owned)
      .collect::<Vec<String>>();
    log_debug!(
      self.environment,
      "Host formatting {} - File length: {} - Plugins: [{}] - Range: {:?}",
      request.file_path.display(),
      request.file_bytes.len(),
      plugin_names.join(", "),
      request.range,
    );
    let scope = self.clone();
    async move {
      let mut file_text = request.file_bytes;
      let mut had_change = false;
      for plugin_name in plugin_names {
        let plugin = scope.get_plugin(&plugin_name);
        match plugin.get_or_create_checking_config_diagnostics(&scope.environment).await {
          Ok(GetPluginResult::Success(initialized_plugin)) => {
            let result = initialized_plugin
              .format_text(InitializedPluginWithConfigFormatRequest {
                file_path: request.file_path.clone(),
                file_bytes: file_text.clone(),
                range: request.range.clone(),
                override_config: plugin.get_merged_overrides_for_path(&request.file_path, &request.override_config),
                on_host_format: scope.create_host_format_callback(),
                token: request.token.clone(),
              })
              .await;
            if let Some(new_text) = result? {
              file_text = new_text;
              had_change = true;
            }
          }
          Ok(GetPluginResult::HadDiagnostics(count)) => return Err(FormatError::new(format!("Had {} configuration errors.", count))),
          Err(err) => return Err(CriticalFormatError(FormatError::new(err)).into()),
        }
      }

      Ok(if had_change { Some(file_text) } else { None })
    }
    .boxed_local()
  }
}

pub struct PluginsScopeAndPathsCollection<TEnvironment: Environment> {
  environment: TEnvironment,
  inner: Vec<PluginsScopeAndPaths<TEnvironment>>,
  /// The base directories of the scopes that found no files, whose plugins
  /// weren't resolved (see `ResolvePluginsScopeAndPathsOptions`).
  base_paths_without_files: Vec<CanonicalizedPathBuf>,
}

impl<TEnvironment: Environment> PluginsScopeAndPathsCollection<TEnvironment> {
  /// Chooses how each Wasm plugin without native code formats, before
  /// anything is formatted: compiled to native code when interpreting what it
  /// formats would take longer than compiling it. What it formats is the
  /// bytes of its files, without the files `incremental_files` (one per
  /// scope) knows are formatted by their size and modification time. That's
  /// the most it formats, as an unchanged file read in full isn't formatted
  /// either.
  ///
  /// Then the plugins it compiles are printed with what they format. More
  /// than the limit is an error before anything is compiled or formatted.
  pub async fn plan_format_engines(&self, incremental_files: &[Option<Arc<IncrementalFile<TEnvironment>>>]) -> Result<()> {
    let mut plugins: Vec<&Rc<PluginWrapper>> = Vec::new();
    for scope_and_paths in &self.inner {
      for plugin in scope_and_paths.scope.plugins.values() {
        if !plugins.iter().any(|existing| Rc::ptr_eq(existing, &plugin.plugin)) {
          plugins.push(&plugin.plugin);
        }
      }
    }
    // A cache file's existence doesn't prove it can be loaded. Keep loaded
    // modules for formatting, and count recovery of unusable ones below.
    let prepared = future::join_all(plugins.iter().map(|plugin| plugin.prepare_format_engine())).await;
    let mut choosing: Vec<&Rc<PluginWrapper>> = Vec::new();
    let mut recovering = Vec::new();
    for (plugin, prepared) in plugins.into_iter().zip(prepared) {
      if plugin.chooses_format_engine() {
        choosing.push(plugin);
        recovering.push(prepared.is_err());
      }
    }
    if choosing.is_empty() {
      return Ok(());
    }

    let mut files = Vec::new();
    for (scope_index, scope_and_paths) in self.inner.iter().enumerate() {
      for (plugin_names, file_paths) in scope_and_paths.file_paths_by_plugins.iter() {
        let plugin_indices = plugin_names
          .names()
          .filter_map(|name| scope_and_paths.scope.plugins.get(name))
          .filter_map(|plugin| choosing.iter().position(|choosing| Rc::ptr_eq(choosing, &plugin.plugin)))
          .collect::<Vec<_>>();
        if !plugin_indices.is_empty() {
          files.push(FilesToMeasure {
            incremental_file: incremental_files.get(scope_index).cloned().flatten(),
            check_content_hash: plugin_indices.iter().any(|index| recovering[*index]),
            plugin_indices,
            file_paths: file_paths.clone(),
          });
        }
      }
    }
    let bytes = bytes_to_format(&self.environment, files, choosing.len()).await?;

    let mut compiling = Vec::new();
    for (plugin, bytes) in choosing.into_iter().zip(bytes) {
      plugin.choose_format_engine(bytes);
      if plugin.compiles_to_format() {
        compiling.push((plugin, bytes));
      }
    }
    let limit = max_plugin_compiles(&self.environment);
    if compiling.len() > limit {
      bail!(
        concat!(
          "Formatting these files would compile {} plugins, more than the limit of {}. ",
          "Set DPRINT_MAX_PLUGIN_COMPILES to a higher number to allow it. Plugins:\n{}"
        ),
        compiling.len(),
        limit,
        compiling
          .iter()
          .map(|(plugin, bytes)| format!("  {} {} ({})", plugin.info().name, plugin.info().version, display_bytes(*bytes)))
          .collect::<Vec<_>>()
          .join("\n"),
      );
    }
    if !compiling.is_empty() {
      log_warn!(
        self.environment,
        "Compiling {} to native code to format these files:\n{}",
        if compiling.len() == 1 {
          "1 plugin".to_string()
        } else {
          format!("{} plugins", compiling.len())
        },
        compiling
          .iter()
          .map(|(plugin, bytes)| format!("  {} {} ({})", plugin.info().name, plugin.info().version, display_bytes(*bytes)))
          .collect::<Vec<_>>()
          .join("\n"),
      );
    }
    Ok(())
  }

  pub fn ensure_valid_for_cli_args(&self, cli_args: &dyn ConfigOptions) -> Result<()> {
    for scope in &self.inner {
      scope.scope.ensure_valid_for_cli_args(cli_args)?;
    }

    // ensure we found some files
    if !cli_args.allow_no_files() {
      let cli_file_patterns = cli_args.file_patterns().and_then(|p| p.include_patterns.as_ref());
      match cli_file_patterns {
        // the user explicitly specified no files to format (ex. `--stdin-files`
        // with no lines), so there's nothing to format and that's ok
        Some(patterns) if patterns.is_empty() => {}
        // when the user specifies a pattern on the command line, just ensure that one scope matched
        Some(_) => {
          let all_empty = self.iter().all(|s| s.file_paths_by_plugins.is_empty());
          if all_empty {
            return Err(
              NoFilesFoundError {
                base_path: self.environment.cwd(),
              }
              .into(),
            );
          }
        }
        // if no args specified then ensure all scopes have files
        None => {
          for scope in &self.inner {
            if let Some(config) = scope.scope.config.as_ref() {
              scope.file_paths_by_plugins.ensure_not_empty(&config.origin.base_path)?;
            }
          }
          if let Some(base_path) = self.base_paths_without_files.first() {
            return Err(NoFilesFoundError { base_path: base_path.clone() }.into());
          }
        }
      }
    }

    Ok(())
  }

  pub fn len(&self) -> usize {
    self.inner.len()
  }

  pub fn is_empty(&self) -> bool {
    self.inner.is_empty()
  }

  pub fn iter(&self) -> impl Iterator<Item = &PluginsScopeAndPaths<TEnvironment>> {
    self.inner.iter()
  }
}

impl<TEnvironment: Environment> IntoIterator for PluginsScopeAndPathsCollection<TEnvironment> {
  type Item = PluginsScopeAndPaths<TEnvironment>;
  type IntoIter = std::vec::IntoIter<PluginsScopeAndPaths<TEnvironment>>;

  fn into_iter(self) -> Self::IntoIter {
    self.inner.into_iter()
  }
}

pub struct PluginsScopeAndPaths<TEnvironment: Environment> {
  pub scope: PluginsScope<TEnvironment>,
  pub file_paths_by_plugins: FilesPathsByPlugins,
}

pub struct ResolvePluginsScopeAndPathsOptions {
  pub skip_traversal: bool,
  /// Leaves out the scopes that found no files, without resolving their
  /// plugins. Commands that only work on the files found set this, so a
  /// plugin is never downloaded or set up for a scope with nothing to format.
  pub skip_scopes_without_files: bool,
}

pub async fn resolve_plugins_scope_and_paths<TEnvironment: Environment>(
  args: &dyn ConfigOptions,
  patterns: &FilePatternArgs,
  environment: &TEnvironment,
  plugin_resolver: &Rc<PluginResolver<TEnvironment>>,
  options: ResolvePluginsScopeAndPathsOptions,
) -> Result<PluginsScopeAndPathsCollection<TEnvironment>> {
  let resolver = PluginsAndPathsResolver {
    args,
    patterns,
    environment,
    plugin_resolver,
    skip_traversal: options.skip_traversal,
    skip_scopes_without_files: options.skip_scopes_without_files,
  };

  resolver.resolve_for_config().await
}

struct PluginsAndPathsResolver<'a, TEnvironment: Environment> {
  args: &'a dyn ConfigOptions,
  patterns: &'a FilePatternArgs,
  environment: &'a TEnvironment,
  plugin_resolver: &'a Rc<PluginResolver<TEnvironment>>,
  skip_traversal: bool,
  skip_scopes_without_files: bool,
}

impl<'a, TEnvironment: Environment> PluginsAndPathsResolver<'a, TEnvironment> {
  /// Finds every config scope and its files first, without touching a
  /// plugin. Only then resolves the plugins of the scopes, so setting up
  /// plugins never competes with the scan, and the scopes that found no files
  /// can be left out before any of their plugins is downloaded or set up.
  pub async fn resolve_for_config(&'a self) -> Result<PluginsScopeAndPathsCollection<TEnvironment>> {
    let config = Rc::new(resolve_config_from_args(self.args, self.environment).await?);
    let config_discovery = self.args.config_discovery(self.environment);
    let mut glob_output = if self.skip_traversal {
      GlobOutput::default()
    } else {
      get_and_resolve_file_paths(&config, self.patterns, config_discovery, self.environment).await?
    };
    let root_config_path = config.origin.source.maybe_local_path().cloned();

    // specified paths outside the config's directory use the config file
    // found in their own directory tree, or the user's global config file
    let outside_scopes = self
      .resolve_outside_base_paths(&mut glob_output, &config, config_discovery, root_config_path.clone())
      .await?;

    let mut scopes = vec![ScopeFiles {
      config: config.clone(),
      file_paths: glob_output.file_paths,
    }];
    let patterns = Rc::new(self.patterns.clone());
    scopes.extend(
      self
        .resolve_for_sub_configs(glob_output.config_files, config.clone(), config_discovery, root_config_path, patterns)
        .await?,
    );
    scopes.extend(outside_scopes);

    let mut base_paths_without_files = Vec::new();
    if self.skip_scopes_without_files {
      scopes.retain(|scope| {
        if !scope.file_paths.is_empty() {
          return true;
        }
        log_debug!(
          self.environment,
          "Not resolving the plugins of {} because it found no files.",
          scope.config.origin.source.display()
        );
        base_paths_without_files.push(scope.config.origin.base_path.clone());
        false
      });
    }

    let resolved = future::join_all(scopes.into_iter().map(|scope| async move {
      let plugins_scope = resolve_plugins_scope(scope.config, self.environment, self.plugin_resolver).await?;
      let file_paths_by_plugins = get_file_paths_by_plugins(&plugins_scope.plugin_name_maps, scope.file_paths, self.environment)?;
      Ok::<_, anyhow::Error>(PluginsScopeAndPaths {
        scope: plugins_scope,
        file_paths_by_plugins,
      })
    }))
    .await;
    let mut result = Vec::with_capacity(resolved.len());
    for scope in resolved {
      result.push(scope?);
    }

    Ok(PluginsScopeAndPathsCollection {
      environment: self.environment.clone(),
      inner: result,
      base_paths_without_files,
    })
  }

  /// Resolves the scopes for specified paths and patterns that are outside
  /// the config's directory. Each one uses the config file found in its own
  /// directory tree when one exists, otherwise the explicitly specified or
  /// global config file, and it's an error when there's no config file to use.
  async fn resolve_outside_base_paths(
    &'a self,
    glob_output: &mut GlobOutput,
    config: &Rc<ResolvedConfig>,
    config_discovery: ConfigDiscovery,
    root_config_path: Option<CanonicalizedPathBuf>,
  ) -> Result<Vec<ScopeFiles>> {
    let outside_base_paths = std::mem::take(&mut glob_output.outside_base_paths);
    if outside_base_paths.is_empty() {
      return Ok(Vec::new());
    }

    // group the paths by the config that governs them
    let mut path_groups: IndexMap<OutsideScopeConfigKey, (OutsideScopeConfig, Vec<String>)> = IndexMap::new();
    for outside_path in outside_base_paths {
      let Some(scope_config) = self.resolve_outside_scope_config(&outside_path, config, config_discovery)? else {
        continue; // skipped with a warning
      };
      path_groups
        .entry(scope_config.group_key())
        .or_insert_with(|| (scope_config, Vec::new()))
        .1
        .push(outside_path.include_pattern);
    }

    let mut result = Vec::new();
    for (_, (scope_config, include_patterns)) in path_groups {
      result.extend(
        self
          .resolve_outside_scope(scope_config, include_patterns, config, config_discovery, root_config_path.clone())
          .await?,
      );
    }
    Ok(result)
  }

  /// Resolves the config that governs the provided outside path. Returns
  /// `None` when the path should be skipped (a warning was logged).
  fn resolve_outside_scope_config(
    &self,
    outside_path: &OutsideBasePath,
    config: &ResolvedConfig,
    config_discovery: ConfigDiscovery,
  ) -> Result<Option<OutsideScopeConfig>> {
    let discover_tree_configs = self.args.config().is_none() && config_discovery.traverse_ancestors();
    if discover_tree_configs && let Some(config_path) = get_default_config_file_in_ancestor_directories(self.environment, &outside_path.config_search_dir)? {
      return Ok(Some(OutsideScopeConfig::ConfigFile(config_path)));
    }

    if self.args.config().is_some() || config.origin.is_global {
      // an explicitly specified config file or the global config file
      // governs explicitly specified paths anywhere
      let root_dir = self.canonical_path_root_dir(&outside_path.config_search_dir)?;
      return Ok(Some(OutsideScopeConfig::RebasedCurrentConfig(root_dir)));
    }

    // only fall back to the global config file when config discovery is in
    // its default mode to match main config file resolution
    if matches!(config_discovery, ConfigDiscovery::Default)
      && let Some(config_path) = self.global_config_path_based_at_path_root(&outside_path.config_search_dir)?
    {
      return Ok(Some(OutsideScopeConfig::ConfigFile(config_path)));
    }

    if self.args.allow_skipping_paths() {
      log_warn!(
        self.environment,
        "WARNING: Skipping '{}' because no dprint config file was found for it.",
        outside_path.include_pattern,
      );
      Ok(None)
    } else {
      bail!(
        concat!(
          "No dprint config file found for '{}'. The path is outside the config file's directory ",
          "and no dprint config file was found in the path's ancestor directories. Create one there ",
          "or set up a global config file by running `dprint init --global`."
        ),
        outside_path.include_pattern,
      );
    }
  }

  /// Resolves the scope and file paths for a group of outside paths that
  /// share a governing config.
  async fn resolve_outside_scope(
    &'a self,
    scope_config: OutsideScopeConfig,
    mut include_patterns: Vec<String>,
    config: &Rc<ResolvedConfig>,
    config_discovery: ConfigDiscovery,
    root_config_path: Option<CanonicalizedPathBuf>,
  ) -> Result<Vec<ScopeFiles>> {
    // carry the negated patterns along so exclusions specified on the
    // command line keep applying in the new scope, but only resolve the
    // grouped paths so files matched by the other args don't get formatted
    // a second time
    include_patterns.extend(self.patterns.include_patterns.iter().flatten().filter(|p| is_negated_glob(p)).cloned());
    // the current scope already handles everything in the config's
    // directory (ex. a `dprint fmt ..` arg covers it with `**`), escaping
    // in case the directory path contains glob characters (ex. `[app]`)
    include_patterns.push(format!("!{}/**", escape_glob_text_for_cli(&config.origin.base_path.to_string_lossy())));
    let patterns = Rc::new(FilePatternArgs {
      include_patterns: Some(include_patterns),
      only_staged: false,
      only_dirty: false,
      ..self.patterns.clone()
    });
    match scope_config {
      OutsideScopeConfig::ConfigFile(config_path) => {
        self
          .resolve_for_config_path(
            config_path,
            config.clone(),
            /* is descendant config */ false,
            config_discovery,
            root_config_path,
            patterns,
          )
          .await
      }
      OutsideScopeConfig::RebasedCurrentConfig(base_path) => {
        // with an explicitly specified config file, don't let other config
        // files take over parts of the scope
        let config_discovery = if self.args.config().is_some() {
          ConfigDiscovery::IgnoreDescendants
        } else {
          config_discovery
        };
        self
          .resolve_for_rebased_config(config, base_path, config_discovery, root_config_path, patterns)
          .await
      }
    }
  }

  /// Resolves the scope and file paths for the provided config with its base
  /// directory changed to the provided path (ex. resolving paths on another
  /// drive against the in-use config file).
  async fn resolve_for_rebased_config(
    &'a self,
    config: &Rc<ResolvedConfig>,
    base_path: CanonicalizedPathBuf,
    config_discovery: ConfigDiscovery,
    root_config_path: Option<CanonicalizedPathBuf>,
    patterns: Rc<FilePatternArgs>,
  ) -> Result<Vec<ScopeFiles>> {
    let mut rebased_config = (**config).clone();
    rebased_config.origin.base_path = base_path;
    self
      .resolve_scope_and_descendants(Rc::new(rebased_config), config_discovery, root_config_path, patterns)
      .await
  }

  /// Gets the global config file based at the root directory of the provided
  /// path so the path is within the resulting config's directory.
  fn global_config_path_based_at_path_root(&self, path: &Path) -> Result<Option<ResolvedConfigPathWithText>> {
    let Some(global_config_path) = resolve_global_config_path_and_text(self.environment)? else {
      return Ok(None);
    };
    Ok(Some(ResolvedConfigPathWithText {
      base_path: self.canonical_path_root_dir(path)?,
      ..global_config_path
    }))
  }

  /// Gets the canonicalized root directory of the provided path (ex. the
  /// drive root on Windows).
  fn canonical_path_root_dir(&self, path: &Path) -> Result<CanonicalizedPathBuf> {
    let root_dir = path.ancestors().last().unwrap();
    Ok(self.environment.canonicalize(root_dir)?)
  }

  async fn resolve_for_sub_config(
    &'a self,
    config_file_path: PathBuf,
    parent_config: Rc<ResolvedConfig>,
    config_discovery: ConfigDiscovery,
    root_config_path: Option<CanonicalizedPathBuf>,
    patterns: Rc<FilePatternArgs>,
  ) -> Result<Vec<ScopeFiles>> {
    log_debug!(self.environment, "Analyzing config file {}", config_file_path.display());
    let config_file_path = self.environment.canonicalize(&config_file_path)?;
    if Some(&config_file_path) == root_config_path.as_ref() {
      // config file specified via `--config` so ignore it
      return Ok(Vec::new());
    }
    let config_path = ResolvedConfigPathWithText {
      content: self.environment.read_file(&config_file_path)?,
      base_path: config_file_path.parent().unwrap(),
      source: PathSource::new_local(config_file_path),
      is_global_config: false,
      is_first_download: false,
    };
    self
      .resolve_for_config_path(
        config_path,
        parent_config,
        /* is descendant config */ true,
        config_discovery,
        root_config_path,
        patterns,
      )
      .await
  }

  /// Resolves the scope and file paths for a config file, recursively
  /// resolving any descendant config files found within its directory.
  fn resolve_for_config_path(
    &'a self,
    config_path: ResolvedConfigPathWithText,
    parent_config: Rc<ResolvedConfig>,
    is_descendant_config: bool,
    config_discovery: ConfigDiscovery,
    root_config_path: Option<CanonicalizedPathBuf>,
    patterns: Rc<FilePatternArgs>,
  ) -> LocalBoxFuture<'a, Result<Vec<ScopeFiles>>> {
    async move {
      let mut config = if is_descendant_config {
        // a nested config that opts into inheriting merges in the ancestor config
        resolve_descendant_config_from_path_with_bytes(&config_path, &parent_config, self.environment).await?
      } else {
        resolve_config_from_path_with_bytes(&config_path, self.environment).await?
      };
      if !self.args.plugins().is_empty() {
        config.plugins.sources.clone_from(&parent_config.plugins.sources);
      }
      self
        .resolve_scope_and_descendants(Rc::new(config), config_discovery, root_config_path, patterns)
        .await
    }
    .boxed_local()
  }

  /// Resolves the plugins scope and file paths for the provided config,
  /// recursively resolving any descendant config files found within its
  /// directory.
  async fn resolve_scope_and_descendants(
    &'a self,
    config: Rc<ResolvedConfig>,
    config_discovery: ConfigDiscovery,
    root_config_path: Option<CanonicalizedPathBuf>,
    patterns: Rc<FilePatternArgs>,
  ) -> Result<Vec<ScopeFiles>> {
    let mut glob_output = get_and_resolve_file_paths(&config, &patterns, config_discovery, self.environment).await?;
    // the root scope already handled paths outside this config's directory
    glob_output.outside_base_paths.clear();
    let mut result = vec![ScopeFiles {
      config: config.clone(),
      file_paths: glob_output.file_paths,
    }];
    result.extend(
      self
        .resolve_for_sub_configs(glob_output.config_files, config, config_discovery, root_config_path, patterns)
        .await?,
    );
    Ok(result)
  }

  /// Resolves the scopes of config files found in subdirectories, all at once.
  /// Results keep the order of `config_file_paths`.
  async fn resolve_for_sub_configs(
    &'a self,
    config_file_paths: Vec<PathBuf>,
    parent_config: Rc<ResolvedConfig>,
    config_discovery: ConfigDiscovery,
    root_config_path: Option<CanonicalizedPathBuf>,
    patterns: Rc<FilePatternArgs>,
  ) -> Result<Vec<ScopeFiles>> {
    let scopes = future::join_all(config_file_paths.into_iter().map(|config_file_path| {
      self.resolve_for_sub_config(
        config_file_path,
        parent_config.clone(),
        config_discovery,
        root_config_path.clone(),
        patterns.clone(),
      )
    }))
    .await;
    let mut result = Vec::new();
    for scope in scopes {
      result.extend(scope?);
    }
    Ok(result)
  }
}

/// A config scope and the files found for it, before its plugins are resolved.
struct ScopeFiles {
  config: Rc<ResolvedConfig>,
  file_paths: Vec<PathBuf>,
}

/// Files a group of plugins formats, for `bytes_to_format`.
struct FilesToMeasure<TEnvironment: Environment> {
  incremental_file: Option<Arc<IncrementalFile<TEnvironment>>>,
  /// Cache recovery must not compile for unchanged files whose recent
  /// modification time couldn't be trusted by the metadata fast path.
  check_content_hash: bool,
  /// The plugins choosing how they format that format the files, as indexes
  /// into the result of `bytes_to_format`.
  plugin_indices: Vec<usize>,
  file_paths: Vec<PathBuf>,
}

/// How many bytes each of `plugin_count` plugins formats, from the files'
/// sizes. It reads metadata on several threads, as there can be tens of
/// thousands of files. Recovering native caches also check incremental hashes
/// when metadata alone can't prove the files are unchanged.
async fn bytes_to_format<TEnvironment: Environment>(
  environment: &TEnvironment,
  files: Vec<FilesToMeasure<TEnvironment>>,
  plugin_count: usize,
) -> Result<Vec<u64>> {
  // starting a thread costs more than reading the metadata of a few hundred files
  const FILES_PER_THREAD: usize = 500;
  let file_count = files.iter().map(|files| files.file_paths.len()).sum::<usize>();
  if file_count == 0 {
    return Ok(vec![0; plugin_count]);
  }
  let thread_count = environment.max_threads().min(file_count.div_ceil(FILES_PER_THREAD)).max(1);
  let environment = environment.clone();
  let bytes = dprint_async_runtime::spawn_blocking(move || {
    let measure = |files: &FilesToMeasure<TEnvironment>, file_paths: &[PathBuf], bytes: &mut [u64]| {
      let incremental_file = files.incremental_file.as_ref().filter(|file| file.has_known_files());
      for file_path in file_paths {
        let Ok(metadata) = environment.fs_metadata(file_path) else {
          continue; // it's reported when it's formatted
        };
        let len = metadata.len();
        let mut is_known_formatted = match (incremental_file, metadata.modified()) {
          (Some(incremental_file), Ok(modified)) => incremental_file.is_known_formatted_by_metadata(file_path, &FileMetadata { len, modified }),
          _ => false,
        };
        if !is_known_formatted
          && files.check_content_hash
          && let Some(incremental_file) = incremental_file
        {
          is_known_formatted = environment
            .read_file_bytes(file_path)
            .is_ok_and(|text| incremental_file.is_file_known_formatted(file_path, &text, None));
        }
        if !is_known_formatted {
          for index in &files.plugin_indices {
            bytes[*index] += len;
          }
        }
      }
    };
    // the groups are split into chunks of about the same number of files,
    // and each thread measures about the same number of chunks
    let chunk_size = file_count.div_ceil(thread_count);
    let chunks = files
      .iter()
      .flat_map(|files| files.file_paths.chunks(chunk_size).map(move |file_paths| (files, file_paths)))
      .collect::<Vec<_>>();
    std::thread::scope(|scope| {
      let handles = chunks
        .chunks(chunks.len().div_ceil(thread_count))
        .map(|chunks| {
          let measure = &measure;
          scope.spawn(move || {
            let mut bytes = vec![0; plugin_count];
            for (files, file_paths) in chunks {
              measure(files, file_paths, &mut bytes);
            }
            bytes
          })
        })
        .collect::<Vec<_>>();
      let mut bytes = vec![0; plugin_count];
      for handle in handles {
        for (total, thread_bytes) in bytes.iter_mut().zip(handle.join().unwrap()) {
          *total += thread_bytes;
        }
      }
      bytes
    })
  })
  .await?;
  Ok(bytes)
}

/// Bytes for people, ex. "1.5 MB".
fn display_bytes(bytes: u64) -> String {
  const KB: u64 = 1024;
  const MB: u64 = 1024 * KB;
  if bytes >= MB {
    format!("{:.1} MB", bytes as f64 / MB as f64)
  } else if bytes >= KB {
    format!("{:.1} KB", bytes as f64 / KB as f64)
  } else {
    format!("{} bytes", bytes)
  }
}

/// The most plugins a format run compiles to native code, unless
/// `DPRINT_MAX_PLUGIN_COMPILES` allows more.
const DEFAULT_MAX_PLUGIN_COMPILES: usize = 50;

fn max_plugin_compiles(environment: &impl Environment) -> usize {
  let Some(value) = environment.env_var("DPRINT_MAX_PLUGIN_COMPILES") else {
    return DEFAULT_MAX_PLUGIN_COMPILES;
  };
  match value.to_str().and_then(|value| value.trim().parse::<usize>().ok()) {
    Some(limit) => limit,
    None => {
      log_warn!(
        environment,
        "Ignoring DPRINT_MAX_PLUGIN_COMPILES={}: it's not a number. Using {}.",
        value.to_string_lossy(),
        DEFAULT_MAX_PLUGIN_COMPILES
      );
      DEFAULT_MAX_PLUGIN_COMPILES
    }
  }
}

/// The config governing paths that are outside the main config's directory.
enum OutsideScopeConfig {
  /// A config file found for the outside path (in its directory tree or
  /// the user's global config file).
  ConfigFile(ResolvedConfigPathWithText),
  /// The config file already in use (an explicitly specified `--config`
  /// or the global config file), rebased at the outside path's root
  /// directory so the path is within the scope and the config's
  /// unanchored patterns still apply.
  RebasedCurrentConfig(CanonicalizedPathBuf),
}

impl OutsideScopeConfig {
  fn group_key(&self) -> OutsideScopeConfigKey {
    match self {
      Self::ConfigFile(config_path) => OutsideScopeConfigKey::ConfigFile(config_path.base_path.clone()),
      Self::RebasedCurrentConfig(base_path) => OutsideScopeConfigKey::RebasedCurrentConfig(base_path.clone()),
    }
  }
}

/// Identifies the config governing a group of outside paths.
///
/// The base directory alone isn't enough because a config file could live in
/// the same directory the current config gets rebased at (ex. a config file at
/// a drive root), which would otherwise silently resolve some paths against the
/// wrong config. The way the config is resolved rules that out today, but
/// keying on the variant makes it impossible to get wrong.
#[derive(PartialEq, Eq, Hash)]
enum OutsideScopeConfigKey {
  ConfigFile(CanonicalizedPathBuf),
  RebasedCurrentConfig(CanonicalizedPathBuf),
}

pub async fn get_plugins_scope_from_args<TEnvironment: Environment>(
  args: &dyn ConfigOptions,
  environment: &TEnvironment,
  plugin_resolver: &Rc<PluginResolver<TEnvironment>>,
) -> Result<PluginsScope<TEnvironment>, ResolvePluginsError> {
  match resolve_config_from_args(args, environment).await {
    Ok(config) => resolve_plugins_scope(Rc::new(config), environment, plugin_resolver).await,
    // ignore
    Err(_) => Ok(PluginsScope {
      environment: environment.clone(),
      config: None,
      plugin_name_maps: Default::default(),
      plugins: Default::default(),
      global_config_diagnostics: Default::default(),
      cached_editor_file_matcher: Default::default(),
    }),
  }
}

#[derive(Debug, Error)]
#[error("No formatting plugins found. Ensure at least one is specified in the 'plugins' array of the configuration file.")]
pub struct NoPluginsFoundError;

#[derive(Debug, Error)]
#[error(transparent)]
pub struct ResolvePluginsError(#[from] anyhow::Error);

pub async fn resolve_plugins_scope<TEnvironment: Environment>(
  config: Rc<ResolvedConfig>,
  environment: &TEnvironment,
  plugin_resolver: &Rc<PluginResolver<TEnvironment>>,
) -> Result<PluginsScope<TEnvironment>, ResolvePluginsError> {
  // resolve the plugins
  let mut plugins = filter_duplicate_plugin_names(plugin_resolver.resolve_plugins(config.plugins.sources.clone()).await?);
  // Configuration resolution has already filtered remote commands and applied
  // inheritance. An explicit exec plugin keeps its selected version and order.
  if config.plugins.config.contains_key("exec") && !plugins.iter().any(|plugin| plugin.info().config_key == "exec") {
    plugins.push(plugin_resolver.resolve_builtin_exec().await);
  }
  let mut config_map = config.plugins.config.clone();

  // resolve each plugin's configuration
  let mut plugins_with_config = Vec::new();
  for plugin in plugins.into_iter() {
    plugins_with_config.push((get_plugin_config_map(&plugin, &mut config_map)?, plugin));
  }

  // now get global config
  let global_config_result = get_global_config(config_map);
  let global_config = global_config_result.config;
  let config_base_path = config.origin.base_path.clone();

  // create the scope
  let plugins = plugins_with_config
    .into_iter()
    .map(|(plugin_config, plugin)| {
      let global_config = global_config.clone();
      let environment = environment.clone();
      let property_origins = config.plugins.origins.plugin_elsewhere(&plugin.info().config_key, &config.origin.source);
      let overrides = resolve_plugin_config_overrides(
        plugin_config.overrides,
        &config_base_path,
        &property_origins,
        &config.origin.source,
        plugin_resolver,
      )?;
      let next_config_id = plugin_resolver.next_config_id();
      Ok(
        async move {
          let format_config = Arc::new(FormatConfig {
            id: next_config_id,
            global: global_config,
            plugin: plugin_config.properties,
          });
          let resolution = resolve_plugin_config(&plugin, &format_config, &environment).await?;
          Ok::<_, anyhow::Error>(Rc::new(PluginWithConfig::new(
            plugin,
            PluginWithConfigOptions {
              associations: plugin_config.associations,
              format_config,
              file_matching: resolution.file_matching,
              overrides,
              serialized_resolved_config: resolution.resolved_config,
              property_origins,
            },
          )))
        }
        .boxed_local(),
      )
    })
    .collect::<Result<Vec<_>>>()?;
  let plugin_results = dprint_async_runtime::future::join_all(plugins).await;
  let mut plugins = Vec::with_capacity(plugin_results.len());
  for result in plugin_results {
    plugins.push(result?);
  }

  Ok(PluginsScope::new(environment.clone(), plugins, config, global_config_result.diagnostics)?)
}

/// Gets which files the plugin formats and its resolved configuration. A
/// plugin that keeps what it resolved configurations to isn't loaded for it
/// when it has resolved this configuration before.
async fn resolve_plugin_config<TEnvironment: Environment>(
  plugin: &PluginWrapper,
  format_config: &Arc<FormatConfig>,
  environment: &TEnvironment,
) -> Result<PluginResolution> {
  let resolution_cache = plugin.resolution_cache();
  if let Some(resolution) = resolution_cache.and_then(|cache| cache.get(environment, format_config)) {
    return Ok(resolution);
  }
  let instance = plugin.initialize().await?;
  let resolution = PluginResolution {
    file_matching: instance.file_matching_info(format_config.clone()).await?,
    resolved_config: instance.resolved_config(format_config.clone()).await?,
  };
  if let Some(cache) = resolution_cache {
    cache.set(environment, format_config, &resolution);
  }
  Ok(resolution)
}

/// Keeps only the highest precedence plugin for each plugin name.
///
/// The same plugin may end up specified more than once with different sources
/// (ex. a config pinning a newer version of a plugin that the config it extends
/// also specifies). Plugins are ordered by descending precedence, so the first
/// entry for a name wins. A built-in counts as the plugin it serves references
/// to, so one exec reference served built in and another run as the process
/// plugin are still the same plugin.
///
/// This can't be done when resolving the configuration because a plugin's name
/// is only known once it has been resolved.
fn filter_duplicate_plugin_names(plugins: Vec<Rc<PluginWrapper>>) -> Vec<Rc<PluginWrapper>> {
  let mut names = HashSet::with_capacity(plugins.len());

  plugins
    .into_iter()
    .filter(|plugin| names.insert(plugin.referenced_plugin_name().to_string()))
    .collect()
}

/// `property_origins` are where the plugin's properties are from, when that's
/// not `config_source` (see `PluginWithConfig::property_origins`).
fn resolve_plugin_config_overrides<TEnvironment: Environment>(
  overrides: Vec<RawPluginConfigOverride>,
  config_base_path: &CanonicalizedPathBuf,
  property_origins: &IndexMap<String, PathSource>,
  config_source: &PathSource,
  plugin_resolver: &Rc<PluginResolver<TEnvironment>>,
) -> Result<Vec<PluginConfigOverride>> {
  overrides
    .into_iter()
    .map(|override_config| {
      let matcher = get_patterns_as_glob_matcher(&override_config.files, config_base_path)?;
      // the override's own properties are from the file it's from, whatever
      // other overrides or the plugin's configuration say for the same name
      let mut override_origins = property_origins.clone();
      for property in override_config.properties.keys() {
        match &override_config.origin.0 {
          Some(origin) if origin != config_source => {
            override_origins.insert(property.clone(), origin.clone());
          }
          _ => {
            override_origins.shift_remove(property);
          }
        }
      }
      Ok(PluginConfigOverride {
        files: override_config.files,
        properties: override_config.properties,
        config_id: plugin_resolver.next_config_id(),
        matcher,
        property_origins: override_origins,
      })
    })
    .collect()
}

#[cfg(test)]
mod test {
  use dprint_configuration::ConfigKeyValue;
  use dprint_configuration::GlobalConfiguration;
  use dprint_platform::environment::*;

  use crate::configuration::ConfigOrigin;
  use crate::configuration::FileRouting;
  use crate::plugins::TestPlugin;

  use super::*;

  fn plugin_with_config(plugin: TestPlugin) -> PluginWithConfig {
    PluginWithConfig::new(
      Rc::new(PluginWrapper::new(Box::new(plugin))),
      PluginWithConfigOptions {
        property_origins: Default::default(),
        associations: None,
        format_config: Arc::new(FormatConfig {
          id: FormatConfigId::from_raw(1),
          global: Default::default(),
          plugin: Default::default(),
        }),
        file_matching: FileMatchingInfo {
          file_extensions: vec!["txt".to_string()],
          file_names: vec![],
          additive: false,
        },
        overrides: Vec::new(),
        serialized_resolved_config: "{}".to_string(),
      },
    )
  }

  fn refers_to_nothing(_reference: &crate::plugins::PluginSourceReference) -> bool {
    false
  }

  static BUILT_IN: crate::plugins::BuiltInFormatter = crate::plugins::BuiltInFormatter {
    name: "built-in",
    cache_revision: 1,
    serves_plugin: "dprint-plugin-built-in",
    refers_to_served_plugin: refers_to_nothing,
  };
  static BUILT_IN_REVISED: crate::plugins::BuiltInFormatter = crate::plugins::BuiltInFormatter { cache_revision: 2, ..BUILT_IN };

  #[test]
  fn incremental_hash_of_a_built_in_is_its_cache_revision_not_its_version() {
    let hash = |plugin: TestPlugin| get_plugin_hash(&plugin_with_config(plugin));
    let built_in = |built_in, version| {
      TestPlugin::new("built-in", "built-in", vec!["txt"], vec![])
        .built_in(built_in)
        .with_version(version)
    };
    // released with dprint, so its version is dprint's, which changes for
    // reasons that have nothing to do with how it formats
    assert_eq!(hash(built_in(&BUILT_IN, "0.58.0")), hash(built_in(&BUILT_IN, "0.59.0")));
    // its cache revision changes when it formats differently
    assert_ne!(hash(built_in(&BUILT_IN, "0.58.0")), hash(built_in(&BUILT_IN_REVISED, "0.58.0")));
    // a loaded plugin is still identified by its name and version
    let loaded = |version| TestPlugin::new("built-in", "built-in", vec!["txt"], vec![]).with_version(version);
    assert_ne!(hash(loaded("1.0.0")), hash(loaded("1.0.1")));
    assert_ne!(hash(loaded("1.0.0")), hash(built_in(&BUILT_IN, "1.0.0")));
  }

  #[test]
  fn a_built_in_is_the_same_plugin_as_the_one_it_serves() {
    let plugins = vec![
      Rc::new(PluginWrapper::new(Box::new(
        TestPlugin::new("built-in", "built-in", vec!["txt"], vec![]).built_in(&BUILT_IN),
      ))),
      // the external plugin it serves, ex. a release it doesn't serve
      // referenced by a configuration file it extends
      Rc::new(PluginWrapper::new(Box::new(TestPlugin::new(
        "dprint-plugin-built-in",
        "built-in",
        vec!["txt"],
        vec![],
      )))),
      Rc::new(PluginWrapper::new(Box::new(TestPlugin::new("other", "other", vec!["md"], vec![])))),
    ];
    let names = filter_duplicate_plugin_names(plugins)
      .iter()
      .map(|plugin| plugin.info().name.clone())
      .collect::<Vec<_>>();
    // the first one has precedence
    assert_eq!(names, vec!["built-in", "other"]);
  }

  // a plugin can derive values at resolution time that aren't present in the
  // raw config map (ex. the exec plugin folds the contents of `cacheKeyFiles`
  // into its `cacheKey`). these must participate in the incremental hash so that
  // changing one of those files invalidates the cache. see issue #1135.
  #[test]
  fn incremental_hash_includes_resolved_config() {
    fn hash_with_resolved_config(resolved_config: &str) -> u64 {
      let plugin = Rc::new(PluginWrapper::new(Box::new(TestPlugin::new("test-plugin", "test-plugin", vec!["txt"], vec![]))));
      let format_config = Arc::new(FormatConfig {
        id: FormatConfigId::from_raw(1),
        global: Default::default(),
        plugin: Default::default(),
      });
      let plugin_with_config = PluginWithConfig::new(
        plugin,
        PluginWithConfigOptions {
          property_origins: Default::default(),
          associations: None,
          format_config,
          file_matching: FileMatchingInfo {
            file_extensions: vec!["txt".to_string()],
            file_names: vec![],
            additive: false,
          },
          overrides: Vec::new(),
          serialized_resolved_config: resolved_config.to_string(),
        },
      );
      let mut hasher = FastInsecureHasher::default();
      plugin_with_config.incremental_hash(&mut hasher);
      hasher.finish()
    }

    // the same raw config map but a different resolved config must produce a different hash
    assert_ne!(
      hash_with_resolved_config(r#"{"cacheKey":"a"}"#),
      hash_with_resolved_config(r#"{"cacheKey":"b"}"#)
    );
    // and the same resolved config must produce the same hash
    assert_eq!(
      hash_with_resolved_config(r#"{"cacheKey":"a"}"#),
      hash_with_resolved_config(r#"{"cacheKey":"a"}"#)
    );
  }

  #[test]
  fn should_include_shebangs_in_plugins_hash() {
    fn hash_with_shebangs(shebangs: Option<IndexMap<String, String>>) -> u64 {
      let environment = crate::environment::TestEnvironment::new();
      let base_path = CanonicalizedPathBuf::new_for_testing("/");
      let config = Rc::new(ResolvedConfig {
        origin: ConfigOrigin {
          source: PathSource::new_local(base_path.join_panic_relative("dprint.json")),
          base_path,
          is_global: false,
        },
        files: Default::default(),
        routing: FileRouting { shebangs },
        execution: Default::default(),
        plugins: Default::default(),
      });
      let scope = PluginsScope::new(environment, vec![Rc::new(create_plugin_with_overrides(Vec::new()))], config, Vec::new()).unwrap();
      scope.plugins_hash()
    }
    fn shebangs(entries: &[(&str, &str)]) -> Option<IndexMap<String, String>> {
      Some(entries.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect())
    }

    assert_eq!(hash_with_shebangs(None), hash_with_shebangs(None));
    assert_eq!(
      hash_with_shebangs(shebangs(&[("#!/bin/sh", "sh")])),
      hash_with_shebangs(shebangs(&[("#!/bin/sh", "sh")]))
    );
    assert_ne!(hash_with_shebangs(None), hash_with_shebangs(shebangs(&[("#!/bin/sh", "sh")])));
    // changing the mapped extension changes which plugin formats the file
    assert_ne!(
      hash_with_shebangs(shebangs(&[("#!/bin/sh", "sh")])),
      hash_with_shebangs(shebangs(&[("#!/bin/sh", "txt")]))
    );
  }

  #[test]
  fn should_hash_associations_with_boundaries() {
    fn hash_with_associations(associations: Vec<&str>) -> u64 {
      let mut plugin = create_plugin_with_overrides(Vec::new());
      plugin.associations = Some(associations.into_iter().map(ToOwned::to_owned).collect());
      get_plugin_hash(&plugin)
    }

    assert_ne!(hash_with_associations(vec!["ab", "c"]), hash_with_associations(vec!["a", "bc"]));
  }

  #[test]
  fn should_hash_override_file_patterns_and_property_keys_with_boundaries() {
    let plugin_ab_c = create_plugin_with_override(vec!["ab".to_string()], ConfigKeyMap::from([("c".to_string(), ConfigKeyValue::from_bool(true))]));
    let plugin_a_bc = create_plugin_with_override(vec!["a".to_string()], ConfigKeyMap::from([("bc".to_string(), ConfigKeyValue::from_bool(true))]));

    assert_ne!(get_plugin_hash(&plugin_ab_c), get_plugin_hash(&plugin_a_bc));
  }

  #[test]
  fn should_include_config_overrides_in_incremental_hash() {
    let config_base_path = CanonicalizedPathBuf::new_for_testing("/");
    let plugin_without_override = create_plugin_with_overrides(Vec::new());
    let plugin_with_override = create_plugin_with_overrides(vec![PluginConfigOverride {
      files: vec!["**/package.txt".to_string()],
      properties: ConfigKeyMap::from([("ending".to_string(), "package".into())]),
      config_id: FormatConfigId::from_raw(2),
      matcher: get_patterns_as_glob_matcher(&["**/package.txt".to_string()], &config_base_path).unwrap(),
      property_origins: Default::default(),
    }]);

    assert_ne!(get_plugin_hash(&plugin_without_override), get_plugin_hash(&plugin_with_override));
  }

  fn get_plugin_hash(plugin: &PluginWithConfig) -> u64 {
    let mut hasher = FastInsecureHasher::default();
    plugin.incremental_hash(&mut hasher);
    hasher.finish()
  }

  fn create_plugin_with_override(files: Vec<String>, properties: ConfigKeyMap) -> PluginWithConfig {
    let config_base_path = CanonicalizedPathBuf::new_for_testing("/config");
    let matcher = get_patterns_as_glob_matcher(&files, &config_base_path).unwrap();
    create_plugin_with_overrides(vec![PluginConfigOverride {
      files,
      properties,
      config_id: FormatConfigId::from_raw(2),
      matcher,
      property_origins: Default::default(),
    }])
  }

  fn create_plugin_with_overrides(overrides: Vec<PluginConfigOverride>) -> PluginWithConfig {
    PluginWithConfig::new(
      Rc::new(PluginWrapper::new(Box::new(TestPlugin::new("test-plugin", "test-plugin", vec!["txt"], vec![])))),
      PluginWithConfigOptions {
        property_origins: Default::default(),
        associations: None,
        format_config: Arc::new(FormatConfig {
          id: FormatConfigId::from_raw(1),
          plugin: ConfigKeyMap::from([("ending".to_string(), "base".into())]),
          global: GlobalConfiguration::default(),
        }),
        file_matching: FileMatchingInfo {
          file_extensions: vec!["txt".to_string()],
          file_names: Vec::new(),
          additive: false,
        },
        overrides,
        serialized_resolved_config: String::new(),
      },
    )
  }
}
