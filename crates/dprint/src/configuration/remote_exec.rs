//! The exec plugin's commands in remote configuration.
//!
//! A remote configuration file (ex. one `extends` refers to by url) can't add
//! process plugins or `includes`, because it could then make dprint run or
//! touch anything. The exec plugin's commands are the same thing: they run
//! programs. So the exec commands of a remote configuration only run when a
//! local configuration file allows it in its exec configuration, either with
//! `"playWithFire": true` for any program, or with a list of the programs
//! remote commands may run (ex. `"playWithFire": ["tombi", "rustfmt"]`).

use anyhow::Result;
use anyhow::bail;
use dprint_core::configuration::ConfigKeyValue;

use super::ConfigMap;
use super::ConfigMapValue;
use crate::environment::Environment;
use crate::plugins::PluginSourceReference;
use crate::plugins::exec_command_program;
use crate::plugins::is_builtin_exec_reference;
use crate::utils::PathSource;

const EXEC_CONFIG_KEY: &str = "exec";
const COMMANDS_KEY: &str = "commands";
const PLAY_WITH_FIRE_KEY: &str = "playWithFire";

/// What remote configuration files specified for the exec plugin while
/// resolving a configuration. It's set aside rather than merged until the
/// local configuration has said what it allows.
#[derive(Default)]
pub struct RemoteExec {
  /// The commands of the highest precedence remote configuration that has
  /// some, when no higher precedence configuration has any.
  commands: Option<RemoteCommands>,
  /// References to the exec plugin in remote configuration.
  plugins: Vec<PluginSourceReference>,
}

struct RemoteCommands {
  commands: Vec<ConfigKeyValue>,
  source: String,
}

enum Policy {
  None,
  AnyProgram,
  Programs(Vec<String>),
}

impl RemoteExec {
  /// Takes the exec commands and plugin references out of a remote
  /// configuration file. `resolved` is the configuration of higher precedence
  /// resolved so far. Returns the other plugins.
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
      if exec_config.properties.shift_remove(PLAY_WITH_FIRE_KEY).is_some() {
        log_warn!(
          environment,
          "Note: \"{}\" is ignored in remote configuration ({}). Specify it in a local configuration file.",
          PLAY_WITH_FIRE_KEY,
          source.display()
        );
      }
      if let Some(commands) = exec_config.properties.shift_remove(COMMANDS_KEY) {
        let has_higher_precedence_commands = self.commands.is_some() || exec_commands(resolved).is_some();
        if !has_higher_precedence_commands && let ConfigKeyValue::Array(commands) = commands {
          self.commands = Some(RemoteCommands {
            commands,
            source: source.display(),
          });
        }
      }
    }
    let (exec_plugins, other_plugins) = plugins.into_iter().partition(|plugin| is_builtin_exec_reference(environment, plugin));
    self.plugins.extend::<Vec<_>>(exec_plugins);
    other_plugins
  }

  /// Adds the remote exec commands and plugin references the local
  /// configuration allows to the resolved configuration.
  pub fn apply(self, config_map: &mut ConfigMap, plugins: &mut Vec<PluginSourceReference>, environment: &impl Environment) -> Result<()> {
    let policy = take_policy(config_map)?;
    if let Some(remote) = self.commands {
      let commands = match &policy {
        Policy::None => {
          log_warn!(
            environment,
            concat!(
              "Note: The exec commands in remote configuration ({}) are ignored for security reasons. ",
              "To run them, specify \"{}\" in the exec configuration of a local configuration file ",
              "(`true` or the programs they may run)."
            ),
            remote.source,
            PLAY_WITH_FIRE_KEY,
          );
          Vec::new()
        }
        Policy::AnyProgram => remote.commands,
        Policy::Programs(programs) => {
          let (allowed, not_allowed): (Vec<_>, Vec<_>) = remote.commands.into_iter().partition(|command| command_programs_allowed(command, programs));
          if !not_allowed.is_empty() {
            let mut not_allowed_programs = Vec::new();
            for program in not_allowed.iter().flat_map(command_programs) {
              if !is_allowed(&program, programs) && !not_allowed_programs.contains(&program) {
                not_allowed_programs.push(program);
              }
            }
            log_warn!(
              environment,
              "Note: Ignored {} exec command(s) in remote configuration ({}) that run programs not listed in \"{}\": {}",
              not_allowed.len(),
              remote.source,
              PLAY_WITH_FIRE_KEY,
              not_allowed_programs.join(", "),
            );
          }
          allowed
        }
      };
      if !commands.is_empty()
        && let Some(ConfigMapValue::PluginConfig(exec_config)) = config_map.get_mut(EXEC_CONFIG_KEY)
      {
        // these have precedence over any commands of lower precedence local configuration
        exec_config.properties.insert(COMMANDS_KEY.to_string(), ConfigKeyValue::Array(commands));
      }
    }
    // the exec plugin of remote configuration only matters when the local
    // configuration doesn't use exec, and only one of it is added
    let uses_exec = plugins.iter().any(|plugin| is_builtin_exec_reference(environment, plugin));
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

fn exec_commands(config_map: &ConfigMap) -> Option<&ConfigKeyValue> {
  match config_map.get(EXEC_CONFIG_KEY) {
    Some(ConfigMapValue::PluginConfig(exec_config)) => exec_config.properties.get(COMMANDS_KEY),
    _ => None,
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
