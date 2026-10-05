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
/// The exec plugin releases the built-in exec serves references to. A config
/// that asks for another version (or no specific one) gets that version, run
/// as the process plugin, since the built-in exec only behaves like the
/// release it was built from. A release is only added here together with
/// tests that the built-in exec handles its configuration the same way.
const SERVED_EXEC_PLUGIN_RELEASES: &[ServedRelease] = &[ServedRelease {
  version: EXEC_PLUGIN_VERSION,
  plugin_file_checksum: "a7898d5f1897e77bff474cec3d948c3ec3a7f455e32de2cc60c8adb9a5dd24aa",
  npm_tarball_checksum: "704701df449dd7e942a71144773778ac529d68c2e4657bfc236d393b898b9a67",
}];

/// A release of the exec plugin the built-in exec serves references to.
///
/// A reference may pin a release's checksum, which is only served built in
/// when it's that release's, so a pin keeps meaning that release. Another
/// checksum gets the plugin it names, which then fails its checksum check
/// like any plugin.
struct ServedRelease {
  version: &'static str,
  /// Of its process plugin file (plugin.json), as a url reference pins it.
  plugin_file_checksum: &'static str,
  /// Of its npm package's tarball, as an npm reference pins it.
  npm_tarball_checksum: &'static str,
}
/// The file of the exec process plugin in its npm package.
const EXEC_PLUGIN_NPM_FILE: &str = "plugin.json";
/// Set to `0` to download and run the exec process plugin instead.
const BUILTIN_EXEC_ENV_VAR: &str = "DPRINT_BUILTIN_EXEC";

/// Creates the built-in exec plugin when the reference is to a version of the
/// exec plugin it serves.
pub fn create_builtin_exec_plugin<TEnvironment: Environment>(environment: &TEnvironment, reference: &PluginSourceReference) -> Option<Box<dyn Plugin>> {
  if !is_builtin_exec_reference(environment, reference) {
    if is_exec_plugin_reference(reference) {
      let reason = match served_release(reference) {
        Ok(_) => format!("{}=0", BUILTIN_EXEC_ENV_VAR),
        Err(reason) => reason,
      };
      log_debug!(environment, "Using the exec process plugin for {}: {}", reference.display(), reason);
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
  Some(Box::new(InProcessPlugin::new(handler::ExecHandler::default)))
}

/// Whether dprint serves the reference with the built-in exec rather than
/// running the exec process plugin it refers to.
pub fn is_builtin_exec_reference<TEnvironment: Environment>(environment: &TEnvironment, reference: &PluginSourceReference) -> bool {
  served_release(reference).is_ok() && environment.env_var(BUILTIN_EXEC_ENV_VAR).is_none_or(|value| value != "0")
}

/// The release the built-in exec serves the reference as, or why it doesn't.
fn served_release(reference: &PluginSourceReference) -> Result<&'static ServedRelease, String> {
  let served_versions = || SERVED_EXEC_PLUGIN_RELEASES.iter().map(|release| release.version).collect::<Vec<_>>().join(", ");
  let release = exec_plugin_reference_version(reference)
    .and_then(|version| SERVED_EXEC_PLUGIN_RELEASES.iter().find(|release| release.version == version))
    .ok_or_else(|| {
      format!(
        "the built-in exec ({} {}) only serves references to version {}.",
        EXEC_PLUGIN_NAME,
        EXEC_PLUGIN_VERSION,
        served_versions()
      )
    })?;
  let expected_checksum = match &reference.path_source {
    PathSource::Npm(_) => release.npm_tarball_checksum,
    PathSource::Remote(_) | PathSource::Local(_) => release.plugin_file_checksum,
  };
  match &reference.checksum {
    Some(checksum) if checksum != expected_checksum => Err(format!(
      "its checksum isn't the one of {} {} ({}), so it's checked against the plugin it names.",
      EXEC_PLUGIN_NAME, release.version, expected_checksum
    )),
    _ => Ok(release),
  }
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
    // installed in node_modules. Only the exec process plugin's file
    // (plugin.json) can be served built in: any other file in the package is
    // resolved as asked, so a wrong path is reported rather than replaced.
    PathSource::Npm(npm) => {
      (npm.specifier.name == "@dprint/exec").then(|| npm.specifier.version.as_deref().filter(|_| npm.specifier.path == EXEC_PLUGIN_NPM_FILE))
    }
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
  fn serves_only_references_to_the_version_it_was_built_from() {
    let environment = TestEnvironment::new();
    let is_builtin = |text: &str| is_builtin_exec_reference(&environment, &parse_reference(text, &environment));
    // the version the built-in exec was built from, however it's referenced
    assert_eq!(EXEC_PLUGIN_VERSION, "0.7.3");
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
      served_release(&parse_reference("npm:@dprint/exec@0.7.3/plugin.json@abc", &environment)).err(),
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
