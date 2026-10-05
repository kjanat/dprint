//! dprint's built-in exec: formats files with external commands.
//!
//! This is the dprint-plugin-exec process plugin
//! (https://github.com/dprint/dprint-plugin-exec, MIT licensed, see LICENSE)
//! running inside the CLI. Running a command needs no plugin boundary: as a
//! downloaded process plugin it only worked on platforms someone published a
//! build of it for (FreeBSD has none, for example), and every file went through
//! an extra process. Configs keep referencing the exec plugin as before; dprint
//! serves those references itself instead of downloading the plugin.

mod configuration;
mod executable;
mod handler;

use crate::environment::Environment;
use crate::plugins::Plugin;
use crate::plugins::PluginSourceReference;
use crate::utils::PathSource;

use super::in_process::InProcessPlugin;

pub const EXEC_PLUGIN_NAME: &str = "dprint-plugin-exec";
/// The dprint-plugin-exec release this was built from.
pub const EXEC_PLUGIN_VERSION: &str = "0.7.3";
/// The schema of the built-in exec's configuration. It describes what this
/// version accepts (ex. `playWithFire` and `setupTimeout`), which the schema
/// published with the exec plugin doesn't.
const EXEC_CONFIG_SCHEMA: &str = include_str!("schema.json");
/// The exec plugin versions the built-in exec serves references to. A config
/// that asks for another version (or no specific one) gets that version, run
/// as the process plugin, since the built-in exec only behaves like the
/// release it was built from. A version is only added here together with
/// tests that the built-in exec handles its configuration the same way.
const SERVED_EXEC_PLUGIN_VERSIONS: &[&str] = &[EXEC_PLUGIN_VERSION];
/// Set to `0` to download and run the exec process plugin instead.
const BUILTIN_EXEC_ENV_VAR: &str = "DPRINT_BUILTIN_EXEC";

/// Creates the built-in exec plugin when the reference is to a version of the
/// exec plugin it serves.
pub fn create_builtin_exec_plugin<TEnvironment: Environment>(environment: &TEnvironment, reference: &PluginSourceReference) -> Option<Box<dyn Plugin>> {
  if !is_builtin_exec_reference(environment, reference) {
    if is_exec_plugin_reference(reference) {
      log_debug!(
        environment,
        "Using the exec process plugin for {}: the built-in exec ({} {}) only serves references to version {}.",
        reference.display(),
        EXEC_PLUGIN_NAME,
        EXEC_PLUGIN_VERSION,
        SERVED_EXEC_PLUGIN_VERSIONS.join(", "),
      );
    }
    return None;
  }
  log_debug!(
    environment,
    "Using the built-in exec ({} {}) for {}",
    EXEC_PLUGIN_NAME,
    EXEC_PLUGIN_VERSION,
    reference.display()
  );
  Some(Box::new(InProcessPlugin::new(handler::ExecHandler::default, EXEC_CONFIG_SCHEMA)))
}

/// Whether dprint serves the reference with the built-in exec rather than
/// running the exec process plugin it refers to.
pub fn is_builtin_exec_reference<TEnvironment: Environment>(environment: &TEnvironment, reference: &PluginSourceReference) -> bool {
  exec_plugin_reference_version(reference).is_some_and(|version| SERVED_EXEC_PLUGIN_VERSIONS.contains(&version))
    && environment.env_var(BUILTIN_EXEC_ENV_VAR).is_none_or(|value| value != "0")
}

/// Whether the reference is to the exec plugin, in any version.
pub fn is_exec_plugin_reference(reference: &PluginSourceReference) -> bool {
  exec_plugin_reference(reference).is_some()
}

/// The exec plugin version the reference asks for, when it's to the exec
/// plugin and names one.
fn exec_plugin_reference_version(reference: &PluginSourceReference) -> Option<&str> {
  exec_plugin_reference(reference).flatten()
}

/// The program an exec command runs, split from its arguments the way exec does.
pub fn exec_command_program(command: &str) -> Option<String> {
  configuration::split_command(command).into_iter().next()
}

/// `Some` when the reference is to the exec plugin, with the version it asks
/// for when it names one.
fn exec_plugin_reference(reference: &PluginSourceReference) -> Option<Option<&str>> {
  match &reference.path_source {
    // ex. npm:@dprint/exec@0.7.3/plugin.json, or without a version for the one
    // installed in node_modules
    PathSource::Npm(npm) => (npm.specifier.name == "@dprint/exec").then_some(npm.specifier.version.as_deref()),
    PathSource::Remote(remote) => {
      let path = remote.url.path();
      match remote.url.host_str() {
        Some("plugins.dprint.dev") => {
          if let Some(version) = path.strip_prefix("/exec-").and_then(|rest| rest.strip_suffix(".json")) {
            // ex. https://plugins.dprint.dev/exec-0.5.0.json
            Some(Some(version))
          } else {
            // ex. https://plugins.dprint.dev/dprint/dprint-plugin-exec/0.7.3/plugin.json,
            // or latest.json for the latest release
            path.strip_prefix("/dprint/dprint-plugin-exec/").map(|rest| rest.strip_suffix("/plugin.json"))
          }
        }
        Some("github.com") => {
          // ex. https://github.com/dprint/dprint-plugin-exec/releases/download/0.5.0/plugin.json,
          // or releases/latest/download/plugin.json for the latest release
          let rest = path.strip_prefix("/dprint/dprint-plugin-exec/releases/")?;
          Some(rest.strip_prefix("download/").and_then(|rest| rest.strip_suffix("/plugin.json")))
        }
        _ => None,
      }
    }
    // a local exec plugin is most likely a build being worked on, so run it
    PathSource::Local(_) => None,
  }
}

#[cfg(test)]
mod test {
  use super::*;
  use crate::environment::TestEnvironment;

  fn parse_reference(text: &str, environment: &TestEnvironment) -> PluginSourceReference {
    let base = PathSource::new_local(crate::environment::CanonicalizedPathBuf::new_for_testing("/"));
    crate::plugins::parse_plugin_source_reference(text, &base, environment).unwrap()
  }

  #[test]
  fn recognizes_exec_plugin_references() {
    let environment = TestEnvironment::new();
    let is_exec = |text: &str| is_exec_plugin_reference(&parse_reference(text, &environment));
    assert!(is_exec("npm:@dprint/exec@0.7.3/plugin.json@abc"));
    assert!(is_exec("https://plugins.dprint.dev/exec-0.5.0.json@abc"));
    assert!(is_exec("https://plugins.dprint.dev/dprint/dprint-plugin-exec/0.7.3/plugin.json"));
    assert!(is_exec("https://github.com/dprint/dprint-plugin-exec/releases/download/0.5.0/plugin.json@abc"));
    assert!(!is_exec("npm:@dprint/typescript@0.96.1"));
    assert!(!is_exec("https://plugins.dprint.dev/typescript-0.96.1.wasm"));
    assert!(!is_exec("https://example.com/exec-0.5.0.json"));
  }

  #[cfg(unix)]
  #[test]
  fn formats_with_built_in_exec_without_downloading_the_plugin() {
    use crate::environment::TestEnvironmentBuilder;
    use crate::test_helpers::run_test_cli;

    // no plugin files are served, so this would fail if it tried to download
    let environment = TestEnvironmentBuilder::new()
      .with_default_config(|config_file| {
        config_file
          .add_plugin("npm:@dprint/exec@0.7.3/plugin.json@0000000000000000000000000000000000000000000000000000000000000000")
          .add_config_section("exec", r#"{ "commands": [{ "command": "tr a-z A-Z", "exts": ["txt"] }] }"#);
      })
      .write_file("/file.txt", "text\n")
      .build();
    run_test_cli(vec!["fmt", "/file.txt"], &environment).unwrap();
    assert_eq!(environment.read_file("/file.txt").unwrap(), "TEXT\n");
    assert_eq!(environment.take_stdout_messages(), vec![crate::test_helpers::get_singular_formatted_text()]);
  }

  #[test]
  fn the_schema_describes_the_configuration() {
    let schema: serde_json::Value = serde_json::from_str(EXEC_CONFIG_SCHEMA).unwrap();
    let keys = |value: &serde_json::Value| value.as_object().unwrap().keys().cloned().collect::<Vec<_>>();
    let command_schema = &schema["properties"]["commands"]["items"];
    assert_eq!(
      keys(&schema["properties"]),
      [
        "lineWidth",
        "indentWidth",
        "useTabs",
        "cacheKey",
        "cwd",
        "timeout",
        "setupTimeout",
        "playWithFire",
        "commands"
      ]
    );
    assert_eq!(
      keys(&command_schema["properties"]),
      ["command", "exts", "fileNames", "associations", "stdin", "cwd", "cacheKeyFiles", "setupCommand"]
    );

    // the configuration accepts all of them. `playWithFire` is read and
    // removed by dprint before the configuration gets here
    let config = serde_json::json!({
      "lineWidth": 100,
      "indentWidth": 4,
      "useTabs": true,
      "cacheKey": "1",
      "cwd": ".",
      "timeout": 60,
      "setupTimeout": 600,
      "commands": [{
        "command": "tr a-z A-Z",
        "exts": ["txt"],
        "fileNames": "README",
        "associations": "**/*.txt",
        "stdin": true,
        "cwd": ".",
        "cacheKeyFiles": ["./src/plugins/implementations/builtin_exec/testdata/one-line.txt"],
        "setupCommand": "true",
      }],
    });
    let result = configuration::Configuration::resolve(serde_json::from_value(config).unwrap(), &Default::default());
    assert_eq!(result.diagnostics, vec![]);
  }

  #[test]
  fn serves_only_references_to_the_version_it_was_built_from() {
    let environment = TestEnvironment::new();
    let is_builtin = |text: &str| is_builtin_exec_reference(&environment, &parse_reference(text, &environment));
    // the version the built-in exec was built from, however it's referenced
    assert_eq!(EXEC_PLUGIN_VERSION, "0.7.3");
    assert!(is_builtin("npm:@dprint/exec@0.7.3/plugin.json@abc"));
    assert!(is_builtin("https://plugins.dprint.dev/exec-0.7.3.json@abc"));
    assert!(is_builtin("https://plugins.dprint.dev/dprint/dprint-plugin-exec/0.7.3/plugin.json"));
    assert!(is_builtin(
      "https://github.com/dprint/dprint-plugin-exec/releases/download/0.7.3/plugin.json@abc"
    ));
    // other versions, older or newer, are what they ask for
    assert!(!is_builtin("npm:@dprint/exec@0.6.0/plugin.json@abc"));
    assert!(!is_builtin("npm:@dprint/exec@0.8.0/plugin.json@abc"));
    assert!(!is_builtin("https://plugins.dprint.dev/exec-0.5.0.json@abc"));
    assert!(!is_builtin("https://plugins.dprint.dev/exec-0.7.30.json@abc"));
    assert!(!is_builtin("https://plugins.dprint.dev/dprint/dprint-plugin-exec/0.8.0/plugin.json"));
    assert!(!is_builtin(
      "https://github.com/dprint/dprint-plugin-exec/releases/download/0.5.0/plugin.json@abc"
    ));
    // and so are references that don't name a version
    assert!(!is_builtin("npm:@dprint/exec"));
    assert!(!is_builtin("https://plugins.dprint.dev/dprint/dprint-plugin-exec/latest.json"));
    assert!(!is_builtin("https://github.com/dprint/dprint-plugin-exec/releases/latest/download/plugin.json"));
    // and they're still recognized as the exec plugin
    assert!(is_exec_plugin_reference(&parse_reference(
      "npm:@dprint/exec@0.8.0/plugin.json@abc",
      &environment
    )));
    assert!(is_exec_plugin_reference(&parse_reference("npm:@dprint/exec", &environment)));
    assert!(create_builtin_exec_plugin(&environment, &parse_reference("npm:@dprint/exec@0.8.0/plugin.json@abc", &environment)).is_none());
  }

  #[test]
  fn uses_the_process_plugin_when_disabled() {
    let environment = TestEnvironment::new();
    environment.set_env_var(BUILTIN_EXEC_ENV_VAR, Some("0"));
    let reference = parse_reference("npm:@dprint/exec@0.7.3/plugin.json", &environment);
    assert!(create_builtin_exec_plugin(&environment, &reference).is_none());
    environment.set_env_var(BUILTIN_EXEC_ENV_VAR, None);
    assert!(create_builtin_exec_plugin(&environment, &reference).is_some());
  }
}
