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

use anyhow::Result;
use anyhow::bail;
use dprint_core::configuration::ConfigKeyValue;

use super::ConfigMap;
use super::ConfigMapValue;
use super::RawPluginConfigOverride;
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

/// What remote configuration files specified for the exec plugin while
/// resolving a configuration. It's set aside rather than merged until the
/// local configuration has said what it allows.
#[derive(Default)]
pub struct RemoteExec {
  /// The commands of the highest precedence remote configuration that has
  /// some, when no higher precedence configuration has any.
  commands: Option<RemoteValue<Vec<ConfigKeyValue>>>,
  /// The working directory of the commands, taken like `commands`. It decides
  /// what a command with a relative path runs, including local commands.
  cwd: Option<RemoteValue<ConfigKeyValue>>,
  /// The overrides of each remote configuration, which can have commands and
  /// a working directory too.
  overrides: Vec<RemoteOverrides>,
  /// References to the exec plugin in remote configuration.
  plugins: Vec<PluginSourceReference>,
}

struct RemoteValue<T> {
  value: T,
  source: String,
}

struct RemoteOverrides {
  overrides: Vec<RawPluginConfigOverride>,
  /// How many exec overrides the configuration of higher precedence had when
  /// these were taken. Lower precedence overrides are merged in before the
  /// existing ones, so these go back in before that many last ones.
  higher_precedence_count: usize,
  source: String,
}

enum Policy {
  None,
  AnyProgram,
  Programs(Vec<String>),
}

/// The remote commands a policy ignored, by the configuration they're from.
#[derive(Default)]
struct IgnoredCommands(Vec<IgnoredSourceCommands>);

struct IgnoredSourceCommands {
  source: String,
  count: usize,
  /// The programs they run that aren't allowed, without duplicates.
  programs: Vec<String>,
}

impl IgnoredCommands {
  fn add(&mut self, source: &str, count: usize, programs: impl IntoIterator<Item = String>) {
    let index = match self.0.iter().position(|ignored| ignored.source == source) {
      Some(index) => index,
      None => {
        self.0.push(IgnoredSourceCommands {
          source: source.to_string(),
          count: 0,
          programs: Vec::new(),
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
      let source = source.display();
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
          source
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
        && let ConfigKeyValue::Array(commands) = commands
      {
        self.commands = Some(RemoteValue {
          value: commands,
          source: source.clone(),
        });
      }
      if let Some(cwd) = exec_config.properties.shift_remove(CWD_KEY)
        && self.cwd.is_none()
        && !has_higher_precedence(CWD_KEY)
      {
        self.cwd = Some(RemoteValue {
          value: cwd,
          source: source.clone(),
        });
      }
      if !exec_config.overrides.is_empty() {
        self.overrides.push(RemoteOverrides {
          overrides: std::mem::take(&mut exec_config.overrides),
          higher_precedence_count: resolved_exec_config.map(|exec_config| exec_config.overrides.len()).unwrap_or(0),
          source,
        });
      }
    }
    let (exec_plugins, other_plugins) = plugins.into_iter().partition(|plugin| is_builtin_exec_reference(environment, plugin));
    self.plugins.extend::<Vec<_>>(exec_plugins);
    other_plugins
  }

  /// Adds the remote exec commands, working directory, overrides and plugin
  /// references the local configuration allows to the resolved configuration.
  pub fn apply(self, config_map: &mut ConfigMap, plugins: &mut Vec<PluginSourceReference>, environment: &impl Environment) -> Result<()> {
    let policy = take_policy(config_map)?;
    let mut ignored_commands = IgnoredCommands::default();
    let mut ignored_cwd_sources = Vec::new();

    if let Some(remote) = self.commands {
      let commands = allowed_commands(remote.value, &policy, &remote.source, &mut ignored_commands);
      if !commands.is_empty()
        && let Some(ConfigMapValue::PluginConfig(exec_config)) = config_map.get_mut(EXEC_CONFIG_KEY)
      {
        // these have precedence over any commands of lower precedence local configuration
        exec_config.properties.insert(COMMANDS_KEY.to_string(), ConfigKeyValue::Array(commands));
      }
    }
    if let Some(remote) = self.cwd {
      if matches!(policy, Policy::AnyProgram) {
        if let Some(ConfigMapValue::PluginConfig(exec_config)) = config_map.get_mut(EXEC_CONFIG_KEY) {
          exec_config.properties.insert(CWD_KEY.to_string(), remote.value);
        }
      } else {
        ignored_cwd_sources.push(remote.source);
      }
    }
    // the lowest precedence ones first, so the count of higher precedence
    // overrides after them stays right for the others
    for remote in self.overrides.into_iter().rev() {
      let overrides = remote
        .overrides
        .into_iter()
        .filter_map(|mut override_config| {
          if let Some(commands) = override_config.properties.shift_remove(COMMANDS_KEY) {
            let commands = match (commands, &policy) {
              // as is, so the plugin reports what's wrong with it
              (commands, Policy::AnyProgram) => Some(commands),
              (ConfigKeyValue::Array(commands), _) => {
                let commands = allowed_commands(commands, &policy, &remote.source, &mut ignored_commands);
                (!commands.is_empty()).then_some(ConfigKeyValue::Array(commands))
              }
              _ => None,
            };
            if let Some(commands) = commands {
              override_config.properties.insert(COMMANDS_KEY.to_string(), commands);
            }
          }
          if let Some(cwd) = override_config.properties.shift_remove(CWD_KEY) {
            if matches!(policy, Policy::AnyProgram) {
              override_config.properties.insert(CWD_KEY.to_string(), cwd);
            } else if !ignored_cwd_sources.contains(&remote.source) {
              ignored_cwd_sources.push(remote.source.clone());
            }
          }
          (!override_config.properties.is_empty()).then_some(override_config)
        })
        .collect::<Vec<_>>();
      if overrides.is_empty() {
        continue;
      }
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
          log_warn!(
            environment,
            "Note: Ignored {} exec command(s) in remote configuration ({}) that run programs not listed in \"{}\": {}",
            ignored.count,
            ignored.source,
            PLAY_WITH_FIRE_KEY,
            ignored.programs.join(", "),
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
    for source in ignored_cwd_sources {
      log_warn!(
        environment,
        concat!(
          "Note: The exec \"{}\" in remote configuration ({}) is ignored for security reasons, as it decides what commands run. ",
          "To use it, specify \"{}\": true in the exec configuration of a local configuration file."
        ),
        CWD_KEY,
        source,
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
      } else {
        plugins.extend(self.plugins.into_iter().next());
      }
    }
    Ok(())
  }
}

/// The remote commands the policy allows. The others are added to `ignored`.
fn allowed_commands(commands: Vec<ConfigKeyValue>, policy: &Policy, source: &str, ignored: &mut IgnoredCommands) -> Vec<ConfigKeyValue> {
  match policy {
    Policy::None => {
      if !commands.is_empty() {
        ignored.add(source, commands.len(), []);
      }
      Vec::new()
    }
    Policy::AnyProgram => commands,
    Policy::Programs(programs) => {
      let (allowed, not_allowed): (Vec<_>, Vec<_>) = commands.into_iter().partition(|command| command_programs_allowed(command, programs));
      let not_allowed_programs = not_allowed
        .iter()
        .flat_map(command_programs)
        .filter(|program| !is_allowed(program, programs))
        .collect::<Vec<_>>();
      if !not_allowed.is_empty() {
        ignored.add(source, not_allowed.len(), not_allowed_programs);
      }
      allowed
    }
  }
}

/// Takes the policy out of the exec configuration, which by now only has what
/// local configuration files specified.
fn take_policy(config_map: &mut ConfigMap) -> Result<Policy> {
  let Some(ConfigMapValue::PluginConfig(exec_config)) = config_map.get_mut(EXEC_CONFIG_KEY) else {
    return Ok(Policy::None);
  };
  Ok(match exec_config.properties.shift_remove(PLAY_WITH_FIRE_KEY) {
    None | Some(ConfigKeyValue::Bool(false)) => Policy::None,
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
  })
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

fn command_programs_allowed(command: &ConfigKeyValue, programs: &[String]) -> bool {
  let ConfigKeyValue::Object(object) = command else {
    return false;
  };
  // a command must say what it runs to be checked
  matches!(object.get("command"), Some(ConfigKeyValue::String(_))) && command_programs(command).iter().all(|program| is_allowed(program, programs))
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
