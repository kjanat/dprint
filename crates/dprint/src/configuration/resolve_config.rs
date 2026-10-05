use std::collections::HashMap;
use std::collections::HashSet;
use std::path::Path;

use anyhow::Result;
use deno_terminal::colors;
use thiserror::Error;

use crate::arg_parser::CliArgs;
use crate::arg_parser::ConfigDiscovery;
use crate::arg_parser::SubCommand;
use crate::environment::CanonicalizedPathBuf;
use crate::environment::Environment;
use crate::plugins::PluginSourceReference;
use crate::plugins::parse_plugin_source_reference;
use crate::utils::PathSource;
use crate::utils::PluginKind;
use crate::utils::ShowConfirmStrategy;
use crate::utils::resolve_path_source_to_file_with_cache;

use super::ConfigMap;
use super::ConfigMapValue;
use super::ConfigSettings;
use super::ExecutionPolicy;
use super::FileRouting;
use super::FileSelection;
use super::PluginConfiguration;
use super::PropertyOrigins;
use super::config_layer::ConfigDocument;
use super::config_layer::ConfigLayer;
use super::config_layer::ConfigReference;
use super::config_layer::LayerOrigin;
use super::remote_exec::RemoteExec;
use super::resolve_main_config_path::ResolvedConfigPathWithText;
use super::resolve_main_config_path::resolve_main_config_path_and_bytes;

/// A configuration with the configuration files it consists of combined: the
/// file it's from, the files that one extends and, for a nested configuration
/// file that inherits, its ancestor's configuration. What those files said
/// about how to combine them is used up.
#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedConfig {
  pub origin: ConfigOrigin,
  pub files: FileSelection,
  pub routing: FileRouting,
  pub execution: ExecutionPolicy,
  pub plugins: PluginConfiguration,
}

/// Where a configuration is from and what it applies to.
#[derive(Clone, Debug, PartialEq)]
pub struct ConfigOrigin {
  pub source: PathSource,
  /// The folder that should be considered the "root".
  pub base_path: CanonicalizedPathBuf,
  /// Whether this is the user's global configuration file.
  pub is_global: bool,
}

impl ResolvedConfig {
  pub fn new(origin: ConfigOrigin, settings: ConfigSettings) -> Self {
    // listing every group makes a new one fail to compile until it's handled
    let ConfigSettings {
      files,
      routing,
      execution,
      plugins,
    } = settings;
    Self {
      origin,
      files,
      routing,
      execution,
      plugins,
    }
  }

  /// Adds what a configuration file this one extends says, where this
  /// configuration doesn't say otherwise.
  fn extend(&mut self, extended: ConfigSettings) -> Result<()> {
    let ConfigSettings {
      files,
      routing,
      execution,
      plugins,
    } = extended;
    self.files.extend(files)?;
    self.routing.extend(routing);
    self.execution.extend(execution);
    self.plugins.extend(plugins)
  }

  /// Adds the configuration of an ancestor directory, for a nested
  /// configuration file that specified `"inherit": true`, where this
  /// configuration doesn't say otherwise.
  fn inherit(&mut self, ancestor: &ResolvedConfig) -> Result<()> {
    let ResolvedConfig {
      origin,
      files,
      routing,
      execution,
      plugins,
    } = ancestor;
    self.files.inherit(files, &origin.base_path, &self.origin.base_path);
    self.routing.inherit(routing);
    self.execution.inherit(execution);
    self.plugins.inherit(plugins)
  }
}

#[derive(Debug, Error)]
#[error(transparent)]
pub enum ResolveConfigError {
  #[error(
    "No config file found at {}. Did you mean to create (dprint init) or specify one (--config <path>)?\n\n{}",
    .config_path.display(),
    colors::gray("Note: dprint now supports global configuration. Set it up with `dprint init --global` then edit with `dprint config edit --global`")
  )]
  NotFound {
    config_path: CanonicalizedPathBuf,
    #[source]
    inner: Option<anyhow::Error>,
  },
  #[error("Config discovery was disabled and no plugins (--plugins <url/path>) and/or config (--config <path>) was specified.")]
  ConfigDiscoveryDisabled,
  Other(#[from] anyhow::Error),
}

pub async fn resolve_config_from_args(args: &CliArgs, environment: &impl Environment) -> Result<ResolvedConfig, ResolveConfigError> {
  struct ConfirmFormatGlobalConfigStrategy<'a> {
    directory: &'a Path,
  }

  impl ShowConfirmStrategy for ConfirmFormatGlobalConfigStrategy<'_> {
    fn render(&self, selected: Option<bool>) -> String {
      format!(
        "{} You're not in a dprint project. Format '{}' anyway? {}{}",
        colors::yellow("Warning"),
        self.directory.display(),
        match selected {
          Some(true) => "Y",
          Some(false) => "N",
          None => "(Y/n) \u{2588}",
        },
        match selected {
          Some(_) => colors::gray(""),
          None => colors::gray("\n\nHint: Specify the directory to bypass this prompt in the future (ex. `dprint fmt .`)"),
        },
      )
    }

    fn default_value(&self) -> bool {
      true
    }
  }

  let config_path_and_bytes = resolve_main_config_path_and_bytes(args, environment).await?;
  let mut resolved_config = match config_path_and_bytes {
    Some(resolved_config_path) => {
      if resolved_config_path.is_global_config
        && let SubCommand::Fmt(fmt) = &args.sub_command
        && !fmt.allow_no_files
        && fmt.patterns.include_patterns.is_none()
        && fmt.patterns.include_pattern_overrides.is_none()
        && !(args.config_discovery_arg_set() && matches!(args.config_discovery(environment), ConfigDiscovery::Global))
      {
        if !environment.is_terminal_interactive() {
          return Err(ResolveConfigError::Other(anyhow::anyhow!(
            "Did not format directory without configuration file. Run `dprint fmt .` or `dprint fmt --config-discovery=global` to bypass this error."
          )));
        } else if !environment.confirm_with_strategy(&ConfirmFormatGlobalConfigStrategy {
          directory: resolved_config_path.base_path.as_ref(),
        })? {
          return Err(ResolveConfigError::Other(anyhow::anyhow!("Confirmation cancelled.")));
        }
      }
      resolve_config_from_path_with_bytes(&resolved_config_path, environment).await?
    }
    None => {
      if !args.plugins.is_empty() {
        // allow no config file when plugins are specified
        ResolvedConfig::new(
          ConfigOrigin {
            source: PathSource::new_local(environment.cwd().join_panic_relative("dprint.json")),
            base_path: environment.cwd().clone(),
            is_global: false,
          },
          ConfigSettings::default(),
        )
      } else if args.config_discovery(environment).traverse_ancestors() {
        return Err(ResolveConfigError::NotFound {
          config_path: environment.cwd().join_panic_relative("dprint.json"),
          inner: None,
        });
      } else {
        return Err(ResolveConfigError::ConfigDiscoveryDisabled);
      }
    }
  };

  if !args.plugins.is_empty() {
    let base_path = PathSource::new_local(environment.cwd());
    let mut plugins = Vec::with_capacity(args.plugins.len());
    for url_or_file_path in args.plugins.iter() {
      plugins.push(parse_plugin_source_reference(url_or_file_path, &base_path, environment)?);
    }

    resolved_config.plugins.sources = plugins;
  }

  Ok(resolved_config)
}

pub async fn resolve_config_from_path_with_bytes<TEnvironment: Environment>(
  config_path_and_text: &ResolvedConfigPathWithText,
  environment: &TEnvironment,
) -> Result<ResolvedConfig, ResolveConfigError> {
  resolve_config_file(config_path_and_text, None, environment).await
}

/// Resolves a configuration file in a directory within the directory of the
/// `ancestor` configuration, which it inherits when it specifies
/// `"inherit": true`.
pub async fn resolve_descendant_config_from_path_with_bytes<TEnvironment: Environment>(
  config_path_and_text: &ResolvedConfigPathWithText,
  ancestor: &ResolvedConfig,
  environment: &TEnvironment,
) -> Result<ResolvedConfig, ResolveConfigError> {
  resolve_config_file(config_path_and_text, Some(ancestor), environment).await
}

/// Resolves a configuration file in three steps: its layers are collected,
/// which uses up their directives, then combined in order of precedence, and
/// last the ancestor's configuration is inherited when it says to.
async fn resolve_config_file<TEnvironment: Environment>(
  config_path_and_text: &ResolvedConfigPathWithText,
  ancestor: Option<&ResolvedConfig>,
  environment: &TEnvironment,
) -> Result<ResolvedConfig, ResolveConfigError> {
  let root = ConfigDocument {
    file: config_path_and_text.as_file_path_with_text_ref(),
    is_first_download: config_path_and_text.is_first_download,
    extended_by: &[],
  }
  .parse(environment)?;
  let collected = collect_layers(root, environment).await?;

  let mut remote_exec = RemoteExec::default();
  let mut layers = collected.layers.into_iter();
  let mut root = layers.next().expect("the configuration file being resolved");
  apply_remote_restrictions(&mut root, &ConfigMap::new(), &mut remote_exec, environment);
  root.record_property_origins();
  let mut config = ResolvedConfig::new(
    ConfigOrigin {
      source: root.origin.source,
      base_path: config_path_and_text.base_path.clone(),
      is_global: config_path_and_text.is_global_config,
    },
    root.settings,
  );
  for mut layer in layers {
    apply_remote_restrictions(&mut layer, &config.plugins.config, &mut remote_exec, environment);
    layer.record_property_origins();
    if let Err(err) = config.extend(layer.settings) {
      return Err(layer.origin.locate(err).into());
    }
  }
  remote_exec.apply(&mut config.plugins, environment)?;

  if let Some(ancestor) = ancestor
    && collected.inherit
  {
    config.inherit(ancestor)?;
  }
  Ok(config)
}

/// A configuration file whose directives were used.
struct CollectedLayer {
  origin: LayerOrigin,
  settings: ConfigSettings,
}

impl CollectedLayer {
  /// Records that its plugin configuration is from this file, for
  /// diagnostics.
  fn record_property_origins(&mut self) {
    let plugins = &mut self.settings.plugins;
    plugins.origins = PropertyOrigins::of(&plugins.config, &self.origin.source);
    for value in plugins.config.values_mut() {
      if let ConfigMapValue::PluginConfig(plugin_config) = value {
        for override_config in &mut plugin_config.overrides {
          override_config.origin.0.get_or_insert_with(|| self.origin.source.clone());
        }
      }
    }
  }
}

struct CollectedLayers {
  /// Whether the configuration file being resolved inherits its ancestor's
  /// configuration.
  inherit: bool,
  /// The configuration file being resolved and the files it extends, highest
  /// precedence first.
  layers: Vec<CollectedLayer>,
}

/// Reads the configuration files a configuration file extends, directly or
/// through another one. A file comes before the files it extends, and those
/// come in the order it lists them, each followed by the files it extends
/// (depth first). That's their precedence, highest first.
///
/// A file that's extended more than once (ex. by two files that are both
/// extended) is only read where it has the highest precedence, since its
/// properties are already set everywhere after that. A file that extends
/// itself, directly or through other files, is an error.
async fn collect_layers(root: ConfigLayer, environment: &impl Environment) -> Result<CollectedLayers> {
  struct PendingReference {
    reference: ConfigReference,
    referrer: LayerOrigin,
  }

  fn push_references(pending: &mut Vec<PendingReference>, referrer: &LayerOrigin, extends: Vec<ConfigReference>) {
    // reversed, so the first one is read next
    for reference in extends.into_iter().rev() {
      pending.push(PendingReference {
        reference,
        referrer: referrer.clone(),
      });
    }
  }

  /// Errors when the reference goes back to the referrer or a file that
  /// extends it.
  fn ensure_not_cycle(reference: &ConfigReference, target: &PathSource, referrer: &LayerOrigin) -> Result<()> {
    if referrer.source != *target && !referrer.extended_by.contains(target) {
      return Ok(());
    }
    let chain = referrer
      .extended_by
      .iter()
      .rev()
      .chain([&referrer.source, target])
      .map(|source| source.display())
      .collect::<Vec<_>>();
    Err(referrer.locate(anyhow::anyhow!(
      "The configuration file '{}' extends itself: {}",
      reference.specifier,
      chain.join(" -> ")
    )))
  }

  let ConfigLayer { origin, directives, settings } = root;
  let inherit = directives.inherit;
  let mut pending = Vec::new();
  push_references(&mut pending, &origin, directives.extends);
  let mut collected_sources = HashSet::from([origin.source.clone()]);
  // where each reference that was read went, which differs for a redirect
  let mut read_targets = HashMap::<PathSource, PathSource>::new();
  let mut layers = vec![CollectedLayer { origin, settings }];

  while let Some(PendingReference { reference, referrer }) = pending.pop() {
    // a reference that was read before is to the file it went to then
    let target = read_targets.get(&reference.target).unwrap_or(&reference.target);
    ensure_not_cycle(&reference, target, &referrer)?;
    if collected_sources.contains(target) {
      continue;
    }
    let file = match resolve_path_source_to_file_with_cache(reference.target.clone(), environment)
      .await
      .and_then(|file| file.into_text())
    {
      Ok(file) => file,
      Err(err) => return Err(referrer.locate(err)),
    };
    // the file may be somewhere else than the reference says (ex. a redirect)
    ensure_not_cycle(&reference, &file.source, &referrer)?;
    read_targets.insert(reference.target, file.source.clone());
    if !collected_sources.insert(file.source.clone()) {
      continue;
    }
    let extended_by = std::iter::once(referrer.source.clone())
      .chain(referrer.extended_by.iter().cloned())
      .collect::<Vec<_>>();
    let ConfigLayer { origin, directives, settings } = ConfigDocument {
      file: file.as_ref(),
      is_first_download: file.is_first_download,
      extended_by: &extended_by,
    }
    .parse(environment)?;
    if directives.inherit {
      return Err(origin.locate(anyhow::anyhow!(
        "The 'inherit' property can't be used in an extended configuration file. Specify it in the configuration file that extends it."
      )));
    }
    push_references(&mut pending, &origin, directives.extends);
    layers.push(CollectedLayer { origin, settings });
  }

  Ok(CollectedLayers { inherit, layers })
}

/// Removes what a remote configuration file isn't trusted to specify, since it
/// could make dprint run or change anything: which files are formatted,
/// non-wasm plugins, and exec commands a local configuration file doesn't
/// allow (see `remote_exec.rs`). `resolved_config` is the configuration of
/// higher precedence resolved so far.
fn apply_remote_restrictions(layer: &mut CollectedLayer, resolved_config: &ConfigMap, remote_exec: &mut RemoteExec, environment: &impl Environment) {
  if layer.origin.source.is_local() {
    return;
  }

  // IMPORTANT
  // =========
  // Remove the includes from remote configuration since we don't want it
  // specifying something like system or some configuration
  // files that it could change. Basically, the end user should have 100%
  // control over what files get formatted.
  // Careful! Don't be fancy and ensure this is removed.
  let removed_includes = layer.settings.files.includes.take(); // NEVER REMOVE THIS STATEMENT
  if removed_includes.is_some() && layer.origin.is_first_download {
    log_warn!(environment, &get_warn_includes_message());
  }

  // the exec plugin and its commands only run when local config allows it
  let plugins = remote_exec.take_from_remote_config(
    &mut layer.settings.plugins.config,
    std::mem::take(&mut layer.settings.plugins.sources),
    resolved_config,
    &layer.origin.source,
    environment,
  );
  // The assumption here is that the user won't be malicious to themselves, so
  // this is only for remote configuration.
  layer.settings.plugins.sources = filter_non_wasm_plugins(plugins, environment); // NEVER REMOVE THIS STATEMENT
  // =========
}

fn filter_non_wasm_plugins(plugins: Vec<PluginSourceReference>, environment: &impl Environment) -> Vec<PluginSourceReference> {
  if plugins.iter().any(|plugin| plugin.plugin_kind() != Some(PluginKind::Wasm)) {
    log_warn!(environment, &get_warn_non_wasm_plugins_message());
    plugins.into_iter().filter(|plugin| plugin.plugin_kind() == Some(PluginKind::Wasm)).collect()
  } else {
    plugins
  }
}

fn get_warn_includes_message() -> String {
  format!(
    "{} The 'includes' property is ignored for security reasons on remote configuration.",
    colors::bold("Note: "),
  )
}

fn get_warn_non_wasm_plugins_message() -> String {
  format!(
    "{} Non-wasm plugins are ignored for security reasons on remote configuration.",
    colors::bold("Note: "),
  )
}

#[cfg(test)]
mod tests {
  use std::path::PathBuf;

  use crate::arg_parser::parse_args;
  use crate::configuration::ConfigMapValue;
  use crate::configuration::RawPluginConfig;
  use crate::configuration::RawPluginConfigOverride;
  use crate::configuration::json_config_text_to_toml;
  use crate::environment::Environment;
  use crate::environment::TestEnvironment;
  use crate::environment::TestEnvironmentBuilder;
  use crate::utils::TestStdInReader;
  use anyhow::Result;
  use dprint_core::configuration::ConfigKeyMap;
  use dprint_core::configuration::ConfigKeyValue;
  use indexmap::IndexMap;
  use pretty_assertions::assert_eq;

  use super::*;

  async fn get_result(url: &str, environment: &impl Environment) -> Result<ResolvedConfig, ResolveConfigError> {
    let args = parse_args(
      vec![String::from(""), String::from("check"), String::from("-c"), String::from(url)],
      TestStdInReader::default(),
    )
    .unwrap();
    resolve_config_from_args(&args, environment).await
  }

  fn local_config_path(path: &str, environment: &TestEnvironment) -> ResolvedConfigPathWithText {
    let canonical = environment.canonicalize(path).unwrap();
    ResolvedConfigPathWithText {
      content: environment.read_file(&canonical).unwrap(),
      base_path: canonical.parent().unwrap(),
      source: PathSource::new_local(canonical),
      is_global_config: false,
      is_first_download: false,
    }
  }

  fn test_origin(config_path: &str, base_path: &str) -> ConfigOrigin {
    ConfigOrigin {
      source: PathSource::new_local(CanonicalizedPathBuf::new_for_testing(config_path)),
      base_path: CanonicalizedPathBuf::new_for_testing(base_path),
      is_global: false,
    }
  }

  async fn resolve_local_config(path: &str, environment: &TestEnvironment) -> ResolvedConfig {
    resolve_config_from_path_with_bytes(&local_config_path(path, environment), environment)
      .await
      .unwrap()
  }

  async fn resolve_local_descendant_config(path: &str, ancestor: &ResolvedConfig, environment: &TestEnvironment) -> Result<ResolvedConfig, ResolveConfigError> {
    resolve_descendant_config_from_path_with_bytes(&local_config_path(path, environment), ancestor, environment).await
  }

  /// Where each configuration file of [`resolve_in_every_format`] is written.
  #[derive(Clone)]
  struct ConfigPaths(IndexMap<String, String>);

  impl ConfigPaths {
    /// The local path or url of the file of that name.
    fn get(&self, name: &str) -> &str {
      &self.0[name]
    }
  }

  /// Writes the configuration files in every combination of JSON and TOML,
  /// resolves them with `resolve` in each, and asserts that every combination
  /// resolves the same as when they're all JSON, which it gives.
  ///
  /// The files are given in JSON by name. A name that's a url is a remote
  /// file, and any other one is a file in `/`. In a file, `<name>` stands for
  /// the file name the file of that name has in the combination (ex.
  /// `"extends": "./<base>"` for `./base.json` or `./base.toml`). What's
  /// resolved is compared with the file names in TOML ones made JSON.
  fn resolve_in_every_format<T: std::fmt::Debug>(files: &[(&str, &str)], resolve: impl AsyncFn(&TestEnvironment, &ConfigPaths) -> T) -> T {
    let file_name = |name: &str, toml: bool| format!("{}.{}", name, if toml { "toml" } else { "json" });
    let mut all_json = None;
    for combination in 0..1usize << files.len() {
      let is_toml = |index: usize| combination & (1 << index) != 0;
      let environment = TestEnvironment::new();
      let paths = ConfigPaths(
        files
          .iter()
          .enumerate()
          .map(|(index, (name, _))| {
            let path = if name.starts_with("https://") {
              file_name(name, is_toml(index))
            } else {
              format!("/{}", file_name(name, is_toml(index)))
            };
            (name.to_string(), path)
          })
          .collect(),
      );
      for (index, (name, json_text)) in files.iter().enumerate() {
        let mut text = json_text.to_string();
        for (other_index, (other_name, _)) in files.iter().enumerate() {
          text = text.replace(&format!("<{}>", other_name), &file_name(other_name, is_toml(other_index)));
        }
        if is_toml(index) {
          text = json_config_text_to_toml(&text).unwrap();
        }
        let path = paths.get(name);
        if path.starts_with("https://") {
          environment.add_remote_file_bytes(path, text.into_bytes());
        } else {
          environment.mk_dir_all(Path::new(path).parent().unwrap()).unwrap();
          environment.write_file(path, &text).unwrap();
        }
      }
      let result = environment.clone().run_in_runtime(resolve(&environment, &paths));
      let text = format!("{:#?}", result).replace(".toml", ".json");
      match &all_json {
        None => all_json = Some((text, result)),
        Some((json_text, _)) => {
          let formats = files
            .iter()
            .enumerate()
            .map(|(index, (name, _))| format!("{} as {}", name, if is_toml(index) { "TOML" } else { "JSON" }))
            .collect::<Vec<_>>();
          assert_eq!(&text, json_text, "{}", formats.join(", "));
        }
      }
    }
    all_json.unwrap().1
  }

  #[test]
  fn inherit_config_should_keep_config_dir_relative_to_each_config_file() {
    // ${configDir} is expanded when each config file is parsed (before the inherit
    // merge), so an inherited value keeps the ancestor's directory rather than being
    // re-based to the nested config file's directory.
    let environment = TestEnvironmentBuilder::new()
      .write_file(
        "/a/dprint.json",
        r#"{
            "test": {
              "fromAncestor": "${configDir}/value"
            }
        }"#,
      )
      .write_file(
        "/a/b/dprint.json",
        r#"{
            "inherit": true,
            "test": {
              "fromNested": "${configDir}/value"
            }
        }"#,
      )
      .build();

    environment.clone().run_in_runtime(async move {
      let parent = resolve_local_config("/a/dprint.json", &environment).await;
      let result = resolve_local_descendant_config("/a/b/dprint.json", &parent, &environment).await.unwrap();
      assert_eq!(
        result.plugins.config,
        ConfigMap::from([(
          "test".to_string(),
          ConfigMapValue::PluginConfig(RawPluginConfig {
            locked: false,
            associations: None,
            overrides: Vec::new(),
            properties: ConfigKeyMap::from([
              // the nested config file's ${configDir} points at its own directory...
              ("fromNested".to_string(), ConfigKeyValue::from_str("/a/b/value")),
              // ...while the inherited value still points at the ancestor's directory
              ("fromAncestor".to_string(), ConfigKeyValue::from_str("/a/value")),
            ]),
          }),
        )])
      );
    });
  }

  #[test]
  fn should_get_local_config_file() {
    let environment = TestEnvironment::new();
    environment
      .write_file(
        &PathBuf::from("/test.json"),
        r#"{
            "plugins": ["https://plugins.dprint.dev/test-plugin.wasm"],
            "includes": ["test"],
            "excludes": ["test-excludes"]
        }"#,
      )
      .unwrap();

    environment.clone().run_in_runtime(async move {
      let result = get_result("/test.json", &environment).await.unwrap();
      assert_eq!(environment.take_stdout_messages().len(), 0);
      assert_eq!(result.origin.base_path, CanonicalizedPathBuf::new_for_testing("/"));
      assert_eq!(result.origin.source.is_local(), true);
      assert_eq!(result.plugins.config.contains_key("includes"), false);
      assert_eq!(result.plugins.config.contains_key("excludes"), false);
      assert_eq!(result.files.includes, Some(vec!["test".to_string()]));
      assert_eq!(result.files.excludes, vec!["test-excludes".to_string()]);
    });
  }

  #[test]
  fn should_not_read_schema_and_project_type_as_configuration() {
    let environment = TestEnvironmentBuilder::new()
      .write_file(
        "/base.toml",
        r#"#:schema ./dprint.schema.json
"$schema" = "./dprint.schema.json"
projectType = "openSource"
lineWidth = 80
"#,
      )
      .write_file(
        "/dprint.json",
        r#"{
            "$schema": "https://dprint.dev/schemas/v0.json",
            "projectType": "openSource",
            "extends": "./base.toml",
            "plugins": ["https://plugins.dprint.dev/test-plugin.wasm"]
        }"#,
      )
      .build();

    environment.clone().run_in_runtime(async move {
      let result = resolve_local_config("/dprint.json", &environment).await;
      // the global configuration only has the extended file's `lineWidth`
      assert_eq!(
        result.plugins.config,
        ConfigMap::from([("lineWidth".to_string(), ConfigMapValue::from_i32(80))])
      );
    });
  }

  #[test]
  fn should_error_when_extends_cycle() {
    let environment = TestEnvironmentBuilder::new()
      .write_file(
        "/a.json",
        r#"{ "extends": "./b.json", "plugins": ["https://plugins.dprint.dev/test-plugin.wasm"] }"#,
      )
      .write_file("/b.json", r#"{ "extends": "./a.json" }"#)
      .build();

    environment.clone().run_in_runtime(async move {
      let err = get_result("/a.json", &environment).await.err().unwrap();
      assert_eq!(
        err.to_string(),
        "The configuration file './a.json' extends itself: /a.json -> /b.json -> /a.json\n    at /b.json"
      );
    });
  }

  #[test]
  fn should_error_when_extends_cycle_goes_through_a_redirect() {
    // x.json redirects to b.json, which extends x.json: so b.json extends itself
    let environment = TestEnvironment::new();
    environment.write_file("/dprint.json", r#"{ "extends": "https://dprint.dev/x.json" }"#).unwrap();
    environment.add_remote_file_redirect("https://dprint.dev/x.json", "https://dprint.dev/b.json");
    environment.add_remote_file("https://dprint.dev/b.json", r#"{ "extends": "https://dprint.dev/x.json" }"#.as_bytes());

    environment.clone().run_in_runtime(async move {
      let err = get_result("/dprint.json", &environment).await.err().unwrap();
      assert_eq!(
        err.to_string(),
        concat!(
          "The configuration file 'https://dprint.dev/x.json' extends itself: ",
          "/dprint.json -> https://dprint.dev/b.json -> https://dprint.dev/b.json\n",
          "    at https://dprint.dev/b.json"
        )
      );
    });
  }

  #[test]
  fn should_error_when_config_extends_itself() {
    let environment = TestEnvironmentBuilder::new()
      .write_file("/a.json", r#"{ "extends": ["./b.json", "./a.json"] }"#)
      .write_file("/b.json", r#"{}"#)
      .build();

    environment.clone().run_in_runtime(async move {
      let err = get_result("/a.json", &environment).await.err().unwrap();
      assert_eq!(
        err.to_string(),
        "The configuration file './a.json' extends itself: /a.json -> /a.json\n    at /a.json"
      );
    });
  }

  #[test]
  fn should_error_when_remote_extends_cycle() {
    let environment = TestEnvironment::new();
    environment.add_remote_file("https://dprint.dev/a.json", r#"{ "extends": "./b.json" }"#.as_bytes());
    environment.add_remote_file("https://dprint.dev/b.json", r#"{ "extends": "./c.json" }"#.as_bytes());
    environment.add_remote_file("https://dprint.dev/c.json", r#"{ "extends": "https://dprint.dev/b.json" }"#.as_bytes());

    environment.clone().run_in_runtime(async move {
      let err = get_result("https://dprint.dev/a.json", &environment).await.err().unwrap();
      assert_eq!(
        err.to_string(),
        concat!(
          "The configuration file 'https://dprint.dev/b.json' extends itself: ",
          "https://dprint.dev/a.json -> https://dprint.dev/b.json -> https://dprint.dev/c.json -> https://dprint.dev/b.json\n",
          "    at https://dprint.dev/c.json\n",
          "    at https://dprint.dev/b.json"
        )
      );
    });
  }

  #[test]
  fn should_use_a_config_extended_more_than_once_where_it_has_the_highest_precedence() {
    let environment = TestEnvironmentBuilder::new()
      .write_file(
        "/dprint.json",
        r#"{
            "extends": ["./b.json", "./c.json"],
            "plugins": ["https://plugins.dprint.dev/test-plugin.wasm"]
        }"#,
      )
      .write_file("/b.json", r#"{ "extends": "./d.json", "excludes": ["b"] }"#)
      .write_file("/c.json", r#"{ "extends": "./d.json", "excludes": ["c"], "lineWidth": 100 }"#)
      .write_file(
        "/d.json",
        r#"{
            "excludes": ["d"],
            "lineWidth": 80,
            "test": { "overrides": { "files": "*.d", "prop": 1 } }
        }"#,
      )
      .build();

    environment.clone().run_in_runtime(async move {
      let result = resolve_local_config("/dprint.json", &environment).await;
      // d comes after b, which extends it first, and only once
      assert_eq!(result.files.excludes, vec!["b".to_string(), "d".to_string(), "c".to_string()]);
      assert_eq!(result.plugins.config.get("lineWidth"), Some(&ConfigMapValue::from_i32(80)));
      let Some(ConfigMapValue::PluginConfig(test)) = result.plugins.config.get("test") else {
        unreachable!();
      };
      assert_eq!(
        test.overrides,
        vec![RawPluginConfigOverride {
          files: vec!["*.d".to_string()],
          properties: ConfigKeyMap::from([("prop".to_string(), ConfigKeyValue::from_i32(1))]),

          origin: Default::default(),
        }]
      );
    });
  }

  #[test]
  fn should_error_for_includes_in_extended_config() {
    let environment = TestEnvironmentBuilder::new()
      .write_file("/dprint.json", r#"{ "extends": "./base.json", "includes": ["src/**"] }"#)
      .write_file("/base.json", r#"{ "includes": ["**/*.ts"] }"#)
      .build();

    environment.clone().run_in_runtime(async move {
      let err = get_result("/dprint.json", &environment).await.err().unwrap();
      assert_eq!(
        err.to_string(),
        concat!(
          "The 'includes' property can't be used in an extended configuration file. Specify it in the configuration file that extends it.\n",
          "    at /base.json"
        )
      );
    });
  }

  #[test]
  fn should_ignore_includes_in_extended_remote_config() {
    let environment = TestEnvironment::new();
    environment.add_remote_file(
      "https://dprint.dev/base.json",
      r#"{ "includes": ["/etc/**"], "excludes": ["dist"] }"#.as_bytes(),
    );
    environment
      .write_file("/dprint.json", r#"{ "extends": "https://dprint.dev/base.json" }"#)
      .unwrap();

    environment.clone().run_in_runtime(async move {
      let result = get_result("/dprint.json", &environment).await.unwrap();
      assert_eq!(environment.take_stderr_messages(), vec![get_warn_includes_message()]);
      assert_eq!(result.files.includes, None);
      assert_eq!(result.files.excludes, vec!["dist".to_string()]);
    });
  }

  #[test]
  fn should_error_for_inherit_in_extended_config() {
    let environment = TestEnvironmentBuilder::new()
      .write_file("/a/dprint.json", r#"{ "extends": "../base.json" }"#)
      .write_file("/base.json", r#"{ "inherit": true }"#)
      .build();

    environment.clone().run_in_runtime(async move {
      let err = get_result("/a/dprint.json", &environment).await.err().unwrap();
      assert_eq!(
        err.to_string(),
        concat!(
          "The 'inherit' property can't be used in an extended configuration file. Specify it in the configuration file that extends it.\n",
          "    at /base.json"
        )
      );
    });
  }

  #[test]
  fn should_use_incremental_of_extended_config_when_not_specified() {
    let environment = TestEnvironmentBuilder::new()
      .write_file("/dprint.json", r#"{ "extends": "./base.json" }"#)
      .write_file("/specified.json", r#"{ "extends": "./base.json", "incremental": true }"#)
      .write_file("/base.json", r#"{ "incremental": false }"#)
      .build();

    environment.clone().run_in_runtime(async move {
      let result = resolve_local_config("/dprint.json", &environment).await;
      assert_eq!(result.execution.incremental, Some(false));
      // and not as an unknown global configuration property
      assert_eq!(result.plugins.config, ConfigMap::new());
      let result = resolve_local_config("/specified.json", &environment).await;
      assert_eq!(result.execution.incremental, Some(true));
    });
  }

  #[test]
  fn should_record_which_file_each_property_is_from() {
    let environment = TestEnvironment::new();
    environment.add_remote_file(
      "https://dprint.dev/exec.json",
      r#"{
            "indentWidth": 4,
            "exec": { "commands": [{ "command": "tombi format -", "exts": ["toml"] }], "timeout": 5 }
        }"#
        .as_bytes(),
    );
    environment
      .write_file(
        "/dprint.json",
        r#"{
            "extends": "https://dprint.dev/exec.json",
            "lineWidth": 80,
            "exec": { "playWithFire": true, "timeout": 10 }
        }"#,
      )
      .unwrap();

    environment.clone().run_in_runtime(async move {
      let result = get_result("/dprint.json", &environment).await.unwrap();
      let remote = PathSource::new_remote_from_str("https://dprint.dev/exec.json");
      let origins = &result.plugins.origins;
      assert_eq!(origins.root_elsewhere("indentWidth", &result.origin.source), Some(&remote));
      assert_eq!(origins.root_elsewhere("lineWidth", &result.origin.source), None);
      // the remote commands the local configuration allows are from the remote file,
      // while the timeout specified in both is the local one
      assert_eq!(
        origins.plugin_elsewhere("exec", &result.origin.source),
        IndexMap::from([("commands".to_string(), remote.clone()), ("indentWidth".to_string(), remote)])
      );
    });
  }

  #[test]
  fn should_get_remote_config_file() {
    let environment = TestEnvironment::new();
    environment.add_remote_file(
      "https://dprint.dev/test.json",
      r#"{
            "plugins": ["https://plugins.dprint.dev/test-plugin.wasm"]
        }"#
        .as_bytes(),
    );

    environment.clone().run_in_runtime(async move {
      let result = get_result("https://dprint.dev/test.json", &environment).await.unwrap();
      assert_eq!(environment.take_stdout_messages().len(), 0);
      assert_eq!(result.origin.base_path, CanonicalizedPathBuf::new_for_testing("/"));
      assert_eq!(result.origin.source.is_remote(), true);
    });
  }

  #[test]
  fn should_warn_on_first_download_for_remote_config_with_includes() {
    let environment = TestEnvironment::new();
    environment.add_remote_file(
      "https://dprint.dev/test.json",
      r#"{
            "plugins": ["https://plugins.dprint.dev/test-plugin.wasm"],
            "includes": ["test"]
        }"#
        .as_bytes(),
    );

    environment.clone().run_in_runtime(async move {
      let result = get_result("https://dprint.dev/test.json", &environment).await.unwrap();
      assert_eq!(environment.take_stdout_messages().len(), 0);
      assert_eq!(environment.take_stderr_messages(), vec![get_warn_includes_message()]);
      assert_eq!(result.files.includes, None);

      environment.clear_logs();
      let result = get_result("https://dprint.dev/test.json", &environment).await.unwrap();
      assert_eq!(environment.take_stderr_messages().len(), 0); // no warning this time
      assert_eq!(result.files.includes, None);
    });
  }

  #[test]
  fn should_warn_on_first_download_for_remote_config_with_includes_and_excludes() {
    let environment = TestEnvironment::new();
    environment.add_remote_file(
      "https://dprint.dev/test.json",
      r#"{
            "plugins": ["https://plugins.dprint.dev/test-plugin.wasm"],
            "includes": [],
            "excludes": []
        }"#
        .as_bytes(),
    );

    environment.clone().run_in_runtime(async move {
      let result = get_result("https://dprint.dev/test.json", &environment).await.unwrap();
      assert_eq!(environment.take_stderr_messages(), vec![get_warn_includes_message()]);
      assert_eq!(result.files.includes, None);
      assert_eq!(result.files.excludes, Vec::<String>::new());

      environment.clear_logs();
      let result = get_result("https://dprint.dev/test.json", &environment).await.unwrap();
      assert_eq!(environment.take_stderr_messages().len(), 0); // no warning this time
      assert_eq!(result.files.includes, None);
      assert_eq!(result.files.excludes, Vec::<String>::new());
    });
  }

  #[test]
  fn should_not_warn_remove_config_no_includes_or_excludes() {
    let environment = TestEnvironment::new();
    environment.add_remote_file(
      "https://dprint.dev/test.json",
      r#"{
            "plugins": ["https://plugins.dprint.dev/test-plugin.wasm"]
        }"#
        .as_bytes(),
    );

    environment.clone().run_in_runtime(async move {
      get_result("https://dprint.dev/test.json", &environment).await.unwrap();
      assert_eq!(environment.take_stdout_messages().len(), 0);
    });
  }

  #[test]
  fn should_handle_single_extends() {
    let environment = TestEnvironment::new();
    environment.add_remote_file(
      "https://dprint.dev/test.json",
      r#"{
            "plugins": ["https://plugins.dprint.dev/test-plugin2.wasm"],
            "lineWidth": 4,
            "otherProp": { "test": 4 }, // should ignore
            "otherProp2": "a",
            "test": {
                "prop": 6,
                "other": "test"
            },
            "test2": {
                "prop": 2
            },
            "includes": ["test"],
            "excludes": ["test-excludes"]
        }"#
        .as_bytes(),
    );
    environment
      .write_file(
        &PathBuf::from("/test.json"),
        r#"{
            "extends": "https://dprint.dev/test.json",
            "plugins": ["https://plugins.dprint.dev/test-plugin.wasm"],
            "lineWidth": 1,
            "otherProp": 6,
            "test": {
                "prop": 5
            },
        }"#,
      )
      .unwrap();

    environment.clone().run_in_runtime(async move {
      let result = get_result("/test.json", &environment).await.unwrap();
      assert_eq!(environment.take_stdout_messages().len(), 0);
      assert_eq!(result.origin.base_path, CanonicalizedPathBuf::new_for_testing("/"));
      assert_eq!(result.origin.source.is_local(), true);
      assert_eq!(result.files.includes, None);
      assert_eq!(result.files.excludes, vec!["test-excludes".to_string()]);
      assert_eq!(
        result.plugins.sources,
        vec![
          PluginSourceReference::new_remote_from_str("https://plugins.dprint.dev/test-plugin.wasm"),
          PluginSourceReference::new_remote_from_str("https://plugins.dprint.dev/test-plugin2.wasm"),
        ]
      );

      let expected_config_map = ConfigMap::from([
        (String::from("lineWidth"), ConfigMapValue::from_i32(1)),
        (String::from("otherProp"), ConfigMapValue::from_i32(6)),
        (String::from("otherProp2"), ConfigMapValue::from_str("a")),
        (
          String::from("test"),
          ConfigMapValue::PluginConfig(RawPluginConfig {
            locked: false,
            associations: None,
            overrides: Vec::new(),
            properties: ConfigKeyMap::from([
              (String::from("prop"), ConfigKeyValue::from_i32(5)),
              (String::from("other"), ConfigKeyValue::from_str("test")),
            ]),
          }),
        ),
        (
          String::from("test2"),
          ConfigMapValue::PluginConfig(RawPluginConfig {
            locked: false,
            associations: None,
            overrides: Vec::new(),
            properties: ConfigKeyMap::from([(String::from("prop"), ConfigKeyValue::from_i32(2))]),
          }),
        ),
      ]);

      assert_eq!(result.plugins.config, expected_config_map);
      let logged_warnings = environment.take_stderr_messages();
      assert_eq!(logged_warnings, vec![get_warn_includes_message()]);
    });
  }

  #[test]
  fn should_dedupe_plugin_specified_in_both_local_and_extended_config() {
    // https://github.com/dprint/dprint/issues/1043
    // A plugin specified locally that is also specified by an extended config
    // must not be duplicated in the resolved plugins. Previously the extended
    // config's plugins were appended without deduplication, producing two
    // entries for the same plugin (and thus the same config key), which caused
    // the extended configuration to be ignored.
    let environment = TestEnvironment::new();
    environment.add_remote_file(
      "https://dprint.dev/test.json",
      r#"{
            "plugins": ["https://plugins.dprint.dev/test-plugin.wasm"],
            "lineWidth": 4,
            "test": {
                "prop": 6
            },
            "excludes": ["test-excludes"]
        }"#
        .as_bytes(),
    );
    environment
      .write_file(
        &PathBuf::from("/test.json"),
        r#"{
            "extends": "https://dprint.dev/test.json",
            "plugins": ["https://plugins.dprint.dev/test-plugin.wasm"]
        }"#,
      )
      .unwrap();

    environment.clone().run_in_runtime(async move {
      let result = get_result("/test.json", &environment).await.unwrap();
      // the plugin must appear only once
      assert_eq!(
        result.plugins.sources,
        vec![PluginSourceReference::new_remote_from_str("https://plugins.dprint.dev/test-plugin.wasm")]
      );
      // and the extended config's settings must be preserved
      assert_eq!(result.files.excludes, vec!["test-excludes".to_string()]);
      assert_eq!(
        result.plugins.config.get("test"),
        Some(&ConfigMapValue::PluginConfig(RawPluginConfig {
          locked: false,
          associations: None,
          overrides: Vec::new(),
          properties: ConfigKeyMap::from([(String::from("prop"), ConfigKeyValue::from_i32(6))]),
        }))
      );
    });
  }

  #[test]
  fn should_keep_extended_config_checksum_for_plugin_specified_without_one() {
    // deduping the plugin must not discard the checksum the extended config
    // specified for it, otherwise the integrity check is silently dropped
    let environment = TestEnvironment::new();
    environment.add_remote_file(
      "https://dprint.dev/test.json",
      r#"{
            "plugins": ["https://plugins.dprint.dev/test-plugin.wasm@checksum"]
        }"#
        .as_bytes(),
    );
    environment
      .write_file(
        &PathBuf::from("/test.json"),
        r#"{
            "extends": "https://dprint.dev/test.json",
            "plugins": ["https://plugins.dprint.dev/test-plugin.wasm"]
        }"#,
      )
      .unwrap();

    environment.clone().run_in_runtime(async move {
      let result = get_result("/test.json", &environment).await.unwrap();
      assert_eq!(
        result.plugins.sources,
        vec![PluginSourceReference {
          path_source: PathSource::new_remote_from_str("https://plugins.dprint.dev/test-plugin.wasm"),
          checksum: Some(String::from("checksum")),
        }]
      );
    });
  }

  #[test]
  fn should_use_own_checksum_over_extended_config_checksum() {
    let environment = TestEnvironment::new();
    environment.add_remote_file(
      "https://dprint.dev/test.json",
      r#"{
            "plugins": ["https://plugins.dprint.dev/test-plugin.wasm@extended-checksum"]
        }"#
        .as_bytes(),
    );
    environment
      .write_file(
        &PathBuf::from("/test.json"),
        r#"{
            "extends": "https://dprint.dev/test.json",
            "plugins": ["https://plugins.dprint.dev/test-plugin.wasm@local-checksum"]
        }"#,
      )
      .unwrap();

    environment.clone().run_in_runtime(async move {
      let result = get_result("/test.json", &environment).await.unwrap();
      assert_eq!(
        result.plugins.sources,
        vec![PluginSourceReference {
          path_source: PathSource::new_remote_from_str("https://plugins.dprint.dev/test-plugin.wasm"),
          checksum: Some(String::from("local-checksum")),
        }]
      );
    });
  }

  #[test]
  fn should_handle_array_extends() {
    let environment = TestEnvironment::new();
    environment.add_remote_file(
      "https://dprint.dev/test.json",
      r#"{
            "plugins": ["https://plugins.dprint.dev/test-plugin2.wasm"],
            "lineWidth": 4,
            "otherProp": 6,
            "test": {
                "prop": 6,
                "other": "test"
            },
            "test2": {
                "prop": 2
            }
        }"#
        .as_bytes(),
    );
    environment.add_remote_file(
      "https://dprint.dev/test2.json",
      r#"{
            "plugins": ["https://plugins.dprint.dev/test-plugin3.wasm"],
            "otherProp": 7,
            "asdf": 4,
            "test": {
                "other": "test2"
            }
        }"#
        .as_bytes(),
    );
    environment
      .write_file(
        &PathBuf::from("/test.json"),
        r#"{
            "extends": [
                "https://dprint.dev/test.json",
                "https://dprint.dev/test2.json",
            ],
            "plugins": ["https://plugins.dprint.dev/test-plugin.wasm"],
            "lineWidth": 1,
            "test": {
                "prop": 5
            },
        }"#,
      )
      .unwrap();

    environment.clone().run_in_runtime(async move {
      let result = get_result("/test.json", &environment).await.unwrap();
      assert_eq!(environment.take_stdout_messages().len(), 0);
      assert_eq!(result.files.includes, None);
      assert_eq!(result.files.excludes, Vec::<String>::new());
      assert_eq!(
        result.plugins.sources,
        vec![
          PluginSourceReference::new_remote_from_str("https://plugins.dprint.dev/test-plugin.wasm"),
          PluginSourceReference::new_remote_from_str("https://plugins.dprint.dev/test-plugin2.wasm"),
          PluginSourceReference::new_remote_from_str("https://plugins.dprint.dev/test-plugin3.wasm"),
        ]
      );

      let expected_config_map = ConfigMap::from([
        (String::from("lineWidth"), ConfigMapValue::from_i32(1)),
        (String::from("otherProp"), ConfigMapValue::from_i32(6)),
        (String::from("asdf"), ConfigMapValue::from_i32(4)),
        (
          String::from("test"),
          ConfigMapValue::PluginConfig(RawPluginConfig {
            locked: false,
            associations: None,
            overrides: Vec::new(),
            properties: ConfigKeyMap::from([
              (String::from("prop"), ConfigKeyValue::from_i32(5)),
              (String::from("other"), ConfigKeyValue::from_str("test")),
            ]),
          }),
        ),
        (
          String::from("test2"),
          ConfigMapValue::PluginConfig(RawPluginConfig {
            locked: false,
            associations: None,
            overrides: Vec::new(),
            properties: ConfigKeyMap::from([(String::from("prop"), ConfigKeyValue::from_i32(2))]),
          }),
        ),
      ]);

      assert_eq!(result.plugins.config, expected_config_map);
    });
  }

  #[test]
  fn should_handle_extends_within_an_extends() {
    let environment = TestEnvironment::new();
    environment.add_remote_file(
      "https://dprint.dev/test.json",
      r#"{
            "extends": "https://dprint.dev/test2.json",
            "plugins": ["https://plugins.dprint.dev/test-plugin2.wasm"],
            "lineWidth": 4,
            "otherProp": 6,
            "test": {
                "prop": 6,
                "other": "test"
            },
            "test2": {
                "prop": 2
            }
        }"#
        .as_bytes(),
    );
    environment.add_remote_file(
      "https://dprint.dev/test2.json",
      r#"{
            "plugins": ["https://plugins.dprint.dev/test-plugin3.wasm"],
            "otherProp": 7,
            "asdf": 4,
            "test": {
                "other": "test2"
            }
        }"#
        .as_bytes(),
    );
    environment.add_remote_file(
      "https://dprint.dev/test3.json",
      r#"{
            "asdf": 4,
            "newProp": "test"
        }"#
        .as_bytes(),
    );
    environment
      .write_file(
        &PathBuf::from("/test.json"),
        r#"{
            "extends": [
                "https://dprint.dev/test.json"
                "https://dprint.dev/test3.json" // should have lowest precedence
            ],
            "plugins": ["https://plugins.dprint.dev/test-plugin.wasm"],
            "lineWidth": 1,
            "test": {
                "prop": 5
            },
        }"#,
      )
      .unwrap();

    environment.clone().run_in_runtime(async move {
      let result = get_result("/test.json", &environment).await.unwrap();
      assert_eq!(environment.take_stdout_messages().len(), 0);
      assert_eq!(result.files.includes, None);
      assert_eq!(result.files.excludes, Vec::<String>::new());
      assert_eq!(
        result.plugins.sources,
        vec![
          PluginSourceReference::new_remote_from_str("https://plugins.dprint.dev/test-plugin.wasm"),
          PluginSourceReference::new_remote_from_str("https://plugins.dprint.dev/test-plugin2.wasm"),
          PluginSourceReference::new_remote_from_str("https://plugins.dprint.dev/test-plugin3.wasm"),
        ]
      );

      let expected_config_map = ConfigMap::from([
        (String::from("lineWidth"), ConfigMapValue::from_i32(1)),
        (String::from("otherProp"), ConfigMapValue::from_i32(6)),
        (String::from("asdf"), ConfigMapValue::from_i32(4)),
        (String::from("newProp"), ConfigMapValue::from_str("test")),
        (
          String::from("test"),
          ConfigMapValue::PluginConfig(RawPluginConfig {
            locked: false,
            associations: None,
            overrides: Vec::new(),
            properties: ConfigKeyMap::from([
              (String::from("prop"), ConfigKeyValue::from_i32(5)),
              (String::from("other"), ConfigKeyValue::from_str("test")),
            ]),
          }),
        ),
        (
          String::from("test2"),
          ConfigMapValue::PluginConfig(RawPluginConfig {
            locked: false,
            associations: None,
            overrides: Vec::new(),
            properties: ConfigKeyMap::from([(String::from("prop"), ConfigKeyValue::from_i32(2))]),
          }),
        ),
      ]);

      assert_eq!(result.plugins.config, expected_config_map);
    });
  }

  #[test]
  fn should_handle_relative_remote_extends() {
    let environment = TestEnvironment::new();
    environment.add_remote_file(
      "https://dprint.dev/test.json",
      r#"{
            "extends": "dir/test.json",
            "prop1": 1
        }"#
        .as_bytes(),
    );
    environment.add_remote_file(
      "https://dprint.dev/dir/test.json",
      r#"{
            "extends": "../otherDir/test.json",
            "prop2": 2
        }"#
        .as_bytes(),
    );
    environment.add_remote_file(
      "https://dprint.dev/otherDir/test.json",
      r#"{
            "extends": "https://test.dprint.dev/test.json",
            "prop3": 3
        }"#
        .as_bytes(),
    );
    environment.add_remote_file(
      "https://test.dprint.dev/test.json",
      r#"{
            "extends": [
                "other.json",
                "dir/test.json"
            ],
            "prop4": 4,
        }"#
        .as_bytes(),
    );
    environment.add_remote_file(
      "https://test.dprint.dev/other.json",
      r#"{
            "prop5": 5,
        }"#
        .as_bytes(),
    );
    environment.add_remote_file(
      "https://test.dprint.dev/dir/test.json",
      r#"{
            "prop6": 6,
        }"#
        .as_bytes(),
    );

    environment.clone().run_in_runtime(async move {
      let result = get_result("https://dprint.dev/test.json", &environment).await.unwrap();
      assert_eq!(environment.take_stdout_messages().len(), 0);

      let expected_config_map = ConfigMap::from([
        (String::from("prop1"), ConfigMapValue::from_i32(1)),
        (String::from("prop2"), ConfigMapValue::from_i32(2)),
        (String::from("prop3"), ConfigMapValue::from_i32(3)),
        (String::from("prop4"), ConfigMapValue::from_i32(4)),
        (String::from("prop5"), ConfigMapValue::from_i32(5)),
        (String::from("prop6"), ConfigMapValue::from_i32(6)),
      ]);
      assert_eq!(result.plugins.config, expected_config_map);
    });
  }

  #[test]
  fn should_handle_remote_in_local_extends() {
    let environment = TestEnvironment::new();
    environment
      .write_file(
        &PathBuf::from("/test.json"),
        r#"{
            "extends": "https://dprint.dev/dir/test.json",
            "prop1": 1
        }"#,
      )
      .unwrap();
    environment.add_remote_file(
      "https://dprint.dev/dir/test.json",
      r#"{
            "extends": "../otherDir/test.json",
            "prop2": 2
        }"#
        .as_bytes(),
    );
    environment.add_remote_file(
      "https://dprint.dev/otherDir/test.json",
      r#"{
            "prop3": 3
        }"#
        .as_bytes(),
    );

    environment.clone().run_in_runtime(async move {
      let result = get_result("/test.json", &environment).await.unwrap();
      assert_eq!(environment.take_stdout_messages().len(), 0);

      let expected_config_map = ConfigMap::from([
        (String::from("prop1"), ConfigMapValue::from_i32(1)),
        (String::from("prop2"), ConfigMapValue::from_i32(2)),
        (String::from("prop3"), ConfigMapValue::from_i32(3)),
      ]);
      assert_eq!(result.plugins.config, expected_config_map);
    });
  }

  #[test]
  fn should_handle_relative_local_extends() {
    let environment = TestEnvironmentBuilder::new()
      .write_file(
        &PathBuf::from("/test.json"),
        r#"{
            "extends": "dir/test.json",
            "prop1": 1
        }"#,
      )
      .write_file(
        &PathBuf::from("/dir/test.json"),
        r#"{
            "extends": "../otherDir/test.json",
            "prop2": 2
        }"#,
      )
      .write_file(
        &PathBuf::from("/otherDir/test.json"),
        r#"{
            "prop3": 3
        }"#,
      )
      .build();

    environment.clone().run_in_runtime(async move {
      let result = get_result("/test.json", &environment).await.unwrap();
      assert_eq!(environment.take_stdout_messages().len(), 0);

      let expected_config_map = ConfigMap::from([
        (String::from("prop1"), ConfigMapValue::from_i32(1)),
        (String::from("prop2"), ConfigMapValue::from_i32(2)),
        (String::from("prop3"), ConfigMapValue::from_i32(3)),
      ]);
      assert_eq!(result.plugins.config, expected_config_map);
    });
  }

  #[test]
  fn should_say_config_file_with_error() {
    let environment = TestEnvironment::new();
    environment.add_remote_file(
      "https://dprint.dev/test.json",
      r#"{
            "extends": "dir/test.json",
            "prop1": 1
        }"#
        .as_bytes(),
    );
    environment.add_remote_file(
      "https://dprint.dev/dir/test.json",
      r#"{
            "prop2" 2
        }"#
        .as_bytes(),
    );

    environment.clone().run_in_runtime(async move {
      let result = get_result("https://dprint.dev/test.json", &environment).await.err().unwrap();
      assert_eq!(
        result.to_string(),
        concat!(
          "Error deserializing. Expected colon after the string or word in object property on line 2 column 21\n",
          "    at https://dprint.dev/dir/test.json"
        )
      );
    });
  }

  #[test]
  fn should_error_extending_locked_config() {
    let environment = TestEnvironment::new();
    environment.add_remote_file(
      "https://dprint.dev/test.json",
      r#"{
            "test": {
                "locked": true,
                "prop": 6,
                "other": "test"
            }
        }"#
        .as_bytes(),
    );
    environment
      .write_file(
        &PathBuf::from("/test.json"),
        r#"{
            "extends": "https://dprint.dev/test.json",
            "test": {
                "prop": 5
            }
        }"#,
      )
      .unwrap();

    environment.clone().run_in_runtime(async move {
      let result = get_result("/test.json", &environment).await.err().unwrap();
      assert_eq!(
        result.to_string(),
        concat!(
          "The configuration for \"test\" was locked, but a parent configuration specified it. ",
          "Locked configurations cannot have their properties overridden.\n",
          "    at https://dprint.dev/test.json",
        )
      );
    });
  }

  #[test]
  fn should_get_locked_config() {
    let environment = TestEnvironment::new();
    environment.add_remote_file(
      "https://dprint.dev/test.json",
      r#"{
            "test": {
                "locked": true,
                "prop": 6,
                "other": "test"
            }
        }"#
        .as_bytes(),
    );
    environment
      .write_file(
        &PathBuf::from("/test.json"),
        r#"{
            "extends": "https://dprint.dev/test.json"
        }"#,
      )
      .unwrap();

    environment.clone().run_in_runtime(async move {
      let result = get_result("/test.json", &environment).await.unwrap();
      assert_eq!(environment.take_stdout_messages().len(), 0);
      let expected_config_map = ConfigMap::from([(
        String::from("test"),
        ConfigMapValue::PluginConfig(RawPluginConfig {
          locked: true,
          associations: None,
          overrides: Vec::new(),
          properties: ConfigKeyMap::from([
            (String::from("prop"), ConfigKeyValue::from_i32(6)),
            (String::from("other"), ConfigKeyValue::from_str("test")),
          ]),
        }),
      )]);

      assert_eq!(result.plugins.config, expected_config_map);
    });
  }

  #[test]
  fn should_handle_locked_on_upstream_config() {
    let environment = TestEnvironment::new();
    environment.add_remote_file(
      "https://dprint.dev/test.json",
      r#"{
            "test": {
                "prop": 6,
                "other": "test"
            }
        }"#
        .as_bytes(),
    );
    environment
      .write_file(
        &PathBuf::from("/test.json"),
        r#"{
            "extends": "https://dprint.dev/test.json",
            "test": {
                "locked": true,
                "prop": 7
            }
        }"#,
      )
      .unwrap();

    environment.clone().run_in_runtime(async move {
      let result = get_result("/test.json", &environment).await.unwrap();
      assert_eq!(environment.take_stdout_messages().len(), 0);
      let expected_config_map = ConfigMap::from([(
        String::from("test"),
        ConfigMapValue::PluginConfig(RawPluginConfig {
          locked: true,
          associations: None,
          overrides: Vec::new(),
          properties: ConfigKeyMap::from([
            (String::from("prop"), ConfigKeyValue::from_i32(7)),
            (String::from("other"), ConfigKeyValue::from_str("test")),
          ]),
        }),
      )]);

      assert_eq!(result.plugins.config, expected_config_map);
    });
  }

  #[test]
  fn should_get_locked_config_and_not_care_if_no_properties_set_in_parent_config() {
    let environment = TestEnvironment::new();
    environment.add_remote_file(
      "https://dprint.dev/test.json",
      r#"{
            "test": {
                "locked": true,
                "prop": 6,
                "other": "test"
            }
        }"#
        .as_bytes(),
    );
    environment
      .write_file(
        &PathBuf::from("/test.json"),
        r#"{
            "extends": "https://dprint.dev/test.json",
            "test": {}
        }"#,
      )
      .unwrap();

    environment.clone().run_in_runtime(async move {
      let result = get_result("/test.json", &environment).await.unwrap();
      assert_eq!(environment.take_stdout_messages().len(), 0);
      let expected_config_map = ConfigMap::from([(
        String::from("test"),
        ConfigMapValue::PluginConfig(RawPluginConfig {
          locked: false,
          associations: None,
          overrides: Vec::new(),
          properties: ConfigKeyMap::from([
            (String::from("prop"), ConfigKeyValue::from_i32(6)),
            (String::from("other"), ConfigKeyValue::from_str("test")),
          ]),
        }),
      )]);

      assert_eq!(result.plugins.config, expected_config_map);
    });
  }

  #[test]
  fn should_use_overrides_on_extended_config() {
    let environment = TestEnvironment::new();
    environment.add_remote_file(
      "https://dprint.dev/test.json",
      r#"{
          "test": {
            "overrides": {
              "files": "**/package.json",
              "lineWidth": 80
            }
          }
        }"#
        .as_bytes(),
    );
    environment
      .write_file(
        &PathBuf::from("/test.json"),
        r#"{
            "extends": "https://dprint.dev/test.json",
            "test": {}
        }"#,
      )
      .unwrap();

    environment.clone().run_in_runtime(async move {
      let result = get_result("/test.json", &environment).await.unwrap();
      assert_eq!(environment.take_stdout_messages().len(), 0);
      let expected_config_map = ConfigMap::from([(
        String::from("test"),
        ConfigMapValue::PluginConfig(RawPluginConfig {
          locked: false,
          associations: None,
          overrides: vec![RawPluginConfigOverride {
            files: vec!["**/package.json".to_string()],
            properties: ConfigKeyMap::from([("lineWidth".to_string(), ConfigKeyValue::from_i32(80))]),

            origin: Default::default(),
          }],
          properties: ConfigKeyMap::new(),
        }),
      )]);

      assert_eq!(result.plugins.config, expected_config_map);
    });
  }

  #[test]
  fn should_order_extended_overrides_before_local_overrides() {
    let environment = TestEnvironment::new();
    environment.add_remote_file(
      "https://dprint.dev/test.json",
      r#"{
          "test": {
            "overrides": {
              "files": "**/*.json",
              "lineWidth": 100
            }
          }
        }"#
        .as_bytes(),
    );
    environment
      .write_file(
        &PathBuf::from("/test.json"),
        r#"{
            "extends": "https://dprint.dev/test.json",
            "test": {
              "overrides": {
                "files": "**/package.json",
                "lineWidth": 80
              }
            }
        }"#,
      )
      .unwrap();

    environment.clone().run_in_runtime(async move {
      let result = get_result("/test.json", &environment).await.unwrap();
      assert_eq!(environment.take_stdout_messages().len(), 0);
      let expected_config_map = ConfigMap::from([(
        String::from("test"),
        ConfigMapValue::PluginConfig(RawPluginConfig {
          locked: false,
          associations: None,
          overrides: vec![
            RawPluginConfigOverride {
              files: vec!["**/*.json".to_string()],
              properties: ConfigKeyMap::from([("lineWidth".to_string(), ConfigKeyValue::from_i32(100))]),

              origin: Default::default(),
            },
            RawPluginConfigOverride {
              files: vec!["**/package.json".to_string()],
              properties: ConfigKeyMap::from([("lineWidth".to_string(), ConfigKeyValue::from_i32(80))]),

              origin: Default::default(),
            },
          ],
          properties: ConfigKeyMap::new(),
        }),
      )]);

      assert_eq!(result.plugins.config, expected_config_map);
    });
  }

  #[test]
  fn should_error_extending_locked_config_with_overrides() {
    let environment = TestEnvironment::new();
    environment.add_remote_file(
      "https://dprint.dev/test.json",
      r#"{
          "test": {
            "locked": true,
            "lineWidth": 80
          }
        }"#
        .as_bytes(),
    );
    environment
      .write_file(
        &PathBuf::from("/test.json"),
        r#"{
            "extends": "https://dprint.dev/test.json",
            "test": {
              "overrides": {
                "files": "**/package.json",
                "lineWidth": 100
              }
            }
        }"#,
      )
      .unwrap();

    environment.clone().run_in_runtime(async move {
      let result = get_result("/test.json", &environment).await.err().unwrap();
      assert_eq!(
        result.to_string(),
        concat!(
          "The configuration for \"test\" was locked, but a parent configuration specified it. ",
          "Locked configurations cannot have their properties overridden.\n",
          "    at https://dprint.dev/test.json",
        )
      );
    });
  }

  #[test]
  fn should_use_associations_on_extended_config() {
    let environment = TestEnvironment::new();
    environment.add_remote_file(
      "https://dprint.dev/test.json",
      r#"{
          "test": {
            "associations": "test"
          }
        }"#
        .as_bytes(),
    );
    environment
      .write_file(
        &PathBuf::from("/test.json"),
        r#"{
            "extends": "https://dprint.dev/test.json",
            "test": {}
        }"#,
      )
      .unwrap();

    environment.clone().run_in_runtime(async move {
      let result = get_result("/test.json", &environment).await.unwrap();
      assert_eq!(environment.take_stdout_messages().len(), 0);
      let expected_config_map = ConfigMap::from([(
        String::from("test"),
        ConfigMapValue::PluginConfig(RawPluginConfig {
          locked: false,
          associations: Some(vec!["test".to_string()]),
          overrides: Vec::new(),
          properties: ConfigKeyMap::new(),
        }),
      )]);

      assert_eq!(result.plugins.config, expected_config_map);
    });
  }

  #[test]
  fn should_override_associations_on_extended_config() {
    let environment = TestEnvironment::new();
    environment.add_remote_file(
      "https://dprint.dev/test.json",
      r#"{
          "test": {
            "associations": "test"
          }
        }"#
        .as_bytes(),
    );
    environment
      .write_file(
        &PathBuf::from("/test.json"),
        r#"{
            "extends": "https://dprint.dev/test.json",
            "test": {
              "associations": ["test1", "test2"]
            }
        }"#,
      )
      .unwrap();

    environment.clone().run_in_runtime(async move {
      let result = get_result("/test.json", &environment).await.unwrap();
      assert_eq!(environment.take_stdout_messages().len(), 0);
      let expected_config_map = ConfigMap::from([(
        String::from("test"),
        ConfigMapValue::PluginConfig(RawPluginConfig {
          locked: false,
          associations: Some(vec!["test1".to_string(), "test2".to_string()]),
          overrides: Vec::new(),
          properties: ConfigKeyMap::new(),
        }),
      )]);

      assert_eq!(result.plugins.config, expected_config_map);
    });
  }

  #[test]
  fn should_handle_relative_remote_plugin() {
    let environment = TestEnvironment::new();
    environment.add_remote_file(
      "https://dprint.dev/test.json",
      r#"{
            "plugins": ["./test-plugin.wasm"]
        }"#
        .as_bytes(),
    );

    environment.clone().run_in_runtime(async move {
      let result = get_result("https://dprint.dev/test.json", &environment).await.unwrap();
      assert_eq!(environment.take_stdout_messages().len(), 0);
      assert_eq!(
        result.plugins.sources,
        vec![PluginSourceReference::new_remote_from_str("https://dprint.dev/test-plugin.wasm")]
      );
    });
  }

  #[test]
  fn should_handle_relative_remote_plugin_in_extends() {
    let environment = TestEnvironment::new();
    environment.add_remote_file(
      "https://dprint.dev/test.json",
      r#"{
            "extends": "dir/test.json"
        }"#
        .as_bytes(),
    );
    environment.add_remote_file(
      "https://dprint.dev/dir/test.json",
      r#"{
            "extends": "../otherDir/test.json"
        }"#
        .as_bytes(),
    );
    environment.add_remote_file(
      "https://dprint.dev/otherDir/test.json",
      r#"{
            "plugins": [
                "../test/plugin.wasm",
            ]
        }"#
        .as_bytes(),
    );

    environment.clone().run_in_runtime(async move {
      let result = get_result("https://dprint.dev/test.json", &environment).await.unwrap();
      assert_eq!(environment.take_stdout_messages().len(), 0);
      assert_eq!(
        result.plugins.sources,
        vec![PluginSourceReference::new_remote_from_str("https://dprint.dev/test/plugin.wasm")]
      );
    });
  }

  #[test]
  fn should_handle_relative_local_plugins() {
    let environment = TestEnvironment::new();
    environment
      .write_file(
        &PathBuf::from("/test.json"),
        r#"{
            "plugins": ["./testing/asdf.wasm"],
        }"#,
      )
      .unwrap();

    environment.clone().run_in_runtime(async move {
      let result = get_result("/test.json", &environment).await.unwrap();
      assert_eq!(environment.take_stdout_messages().len(), 0);
      assert_eq!(result.plugins.sources, vec![PluginSourceReference::new_local("/testing/asdf.wasm")]);
    });
  }

  #[test]
  fn should_handle_relative_local_plugins_in_extends() {
    let environment = TestEnvironmentBuilder::new()
      .write_file(
        &PathBuf::from("/test.json"),
        r#"{
            "extends": "./other/test.json",
        }"#,
      )
      .write_file(
        &PathBuf::from("/other/test.json"),
        r#"{
            "projectType": "openSource", // test having this in base config
            "plugins": ["./testing/asdf.wasm"],
        }"#,
      )
      .build();

    environment.clone().run_in_runtime(async move {
      let result = get_result("/test.json", &environment).await.unwrap();
      assert_eq!(environment.take_stdout_messages().len(), 0);
      assert_eq!(result.plugins.sources, vec![PluginSourceReference::new_local("/other/testing/asdf.wasm")]);
    });
  }

  #[test]
  fn should_handle_incremental_flag_when_not_specified() {
    let environment = TestEnvironment::new();
    environment
      .write_file(
        &PathBuf::from("/test.json"),
        r#"{
            "plugins": ["./testing/asdf.wasm"],
        }"#,
      )
      .unwrap();

    environment.clone().run_in_runtime(async move {
      let result = get_result("/test.json", &environment).await.unwrap();
      assert_eq!(environment.take_stdout_messages().len(), 0);
      assert_eq!(result.execution.incremental, None);
    });
  }

  #[test]
  fn should_handle_incremental_flag_when_true() {
    let environment = TestEnvironment::new();
    environment
      .write_file(
        &PathBuf::from("/test.json"),
        r#"{
            "incremental": true,
            "plugins": ["./testing/asdf.wasm"],
        }"#,
      )
      .unwrap();

    environment.clone().run_in_runtime(async move {
      let result = get_result("/test.json", &environment).await.unwrap();
      assert_eq!(environment.take_stdout_messages().len(), 0);
      assert_eq!(result.execution.incremental, Some(true));
    });
  }

  #[test]
  fn should_handle_incremental_flag_when_false() {
    let environment = TestEnvironment::new();
    environment
      .write_file(
        &PathBuf::from("/test.json"),
        r#"{
            "incremental": false,
            "plugins": ["./testing/asdf.wasm"],
        }"#,
      )
      .unwrap();

    environment.clone().run_in_runtime(async move {
      let result = get_result("/test.json", &environment).await.unwrap();
      assert_eq!(environment.take_stdout_messages().len(), 0);
      assert_eq!(result.execution.incremental, Some(false));
    });
  }

  #[test]
  fn should_parse_shebangs_property() {
    let environment = TestEnvironment::new();
    environment
      .write_file(
        &PathBuf::from("/test.json"),
        r##"{
            "shebangs": {
              "#!/bin/sh": "sh",
              "#!/usr/bin/env node": ".js",
              "#!/usr/bin/env deno  ": "TS"
            },
            "plugins": ["./testing/asdf.wasm"],
        }"##,
      )
      .unwrap();

    environment.clone().run_in_runtime(async move {
      let result = get_result("/test.json", &environment).await.unwrap();
      assert_eq!(environment.take_stdout_messages().len(), 0);
      let shebangs = result.routing.shebangs.unwrap();
      assert_eq!(shebangs.get("#!/bin/sh").map(|s| s.as_str()), Some("sh"));
      // the leading dot is removed and the extension lowercased
      assert_eq!(shebangs.get("#!/usr/bin/env node").map(|s| s.as_str()), Some("js"));
      assert_eq!(shebangs.get("#!/usr/bin/env deno").map(|s| s.as_str()), Some("ts"));
    });
  }

  #[test]
  fn should_merge_shebangs_from_extended_config() {
    let environment = TestEnvironment::new();
    environment.add_remote_file(
      "https://dprint.dev/test.json",
      r##"{
            "shebangs": {
              "#!/bin/sh": "sh",
              "#!/usr/bin/env node": "js"
            },
            "plugins": ["https://plugins.dprint.dev/test-plugin.wasm"]
        }"##
        .as_bytes(),
    );
    environment
      .write_file(
        &PathBuf::from("/test.json"),
        r##"{
            "extends": "https://dprint.dev/test.json",
            "shebangs": {
              "#!/usr/bin/env node": "ts",
              "#!/usr/bin/env python3": "py"
            }
        }"##,
      )
      .unwrap();

    environment.clone().run_in_runtime(async move {
      let result = get_result("/test.json", &environment).await.unwrap();
      assert_eq!(environment.take_stdout_messages().len(), 0);
      let shebangs = result.routing.shebangs.unwrap();
      assert_eq!(
        shebangs.into_iter().collect::<Vec<_>>(),
        vec![
          // the extending config's entry wins
          ("#!/usr/bin/env node".to_string(), "ts".to_string()),
          ("#!/usr/bin/env python3".to_string(), "py".to_string()),
          ("#!/bin/sh".to_string(), "sh".to_string()),
        ]
      );
      assert!(!result.plugins.config.contains_key("shebangs"));
    });
  }

  #[test]
  fn should_merge_shebangs_from_local_extends_chain() {
    let environment = TestEnvironment::new();
    environment
      .write_file(
        &PathBuf::from("/base2.json"),
        r##"{
            "shebangs": {
              "#!/bin/sh": "base2",
              "#!/usr/bin/env node": "base2",
              "#!/usr/bin/env python3": "base2"
            }
        }"##,
      )
      .unwrap();
    environment
      .write_file(
        &PathBuf::from("/base1.json"),
        r##"{
            "extends": "./base2.json",
            "shebangs": {
              "#!/usr/bin/env node": "base1",
              "#!/usr/bin/env python3": "base1"
            }
        }"##,
      )
      .unwrap();
    environment
      .write_file(
        &PathBuf::from("/test.json"),
        r##"{
            "extends": "./base1.json",
            "shebangs": {
              "#!/usr/bin/env python3": "test"
            },
            "plugins": ["./testing/asdf.wasm"],
        }"##,
      )
      .unwrap();

    environment.clone().run_in_runtime(async move {
      let result = get_result("/test.json", &environment).await.unwrap();
      assert_eq!(environment.take_stdout_messages().len(), 0);
      assert_eq!(
        result.routing.shebangs.unwrap().into_iter().collect::<Vec<_>>(),
        vec![
          ("#!/usr/bin/env python3".to_string(), "test".to_string()),
          ("#!/usr/bin/env node".to_string(), "base1".to_string()),
          ("#!/bin/sh".to_string(), "base2".to_string()),
        ]
      );
    });
  }

  #[test]
  fn should_error_when_shebangs_value_invalid() {
    fn get_error(shebangs: &str) -> String {
      let environment = TestEnvironment::new();
      environment
        .write_file(
          &PathBuf::from("/test.json"),
          &format!(r##"{{ "shebangs": {}, "plugins": ["./testing/asdf.wasm"] }}"##, shebangs),
        )
        .unwrap();
      environment.clone().run_in_runtime(async move {
        let err = get_result("/test.json", &environment).await.err().unwrap();
        err.to_string()
      })
    }

    assert_eq!(
      get_error(r##"{ "#!/bin/sh": "" }"##),
      "Expected a file extension (ex. \"sh\") for shebang '#!/bin/sh' in the 'shebangs' property, but found ''.\n    at /test.json"
    );
    for extension in [".", "*.sh", " sh", "tar.gz"] {
      assert!(
        get_error(&format!(r##"{{ "#!/bin/sh": "{}" }}"##, extension)).contains(&format!("but found '{}'.", extension)),
        "{}",
        extension
      );
    }
    assert_eq!(
      get_error(r##"{ "/bin/sh": "sh" }"##),
      "Expected the key '/bin/sh' in the 'shebangs' property to be a shebang line starting with '#!'.\n    at /test.json"
    );
    assert!(get_error(r##"{ " #!/bin/sh": "sh" }"##).contains("to be a shebang line starting with '#!'"));
    assert!(get_error(r##"{ "#!/bin/sh\ntext": "sh" }"##).contains("to be a shebang line starting with '#!'"));
  }

  #[test]
  fn should_error_when_shebangs_value_not_a_string() {
    let environment = TestEnvironment::new();
    environment
      .write_file(
        &PathBuf::from("/test.json"),
        r##"{
            "shebangs": {
              "#!/bin/sh": 5
            },
            "plugins": ["./testing/asdf.wasm"],
        }"##,
      )
      .unwrap();

    environment.clone().run_in_runtime(async move {
      let err = get_result("/test.json", &environment).await.err().unwrap();
      assert!(err.to_string().contains("Expected a string file extension for shebang '#!/bin/sh'"));
    });
  }

  #[test]
  fn should_parse_inherit_property() {
    let environment = TestEnvironment::new();
    environment
      .write_file(
        &PathBuf::from("/test.json"),
        r#"{
            "inherit": true,
            "plugins": ["./testing/asdf.wasm"],
        }"#,
      )
      .unwrap();

    environment.clone().run_in_runtime(async move {
      let result = get_result("/test.json", &environment).await.unwrap();
      assert_eq!(environment.take_stdout_messages().len(), 0);
      // should not leak into the config map (which would cause an unknown property diagnostic)
      assert_eq!(result.plugins.config.contains_key("inherit"), false);
    });
  }

  #[test]
  fn inherit_config_should_merge_ancestor_config() {
    let parent = ResolvedConfig {
      origin: test_origin("/dprint.json", "/"),
      files: FileSelection {
        includes: Some(vec!["**/*.txt".to_string()]),
        // both patterns match at any depth, so both rebase into the nested directory
        excludes: vec!["**/node_modules".to_string(), "dist".to_string()],
      },
      routing: FileRouting {
        shebangs: Some(IndexMap::from([
          ("#!/bin/sh".to_string(), "sh".to_string()),
          ("#!/usr/bin/env node".to_string(), "js".to_string()),
        ])),
      },
      execution: ExecutionPolicy { incremental: Some(true) },
      plugins: PluginConfiguration {
        origins: Default::default(),
        remote_exec: Default::default(),
        sources: vec![
          PluginSourceReference::new_remote_from_str("https://plugins.dprint.dev/test-plugin.wasm"),
          PluginSourceReference::new_remote_from_str("https://plugins.dprint.dev/json.wasm"),
        ],
        config: ConfigMap::from([
          ("lineWidth".to_string(), ConfigMapValue::from_i32(80)),
          (
            "test".to_string(),
            ConfigMapValue::PluginConfig(RawPluginConfig {
              locked: false,
              associations: None,
              overrides: Vec::new(),
              properties: ConfigKeyMap::from([
                ("indentWidth".to_string(), ConfigKeyValue::from_i32(4)),
                ("newLineKind".to_string(), ConfigKeyValue::from_str("crlf")),
              ]),
            }),
          ),
        ]),
      },
    };
    let mut result = ResolvedConfig {
      origin: test_origin("/sub/dprint.json", "/sub"),
      files: FileSelection {
        includes: None,
        excludes: vec!["sub-excludes".to_string()],
      },
      routing: FileRouting {
        shebangs: Some(IndexMap::from([("#!/usr/bin/env node".to_string(), "ts".to_string())])),
      },
      execution: Default::default(),
      plugins: PluginConfiguration {
        origins: Default::default(),
        remote_exec: Default::default(),
        // a plugin specified in the child has precedence over the ancestor's
        sources: vec![PluginSourceReference::new_remote_from_str("https://plugins.dprint.dev/test-plugin.wasm")],
        config: ConfigMap::from([(
          "test".to_string(),
          ConfigMapValue::PluginConfig(RawPluginConfig {
            locked: false,
            associations: None,
            overrides: Vec::new(),
            properties: ConfigKeyMap::from([("indentWidth".to_string(), ConfigKeyValue::from_i32(2))]),
          }),
        )]),
      },
    };

    result.inherit(&parent).unwrap();
    // child plugins first, then ancestor's, with duplicates removed
    assert_eq!(
      result.plugins.sources,
      vec![
        PluginSourceReference::new_remote_from_str("https://plugins.dprint.dev/test-plugin.wasm"),
        PluginSourceReference::new_remote_from_str("https://plugins.dprint.dev/json.wasm"),
      ]
    );
    // ancestor excludes (rebased, droppable) come first, then the nested config's own
    assert_eq!(
      result.files.excludes,
      vec!["**/node_modules".to_string(), "dist".to_string(), "sub-excludes".to_string()]
    );
    // includes are not inherited
    assert_eq!(result.files.includes, None);
    // incremental is inherited when not specified
    assert_eq!(result.execution.incremental, Some(true));
    // the nested config has shebangs of its own, so the ancestor's aren't used
    assert_eq!(
      result.routing.shebangs.unwrap().into_iter().collect::<Vec<_>>(),
      vec![("#!/usr/bin/env node".to_string(), "ts".to_string())]
    );
    assert_eq!(
      result.plugins.config,
      ConfigMap::from([
        (
          "test".to_string(),
          ConfigMapValue::PluginConfig(RawPluginConfig {
            locked: false,
            associations: None,
            overrides: Vec::new(),
            properties: ConfigKeyMap::from([
              // child wins on conflicts...
              ("indentWidth".to_string(), ConfigKeyValue::from_i32(2)),
              // ...but inherits values it didn't override
              ("newLineKind".to_string(), ConfigKeyValue::from_str("crlf")),
            ]),
          }),
        ),
        ("lineWidth".to_string(), ConfigMapValue::from_i32(80)),
      ])
    );
  }

  #[test]
  fn inherit_config_should_not_inherit_shebangs_when_specifying_none() {
    let environment = TestEnvironmentBuilder::new()
      .write_file("/dprint.json", r##"{ "shebangs": { "#!/bin/sh": "sh" } }"##)
      .write_file("/sub/dprint.json", r#"{ "inherit": true, "shebangs": {} }"#)
      .write_file("/other/dprint.json", r#"{ "inherit": true }"#)
      .build();

    environment.clone().run_in_runtime(async move {
      let ancestor = resolve_local_config("/dprint.json", &environment).await;
      let result = resolve_local_descendant_config("/sub/dprint.json", &ancestor, &environment).await.unwrap();
      assert_eq!(result.routing.shebangs, Some(IndexMap::new()));
      let result = resolve_local_descendant_config("/other/dprint.json", &ancestor, &environment).await.unwrap();
      assert_eq!(result.routing.shebangs, ancestor.routing.shebangs);
    });
  }

  #[test]
  fn inherit_config_should_rebase_ancestor_excludes_onto_nested_directory() {
    fn inherited_excludes(ancestor: &[&str], ancestor_base: &str, new_base: &str) -> Vec<String> {
      let mut files = FileSelection::default();
      files.inherit(
        &FileSelection {
          includes: None,
          excludes: ancestor.iter().map(|s| s.to_string()).collect(),
        },
        &CanonicalizedPathBuf::new_for_testing(ancestor_base),
        &CanonicalizedPathBuf::new_for_testing(new_base),
      );
      files.excludes
    }

    // depth-relative patterns keep matching within the nested directory
    assert_eq!(inherited_excludes(&["**/node_modules"], "/", "/sub"), vec!["**/node_modules".to_string()]);
    // a pattern with no slash matches its name at any depth, so it's kept as-is
    assert_eq!(inherited_excludes(&["dist"], "/", "/sub"), vec!["dist".to_string()]);
    assert_eq!(inherited_excludes(&["dist/"], "/", "/sub"), vec!["dist/".to_string()]);
    // ...unless it names the nested directory or one of its ancestors, in which
    // case everything in the nested directory is excluded
    assert_eq!(inherited_excludes(&["sub"], "/", "/sub"), vec!["**".to_string()]);
    assert_eq!(inherited_excludes(&["sub"], "/", "/sub/nested"), vec!["**".to_string()]);
    // ...including when it names an ancestor other than the first one below the base
    assert_eq!(inherited_excludes(&["nested"], "/", "/sub/nested"), vec!["**".to_string()]);
    assert_eq!(inherited_excludes(&["nested"], "/", "/sub/nested/deep"), vec!["**".to_string()]);
    // ...and when it names one with a wildcard
    assert_eq!(inherited_excludes(&["su*"], "/", "/sub"), vec!["**".to_string()]);
    assert_eq!(inherited_excludes(&["neste*"], "/", "/sub/nested"), vec!["**".to_string()]);
    assert_eq!(inherited_excludes(&["**/sub"], "/", "/sub/nested"), vec!["**".to_string()]);
    // a wildcard matching none of them keeps matching its name at any depth
    assert_eq!(inherited_excludes(&["ot*"], "/", "/sub"), vec!["ot*".to_string()]);
    // a pattern is normalized the way the ancestor config interprets it before
    // rebasing, so a backslash separator is a separator and not part of a name
    assert_eq!(inherited_excludes(&["dist\\sub"], "/", "/sub"), Vec::<String>::new());
    assert_eq!(inherited_excludes(&["sub\\dist"], "/", "/sub"), vec!["dist".to_string()]);
    // a leading `/` anchors to the ancestor config's directory
    assert_eq!(inherited_excludes(&["/sub/dist"], "/", "/sub"), vec!["./dist".to_string()]);
    assert_eq!(inherited_excludes(&["/dist"], "/", "/sub"), Vec::<String>::new());
    // a pattern anchored into the nested directory is rebased to be relative to it
    assert_eq!(inherited_excludes(&["sub/dist"], "/", "/sub"), vec!["dist".to_string()]);
    // an anchored pattern that points outside the nested directory is dropped
    assert_eq!(inherited_excludes(&["other/dist"], "/", "/sub"), Vec::<String>::new());
    // dropping leaves the nested config's own excludes intact
    let mut files = FileSelection {
      includes: None,
      excludes: vec!["own".to_string()],
    };
    files.inherit(
      &FileSelection {
        includes: None,
        excludes: vec!["other/dist".to_string()],
      },
      &CanonicalizedPathBuf::new_for_testing("/"),
      &CanonicalizedPathBuf::new_for_testing("/sub"),
    );
    assert_eq!(files.excludes, vec!["own".to_string()]);
  }

  #[test]
  fn inherit_config_should_error_overriding_locked_ancestor_config() {
    fn config_with_test_plugin(origin: ConfigOrigin, locked: bool, indent_width: i32) -> ResolvedConfig {
      ResolvedConfig {
        origin,
        files: Default::default(),
        routing: Default::default(),
        execution: Default::default(),
        plugins: PluginConfiguration {
          origins: Default::default(),
          remote_exec: Default::default(),
          sources: Vec::new(),
          config: ConfigMap::from([(
            "test".to_string(),
            ConfigMapValue::PluginConfig(RawPluginConfig {
              locked,
              associations: None,
              overrides: Vec::new(),
              properties: ConfigKeyMap::from([("indentWidth".to_string(), ConfigKeyValue::from_i32(indent_width))]),
            }),
          )]),
        },
      }
    }
    let parent = config_with_test_plugin(test_origin("/dprint.json", "/"), true, 4);
    let mut child = config_with_test_plugin(test_origin("/sub/dprint.json", "/sub"), false, 2);

    let err = child.inherit(&parent).err().unwrap();
    assert_eq!(
      err.to_string(),
      concat!(
        "The configuration for \"test\" was locked, but a parent configuration specified it. ",
        "Locked configurations cannot have their properties overridden."
      )
    );
  }

  /// The same configuration files resolve the same whether they're JSON, TOML
  /// or some of each (see [`resolve_in_every_format`]).
  mod formats {
    use pretty_assertions::assert_eq;

    use super::*;

    /// Resolves the configuration file of that name, and what it logged.
    async fn resolve(environment: &TestEnvironment, paths: &ConfigPaths, name: &str) -> (Result<ResolvedConfig, String>, Vec<String>) {
      let result = get_result(paths.get(name), environment).await.map_err(|err| err.to_string());
      (result, environment.take_stderr_messages())
    }

    fn plugin_config<'a>(config: &'a ResolvedConfig, key: &str) -> &'a RawPluginConfig {
      match config.plugins.config.get(key) {
        Some(ConfigMapValue::PluginConfig(plugin)) => plugin,
        other => panic!("expected the configuration of {}, got {:?}", key, other),
      }
    }

    #[test]
    fn resolves_every_kind_of_property_the_same() {
      let (result, messages) = resolve_in_every_format(
        &[
          (
            "dprint",
            r##"{
              "extends": "./<base>",
              "lineWidth": 100,
              "useTabs": true,
              "incremental": false,
              "includes": ["src/**"],
              "excludes": ["**/dist", "generated"],
              "shebangs": { "#!/usr/bin/env node": "js" },
              "plugins": ["https://plugins.dprint.dev/test-plugin.wasm", "npm:@dprint/exec@0.7.3/plugin.json@704701df449dd7e942a71144773778ac529d68c2e4657bfc236d393b898b9a67"],
              "test": {
                "associations": ["**/*.txt", "!**/skip.txt"],
                "binaryExpression.operatorPosition": "sameLine",
                "nested": { "deeper": [1, 2], "flag": false },
                "overrides": [{ "files": "**/*.md", "lineWidth": 40 }]
              },
              "exec": {
                "timeout": 5,
                "commands": [
                  { "command": "tr a-z A-Z", "exts": ["txt"], "stdin": true },
                  { "command": "cat", "fileNames": ["README"] }
                ]
              }
            }"##,
          ),
          ("base", r#"{ "indentWidth": 4, "test": { "newLineKind": "crlf" } }"#),
        ],
        async |environment, paths| resolve(environment, paths, "dprint").await,
      );
      assert_eq!(messages, Vec::<String>::new());
      let config = result.unwrap();
      assert_eq!(config.files.includes, Some(vec!["src/**".to_string()]));
      assert_eq!(config.execution.incremental, Some(false));
      assert_eq!(config.plugins.sources.len(), 2);
      assert_eq!(config.plugins.config.get("indentWidth"), Some(&ConfigMapValue::from_i32(4)));
      let test = plugin_config(&config, "test");
      assert_eq!(test.associations, Some(vec!["**/*.txt".to_string(), "!**/skip.txt".to_string()]));
      assert_eq!(test.overrides.len(), 1);
      assert_eq!(test.properties.get("newLineKind"), Some(&ConfigKeyValue::from_str("crlf")));
      let ConfigKeyValue::Array(commands) = &plugin_config(&config, "exec").properties["commands"] else {
        unreachable!();
      };
      assert_eq!(commands.len(), 2);
    }

    #[test]
    fn reads_a_file_extended_twice_once_where_it_has_the_highest_precedence() {
      let (result, _) = resolve_in_every_format(
        &[
          (
            "dprint",
            r#"{ "extends": ["./<b>", "./<c>"], "plugins": ["https://plugins.dprint.dev/test-plugin.wasm"] }"#,
          ),
          ("b", r#"{ "extends": "./<d>", "excludes": ["b"] }"#),
          ("c", r#"{ "extends": "./<d>", "excludes": ["c"], "lineWidth": 100 }"#),
          (
            "d",
            r#"{ "excludes": ["d"], "lineWidth": 80, "test": { "overrides": [{ "files": "*.d", "prop": 1 }] } }"#,
          ),
        ],
        async |environment, paths| resolve(environment, paths, "dprint").await,
      );
      let config = result.unwrap();
      assert_eq!(config.files.excludes, vec!["b".to_string(), "d".to_string(), "c".to_string()]);
      assert_eq!(config.plugins.config.get("lineWidth"), Some(&ConfigMapValue::from_i32(80)));
      assert_eq!(plugin_config(&config, "test").overrides.len(), 1);
    }

    #[test]
    fn stops_at_an_extends_cycle() {
      let (result, _) = resolve_in_every_format(
        &[
          ("a", r#"{ "extends": "./<b>", "plugins": ["https://plugins.dprint.dev/test-plugin.wasm"] }"#),
          ("b", r#"{ "extends": "./<c>" }"#),
          ("c", r#"{ "extends": "./<a>" }"#),
        ],
        async |environment, paths| resolve(environment, paths, "a").await,
      );
      assert_eq!(
        result.unwrap_err(),
        "The configuration file './a.json' extends itself: /a.json -> /b.json -> /c.json -> /a.json\n    at /c.json\n    at /b.json"
      );
    }

    #[test]
    fn stops_at_a_remote_extends_cycle() {
      let (result, _) = resolve_in_every_format(
        &[
          ("dprint", r#"{ "extends": "<https://dprint.dev/a>" }"#),
          ("https://dprint.dev/a", r#"{ "extends": "<https://dprint.dev/b>" }"#),
          ("https://dprint.dev/b", r#"{ "extends": "<https://dprint.dev/a>" }"#),
        ],
        async |environment, paths| resolve(environment, paths, "dprint").await,
      );
      assert_eq!(
        result.unwrap_err(),
        concat!(
          "The configuration file 'https://dprint.dev/a.json' extends itself: ",
          "/dprint.json -> https://dprint.dev/a.json -> https://dprint.dev/b.json -> https://dprint.dev/a.json\n",
          "    at https://dprint.dev/b.json\n",
          "    at https://dprint.dev/a.json"
        )
      );
    }

    #[test]
    fn errors_overriding_a_locked_configuration_it_extends() {
      let files = |own_test_config: &'static str| {
        [
          ("dprint", own_test_config),
          ("https://dprint.dev/locked", r#"{ "test": { "locked": true, "prop": 6, "other": "test" } }"#),
        ]
      };
      let (result, _) = resolve_in_every_format(
        &files(r#"{ "extends": "<https://dprint.dev/locked>", "test": { "prop": 5 } }"#),
        async |environment, paths| resolve(environment, paths, "dprint").await,
      );
      assert_eq!(
        result.unwrap_err(),
        concat!(
          "The configuration for \"test\" was locked, but a parent configuration specified it. ",
          "Locked configurations cannot have their properties overridden.\n",
          "    at https://dprint.dev/locked.json",
        )
      );

      // and it's used as is when the configuration doesn't
      let (result, _) = resolve_in_every_format(&files(r#"{ "extends": "<https://dprint.dev/locked>" }"#), async |environment, paths| {
        resolve(environment, paths, "dprint").await
      });
      let config = result.unwrap();
      let test = plugin_config(&config, "test");
      assert!(test.locked);
      assert_eq!(test.properties.get("prop"), Some(&ConfigKeyValue::from_i32(6)));
    }

    #[test]
    fn inherits_the_ancestor_configuration() {
      let (ancestor, result) = resolve_in_every_format(
        &[
          (
            "dprint",
            r##"{
              "lineWidth": 80,
              "incremental": true,
              "excludes": ["**/node_modules", "sub/dist"],
              "shebangs": { "#!/bin/sh": "sh" },
              "plugins": ["https://plugins.dprint.dev/test-plugin.wasm"],
              "test": { "indentWidth": 4, "newLineKind": "crlf" }
            }"##,
          ),
          (
            "sub/dprint",
            r#"{ "inherit": true, "extends": "/<sub/base>", "excludes": ["own"], "test": { "indentWidth": 2 } }"#,
          ),
          ("sub/base", r#"{ "useTabs": true }"#),
        ],
        async |environment, paths| {
          let ancestor = resolve_local_config(paths.get("dprint"), environment).await;
          let result = resolve_local_descendant_config(paths.get("sub/dprint"), &ancestor, environment)
            .await
            .map_err(|err| err.to_string());
          (ancestor, result)
        },
      );
      let config = result.unwrap();
      assert_eq!(config.plugins.sources, ancestor.plugins.sources);
      assert_eq!(
        config.files.excludes,
        vec!["**/node_modules".to_string(), "dist".to_string(), "own".to_string()]
      );
      assert_eq!(config.execution.incremental, Some(true));
      assert_eq!(config.routing.shebangs, ancestor.routing.shebangs);
      assert_eq!(config.plugins.config.get("lineWidth"), Some(&ConfigMapValue::from_i32(80)));
      assert_eq!(config.plugins.config.get("useTabs"), Some(&ConfigMapValue::from_bool(true)));
      let test = plugin_config(&config, "test");
      assert_eq!(test.properties.get("indentWidth"), Some(&ConfigKeyValue::from_i32(2)));
      assert_eq!(test.properties.get("newLineKind"), Some(&ConfigKeyValue::from_str("crlf")));
    }

    #[test]
    fn errors_overriding_a_locked_ancestor_configuration() {
      let (_, result) = resolve_in_every_format(
        &[
          (
            "dprint",
            r#"{ "plugins": ["https://plugins.dprint.dev/test-plugin.wasm"], "test": { "locked": true, "indentWidth": 4 } }"#,
          ),
          ("sub/dprint", r#"{ "inherit": true, "test": { "indentWidth": 2 } }"#),
        ],
        async |environment, paths| {
          let ancestor = resolve_local_config(paths.get("dprint"), environment).await;
          let result = resolve_local_descendant_config(paths.get("sub/dprint"), &ancestor, environment)
            .await
            .map_err(|err| err.to_string());
          (ancestor, result)
        },
      );
      assert_eq!(
        result.unwrap_err(),
        concat!(
          "The configuration for \"test\" was locked, but a parent configuration specified it. ",
          "Locked configurations cannot have their properties overridden."
        )
      );
    }

    #[test]
    fn says_which_file_an_invalid_property_is_in() {
      let (result, _) = resolve_in_every_format(
        &[
          (
            "dprint",
            r#"{ "extends": "./<base>", "plugins": ["https://plugins.dprint.dev/test-plugin.wasm"] }"#,
          ),
          ("base", r#"{ "incremental": "yes" }"#),
        ],
        async |environment, paths| resolve(environment, paths, "dprint").await,
      );
      assert_eq!(result.unwrap_err(), "Expected boolean in 'incremental' property.\n    at /base.json");
    }
  }

  mod remote_exec {
    use pretty_assertions::assert_eq;

    use super::*;

    const EXEC_PLUGIN: &str = "npm:@dprint/exec@0.7.3/plugin.json@704701df449dd7e942a71144773778ac529d68c2e4657bfc236d393b898b9a67";
    const REMOTE_URL: &str = "https://dprint.dev/exec.json";

    fn remote_config(exec_extra: &str) -> String {
      format!(
        r#"{{
          "exec": {{
            {}
            "commands": [
              {{ "command": "tombi format -", "exts": ["toml"] }},
              {{ "command": "rustfmt --edition 2024", "exts": ["rs"], "setupCommand": "rustup component add rustfmt" }},
              {{ "command": "evil", "exts": ["txt"] }}
            ]
          }},
          "plugins": ["{}"]
        }}"#,
        exec_extra, EXEC_PLUGIN
      )
    }

    /// What the tests look at in a resolved configuration.
    #[derive(Debug)]
    struct Resolved {
      plugins: Vec<String>,
      exec: Exec,
      messages: Vec<String>,
    }

    /// The resolved exec configuration, with each command by what it runs.
    #[derive(Debug, Default, PartialEq)]
    struct Exec {
      properties: ExecProperties,
      overrides: Vec<ExecOverride>,
    }

    #[derive(Debug, PartialEq)]
    struct ExecOverride {
      files: Vec<String>,
      properties: ExecProperties,
    }

    #[derive(Debug, Default, PartialEq)]
    struct ExecProperties {
      commands: Vec<String>,
      cwd: Option<String>,
      /// The names of the other properties, so that one a remote configuration
      /// can't set (ex. `playWithFire`) doesn't go unnoticed.
      other_keys: Vec<String>,
    }

    impl ExecProperties {
      fn new(commands: &[&str], cwd: Option<&str>) -> Self {
        Self {
          commands: commands.iter().map(|command| command.to_string()).collect(),
          cwd: cwd.map(ToOwned::to_owned),
          other_keys: Vec::new(),
        }
      }

      fn from_config(properties: &ConfigKeyMap) -> Self {
        // what a command runs, or all of it when it doesn't say
        let command_text = |command: &ConfigKeyValue| match command {
          ConfigKeyValue::Object(object) => match object.get("command") {
            Some(ConfigKeyValue::String(text)) => text.clone(),
            _ => format!("{:?}", command),
          },
          _ => format!("{:?}", command),
        };
        Self {
          commands: match properties.get("commands") {
            Some(ConfigKeyValue::Array(commands)) => commands.iter().map(command_text).collect(),
            Some(other) => vec![format!("{:?}", other)],
            None => Vec::new(),
          },
          cwd: properties.get("cwd").map(|cwd| match cwd {
            ConfigKeyValue::String(cwd) => cwd.clone(),
            other => format!("{:?}", other),
          }),
          other_keys: properties.keys().filter(|key| *key != "commands" && *key != "cwd").cloned().collect(),
        }
      }
    }

    impl ExecOverride {
      fn new(files: &str, commands: &[&str], cwd: Option<&str>) -> Self {
        Self {
          files: vec![files.to_string()],
          properties: ExecProperties::new(commands, cwd),
        }
      }
    }

    fn resolve(local_config: &str, remote_config: &str) -> Result<Resolved, String> {
      resolve_with_base(
        local_config,
        remote_config,
        r#"{ "exec": { "commands": [{ "command": "local-base", "exts": ["md"] }] } }"#,
      )
    }

    /// Resolves the local configuration, which may extend `REMOTE_URL` and
    /// `./base.json`, with each of the three files in JSON and in TOML (see
    /// [`resolve_in_every_format`]).
    fn resolve_with_base(local_config: &str, remote_config: &str, base_config: &str) -> Result<Resolved, String> {
      let local_config = local_config.replace(REMOTE_URL, "<https://dprint.dev/exec>").replace("./base.json", "./<base>");
      resolve_files(&[
        ("dprint", local_config.as_str()),
        ("base", base_config),
        ("https://dprint.dev/exec", remote_config),
      ])
    }

    /// Resolves the file `dprint` of the files (see `resolve_in_every_format`).
    fn resolve_files(files: &[(&str, &str)]) -> Result<Resolved, String> {
      resolve_in_every_format(files, async |environment, paths| {
        let result = get_result(paths.get("dprint"), environment).await.map_err(|err| err.to_string())?;
        Ok(Resolved {
          plugins: plugin_names(&result),
          exec: exec_of(&result),
          messages: environment.take_stderr_messages(),
        })
      })
    }

    fn plugin_names(config: &ResolvedConfig) -> Vec<String> {
      config.plugins.sources.iter().map(|plugin| plugin.to_string()).collect()
    }

    fn exec_of(config: &ResolvedConfig) -> Exec {
      match config.plugins.config.get("exec") {
        Some(ConfigMapValue::PluginConfig(exec)) => Exec {
          properties: ExecProperties::from_config(&exec.properties),
          overrides: exec
            .overrides
            .iter()
            .map(|override_config| ExecOverride {
              files: override_config.files.clone(),
              properties: ExecProperties::from_config(&override_config.properties),
            })
            .collect(),
        },
        _ => Exec::default(),
      }
    }

    /// A remote configuration that runs its commands through the working
    /// directory and overrides rather than `commands`.
    fn remote_config_with_overrides() -> String {
      r#"{
        "exec": {
          "cwd": "/remote-cwd",
          "overrides": [{
            "files": "**/*.txt",
            "playWithFire": true,
            "cwd": "/remote-override-cwd",
            "commands": [
              { "command": "tombi format -", "exts": ["txt"] },
              { "command": "evil", "exts": ["txt"] }
            ]
          }]
        }
      }"#
        .to_string()
    }

    const ALL_REMOTE_COMMANDS: [&str; 3] = ["tombi format -", "rustfmt --edition 2024", "evil"];

    #[test]
    fn ignores_remote_exec_commands_by_default() {
      let result = resolve(
        &format!(r#"{{ "extends": "{}", "plugins": ["{}"] }}"#, REMOTE_URL, EXEC_PLUGIN),
        &remote_config(""),
      )
      .unwrap();
      assert_eq!(result.plugins, vec![EXEC_PLUGIN.to_string()]);
      assert_eq!(result.exec, Exec::default());
      assert_eq!(
        result.messages,
        vec![
          concat!(
            "Note: The exec commands in remote configuration (https://dprint.dev/exec.json) are ignored for security reasons. ",
            "To run them, specify \"playWithFire\" in the exec configuration of a local configuration file ",
            "(`true` or the programs they may run)."
          )
          .to_string()
        ]
      );
    }

    #[test]
    fn ignores_the_remote_exec_plugin_by_default() {
      let result = resolve(&format!(r#"{{ "extends": "{}" }}"#, REMOTE_URL), &remote_config("")).unwrap();
      assert_eq!(result.plugins, Vec::<String>::new());
      assert_eq!(result.exec, Exec::default());
      assert_eq!(result.messages.len(), 2);
      assert!(
        result.messages[1].starts_with("Note: The exec plugin in remote configuration is ignored"),
        "{:?}",
        result.messages
      );
    }

    #[test]
    fn treats_another_remote_exec_version_as_a_process_plugin() {
      // the built-in exec only serves the version it was built from, so a remote
      // configuration asking for another one asks for a process plugin
      let other_version = "npm:@dprint/exec@0.6.0/plugin.json@abc";
      let result = resolve(
        &format!(r#"{{ "extends": "{}", "exec": {{ "playWithFire": true }} }}"#, REMOTE_URL),
        &format!(r#"{{ "plugins": ["{}"] }}"#, other_version),
      )
      .unwrap();
      assert_eq!(result.plugins, Vec::<String>::new());
      assert_eq!(result.messages, vec![get_warn_non_wasm_plugins_message()]);
    }

    #[test]
    fn doesnt_add_the_remote_exec_plugin_when_local_config_uses_another_exec_version() {
      let other_version = "npm:@dprint/exec@0.6.0/plugin.json@abc";
      let result = resolve(
        &format!(
          r#"{{ "extends": "{}", "plugins": ["{}"], "exec": {{ "playWithFire": true }} }}"#,
          REMOTE_URL, other_version
        ),
        &remote_config(""),
      )
      .unwrap();
      assert_eq!(result.plugins, vec![other_version.to_string()]);
    }

    #[test]
    fn runs_any_remote_exec_command_when_playing_with_fire() {
      let result = resolve(
        &format!(r#"{{ "extends": "{}", "exec": {{ "playWithFire": true }} }}"#, REMOTE_URL),
        &remote_config(""),
      )
      .unwrap();
      assert_eq!(result.plugins, vec![EXEC_PLUGIN.to_string()]);
      // the plugin doesn't see the setting
      assert_eq!(
        result.exec,
        Exec {
          properties: ExecProperties::new(&ALL_REMOTE_COMMANDS, None),
          overrides: Vec::new(),
        }
      );
      assert_eq!(result.messages, Vec::<String>::new());
    }

    #[test]
    fn runs_remote_exec_commands_of_listed_programs() {
      let result = resolve(
        &format!(r#"{{ "extends": "{}", "exec": {{ "playWithFire": ["tombi", "rustfmt"] }} }}"#, REMOTE_URL),
        &remote_config(""),
      )
      .unwrap();
      assert_eq!(result.plugins, vec![EXEC_PLUGIN.to_string()]);
      // rustfmt's setup command runs rustup, which isn't listed
      assert_eq!(
        result.exec,
        Exec {
          properties: ExecProperties::new(&["tombi format -"], None),
          overrides: Vec::new(),
        }
      );
      assert_eq!(
        result.messages,
        vec![
          "Note: Ignored 2 exec command(s) in remote configuration (https://dprint.dev/exec.json) that run programs not listed in \"playWithFire\": rustup, evil"
            .to_string()
        ]
      );
    }

    #[test]
    fn remote_config_cannot_allow_itself() {
      let result = resolve(&format!(r#"{{ "extends": "{}" }}"#, REMOTE_URL), &remote_config(r#""playWithFire": true,"#)).unwrap();
      assert_eq!(result.plugins, Vec::<String>::new());
      assert_eq!(result.exec, Exec::default());
      assert_eq!(
        result.messages[0],
        "Note: \"playWithFire\" is ignored in remote configuration (https://dprint.dev/exec.json). Specify it in a local configuration file."
      );
    }

    #[test]
    fn higher_precedence_local_commands_win() {
      let result = resolve(
        &format!(
          r#"{{ "extends": "{}", "exec": {{ "playWithFire": true, "commands": [{{ "command": "local", "exts": ["txt"] }}] }} }}"#,
          REMOTE_URL
        ),
        &remote_config(""),
      )
      .unwrap();
      assert_eq!(result.exec.properties, ExecProperties::new(&["local"], None));
    }

    #[test]
    fn allowed_remote_commands_win_over_lower_precedence_local_ones() {
      let local = |play_with_fire: &str| format!(r#"{{ "extends": ["{}", "./base.json"], "exec": {{ {} }} }}"#, REMOTE_URL, play_with_fire);
      let result = resolve(&local(r#""playWithFire": true"#), &remote_config("")).unwrap();
      assert_eq!(result.exec.properties, ExecProperties::new(&ALL_REMOTE_COMMANDS, None));
      // when not allowed, the local ones apply
      let result = resolve(&local(""), &remote_config("")).unwrap();
      assert_eq!(result.exec.properties, ExecProperties::new(&["local-base"], None));
    }

    #[test]
    fn ignores_remote_cwd_and_override_commands_by_default() {
      let result = resolve(
        &format!(
          r#"{{ "extends": "{}", "exec": {{ "commands": [{{ "command": "./local", "exts": ["txt"] }}] }}, "plugins": ["{}"] }}"#,
          REMOTE_URL, EXEC_PLUGIN
        ),
        &remote_config_with_overrides(),
      )
      .unwrap();
      // a relative command would otherwise run from the remote working
      // directory, and the override is left without anything
      assert_eq!(
        result.exec,
        Exec {
          properties: ExecProperties::new(&["./local"], None),
          overrides: Vec::new(),
        }
      );
      assert_eq!(
        result.messages,
        vec![
          "Note: \"playWithFire\" is ignored in remote configuration (https://dprint.dev/exec.json). Specify it in a local configuration file.".to_string(),
          concat!(
            "Note: The exec commands in remote configuration (https://dprint.dev/exec.json) are ignored for security reasons. ",
            "To run them, specify \"playWithFire\" in the exec configuration of a local configuration file ",
            "(`true` or the programs they may run)."
          )
          .to_string(),
          concat!(
            "Note: The exec \"cwd\" in remote configuration (https://dprint.dev/exec.json) is ignored for security reasons, as it decides what commands run. ",
            "To use it, specify \"playWithFire\": true in the exec configuration of a local configuration file."
          )
          .to_string(),
        ]
      );
    }

    #[test]
    fn runs_remote_override_commands_of_listed_programs_without_remote_cwd() {
      let result = resolve(
        &format!(r#"{{ "extends": "{}", "exec": {{ "playWithFire": ["tombi"] }} }}"#, REMOTE_URL),
        &remote_config_with_overrides(),
      )
      .unwrap();
      assert_eq!(
        result.exec,
        Exec {
          properties: ExecProperties::default(),
          overrides: vec![ExecOverride::new("**/*.txt", &["tombi format -"], None)],
        }
      );
      assert_eq!(
        result.messages[1],
        "Note: Ignored 1 exec command(s) in remote configuration (https://dprint.dev/exec.json) that run programs not listed in \"playWithFire\": evil"
      );
    }

    #[test]
    fn applies_remote_cwd_and_overrides_in_precedence_order_when_playing_with_fire() {
      let override_config = |command: &str| format!(r#"{{ "files": "**/*.txt", "commands": [{{ "command": "{}", "exts": ["txt"] }}] }}"#, command);
      let result = resolve_with_base(
        &format!(
          r#"{{ "extends": ["{}", "./base.json"], "exec": {{ "playWithFire": true, "overrides": [{}] }} }}"#,
          REMOTE_URL,
          override_config("local")
        ),
        &remote_config_with_overrides(),
        &format!(r#"{{ "exec": {{ "cwd": "/base-cwd", "overrides": [{}] }} }}"#, override_config("local-base")),
      )
      .unwrap();
      // The remote configuration has precedence over the base it's listed
      // before. Later overrides win, so they're ordered from lowest to
      // highest precedence.
      assert_eq!(
        result.exec,
        Exec {
          properties: ExecProperties::new(&[], Some("/remote-cwd")),
          overrides: vec![
            ExecOverride::new("**/*.txt", &["local-base"], None),
            ExecOverride::new("**/*.txt", &["tombi format -", "evil"], Some("/remote-override-cwd")),
            ExecOverride::new("**/*.txt", &["local"], None),
          ],
        }
      );
    }

    #[test]
    fn errors_for_an_invalid_value() {
      let err = resolve(
        &format!(r#"{{ "extends": "{}", "exec": {{ "playWithFire": "yes" }} }}"#, REMOTE_URL),
        &remote_config(""),
      )
      .err();
      assert_eq!(
        err,
        Some("Expected \"exec.playWithFire\" to be true, false, or an array of programs.".to_string())
      );
    }

    #[test]
    fn ignores_the_remote_exec_plugin_when_none_of_its_commands_are_allowed() {
      let result = resolve(
        &format!(r#"{{ "extends": "{}", "exec": {{ "playWithFire": ["prettier"] }} }}"#, REMOTE_URL),
        &remote_config(""),
      )
      .unwrap();
      // without commands its configuration would only be an error
      assert_eq!(result.plugins, Vec::<String>::new());
      assert_eq!(result.exec, Exec::default());
      assert_eq!(
        result.messages.last().unwrap(),
        "Note: The exec plugin in remote configuration is ignored, as none of the exec commands are allowed by \"playWithFire\"."
      );
    }

    #[test]
    fn keeps_invalid_remote_commands_over_lower_precedence_ones() {
      // the extended remote configuration's commands aren't an array, which
      // still takes precedence over the commands of the one it extends
      let resolve_allowing = |play_with_fire: &str| {
        let local_config = format!(
          r#"{{ "extends": "<https://dprint.dev/exec>", "exec": {{ "playWithFire": {} }} }}"#,
          play_with_fire
        );
        resolve_files(&[
          ("dprint", local_config.as_str()),
          (
            "https://dprint.dev/exec",
            r#"{ "extends": "<https://dprint.dev/lower>", "exec": { "commands": "evil" } }"#,
          ),
          (
            "https://dprint.dev/lower",
            r#"{ "exec": { "commands": [{ "command": "evil", "exts": ["txt"] }] }, "plugins": ["npm:@dprint/exec@0.7.3/plugin.json@704701df449dd7e942a71144773778ac529d68c2e4657bfc236d393b898b9a67"] }"#,
          ),
        ])
        .unwrap()
      };
      for play_with_fire in ["true", r#"["evil"]"#] {
        let result = resolve_allowing(play_with_fire);
        // which runs nothing, and the exec plugin reports
        assert_eq!(result.exec.properties.commands, vec![r#"String("evil")"#.to_string()], "{}", play_with_fire);
        assert_eq!(result.plugins, vec![EXEC_PLUGIN.to_string()]);
      }
      let result = resolve_allowing("false");
      assert_eq!(result.exec, Exec::default());
      assert_eq!(result.plugins, Vec::<String>::new());
    }

    #[test]
    fn applies_a_nested_configurations_play_with_fire_to_the_remote_commands_it_inherits() {
      let inherit = |nested_exec: &str| {
        let nested_config = format!(r#"{{ "inherit": true{} }}"#, nested_exec);
        // with a property the exec plugin 0.7.3 doesn't have
        let remote_config = remote_config(r#""shell": "bash","#);
        resolve_in_every_format(
          &[
            ("dprint", r#"{ "extends": "<https://dprint.dev/exec>", "exec": { "playWithFire": true } }"#),
            ("sub/dprint", nested_config.as_str()),
            ("https://dprint.dev/exec", remote_config.as_str()),
          ],
          async |environment, paths| {
            let ancestor = resolve_local_config(paths.get("dprint"), environment).await;
            let result = resolve_local_descendant_config(paths.get("sub/dprint"), &ancestor, environment).await.unwrap();
            (exec_of(&result), plugin_names(&result))
          },
        )
      };

      // what the ancestor allowed, when it doesn't say
      let (exec, plugins) = inherit("");
      assert_eq!(exec.properties.commands, ALL_REMOTE_COMMANDS.to_vec());
      assert_eq!(exec.properties.other_keys, vec!["shell".to_string()]);
      assert_eq!(plugins, vec![EXEC_PLUGIN.to_string()]);
      // nothing remote, when it doesn't allow any
      let (exec, plugins) = inherit(r#", "exec": { "playWithFire": false }"#);
      assert_eq!(exec, Exec::default());
      assert_eq!(plugins, Vec::<String>::new());
      // the remote commands of the programs it allows
      let (exec, plugins) = inherit(r#", "exec": { "playWithFire": ["tombi"] }"#);
      assert_eq!(exec.properties.commands, vec!["tombi format -".to_string()]);
      assert_eq!(exec.properties.other_keys, Vec::<String>::new());
      assert_eq!(plugins, vec![EXEC_PLUGIN.to_string()]);
    }

    /// A remote configuration with exec properties the exec plugin 0.7.3
    /// doesn't have (ex. ones a later version might add), at the root, in an
    /// override and in a command, next to ones it has that don't decide what
    /// runs.
    fn remote_config_with_unknown_properties() -> String {
      r#"{
        "exec": {
          "lineWidth": 100,
          "cacheKey": "remote",
          "shell": "bash",
          "commands": [
            { "command": "tombi format -", "exts": ["toml"] },
            { "command": "tombi lint", "exts": ["toml"], "shell": "bash" }
          ],
          "overrides": [{ "files": "**/*.txt", "timeout": 5, "env": { "PATH": "/remote" } }]
        }
      }"#
        .to_string()
    }

    fn exec_with(commands: &[&str], other_keys: &[&str], override_keys: &[&str]) -> Exec {
      let keys = |keys: &[&str]| keys.iter().map(|key| key.to_string()).collect::<Vec<_>>();
      Exec {
        properties: ExecProperties {
          other_keys: keys(other_keys),
          ..ExecProperties::new(commands, None)
        },
        overrides: vec![ExecOverride {
          files: vec!["**/*.txt".to_string()],
          properties: ExecProperties {
            other_keys: keys(override_keys),
            ..ExecProperties::default()
          },
        }],
      }
    }

    #[test]
    fn only_uses_remote_exec_properties_known_not_to_decide_what_runs() {
      let resolve_allowing = |play_with_fire: &str| {
        resolve(
          &format!(r#"{{ "extends": "{}", "exec": {{ "playWithFire": {} }} }}"#, REMOTE_URL, play_with_fire),
          &remote_config_with_unknown_properties(),
        )
        .unwrap()
      };
      let ignored_property = |key: &str| {
        format!(
          concat!(
            "Note: The exec \"{}\" in remote configuration (https://dprint.dev/exec.json) is ignored for security reasons, ",
            "as the exec plugin 0.7.3 doesn't have it, so dprint can't tell what it does. ",
            "To use it, specify \"playWithFire\": true in the exec configuration of a local configuration file."
          ),
          key
        )
      };

      // the programs a command runs can't be checked when it has others
      let result = resolve_allowing(r#"["tombi"]"#);
      assert_eq!(result.exec, exec_with(&["tombi format -"], &["lineWidth", "cacheKey"], &["timeout"]));
      assert_eq!(
        result.messages,
        vec![
          concat!(
            "Note: Ignored 1 exec command(s) in remote configuration (https://dprint.dev/exec.json) that have properties ",
            "the exec plugin 0.7.3 doesn't, which only run with \"playWithFire\": true: shell"
          )
          .to_string(),
          ignored_property("shell"),
          ignored_property("env"),
        ]
      );

      let result = resolve_allowing("false");
      assert_eq!(result.exec, exec_with(&[], &["lineWidth", "cacheKey"], &["timeout"]));
      assert_eq!(result.messages[1..], [ignored_property("shell"), ignored_property("env")]);

      // all of them when any program may run
      let result = resolve_allowing("true");
      assert_eq!(
        result.exec,
        exec_with(&["tombi format -", "tombi lint"], &["lineWidth", "cacheKey", "shell"], &["timeout", "env"])
      );
      assert_eq!(result.messages, Vec::<String>::new());
    }
  }

  #[test]
  fn should_ignore_non_wasm_plugins_in_remote_config() {
    let environment = TestEnvironment::new();
    environment.add_remote_file(
      "https://dprint.dev/test.json",
      r#"{
            "plugins": ["./test-plugin.json@checksum"]
        }"#
        .as_bytes(),
    );

    environment.clone().run_in_runtime(async move {
      let result = get_result("https://dprint.dev/test.json", &environment).await.unwrap();
      assert_eq!(result.plugins.sources, vec![]);
      assert_eq!(environment.take_stderr_messages(), vec![get_warn_non_wasm_plugins_message()]);
    });
  }

  #[test]
  fn should_ignore_non_wasm_plugins_in_remote_extends() {
    let environment = TestEnvironment::new();
    environment
      .write_file(
        &PathBuf::from("/test.json"),
        r#"{
            "extends": "https://dprint.dev/dir/test.json",
            "prop1": 1
        }"#,
      )
      .unwrap();
    environment.add_remote_file(
      "https://dprint.dev/dir/test.json",
      r#"{
            "plugins": ["./test-plugin.json@checksum"]
        }"#
        .as_bytes(),
    );

    environment.clone().run_in_runtime(async move {
      let result = get_result("/test.json", &environment).await.unwrap();
      assert_eq!(environment.take_stderr_messages(), vec![get_warn_non_wasm_plugins_message()]);
      assert_eq!(result.plugins.sources, vec![]);
    });
  }

  #[test]
  fn should_not_allow_non_wasm_plugins_in_local_extends() {
    let environment = TestEnvironmentBuilder::new()
      .write_file(
        &PathBuf::from("/test.json"),
        r#"{
            "extends": "dir/test.json",
            "prop1": 1
        }"#,
      )
      .write_file(
        &PathBuf::from("/dir/test.json"),
        r#"{
            "plugins": ["./test-plugin.json@checksum"]
        }"#,
      )
      .build();

    environment.clone().run_in_runtime(async move {
      let result = get_result("/test.json", &environment).await.unwrap();
      assert_eq!(environment.take_stdout_messages().len(), 0);
      assert_eq!(
        result.plugins.sources,
        vec![PluginSourceReference {
          path_source: PathSource::new_local(CanonicalizedPathBuf::new_for_testing("/dir/test-plugin.json")),
          checksum: Some(String::from("checksum")),
        }]
      );
    });
  }

  #[test]
  fn should_ignore_project_type() {
    // ignore the projectType property
    let environment = TestEnvironment::new();
    environment
      .write_file(
        &PathBuf::from("/test.json"),
        r#"{
            "projectType": "openSource",
            "plugins": ["https://plugins.dprint.dev/test-plugin.wasm"],
            "includes": ["test"],
            "excludes": ["test"]
        }"#,
      )
      .unwrap();

    environment.clone().run_in_runtime(async move {
      let result = get_result("/test.json", &environment).await.unwrap();
      assert_eq!(environment.take_stdout_messages().len(), 0);
      assert_eq!(result.plugins.config.is_empty(), true); // should not include projectType
    });
  }

  #[test]
  fn should_resolve_config_dir_local_file() {
    let environment = TestEnvironmentBuilder::new()
      .add_remote_file(
        "https://dprint.dev/test.json",
        r#"{
      "extends": "./next.json",
      "otherPlugin": {
        "value": "${originConfigDir}/origin"
      }
}"#,
      )
      .add_remote_file(
        "https://dprint.dev/next.json",
        r#"{
      "final": {
        "value": "${originConfigDir}/final && \\${configDir}/escaped"
      }
}"#,
      )
      .write_file(
        "/dir/dprint.json",
        r#"{
      "extends": "https://dprint.dev/test.json",
      "plugin": {
        "value": "${configDir}/test && ${originConfigDir}/other"
      }
}"#,
      )
      .build();

    environment.clone().run_in_runtime(async move {
      let config = get_result("/dir/dprint.json", &environment).await.unwrap();
      assert_eq!(
        config.plugins.config,
        ConfigMap::from([
          (
            "plugin".to_string(),
            ConfigMapValue::PluginConfig(RawPluginConfig {
              locked: false,
              associations: None,
              overrides: Vec::new(),
              properties: ConfigKeyMap::from([(String::from("value"), ConfigKeyValue::from_str("/dir/test && /dir/other"))]),
            }),
          ),
          (
            "otherPlugin".to_string(),
            ConfigMapValue::PluginConfig(RawPluginConfig {
              locked: false,
              associations: None,
              overrides: Vec::new(),
              properties: ConfigKeyMap::from([(String::from("value"), ConfigKeyValue::from_str("/dir/origin"))]),
            }),
          ),
          (
            "final".to_string(),
            ConfigMapValue::PluginConfig(RawPluginConfig {
              locked: false,
              associations: None,
              overrides: Vec::new(),
              properties: ConfigKeyMap::from([(String::from("value"), ConfigKeyValue::from_str("/dir/final && ${configDir}/escaped"))]),
            }),
          )
        ])
      );
    });
  }

  #[test]
  fn should_error_remote_config_file_with_config_dir() {
    let environment = TestEnvironment::new();
    environment.add_remote_file(
      "https://dprint.dev/test.json",
      r#"{
      "plugin": {
        "value": "${configDir}/test"
      }
}"#
        .as_bytes(),
    );

    environment.clone().run_in_runtime(async move {
      let result = get_result("https://dprint.dev/test.json", &environment).await.err().unwrap();
      assert_eq!(
        result.to_string(),
        "Cannot use ${configDir} template in remote configuration files. Maybe use ${originConfigDir} instead?\n    at https://dprint.dev/test.json"
      );
    });
  }

  #[test]
  fn should_error_remote_origin_config_file_with_config_dir() {
    let environment = TestEnvironment::new();
    environment.add_remote_file(
      "https://dprint.dev/test.json",
      r#"{
      "extends": "./next.json",
      "otherPlugin": {
        "value": "test"
      }
}"#
        .as_bytes(),
    );
    environment.add_remote_file(
      "https://dprint.dev/next.json",
      r#"{
      "final": {
        "value": "${originConfigDir}/final"
      }
}"#
        .as_bytes(),
    );

    environment.clone().run_in_runtime(async move {
      let result = get_result("https://dprint.dev/test.json", &environment).await.err().unwrap();
      assert_eq!(
        result.to_string(),
        "Cannot use ${originConfigDir} template when the origin configuration file (https://dprint.dev/test.json) is remote.\n    at https://dprint.dev/next.json"
      );
    });
  }

  #[test]
  fn should_error_unknown_template() {
    let environment = TestEnvironment::new();
    environment.add_remote_file(
      "https://dprint.dev/test.json",
      r#"{
      "plugin": {
        "value": "${unknown}/test"
      }
}"#
        .as_bytes(),
    );

    environment.clone().run_in_runtime(async move {
      let result = get_result("https://dprint.dev/test.json", &environment).await.err().unwrap();
      assert_eq!(
        result.to_string(),
        concat!(
          "Unknown template literal ${unknown}. Only ${configDir} and ${originConfigDir} are supported. If you meant to pass this to a plugin, escape the dollar sign with two back slashes.\n",
          "    at https://dprint.dev/test.json"
        ),
      );
    });
  }
}
