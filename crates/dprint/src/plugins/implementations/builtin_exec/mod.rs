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
/// Set to `0` to download and run the exec process plugin instead.
const BUILTIN_EXEC_ENV_VAR: &str = "DPRINT_BUILTIN_EXEC";

/// Creates the built-in exec plugin when the reference is to the exec plugin.
pub fn create_builtin_exec_plugin<TEnvironment: Environment>(environment: &TEnvironment, reference: &PluginSourceReference) -> Option<Box<dyn Plugin>> {
  if !is_exec_plugin_reference(reference) || environment.env_var(BUILTIN_EXEC_ENV_VAR).is_some_and(|value| value == "0") {
    return None;
  }
  log_debug!(environment, "Using the built-in exec plugin for {}", reference.display());
  Some(Box::new(InProcessPlugin::new(handler::ExecHandler::default)))
}

fn is_exec_plugin_reference(reference: &PluginSourceReference) -> bool {
  match &reference.path_source {
    // ex. npm:@dprint/exec@0.7.3/plugin.json
    PathSource::Npm(npm) => npm.specifier.name == "@dprint/exec",
    PathSource::Remote(remote) => {
      let path = remote.url.path();
      match remote.url.host_str() {
        // ex. https://plugins.dprint.dev/exec-0.5.0.json
        Some("plugins.dprint.dev") => (path.starts_with("/exec-") && path.ends_with(".json")) || path.starts_with("/dprint/dprint-plugin-exec/"),
        // ex. https://github.com/dprint/dprint-plugin-exec/releases/download/0.5.0/plugin.json
        Some("github.com") => path.starts_with("/dprint/dprint-plugin-exec/releases/"),
        _ => false,
      }
    }
    // a local exec plugin is most likely a build being worked on, so run it
    PathSource::Local(_) => false,
  }
}

#[cfg(test)]
mod test {
  use super::*;
  use crate::environment::TestEnvironment;
  use crate::environment::TestEnvironmentBuilder;
  use crate::test_helpers::run_test_cli;

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
  fn uses_the_process_plugin_when_disabled() {
    let environment = TestEnvironment::new();
    environment.set_env_var(BUILTIN_EXEC_ENV_VAR, Some("0"));
    let reference = parse_reference("npm:@dprint/exec@0.7.3/plugin.json", &environment);
    assert!(create_builtin_exec_plugin(&environment, &reference).is_none());
    environment.set_env_var(BUILTIN_EXEC_ENV_VAR, None);
    assert!(create_builtin_exec_plugin(&environment, &reference).is_some());
  }
}
