//! The exec plugin's commands in remote configuration.
//!
//! A remote configuration file (ex. one `extends` refers to by url) can't add
//! process plugins or `includes`, because it could then make dprint run or
//! touch anything. The exec plugin's commands are the same thing: they run
//! programs. So the exec commands of a remote configuration only run when a
//! local configuration file allows it in its exec configuration, either with
//! `"playWithFire": true` for any program, or with a list of the programs
//! remote commands may run (ex. `"playWithFire": ["tombi", "rustfmt"]`).
//! That includes the commands in its overrides. Its working directory (`cwd`)
//! decides what a command with a relative path runs, local commands included,
//! so it's only used with `"playWithFire": true`.
//!
//! This is written for the exec plugin's 0.7.3 configuration, so it fails
//! closed: of the other remote exec properties, at the root or in an override,
//! only the ones 0.7.3 has that don't decide what runs are used without
//! `"playWithFire": true` (any others might, ex. ones a later version adds),
//! and a remote command with properties 0.7.3 doesn't have only runs with it.
//!
//! A nested configuration that inherits its ancestor's configuration and
//! specifies `"playWithFire"` itself only runs the remote commands it inherits
//! that its own `"playWithFire"` allows.

use anyhow::Result;
use anyhow::bail;
use dprint_core::configuration::ConfigKeyValue;
use indexmap::IndexMap;

use super::ConfigMap;
use super::ConfigMapValue;
use super::PluginConfiguration;
use super::RawPluginConfigOverride;
use super::ValueOrigin;
use crate::environment::Environment;
use crate::plugins::PluginSourceReference;
use crate::plugins::exec_command_program;
use crate::plugins::is_builtin_exec_reference;
use crate::plugins::is_exec_plugin_reference;
use crate::utils::PathSource;

const EXEC_CONFIG_KEY: &str = "exec";
const COMMANDS_KEY: &str = "commands";
const PLAY_WITH_FIRE_KEY: &str = "playWithFire";
const CWD_KEY: &str = "cwd";
/// The exec plugin 0.7.3 properties that don't decide what runs, which remote
/// configuration may specify without `"playWithFire": true`.
const UNRESTRICTED_KEYS: &[&str] = &["lineWidth", "indentWidth", "useTabs", "cacheKey", "timeout", "setupTimeout"];
/// The properties of an exec plugin 0.7.3 command.
const COMMAND_KEYS: &[&str] = &["command", "exts", "fileNames", "associations", "stdin", "cwd", "cacheKeyFiles", "setupCommand"];

/// What remote configuration files specified for the exec plugin while
/// resolving a configuration. It's set aside rather than merged until the
/// local configuration has said what it allows.
#[derive(Default)]
pub struct RemoteExec {
  /// The commands of the highest precedence remote configuration that has
  /// some, when no higher precedence configuration has any. Even when they
  /// aren't an array, as they still take precedence over lower ones.
  commands: Option<RemoteValue<ConfigKeyValue>>,
  /// The other properties that may decide what runs, by name, each taken like
  /// `commands`: the working directory of the commands (`cwd`), which decides
  /// what a command with a relative path runs, including local commands, and
  /// any property the exec plugin 0.7.3 doesn't have.
  restricted: IndexMap<String, RemoteValue<ConfigKeyValue>>,
  /// The overrides of each remote configuration, which can have commands and
  /// a working directory too.
  overrides: Vec<RemoteOverrides>,
  /// References to the exec plugin in remote configuration.
  plugins: Vec<PluginSourceReference>,
}

struct RemoteValue<T> {
  value: T,
  source: PathSource,
}

struct RemoteOverrides {
  overrides: Vec<RawPluginConfigOverride>,
  /// How many exec overrides the configuration of higher precedence had when
  /// these were taken. Lower precedence overrides are merged in before the
  /// existing ones, so these go back in before that many last ones.
  higher_precedence_count: usize,
  source: PathSource,
}

/// The remote commands `"playWithFire"` allows.
#[derive(Clone, Debug, PartialEq)]
enum Policy {
  None,
  AnyProgram,
  Programs(Vec<String>),
}

/// What remote configuration added to a resolved exec configuration, and the
/// `"playWithFire"` that allowed it. A nested configuration that inherits the
/// resolved one applies its own `"playWithFire"` to what it inherits of it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RemoteExecProvenance {
  /// What local configuration files specified for `"playWithFire"`, if they did.
  policy: Option<Policy>,
  /// The remote commands, as they were added.
  commands: Option<ConfigKeyValue>,
  /// The remote properties that may decide what runs (ex. `cwd`), as they
  /// were added.
  restricted: IndexMap<String, ConfigKeyValue>,
  /// The remote overrides, as they were added.
  overrides: Vec<RawPluginConfigOverride>,
  /// The remote exec plugin, when it was added.
  plugin: Option<PluginSourceReference>,
}

/// The remote commands a policy ignored, by the configuration they're from.
#[derive(Default)]
struct IgnoredCommands(Vec<IgnoredSourceCommands>);

struct IgnoredSourceCommands {
  source: String,
  count: usize,
  /// The programs they run that aren't allowed, without duplicates.
  programs: Vec<String>,
  /// Their properties the exec plugin 0.7.3 doesn't have, without duplicates.
  properties: Vec<String>,
}

/// The remote properties that may decide what runs a policy ignored, as the
/// configuration they're from and their name.
#[derive(Default)]
struct IgnoredProperties(Vec<(String, String)>);

impl IgnoredProperties {
  fn add(&mut self, source: &str, key: &str) {
    if !self
      .0
      .iter()
      .any(|(ignored_source, ignored_key)| ignored_source == source && ignored_key == key)
    {
      self.0.push((source.to_string(), key.to_string()));
    }
  }
}

impl IgnoredCommands {
  fn add(&mut self, source: &str, count: usize, programs: impl IntoIterator<Item = String>, properties: impl IntoIterator<Item = String>) {
    let index = match self.0.iter().position(|ignored| ignored.source == source) {
      Some(index) => index,
      None => {
        self.0.push(IgnoredSourceCommands {
          source: source.to_string(),
          count: 0,
          programs: Vec::new(),
          properties: Vec::new(),
        });
        self.0.len() - 1
      }
    };
    let ignored = &mut self.0[index];
    ignored.count += count;
    for program in programs {
      if !ignored.programs.contains(&program) {
        ignored.programs.push(program);
      }
    }
    for property in properties {
      if !ignored.properties.contains(&property) {
        ignored.properties.push(property);
      }
    }
  }
}

impl RemoteExec {
  /// Takes the exec commands, working directory, overrides and plugin
  /// references out of a remote configuration file. `resolved` is the
  /// configuration of higher precedence resolved so far. Returns the other
  /// plugins.
  pub fn take_from_remote_config(
    &mut self,
    config_map: &mut ConfigMap,
    plugins: Vec<PluginSourceReference>,
    resolved: &ConfigMap,
    source: &PathSource,
    environment: &impl Environment,
  ) -> Vec<PluginSourceReference> {
    if let Some(ConfigMapValue::PluginConfig(exec_config)) = config_map.get_mut(EXEC_CONFIG_KEY) {
      // a remote configuration can't allow itself to run commands
      let mut has_play_with_fire = exec_config.properties.shift_remove(PLAY_WITH_FIRE_KEY).is_some();
      for override_config in &mut exec_config.overrides {
        has_play_with_fire |= override_config.properties.shift_remove(PLAY_WITH_FIRE_KEY).is_some();
      }
      if has_play_with_fire {
        log_warn!(
          environment,
          "Note: \"{}\" is ignored in remote configuration ({}). Specify it in a local configuration file.",
          PLAY_WITH_FIRE_KEY,
          source.display()
        );
      }
      let resolved_exec_config = match resolved.get(EXEC_CONFIG_KEY) {
        Some(ConfigMapValue::PluginConfig(exec_config)) => Some(exec_config),
        _ => None,
      };
      let has_higher_precedence = |key: &str| resolved_exec_config.is_some_and(|exec_config| exec_config.properties.contains_key(key));
      if let Some(commands) = exec_config.properties.shift_remove(COMMANDS_KEY)
        && self.commands.is_none()
        && !has_higher_precedence(COMMANDS_KEY)
      {
        self.commands = Some(RemoteValue {
          value: commands,
          source: source.clone(),
        });
      }
      let restricted_keys = exec_config.properties.keys().filter(|key| !is_unrestricted(key)).cloned().collect::<Vec<_>>();
      for key in restricted_keys {
        let value = exec_config.properties.shift_remove(&key).unwrap();
        if !self.restricted.contains_key(&key) && !has_higher_precedence(&key) {
          self.restricted.insert(key, RemoteValue { value, source: source.clone() });
        }
      }
      if !exec_config.overrides.is_empty() {
        let mut overrides = std::mem::take(&mut exec_config.overrides);
        for override_config in &mut overrides {
          override_config.origin = ValueOrigin(Some(source.clone()));
        }
        self.overrides.push(RemoteOverrides {
          overrides,
          higher_precedence_count: resolved_exec_config.map(|exec_config| exec_config.overrides.len()).unwrap_or(0),
          source: source.clone(),
        });
      }
    }
    let (exec_plugins, other_plugins) = plugins.into_iter().partition(|plugin| is_builtin_exec_reference(environment, plugin));
    self.plugins.extend::<Vec<_>>(exec_plugins);
    other_plugins
  }

  /// Adds the remote exec commands, working directory, overrides and plugin
  /// references the local configuration allows to the resolved configuration,
  /// and records what was added (see `PluginConfiguration::remote_exec`).
  pub fn apply(self, plugin_configuration: &mut PluginConfiguration, environment: &impl Environment) -> Result<()> {
    let PluginConfiguration {
      sources: plugins,
      config: config_map,
      origins,
      remote_exec,
    } = plugin_configuration;
    let specified_policy = take_policy(config_map)?;
    let policy = specified_policy.clone().unwrap_or(Policy::None);
    let mut provenance = RemoteExecProvenance {
      policy: specified_policy,
      ..Default::default()
    };
    let mut ignored_commands = IgnoredCommands::default();
    let mut ignored_properties = IgnoredProperties::default();

    if let Some(remote) = self.commands
      && let Some(commands) = allowed_commands_value(remote.value, &policy, &remote.source.display(), &mut ignored_commands)
      && let Some(ConfigMapValue::PluginConfig(exec_config)) = config_map.get_mut(EXEC_CONFIG_KEY)
    {
      // these have precedence over any commands of lower precedence local configuration
      exec_config.properties.insert(COMMANDS_KEY.to_string(), commands.clone());
      origins.set_plugin_property(EXEC_CONFIG_KEY, COMMANDS_KEY, &remote.source);
      provenance.commands = Some(commands);
    }
    for (key, remote) in self.restricted {
      if matches!(policy, Policy::AnyProgram) {
        if let Some(ConfigMapValue::PluginConfig(exec_config)) = config_map.get_mut(EXEC_CONFIG_KEY) {
          exec_config.properties.insert(key.clone(), remote.value.clone());
          origins.set_plugin_property(EXEC_CONFIG_KEY, &key, &remote.source);
          provenance.restricted.insert(key, remote.value);
        }
      } else {
        ignored_properties.add(&remote.source.display(), &key);
      }
    }
    // the lowest precedence ones first, so the count of higher precedence
    // overrides after them stays right for the others
    for remote in self.overrides.into_iter().rev() {
      let overrides = remote
        .overrides
        .into_iter()
        .filter_map(|override_config| {
          allowed_override(
            override_config,
            &policy,
            &remote.source.display(),
            &mut ignored_commands,
            &mut ignored_properties,
          )
        })
        .collect::<Vec<_>>();
      if overrides.is_empty() {
        continue;
      }
      for override_config in &overrides {
        for property in override_config.properties.keys() {
          origins.add_plugin_property(EXEC_CONFIG_KEY, property, &remote.source);
        }
      }
      origins.add_root_property(EXEC_CONFIG_KEY, &remote.source);
      provenance.overrides.extend(overrides.iter().cloned());
      let exec_config = config_map
        .entry(EXEC_CONFIG_KEY.to_string())
        .or_insert_with(|| ConfigMapValue::PluginConfig(Default::default()));
      if let ConfigMapValue::PluginConfig(exec_config) = exec_config {
        let index = exec_config.overrides.len().saturating_sub(remote.higher_precedence_count);
        exec_config.overrides.splice(index..index, overrides);
      }
    }

    for ignored in ignored_commands.0 {
      match &policy {
        Policy::Programs(_) => {
          let mut reasons = Vec::new();
          if !ignored.programs.is_empty() {
            reasons.push(format!(
              "run programs not listed in \"{}\": {}",
              PLAY_WITH_FIRE_KEY,
              ignored.programs.join(", ")
            ));
          }
          if !ignored.properties.is_empty() {
            reasons.push(format!(
              "have properties the exec plugin 0.7.3 doesn't, which only run with \"{}\": true: {}",
              PLAY_WITH_FIRE_KEY,
              ignored.properties.join(", ")
            ));
          }
          log_warn!(
            environment,
            "Note: Ignored {} exec command(s) in remote configuration ({}) that {}",
            ignored.count,
            ignored.source,
            reasons.join(", or that "),
          );
        }
        _ => log_warn!(
          environment,
          concat!(
            "Note: The exec commands in remote configuration ({}) are ignored for security reasons. ",
            "To run them, specify \"{}\" in the exec configuration of a local configuration file ",
            "(`true` or the programs they may run)."
          ),
          ignored.source,
          PLAY_WITH_FIRE_KEY,
        ),
      }
    }
    for (source, key) in ignored_properties.0 {
      log_warn!(
        environment,
        concat!(
          "Note: The exec \"{}\" in remote configuration ({}) is ignored for security reasons, as {}. ",
          "To use it, specify \"{}\": true in the exec configuration of a local configuration file."
        ),
        key,
        source,
        if key == CWD_KEY {
          "it decides what commands run"
        } else {
          "the exec plugin 0.7.3 doesn't have it, so dprint can't tell what it does"
        },
        PLAY_WITH_FIRE_KEY,
      );
    }

    // the exec plugin of remote configuration only matters when the local
    // configuration doesn't use exec (in any version), and only one of it is
    // added
    let uses_exec = plugins.iter().any(is_exec_plugin_reference);
    if !self.plugins.is_empty() && !uses_exec {
      if matches!(policy, Policy::None) {
        log_warn!(
          environment,
          concat!(
            "Note: The exec plugin in remote configuration is ignored for security reasons. ",
            "To use it, specify \"{}\" in the exec configuration of a local configuration file ",
            "(`true` or the programs its commands may run)."
          ),
          PLAY_WITH_FIRE_KEY,
        );
      } else if !has_commands(config_map) {
        // without any, its configuration would only be an error
        log_warn!(
          environment,
          "Note: The exec plugin in remote configuration is ignored, as none of the exec commands are allowed by \"{}\".",
          PLAY_WITH_FIRE_KEY,
        );
      } else {
        provenance.plugin = self.plugins.into_iter().next();
        plugins.extend(provenance.plugin.clone());
      }
    }
    *remote_exec = provenance;
    Ok(())
  }
}

impl RemoteExecProvenance {
  /// Applies a nested configuration's own `"playWithFire"` to what remote
  /// configuration added to the configuration it inherits (`config_map` and
  /// `plugins`), and says what of that is left.
  pub fn filter_inherited(&self, nested: &RemoteExecProvenance, config_map: &mut ConfigMap, plugins: &mut Vec<PluginSourceReference>) -> RemoteExecProvenance {
    let Some(policy) = &nested.policy else {
      // the nested configuration goes by what its ancestor allowed
      return self.clone();
    };
    let mut left = RemoteExecProvenance {
      policy: self.policy.clone(),
      ..Default::default()
    };
    // what's ignored here was already noted for the ancestor
    let mut ignored_commands = IgnoredCommands::default();
    let mut ignored_properties = IgnoredProperties::default();
    if let Some(ConfigMapValue::PluginConfig(exec_config)) = config_map.get_mut(EXEC_CONFIG_KEY) {
      // the remote values are found by what they are, which is what local
      // values of higher precedence would have replaced
      if let Some(commands) = &self.commands
        && exec_config.properties.get(COMMANDS_KEY) == Some(commands)
      {
        exec_config.properties.shift_remove(COMMANDS_KEY);
        if let Some(commands) = allowed_commands_value(commands.clone(), policy, "", &mut ignored_commands) {
          exec_config.properties.insert(COMMANDS_KEY.to_string(), commands.clone());
          left.commands = Some(commands);
        }
      }
      for (key, value) in &self.restricted {
        if exec_config.properties.get(key) != Some(value) {
          continue;
        }
        if matches!(policy, Policy::AnyProgram) {
          left.restricted.insert(key.clone(), value.clone());
        } else {
          exec_config.properties.shift_remove(key);
        }
      }
      for override_config in &self.overrides {
        let Some(index) = exec_config.overrides.iter().position(|existing| existing == override_config) else {
          continue;
        };
        match allowed_override(override_config.clone(), policy, "", &mut ignored_commands, &mut ignored_properties) {
          Some(override_config) => {
            exec_config.overrides[index] = override_config.clone();
            left.overrides.push(override_config);
          }
          None => {
            exec_config.overrides.remove(index);
          }
        }
      }
    }
    if let Some(plugin) = &self.plugin
      && let Some(index) = plugins.iter().position(|existing| existing == plugin)
    {
      if matches!(policy, Policy::None) {
        plugins.remove(index);
      } else {
        left.plugin = Some(plugin.clone());
      }
    }
    left
  }

  /// Adds what a nested configuration inherits (see `filter_inherited`).
  pub fn inherit(&mut self, inherited: RemoteExecProvenance) {
    if self.policy.is_none() {
      self.policy = inherited.policy;
    }
    if self.commands.is_none() {
      self.commands = inherited.commands;
    }
    for (key, value) in inherited.restricted {
      self.restricted.entry(key).or_insert(value);
    }
    self.overrides.extend(inherited.overrides);
    if self.plugin.is_none() {
      self.plugin = inherited.plugin;
    }
  }

  /// Leaves out the remote exec plugin a nested configuration inherited when
  /// none of the commands are left for it to run.
  pub fn remove_unused_plugin(&mut self, config_map: &ConfigMap, plugins: &mut Vec<PluginSourceReference>) {
    if let Some(plugin) = &self.plugin
      && !has_commands(config_map)
    {
      plugins.retain(|existing| existing != plugin);
      self.plugin = None;
    }
  }
}

/// Whether the exec configuration has commands.
fn has_commands(config_map: &ConfigMap) -> bool {
  matches!(config_map.get(EXEC_CONFIG_KEY), Some(ConfigMapValue::PluginConfig(exec_config)) if exec_config.properties.contains_key(COMMANDS_KEY))
}

/// A remote override with what the policy allows of its commands and the
/// other properties that may decide what runs, or `None` when nothing is
/// left of it.
fn allowed_override(
  mut override_config: RawPluginConfigOverride,
  policy: &Policy,
  source: &str,
  ignored_commands: &mut IgnoredCommands,
  ignored_properties: &mut IgnoredProperties,
) -> Option<RawPluginConfigOverride> {
  if let Some(commands) = override_config.properties.shift_remove(COMMANDS_KEY)
    && let Some(commands) = allowed_commands_value(commands, policy, source, ignored_commands)
  {
    override_config.properties.insert(COMMANDS_KEY.to_string(), commands);
  }
  if !matches!(policy, Policy::AnyProgram) {
    override_config.properties.retain(|key, _| {
      let allowed = is_unrestricted(key);
      if !allowed {
        ignored_properties.add(source, key);
      }
      allowed
    });
  }
  (!override_config.properties.is_empty()).then_some(override_config)
}

/// Whether remote configuration may specify an exec property without
/// `"playWithFire": true`. `commands` is checked on its own.
fn is_unrestricted(key: &str) -> bool {
  key == COMMANDS_KEY || UNRESTRICTED_KEYS.contains(&key)
}

/// The remote `commands` value the policy allows, if any. A value that isn't
/// an array runs nothing, so unless remote commands are ignored altogether, it
/// stays for the exec plugin to report what's wrong with it.
fn allowed_commands_value(commands: ConfigKeyValue, policy: &Policy, source: &str, ignored: &mut IgnoredCommands) -> Option<ConfigKeyValue> {
  match (commands, policy) {
    (ConfigKeyValue::Array(commands), _) => {
      let commands = allowed_commands(commands, policy, source, ignored);
      (!commands.is_empty()).then_some(ConfigKeyValue::Array(commands))
    }
    (_, Policy::None) => {
      ignored.add(source, 1, [], []);
      None
    }
    (commands, _) => Some(commands),
  }
}

/// The remote commands the policy allows. The others are added to `ignored`.
fn allowed_commands(commands: Vec<ConfigKeyValue>, policy: &Policy, source: &str, ignored: &mut IgnoredCommands) -> Vec<ConfigKeyValue> {
  match policy {
    Policy::None => {
      if !commands.is_empty() {
        ignored.add(source, commands.len(), [], []);
      }
      Vec::new()
    }
    Policy::AnyProgram => commands,
    Policy::Programs(programs) => {
      let (allowed, not_allowed): (Vec<_>, Vec<_>) = commands.into_iter().partition(|command| command_allowed(command, programs));
      let not_allowed_programs = not_allowed
        .iter()
        .flat_map(command_programs)
        .filter(|program| !is_allowed(program, programs))
        .collect::<Vec<_>>();
      let unknown_properties = not_allowed.iter().flat_map(unknown_command_keys).collect::<Vec<_>>();
      if !not_allowed.is_empty() {
        ignored.add(source, not_allowed.len(), not_allowed_programs, unknown_properties);
      }
      allowed
    }
  }
}

/// Takes the policy out of the exec configuration, which by now only has what
/// local configuration files specified. `None` when they didn't specify one.
fn take_policy(config_map: &mut ConfigMap) -> Result<Option<Policy>> {
  let Some(ConfigMapValue::PluginConfig(exec_config)) = config_map.get_mut(EXEC_CONFIG_KEY) else {
    return Ok(None);
  };
  Ok(Some(match exec_config.properties.shift_remove(PLAY_WITH_FIRE_KEY) {
    None => return Ok(None),
    Some(ConfigKeyValue::Bool(false)) => Policy::None,
    Some(ConfigKeyValue::Bool(true)) => Policy::AnyProgram,
    Some(ConfigKeyValue::Array(values)) => Policy::Programs(
      values
        .into_iter()
        .map(|value| match value {
          ConfigKeyValue::String(program) => Ok(program),
          _ => bail!("Expected the \"exec.{}\" programs to be strings.", PLAY_WITH_FIRE_KEY),
        })
        .collect::<Result<_>>()?,
    ),
    Some(_) => bail!("Expected \"exec.{}\" to be true, false, or an array of programs.", PLAY_WITH_FIRE_KEY),
  }))
}

/// The programs a command object runs: its command and its setup command.
fn command_programs(command: &ConfigKeyValue) -> Vec<String> {
  let ConfigKeyValue::Object(command) = command else {
    return Vec::new();
  };
  ["command", "setupCommand"]
    .into_iter()
    .filter_map(|key| match command.get(key) {
      Some(ConfigKeyValue::String(text)) => exec_command_program(text),
      _ => None,
    })
    .collect()
}

/// Whether a remote command only runs programs the list allows.
fn command_allowed(command: &ConfigKeyValue, programs: &[String]) -> bool {
  let ConfigKeyValue::Object(object) = command else {
    return false;
  };
  // a command must say what it runs to be checked, and other properties
  // than the exec plugin 0.7.3's might change what it runs
  matches!(object.get("command"), Some(ConfigKeyValue::String(_)))
    && unknown_command_keys(command).is_empty()
    && command_programs(command).iter().all(|program| is_allowed(program, programs))
}

/// The properties of a command the exec plugin 0.7.3 doesn't have.
fn unknown_command_keys(command: &ConfigKeyValue) -> Vec<String> {
  match command {
    ConfigKeyValue::Object(object) => object.keys().filter(|key| !COMMAND_KEYS.contains(&key.as_str())).cloned().collect(),
    _ => Vec::new(),
  }
}

fn is_allowed(program: &str, programs: &[String]) -> bool {
  programs.iter().any(|allowed| {
    if cfg!(windows) {
      allowed.eq_ignore_ascii_case(program)
    } else {
      allowed == program
    }
  })
}
