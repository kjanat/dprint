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
pub mod input;
mod legacy;
mod template;

pub use legacy::COMMANDS_RELEASE as EXEC_COMMANDS_RELEASE;
pub use legacy::is_exec_plugin_reference;
pub use legacy::knows_exec_plugin_commands;

use crate::environment::Environment;
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
const CACHE_REVISION: u32 = 1;

pub static BUILT_IN: BuiltInFormatter = BuiltInFormatter {
  name: "exec",
  cache_revision: CACHE_REVISION,
  serves_plugin: legacy::PLUGIN_NAME,
  refers_to_served_plugin: is_exec_plugin_reference,
};

/// Set to `0` to download and run the exec process plugin instead.
const BUILTIN_EXEC_ENV_VAR: &str = "DPRINT_BUILTIN_EXEC";

/// Creates the built-in exec when the reference is to an exec plugin release
/// it serves.
pub fn create_builtin_exec_plugin<TEnvironment: Environment>(environment: &TEnvironment, reference: &PluginSourceReference) -> Option<Box<dyn Plugin>> {
  if !is_builtin_exec_reference(environment, reference) {
    if is_exec_plugin_reference(reference) {
      let reason = match legacy::served_release(reference) {
        Ok(_) => format!("{}=0", BUILTIN_EXEC_ENV_VAR),
        Err(reason) => reason,
      };
      log_debug!(environment, "Using the exec process plugin for {}: {}", reference.display(), reason);
    }
    return None;
  }
  log_debug!(environment, "Using the built-in exec for {}", reference.display());
  Some(Box::new(InProcessPlugin::new(
    handler::ExecHandler::default,
    input::exec_config_schema(),
    &BUILT_IN,
  )))
}

/// Whether dprint serves the reference with the built-in exec rather than
/// running the exec process plugin it refers to.
pub fn is_builtin_exec_reference<TEnvironment: Environment>(environment: &TEnvironment, reference: &PluginSourceReference) -> bool {
  legacy::served_release(reference).is_ok() && environment.env_var(BUILTIN_EXEC_ENV_VAR).is_none_or(|value| value != "0")
}

/// The program an exec command runs, split from its arguments the way exec does.
pub fn exec_command_program(command: &str) -> Option<String> {
  configuration::split_command(command).into_iter().next()
}

#[cfg(test)]
mod test {
  use super::*;
  use crate::environment::TestEnvironment;
  use crate::environment::TestEnvironmentBuilder;
  use crate::test_helpers::run_test_cli;
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

  #[cfg(unix)]
  #[test]
  fn formats_with_built_in_exec_without_downloading_the_plugin() {
    // no plugin files are served, so this would fail if it tried to download
    let environment = TestEnvironmentBuilder::new()
      .with_default_config(|config_file| {
        config_file
          .add_plugin("npm:@dprint/exec@0.7.3/plugin.json@704701df449dd7e942a71144773778ac529d68c2e4657bfc236d393b898b9a67")
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
      }],
    });
    assert_eq!(validate_with_schema(&schema, &config), Ok(()));
    let result = configuration::Configuration::resolve(serde_json::from_value(config.clone()).unwrap(), &Default::default());
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
        let reads = dprint_config_model::from_json::<input::ExecConfigInput>(config.clone());
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
    assert_eq!((nulls_accepted, nulls_rejected), (16, 1));

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
        "unknown field `unknown`, expected one of `command`, `exts`, `fileNames`, `associations`, `stdin`, `cwd`, `cacheKeyFiles`, `setupCommand`",
      ),
      (
        serde_json::json!({ "commands": [5] }),
        "commands[0]",
        "invalid type: integer `5`, expected a command (an object)",
      ),
    ] {
      assert!(validate_with_schema(&schema, &config).is_err(), "{}", config);
      let result = configuration::Configuration::resolve(serde_json::from_value(config.clone()).unwrap(), &Default::default());
      assert_eq!(
        result.diagnostics,
        vec![dprint_core::configuration::ConfigurationDiagnostic {
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
      let resolved = configuration::Configuration::resolve(serde_json::from_value(config.clone()).unwrap(), &Default::default());
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

  #[test]
  fn serves_only_references_to_the_version_it_was_built_from() {
    let environment = TestEnvironment::new();
    let is_builtin = |text: &str| is_builtin_exec_reference(&environment, &parse_reference(text, &environment));
    // the release whose references it serves, however it's referenced
    assert_eq!(legacy::COMMANDS_RELEASE, "0.7.3");
    assert!(is_builtin("npm:@dprint/exec@0.7.3/plugin.json"));
    assert!(is_builtin(&format!("npm:@dprint/exec@0.7.3/plugin.json@{}", NPM_TARBALL_CHECKSUM)));
    assert!(is_builtin("https://plugins.dprint.dev/exec-0.7.3.json"));
    assert!(is_builtin(&format!("https://plugins.dprint.dev/exec-0.7.3.json@{}", PLUGIN_FILE_CHECKSUM)));
    assert!(is_builtin("https://plugins.dprint.dev/dprint/dprint-plugin-exec/0.7.3/plugin.json"));
    assert!(is_builtin(&format!(
      "https://github.com/dprint/dprint-plugin-exec/releases/download/0.7.3/plugin.json@{}",
      PLUGIN_FILE_CHECKSUM
    )));
    // other versions, older or newer, are what they ask for
    assert!(!is_builtin("npm:@dprint/exec@0.6.0/plugin.json@abc"));
    assert!(!is_builtin("npm:@dprint/exec@0.8.0/plugin.json@abc"));
    assert!(!is_builtin("https://plugins.dprint.dev/exec-0.5.0.json@abc"));
    assert!(!is_builtin("https://plugins.dprint.dev/exec-0.7.30.json@abc"));
    assert!(!is_builtin("https://plugins.dprint.dev/dprint/dprint-plugin-exec/0.8.0/plugin.json"));
    assert!(!is_builtin(
      "https://github.com/dprint/dprint-plugin-exec/releases/download/0.5.0/plugin.json@abc"
    ));
    // and other files of the npm package, which are resolved as asked
    assert!(!is_builtin("npm:@dprint/exec@0.7.3/alternate.json@abc"));
    assert!(!is_builtin("npm:@dprint/exec@0.7.3/plugin.wasm@abc"));
    assert!(!is_builtin("npm:@dprint/exec@0.7.3"));
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

  /// The checksums of the exec plugin's 0.7.3 release: of its npm package's
  /// tarball (as dprint's own configuration pins it) and of its plugin.json
  /// (as plugins.dprint.dev and its GitHub release serve it).
  const NPM_TARBALL_CHECKSUM: &str = "704701df449dd7e942a71144773778ac529d68c2e4657bfc236d393b898b9a67";
  const PLUGIN_FILE_CHECKSUM: &str = "a7898d5f1897e77bff474cec3d948c3ec3a7f455e32de2cc60c8adb9a5dd24aa";

  #[test]
  fn serves_only_references_pinning_the_checksum_of_the_release_it_was_built_from() {
    let environment = TestEnvironment::new();
    let is_builtin = |text: &str| is_builtin_exec_reference(&environment, &parse_reference(text, &environment));
    // another checksum keeps the reference the integrity pin it is
    assert!(!is_builtin("npm:@dprint/exec@0.7.3/plugin.json@abc"));
    assert!(!is_builtin(
      "npm:@dprint/exec@0.7.3/plugin.json@0000000000000000000000000000000000000000000000000000000000000000"
    ));
    assert!(!is_builtin("https://plugins.dprint.dev/exec-0.7.3.json@abc"));
    // including the checksum of the other file of the release
    assert!(!is_builtin(&format!("npm:@dprint/exec@0.7.3/plugin.json@{}", PLUGIN_FILE_CHECKSUM)));
    assert!(!is_builtin(&format!("https://plugins.dprint.dev/exec-0.7.3.json@{}", NPM_TARBALL_CHECKSUM)));
    // and checksums are compared as written, like when they're checked
    assert!(!is_builtin(&format!(
      "npm:@dprint/exec@0.7.3/plugin.json@{}",
      NPM_TARBALL_CHECKSUM.to_uppercase()
    )));
    assert_eq!(
      legacy::served_release(&parse_reference("npm:@dprint/exec@0.7.3/plugin.json@abc", &environment)).err(),
      Some(format!(
        "its checksum isn't the one of dprint-plugin-exec 0.7.3 ({}), so it's checked against the plugin it names.",
        NPM_TARBALL_CHECKSUM
      ))
    );
  }

  #[test]
  fn checks_the_checksum_of_a_reference_it_doesnt_serve() {
    // the plugin it names is downloaded and fails its checksum check, rather
    // than being replaced by the built-in exec
    let environment = TestEnvironmentBuilder::new()
      .with_default_config(|config_file| {
        config_file
          .add_plugin("https://plugins.dprint.dev/exec-0.7.3.json@0000000000000000000000000000000000000000000000000000000000000000")
          .add_config_section("exec", r#"{ "commands": [{ "command": "tr a-z A-Z", "exts": ["txt"] }] }"#);
      })
      .add_remote_file("https://plugins.dprint.dev/exec-0.7.3.json", "{}")
      .write_file("/file.txt", "text\n")
      .build();
    let error = run_test_cli(vec!["fmt", "/file.txt"], &environment).err().unwrap();
    error.assert_exit_code(12);
    assert!(error.to_string().contains("The checksum did not match the expected checksum."), "{}", error);
    assert_eq!(environment.read_file("/file.txt").unwrap(), "text\n");
    environment.take_stderr_messages();
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
