//! dprint's built-in exec: formats files with external commands.
//!
//! It started as the dprint-plugin-exec process plugin
//! (https://github.com/dprint/dprint-plugin-exec, MIT licensed, see LICENSE),
//! moved into the CLI. Running a command needs no plugin boundary: as a
//! downloaded process plugin it only ran on platforms someone published a
//! build for (FreeBSD has none, for example), and every file went through an
//! extra process.
//!
//! It's a formatter of its own now, released with dprint (see [`BUILT_IN`]).
//! Configuration files keep referencing the exec plugin as before, and dprint
//! serves the references to the release it knows (see `legacy.rs`) with it.

mod configuration;
mod executable;
#[cfg(windows)]
pub use executable::find_with_path_ext;
mod handler;
mod served;
pub use kprint_config::exec::input;
use kprint_config::exec::legacy;
mod template;

pub use legacy::COMMANDS_RELEASE as EXEC_COMMANDS_RELEASE;
pub use legacy::is_exec_plugin_reference;
pub use legacy::knows_exec_plugin_commands;

use crate::environment::PluginEnvironment as Environment;
use crate::plugins::BuiltInFormatter;
use crate::plugins::Plugin;
use crate::plugins::PluginSourceReference;

use super::in_process::InProcessPlugin;

/// Changes whenever the built-in exec can format a file differently for the
/// same configuration, which makes the incremental cache forget the files it
/// formatted. For example: what it passes a command, how it chooses the
/// commands for a file, or what it does with their output.
/// `handler::test::a_change_in_formatting_bumps_the_cache_revision` fails when
/// what it makes of a set of files changes.
const CACHE_REVISION: u32 = 2;

pub static BUILT_IN: BuiltInFormatter = BuiltInFormatter {
  name: "exec",
  cache_revision: CACHE_REVISION,
  serves_plugin: legacy::PLUGIN_NAME,
  refers_to_served_plugin: is_exec_plugin_reference,
};

/// Set to `0` to download and run the exec process plugin instead.
const BUILTIN_EXEC_ENV_VAR: &str = "DPRINT_BUILTIN_EXEC";

/// Creates the built-in exec when the reference is to the exec plugin, in any
/// release.
pub fn create_builtin_exec_plugin<TEnvironment: Environment>(environment: &TEnvironment, reference: &PluginSourceReference) -> Option<Box<dyn Plugin>> {
  if !is_builtin_exec_reference(environment, reference) {
    if is_exec_plugin_reference(reference) {
      log_debug!(
        environment,
        "Using the exec process plugin for {}: {}=0",
        reference.display(),
        BUILTIN_EXEC_ENV_VAR
      );
    }
    return None;
  }
  log_debug!(environment, "Using the built-in exec for {}", reference.display());
  Some(Box::new(ExecFormatter::default()))
}

/// The host's command formatter. Its input contract and schema are defined by
/// `kprint_config::exec::input`, independently of any external plugin release.
pub type ExecFormatter = InProcessPlugin<handler::ExecHandler>;

impl Default for ExecFormatter {
  fn default() -> Self {
    Self::new(handler::ExecHandler::default, input::exec_config_schema(), &BUILT_IN)
  }
}

/// Whether dprint serves the reference with the built-in exec rather than
/// running the exec process plugin it refers to.
pub use kprint_config::exec::is_builtin_exec_reference;

#[cfg(test)]
mod test {
  use super::*;
  use crate::environment::TestEnvironment;
  use crate::utils::PathSource;

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

  #[test]
  fn the_schema_describes_the_configuration() {
    use crate::test_helpers::validate_with_schema;

    // the configuration and its schema are read from and generated from the
    // same types (see `input.rs`), so a configuration with every property
    // resolves without diagnostics exactly when the schema accepts it.
    // `playWithFire` is read by dprint before the configuration gets here,
    // and accepted here too
    let schema: serde_json::Value = serde_json::from_str(input::exec_config_schema()).unwrap();
    let config = serde_json::json!({
      "lineWidth": 100,
      "indentWidth": 4,
      "useTabs": true,
      "cacheKey": "1",
      "cwd": ".",
      "timeout": 60,
      "setupTimeout": 600,
      "playWithFire": ["tr"],
      "commands": [{
        "command": "tr a-z A-Z",
        "exts": ["txt"],
        "fileNames": "README",
        "associations": "**/*.txt",
        "stdin": true,
        "cwd": ".",
        "cacheKeyFiles": ["./src/plugins/implementations/builtin_exec/testdata/one-line.txt"],
        "setupCommand": "true",
        "serve": true,
        "workers": 2,
      }],
    });
    assert_eq!(validate_with_schema(&schema, &config), Ok(()));
    let result = configuration::Configuration::resolve(
      serde_json::from_value(config.clone()).unwrap(),
      &Default::default(),
      &crate::environment::CanonicalizedPathBuf::new_for_testing("/"),
    );
    assert_eq!(result.diagnostics, vec![]);
    assert_eq!(result.config.timeout, 60);
    assert_eq!(result.config.setup_timeout, 600);

    // each property left out, `null` and as it is, of the configuration and
    // of a command: the types and the schema agree on every one (a property
    // of an `Option` reads `null` as left out, `command` isn't optional)
    fn properties_of(value: &serde_json::Value, pointer: &str) -> Vec<(String, String)> {
      let mut result = Vec::new();
      match value {
        serde_json::Value::Object(object) => {
          for (name, value) in object {
            result.push((pointer.to_string(), name.clone()));
            result.extend(properties_of(value, &format!("{}/{}", pointer, name)));
          }
        }
        serde_json::Value::Array(values) => {
          for (index, value) in values.iter().enumerate() {
            result.extend(properties_of(value, &format!("{}/{}", pointer, index)));
          }
        }
        _ => {}
      }
      result
    }
    let mut nulls_accepted = 0;
    let mut nulls_rejected = 0;
    for (parent_pointer, name) in properties_of(&config, "") {
      for variant in ["left out", "null", "as it is"] {
        let mut config = config.clone();
        let parent = config.pointer_mut(&parent_pointer).unwrap().as_object_mut().unwrap();
        match variant {
          "left out" => {
            parent.shift_remove(&name);
          }
          "null" => {
            parent[&name] = serde_json::Value::Null;
          }
          _ => {}
        }
        let reads = kprint_config::from_json::<input::ExecConfigInput>(config.clone());
        let validates = validate_with_schema(&schema, &config);
        assert_eq!(
          reads.is_ok(),
          validates.is_ok(),
          "{}/{} {}: types {:?}, schema {:?}",
          parent_pointer,
          name,
          variant,
          reads.err(),
          validates.err()
        );
        if variant == "null" {
          if reads.is_ok() {
            nulls_accepted += 1;
          } else {
            nulls_rejected += 1;
          }
        }
      }
    }
    assert_eq!((nulls_accepted, nulls_rejected), (18, 1));

    // what the schema rejects, the configuration does too, saying where
    for (config, property, message) in [
      (serde_json::json!({ "timeout": "60" }), "timeout", "invalid type: string \"60\", expected u32"),
      (
        serde_json::json!({ "unknown": true }),
        "unknown",
        "unknown field `unknown`, expected one of `lineWidth`, `indentWidth`, `useTabs`, `cacheKey`, `cwd`, `timeout`, `setupTimeout`, `playWithFire`, `commands`",
      ),
      (
        serde_json::json!({ "commands": [{ "command": "fmt", "exts": [1] }] }),
        "commands[0].exts",
        "Expected a string or an array of strings.",
      ),
      (
        serde_json::json!({ "commands": [{ "command": "fmt", "exts": ["txt"], "unknown": true }] }),
        "commands[0].unknown",
        "unknown field `unknown`, expected one of `command`, `exts`, `fileNames`, `associations`, `stdin`, `cwd`, `cacheKeyFiles`, `setupCommand`, `serve`, `workers`",
      ),
      (
        serde_json::json!({ "commands": [{ "command": "fmt", "exts": ["txt"], "serve": true, "workers": 0 }] }),
        "commands[0].workers",
        "Expected at least 1.",
      ),
      (
        serde_json::json!({ "commands": [5] }),
        "commands[0]",
        "invalid type: integer `5`, expected a command (an object)",
      ),
    ] {
      assert!(validate_with_schema(&schema, &config).is_err(), "{}", config);
      let result = configuration::Configuration::resolve(
        serde_json::from_value(config.clone()).unwrap(),
        &Default::default(),
        &crate::environment::CanonicalizedPathBuf::new_for_testing("/"),
      );
      assert_eq!(
        result.diagnostics,
        vec![kprint_configuration::ConfigurationDiagnostic {
          property_name: property.to_string(),
          message: message.to_string(),
        }],
        "{}",
        config
      );
      assert!(!result.config.is_valid);
    }
  }

  #[test]
  fn the_schema_accepts_the_commands_the_configuration_does() {
    use crate::test_helpers::validate_with_schema;
    use serde_json::json;

    let schema: serde_json::Value = serde_json::from_str(input::exec_config_schema()).unwrap();
    for command in [
      json!({ "command": "fmt", "exts": "txt" }),
      json!({ "command": "fmt", "exts": ["txt"] }),
      json!({ "command": "fmt", "fileNames": "README" }),
      json!({ "command": "fmt", "associations": "**/*.txt" }),
      json!({ "command": "fmt", "associations": ["**/*.txt"] }),
      json!({ "command": "fmt", "exts": [], "fileNames": ["README"] }),
      // what to format with it is empty
      json!({ "command": "fmt" }),
      json!({ "command": "fmt", "exts": [] }),
      json!({ "command": "fmt", "fileNames": [] }),
      json!({ "command": "fmt", "associations": [] }),
      json!({ "command": "fmt", "exts": [], "fileNames": [], "associations": [] }),
      json!({ "command": "fmt", "associations": ["**/*.txt", "**/*.md"] }),
      json!({ "command": "fmt", "exts": "txt", "unknown": true }),
    ] {
      let config = json!({ "commands": [command] });
      let resolved = configuration::Configuration::resolve(
        serde_json::from_value(config.clone()).unwrap(),
        &Default::default(),
        &crate::environment::CanonicalizedPathBuf::new_for_testing("/"),
      );
      assert_eq!(
        validate_with_schema(&schema, &config).is_ok(),
        resolved.diagnostics.is_empty(),
        "{}: {:?}",
        command,
        resolved.diagnostics
      );
    }

    // a configuration file may set some of it and get the rest from one it
    // extends, ex. only allow the extended configuration's commands to run
    for config in [json!({}), json!({ "playWithFire": true }), json!({ "lineWidth": 80 })] {
      assert_eq!(validate_with_schema(&schema, &config), Ok(()), "{}", config);
    }
  }

  /// References to the exec plugin in every form they're written in.
  const EXEC_REFERENCES: &[&str] = &[
    // the built-in by name
    "exec",
    // npm, with or without a version or the package's file, and with any checksum
    "npm:@dprint/exec@0.7.3/plugin.json",
    "npm:@dprint/exec@0.7.3/plugin.json@704701df449dd7e942a71144773778ac529d68c2e4657bfc236d393b898b9a67",
    "npm:@dprint/exec@0.7.3/plugin.json@abc",
    "npm:@dprint/exec@0.6.0/plugin.json",
    "npm:@dprint/exec@0.8.0/plugin.json@abc",
    "npm:@dprint/exec@0.7.3",
    "npm:@dprint/exec@0.7.3/plugin.wasm",
    "npm:@dprint/exec",
    // plugins.dprint.dev, in any release and the latest one
    "https://plugins.dprint.dev/exec-0.7.3.json",
    "https://plugins.dprint.dev/exec-0.7.3.json@a7898d5f1897e77bff474cec3d948c3ec3a7f455e32de2cc60c8adb9a5dd24aa",
    "https://plugins.dprint.dev/exec-0.5.0.json@abc",
    "https://plugins.dprint.dev/exec-0.7.30.json",
    "https://plugins.dprint.dev/dprint/exec-0.7.3.json",
    "https://plugins.dprint.dev/dprint/dprint-plugin-exec/0.7.3/plugin.json",
    "https://plugins.dprint.dev/dprint/dprint-plugin-exec/0.8.0/plugin.json",
    "https://plugins.dprint.dev/dprint/dprint-plugin-exec/latest.json",
    "https://plugins.dprint.dev/dprint/exec/latest.json",
    // GitHub releases
    "https://github.com/dprint/dprint-plugin-exec/releases/download/0.5.0/plugin.json@abc",
    "https://github.com/dprint/dprint-plugin-exec/releases/download/0.7.3/plugin.json",
    "https://github.com/dprint/dprint-plugin-exec/releases/latest/download/plugin.json",
    // the npm package from a CDN
    "https://cdn.jsdelivr.net/npm/@dprint/exec@0.7.3/plugin.json",
    "https://cdn.jsdelivr.net/npm/@dprint/exec/plugin.json",
    "https://unpkg.com/@dprint/exec@0.8.0/plugin.json",
  ];

  #[test]
  fn serves_every_reference_to_the_exec_plugin() {
    let environment = TestEnvironment::new();
    for text in EXEC_REFERENCES {
      let reference = parse_reference(text, &environment);
      assert!(is_exec_plugin_reference(&reference), "{}", text);
      assert!(is_builtin_exec_reference(&environment, &reference), "{}", text);
      assert!(create_builtin_exec_plugin(&environment, &reference).is_some(), "{}", text);
    }
  }

  #[test]
  fn leaves_other_plugins_alone() {
    let environment = TestEnvironment::new();
    for text in [
      "npm:@dprint/typescript@0.96.1",
      "npm:@dprint/exec-other@0.1.0/plugin.json",
      "https://plugins.dprint.dev/typescript-0.96.1.wasm",
      "https://plugins.dprint.dev/kjanat/exec-0.1.0.json",
      "https://plugins.dprint.dev/dprint/execute-0.1.0.json",
      "https://example.com/exec-0.5.0.json",
      "https://github.com/someone/dprint-plugin-exec/releases/download/0.5.0/plugin.json",
      "https://cdn.jsdelivr.net/npm/@dprint/exec-other@0.1.0/plugin.json",
      "./exec/plugin.json",
    ] {
      let reference = parse_reference(text, &environment);
      assert!(!is_exec_plugin_reference(&reference), "{}", text);
      assert!(create_builtin_exec_plugin(&environment, &reference).is_none(), "{}", text);
    }
  }

  #[test]
  fn knows_the_commands_of_the_release_it_reads() {
    let environment = TestEnvironment::new();
    let knows = |text: &str| knows_exec_plugin_commands(&parse_reference(text, &environment));
    assert_eq!(legacy::COMMANDS_RELEASE, "0.7.3");
    assert!(knows("npm:@dprint/exec@0.7.3/plugin.json"));
    assert!(knows("https://plugins.dprint.dev/exec-0.7.3.json@abc"));
    // as the process plugin, another release can read commands differently
    assert!(!knows("npm:@dprint/exec@0.8.0/plugin.json"));
    assert!(!knows("https://plugins.dprint.dev/dprint/dprint-plugin-exec/latest.json"));
  }

  #[test]
  fn uses_the_process_plugin_when_disabled() {
    let environment = TestEnvironment::new();
    environment.set_env_var(BUILTIN_EXEC_ENV_VAR, Some("0"));
    for text in EXEC_REFERENCES {
      let reference = parse_reference(text, &environment);
      assert!(create_builtin_exec_plugin(&environment, &reference).is_none(), "{}", text);
    }
    environment.set_env_var(BUILTIN_EXEC_ENV_VAR, None);
    let reference = parse_reference("npm:@dprint/exec@0.7.3/plugin.json", &environment);
    assert!(create_builtin_exec_plugin(&environment, &reference).is_some());
  }
}
