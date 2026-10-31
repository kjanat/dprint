use kprint_configuration::ConfigKeyMap;
use kprint_configuration::ConfigurationDiagnostic;
use kprint_configuration::GlobalConfiguration;
use kprint_configuration::RECOMMENDED_GLOBAL_CONFIGURATION;
use kprint_configuration::ResolveConfigurationResult;
use kprint_discovery::GlobMatcher;
use kprint_discovery::get_patterns_as_glob_matcher;
use serde::Serialize;
use serde::Serializer;
use sha2::Digest;
use sha2::Sha256;
use std::collections::BTreeMap;
use std::fs::read_to_string;
use std::num::NonZeroUsize;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use super::input::Associations;
use super::input::ExecCommandInput;
use super::input::ExecConfigInput;
use super::input::Invocation;
use super::input::InvocationInput;
use super::input::default_serve;
use super::input::default_setup_timeout;
use super::input::default_stdin;
use super::input::default_timeout;
use super::input::default_workers;
use super::template::uses_variable;
use super::template::validate_template;
use crate::environment::CanonicalizedPathBuf;

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Configuration {
  /// Doesn't allow formatting unless the configuration had no diagnostics.
  pub is_valid: bool,
  pub cache_key: String,
  pub line_width: u32,
  pub use_tabs: bool,
  pub indent_width: u8,
  /// Formatting commands to run
  pub commands: Vec<CommandConfiguration>,
  pub timeout: u32,
  /// Seconds a setup command may run before it's killed. Not serialized
  /// because it can't change formatting output, so it shouldn't invalidate
  /// the incremental cache.
  #[serde(skip_serializing)]
  pub setup_timeout: u32,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandConfiguration {
  pub executable: String,
  /// Executable arguments to add
  pub args: Vec<String>,
  #[serde(skip_serializing_if = "BTreeMap::is_empty")]
  pub env: BTreeMap<String, String>,
  pub cwd: PathBuf,
  pub stdin: bool,
  pub associations: Option<CommandAssociations>,
  pub file_extensions: Vec<String>,
  pub file_names: Vec<String>,
  pub cache_key_files_hash: Option<String>,
  /// Command to run once before this command formats its first file.
  pub setup_command: Option<SetupCommand>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub serve: Option<Serve>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Serve {
  pub workers: NonZeroUsize,
}

/// A command run a single time before formatting begins (ex. to install a
/// toolchain so that parallel formatting doesn't race installing it).
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SetupCommand {
  pub executable: String,
  /// Executable arguments to add
  pub args: Vec<String>,
  #[serde(skip_serializing_if = "BTreeMap::is_empty")]
  pub env: BTreeMap<String, String>,
}

impl CommandConfiguration {
  pub fn matches_exts_or_filenames(&self, path: &Path) -> bool {
    if let Some(filename) = path.file_name() {
      let filename = filename.to_string_lossy().to_lowercase();
      for ext in &self.file_extensions {
        if filename.ends_with(ext) {
          return true;
        }
      }
      self.file_names.iter().any(|name| name == &filename)
    } else {
      false
    }
  }
}

/// A command's `associations` glob, which matches paths relative to the
/// configuration's directory like dprint's own `associations` do.
#[derive(Clone)]
pub struct CommandAssociations {
  pub glob: String,
  matcher: Arc<GlobMatcher>,
}

impl CommandAssociations {
  pub fn matches(&self, path: &Path) -> bool {
    self.matcher.matches(path)
  }
}

impl Serialize for CommandAssociations {
  fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&self.glob)
  }
}

impl Configuration {
  /// Resolves the plugin's configuration: the values are read as what the
  /// configuration may hold (see [`ExecConfigInput`], which is also what its
  /// schema describes), and that is resolved to what formatting runs with.
  /// A value that isn't what it may be is a diagnostic saying where, and
  /// nothing more is resolved from it.
  /// A command's `associations` are relative to `base_dir`.
  pub fn resolve(config: ConfigKeyMap, global_config: &GlobalConfiguration, base_dir: &CanonicalizedPathBuf) -> ResolveConfigurationResult<Configuration> {
    let (input, mut diagnostics) = match kprint_config::from_values::<ExecConfigInput>(config) {
      Ok(input) => (input, Vec::new()),
      Err(err) => (
        ExecConfigInput {
          commands: Some(Vec::new()),
          ..Default::default()
        },
        vec![ConfigurationDiagnostic {
          property_name: err.path,
          message: err.message,
        }],
      ),
    };

    let mut resolved_config = Configuration {
      is_valid: true,
      cache_key: "0".to_string(),
      line_width: input
        .line_width
        .unwrap_or(global_config.line_width.unwrap_or(RECOMMENDED_GLOBAL_CONFIGURATION.line_width)),
      use_tabs: input
        .use_tabs
        .unwrap_or(global_config.use_tabs.unwrap_or(RECOMMENDED_GLOBAL_CONFIGURATION.use_tabs)),
      indent_width: input
        .indent_width
        .unwrap_or(global_config.indent_width.unwrap_or(RECOMMENDED_GLOBAL_CONFIGURATION.indent_width)),
      commands: Vec::new(),
      timeout: input.timeout.unwrap_or_else(default_timeout),
      setup_timeout: input.setup_timeout.unwrap_or_else(default_setup_timeout),
    };

    let names = ExecConfigInput::property_names();
    let mut cache_key_file_hashes = Vec::new();
    match input.commands {
      Some(commands) => {
        for (i, command) in commands.into_iter().enumerate() {
          let (command_config, command_diagnostics) = resolve_command(command, input.cwd.as_deref(), base_dir);
          diagnostics.extend(command_diagnostics.into_iter().map(|mut diagnostic| {
            diagnostic.property_name = format!("{}[{}].{}", names.commands, i, diagnostic.property_name);
            diagnostic
          }));
          if let Some(mut command_config) = command_config {
            if let Some(cache_key_files_hash) = command_config.cache_key_files_hash.take() {
              cache_key_file_hashes.push(cache_key_files_hash);
            }
            resolved_config.commands.push(command_config);
          }
        }
      }
      None => diagnostics.push(ConfigurationDiagnostic {
        property_name: names.commands.clone(),
        message: format!(
          "Expected to find a \"{}\" array property (see https://github.com/dprint/dprint-plugin-exec for instructions)",
          names.commands
        ),
      }),
    }

    if let Some(cache_key) = compute_cache_key(input.cache_key, &cache_key_file_hashes) {
      resolved_config.cache_key = cache_key;
    }

    resolved_config.is_valid = diagnostics.is_empty();

    ResolveConfigurationResult {
      config: resolved_config,
      diagnostics,
    }
  }
}

/// Resolves one command. A diagnostic's property is the command's.
fn resolve_command(
  command: ExecCommandInput,
  root_cwd: Option<&str>,
  base_dir: &CanonicalizedPathBuf,
) -> (Option<CommandConfiguration>, Vec<ConfigurationDiagnostic>) {
  let names = ExecCommandInput::property_names();
  let mut diagnostics = Vec::new();
  let Some(invocation) = command.command.resolve() else {
    diagnostics.push(ConfigurationDiagnostic {
      property_name: names.command.clone(),
      message: "Expected to find a command name.".to_string(),
    });
    return (None, diagnostics);
  };

  for arg in &invocation.args {
    if let Err(err) = validate_template(arg) {
      diagnostics.push(ConfigurationDiagnostic {
        property_name: names.command.clone(),
        message: format!("Invalid template in argument '{}': {}", arg, err),
      });
    }
  }
  for (name, value) in &invocation.env {
    if let Err(err) = validate_template(value) {
      diagnostics.push(ConfigurationDiagnostic {
        property_name: names.command.clone(),
        message: format!("Invalid template in environment variable '{}': {}", name, err),
      });
    }
  }

  let cwd = get_cwd(command.cwd.or_else(|| root_cwd.map(ToOwned::to_owned)));

  // computed here rather than when formatting so an unreadable file is a
  // configuration diagnostic
  let cache_key_files_hash = match command.cache_key_files {
    Some(cache_key_files) => {
      let mut hasher = Sha256::new();
      for file in cache_key_files {
        let file = cwd.join(file);
        // plugin config resolution has no environment to read files with,
        // so this reads them directly like the exec process plugin does
        #[allow(clippy::disallowed_methods)]
        let contents = match read_to_string(&file) {
          Ok(contents) => contents,
          Err(err) => {
            diagnostics.push(ConfigurationDiagnostic {
              property_name: names.cache_key_files.clone(),
              message: format!("Unable to read file '{}': {}.", file.display(), err),
            });
            return (None, diagnostics);
          }
        };
        hasher.update(contents);
      }
      Some(format!("{:x}", hasher.finalize()))
    }
    None => None,
  };

  let setup_command = command
    .setup_command
    .and_then(|setup_command| resolve_setup_command(setup_command, &mut diagnostics));
  let stdin = command.stdin.unwrap_or_else(default_stdin);
  let serve = resolve_serve(&command.serve, command.workers, stdin, &invocation, &mut diagnostics);

  let associations = match command.associations {
    None => None,
    Some(Associations::One(glob)) => Some(glob),
    Some(Associations::Many(mut globs)) => match globs.len() {
      0 => None,
      1 => globs.pop(),
      _ => {
        diagnostics.push(ConfigurationDiagnostic {
          property_name: names.associations.clone(),
          message: "Unfortunately multiple globs haven't been implemented yet. Please provide a single glob or consider contributing this feature.".to_string(),
        });
        None
      }
    },
  };
  let associations = associations.and_then(|glob| match get_patterns_as_glob_matcher(std::slice::from_ref(&glob), base_dir) {
    Ok(matcher) => Some(CommandAssociations {
      glob,
      matcher: Arc::new(matcher),
    }),
    Err(err) => {
      diagnostics.push(ConfigurationDiagnostic {
        message: format!("Error parsing associations glob: {:#}", err),
        property_name: names.associations.clone(),
      });
      None
    }
  });

  let config = CommandConfiguration {
    executable: invocation.executable,
    args: invocation.args,
    env: invocation.env,
    setup_command,
    associations,
    cwd,
    stdin,
    serve,
    file_extensions: command
      .exts
      .map(Vec::from)
      .unwrap_or_default()
      .into_iter()
      .map(|ext| if ext.starts_with('.') { ext } else { format!(".{}", ext) })
      .collect(),
    file_names: command.file_names.map(Vec::from).unwrap_or_default(),
    cache_key_files_hash,
  };

  if diagnostics.is_empty() && config.file_names.is_empty() && config.file_extensions.is_empty() && config.associations.is_none() {
    diagnostics.push(ConfigurationDiagnostic {
      property_name: names.exts.clone(),
      message: format!(
        "You must specify either: {} (recommended), {}, or {}",
        names.exts, names.file_names, names.associations
      ),
    })
  }

  (Some(config), diagnostics)
}

fn resolve_serve(
  serve: &Option<bool>,
  workers: Option<u32>,
  stdin: bool,
  invocation: &Invocation,
  diagnostics: &mut Vec<ConfigurationDiagnostic>,
) -> Option<Serve> {
  let names = ExecCommandInput::property_names();
  if !serve.unwrap_or_else(default_serve) {
    if workers.is_some() {
      diagnostics.push(ConfigurationDiagnostic {
        property_name: names.workers.clone(),
        message: format!("Only a served command has it. Set \"{}\": true to serve the command.", names.serve),
      });
    }
    return None;
  }
  if !stdin {
    diagnostics.push(ConfigurationDiagnostic {
      property_name: names.serve.clone(),
      message: "A served command gets the files on stdin, so \"stdin\" can't be false.".to_string(),
    });
  }
  if invocation
    .args
    .iter()
    .chain(invocation.env.values())
    .any(|value| uses_variable(value, "file_path"))
  {
    diagnostics.push(ConfigurationDiagnostic {
      property_name: names.command.clone(),
      message: "A served command formats every file, so its arguments and environment can't use {{file_path}}. It gets each file's path in its request."
        .to_string(),
    });
  }
  let workers = match NonZeroUsize::new(workers.unwrap_or_else(default_workers) as usize) {
    Some(workers) => workers,
    None => {
      diagnostics.push(ConfigurationDiagnostic {
        property_name: names.workers.clone(),
        message: "Expected at least 1.".to_string(),
      });
      NonZeroUsize::MIN
    }
  };
  Some(Serve { workers })
}

fn resolve_setup_command(setup_command: InvocationInput, diagnostics: &mut Vec<ConfigurationDiagnostic>) -> Option<SetupCommand> {
  let Some(invocation) = setup_command.resolve() else {
    diagnostics.push(ConfigurationDiagnostic {
      property_name: ExecCommandInput::property_names().setup_command.clone(),
      message: "Expected to find a command name.".to_string(),
    });
    return None;
  };
  Some(SetupCommand {
    executable: invocation.executable,
    args: invocation.args,
    env: invocation.env,
  })
}

// commands run in the process' cwd by default, like in the exec process plugin
#[allow(clippy::disallowed_methods)]
fn get_cwd(dir: Option<String>) -> PathBuf {
  match dir {
    Some(dir) => PathBuf::from(dir),
    None => std::env::current_dir().expect("should get cwd"),
  }
}

fn compute_cache_key(root_cache_key: Option<String>, cache_key_file_hashes: &[String]) -> Option<String> {
  match (root_cache_key, compute_cache_key_files_hash(cache_key_file_hashes)) {
    (Some(root), Some(files)) => Some(format!("{}{}", root, files)),
    (Some(root), None) => Some(root),
    (None, Some(files)) => Some(files),
    (None, None) => None,
  }
}

fn compute_cache_key_files_hash(cache_key_file_hashes: &[String]) -> Option<String> {
  if cache_key_file_hashes.is_empty() {
    return None;
  }

  let mut hasher = Sha256::new();
  for file_hash in cache_key_file_hashes {
    hasher.update(file_hash);
  }
  Some(format!("{:x}", hasher.finalize()))
}

#[cfg(test)]
mod tests {
  #[test]
  fn the_schemas_defaults_are_what_an_empty_configuration_resolves_to() {
    use kprint_config::exec::input::ExecCommandInput;
    use kprint_config::exec::input::ExecConfigInput;
    use kprint_config::exec::input::PlayWithFire;
    use kprint_config::exec::input::StringOrStrings;
    use kprint_config::exec::input::exec_config_schema;
    let schema: serde_json::Value = serde_json::from_str(exec_config_schema()).unwrap();
    let resolved = Configuration::resolve(
      kprint_config::to_values(&ExecConfigInput {
        commands: Some(vec![ExecCommandInput {
          command: "fmt".into(),
          exts: Some(StringOrStrings::One("txt".to_string())),
          ..Default::default()
        }]),
        ..Default::default()
      })
      .unwrap(),
      &GlobalConfiguration::default(),
      &crate::environment::CanonicalizedPathBuf::new_for_testing("/"),
    );
    assert_eq!(resolved.diagnostics, Vec::new());
    assert_eq!(schema["properties"]["timeout"]["default"], serde_json::json!(resolved.config.timeout));
    assert_eq!(
      schema["properties"]["setupTimeout"]["default"],
      serde_json::json!(resolved.config.setup_timeout)
    );
    assert_eq!(
      schema["properties"]["commands"]["items"]["properties"]["stdin"]["default"],
      serde_json::json!(resolved.config.commands[0].stdin)
    );
    assert_eq!(
      schema["properties"]["playWithFire"]["default"],
      serde_json::to_value(PlayWithFire::default()).unwrap()
    );
    assert_eq!(kprint_config::to_values(&ExecConfigInput::default()).unwrap(), ConfigKeyMap::new());
  }

  use super::*;
  use kprint_configuration::ConfigKeyValue;
  use kprint_configuration::resolve_global_config;
  use pretty_assertions::assert_eq;
  use serde_json::json;

  #[test]
  fn handle_global_config() {
    let mut global_config = ConfigKeyMap::from([
      ("lineWidth".to_string(), ConfigKeyValue::from_i32(80)),
      ("indentWidth".to_string(), ConfigKeyValue::from_i32(8)),
      ("useTabs".to_string(), ConfigKeyValue::from_bool(true)),
    ]);
    let global_config = resolve_global_config(&mut global_config).config;
    let config = Configuration::resolve(
      ConfigKeyMap::new(),
      &global_config,
      &crate::environment::CanonicalizedPathBuf::new_for_testing("/"),
    )
    .config;
    assert_eq!(config.line_width, 80);
    assert_eq!(config.indent_width, 8);
    assert!(config.use_tabs);
  }

  #[test]
  fn general_test() {
    let unresolved_config = parse_config(json!({
      "cacheKey": "2",
      "timeout": 5
    }));
    let result = Configuration::resolve(
      unresolved_config,
      &Default::default(),
      &crate::environment::CanonicalizedPathBuf::new_for_testing("/"),
    );
    let config = result.config;
    assert_eq!(config.line_width, 120);
    assert_eq!(config.indent_width, 2);
    assert!(!config.use_tabs);
    assert_eq!(config.cache_key, "2");
    assert_eq!(config.timeout, 5);
    assert_eq!(
      result.diagnostics,
      vec![ConfigurationDiagnostic {
        property_name: "commands".to_string(),
        message: "Expected to find a \"commands\" array property (see https://github.com/dprint/dprint-plugin-exec for instructions)".to_string(),
      }]
    );
  }

  #[test]
  fn empty_command_name() {
    let config = parse_config(json!({
      "commands": [{
        "command": "",
      }],
    }));
    run_diagnostics_test(
      config,
      vec![ConfigurationDiagnostic {
        property_name: "commands[0].command".to_string(),
        message: "Expected to find a command name.".to_string(),
      }],
    )
  }

  #[test]
  fn cwd_test() {
    let unresolved_config = parse_config(json!({
      "cwd": "test-cwd",
      "commands": [{
        "command": "1"
      }, {
        "cwd": "test-cwd2",
        "command": "1"
      }]
    }));
    let result = Configuration::resolve(
      unresolved_config,
      &Default::default(),
      &crate::environment::CanonicalizedPathBuf::new_for_testing("/"),
    );
    let config = result.config;
    assert_eq!(config.commands[0].cwd, PathBuf::from("test-cwd"));
    assert_eq!(config.commands[1].cwd, PathBuf::from("test-cwd2"));
  }

  #[test]
  fn handle_associations_value() {
    let unresolved_config = parse_config(json!({
      "commands": [{
        "command": "command",
        "associations": ["**/*.rs"]
      }],
    }));
    let mut config = Configuration::resolve(
      unresolved_config,
      &Default::default(),
      &crate::environment::CanonicalizedPathBuf::new_for_testing("/"),
    )
    .config;
    assert!(config.commands.remove(0).associations.is_some());

    let unresolved_config = parse_config(json!({
      "commands": [{
        "command": "command",
        "associations": []
      }],
    }));
    let mut config = Configuration::resolve(
      unresolved_config,
      &Default::default(),
      &crate::environment::CanonicalizedPathBuf::new_for_testing("/"),
    )
    .config;
    assert!(config.commands.remove(0).associations.is_none());

    let unresolved_config = parse_config(json!({
      "commands": [{
        "command": "command",
        "associations": [
          "**/*.rs",
          "**/*.json",
        ]
      }],
    }));
    run_diagnostics_test(
      unresolved_config,
      vec![ConfigurationDiagnostic {
        property_name: "commands[0].associations".to_string(),
        message: "Unfortunately multiple globs haven't been implemented yet. Please provide a single glob or consider contributing this feature.".to_string(),
      }],
    );

    // what the value may be is the input's to say (see `input.rs`)
    for associations in [json!([true]), json!(true)] {
      let unresolved_config = parse_config(json!({
        "commands": [{
          "command": "command",
          "associations": associations
        }],
      }));
      run_diagnostics_test(
        unresolved_config,
        vec![ConfigurationDiagnostic {
          property_name: "commands[0].associations".to_string(),
          message: "Expected a glob or an array with one glob.".to_string(),
        }],
      );
    }
  }

  #[test]
  fn a_value_that_isnt_what_it_may_be_is_one_diagnostic_saying_where() {
    // nothing else is resolved from it, so the rest is the defaults
    let result = Configuration::resolve(
      parse_config(json!({ "timeout": 5, "commands": [{ "command": "fmt", "exts": ["txt"], "cwd": 1 }] })),
      &Default::default(),
      &crate::environment::CanonicalizedPathBuf::new_for_testing("/"),
    );
    assert_eq!(
      result.diagnostics,
      vec![ConfigurationDiagnostic {
        property_name: "commands[0].cwd".to_string(),
        message: "invalid type: integer `1`, expected a string".to_string(),
      }]
    );
    assert!(!result.config.is_valid);
    assert_eq!(result.config.timeout, 30);
    assert_eq!(result.config.commands.len(), 0);
  }

  #[test]
  fn reports_an_unknown_template_variable_in_the_configuration() {
    // the variable is `file_path`, which the configuration says before
    // anything is formatted
    let config: ConfigKeyMap = serde_json::from_value(serde_json::json!({ "commands": [{ "command": "cat {{filePath}}", "exts": ["txt"] }] })).unwrap();
    let result = Configuration::resolve(config, &Default::default(), &crate::environment::CanonicalizedPathBuf::new_for_testing("/"));
    assert_eq!(result.diagnostics.len(), 1);
    assert_eq!(result.diagnostics[0].property_name, "commands[0].command");
    assert!(
      result.diagnostics[0]
        .message
        .starts_with("Invalid template in argument '{{filePath}}': Unknown variable '{{filePath}}'."),
      "{}",
      result.diagnostics[0].message
    );
  }

  #[test]
  fn splits_commands_like_the_exec_plugin() {
    let cases: &[(&str, &[&str])] = &[
      // the tests of splitty 1.0.1 (https://github.com/Canop/splitty), which
      // the exec plugin split commands with, without the empty parts it
      // leaves out
      ("", &[]),
      ("    ", &[]),
      (" a    试bc d  ", &["a", "试bc", "d"]),
      ("e^iπ^ = 1", &["e^iπ^", "=", "1"]),
      ("1234", &["1234"]),
      ("1234\"", &["1234\""]),
      (r#"""#, &["\""]),
      (r#""a""#, &["a"]),
      (r#" " "#, &["\" "]),
      (r#"a  "deux mots" b"#, &["a", "deux mots", "b"]),
      (r#" " ""#, &[" "]),
      (r#" a  "2 * 试" x"x "z "#, &["a", "2 * 试", "x\"x", "\"z "]),
      (r#"""""#, &["\""]),
      (r#""""""#, &["\"\""]),
      // empty parts are left out
      (r#""""#, &[]),
      (r#"a "" b"#, &["a", "b"]),
      // only spaces separate parts
      ("a\tb c", &["a\tb", "c"]),
      // a part that starts with a quote ends at a quote followed by a space
      (r#""a b" c"#, &["a b", "c"]),
      (r#""a" "b c""#, &["a", "b c"]),
      (r#""a"" b"#, &["a\"", "b"]),
      (r#""a"b c"#, &["\"a\"b c"]),
      (r#""a b"#, &["\"a b"]),
      (
        r#"prettier --stdin-filepath "{{file_path}}""#,
        &["prettier", "--stdin-filepath", "{{file_path}}"],
      ),
      (
        "rustup toolchain install nightly-2025-09-01",
        &["rustup", "toolchain", "install", "nightly-2025-09-01"],
      ),
    ];
    for (command, parts) in cases {
      assert_eq!(kprint_config::exec::split_command(command), *parts, "{:?}", command);
    }
  }

  #[test]
  fn setup_command() {
    let unresolved_config = parse_config(json!({
      "commands": [{
        "command": "command",
        "exts": ["txt"],
        "setupCommand": "rustup toolchain install nightly-2025-09-01",
      }],
    }));
    let result = Configuration::resolve(
      unresolved_config,
      &Default::default(),
      &crate::environment::CanonicalizedPathBuf::new_for_testing("/"),
    );
    assert!(result.diagnostics.is_empty());
    let setup = result.config.commands[0].setup_command.as_ref().unwrap();
    assert_eq!(setup.executable, "rustup");
    assert_eq!(setup.args, vec!["toolchain", "install", "nightly-2025-09-01"]);
  }

  #[test]
  fn setup_command_empty() {
    let unresolved_config = parse_config(json!({
      "commands": [{
        "command": "command",
        "exts": ["txt"],
        "setupCommand": "",
      }],
    }));
    run_diagnostics_test(
      unresolved_config,
      vec![ConfigurationDiagnostic {
        property_name: "commands[0].setupCommand".to_string(),
        message: "Expected to find a command name.".to_string(),
      }],
    );
  }

  #[test]
  fn serves_a_command_with_one_worker_unless_it_says_more() {
    let commands = |workers: serde_json::Value| {
      let mut command = json!({ "command": "fmt", "exts": ["txt"], "serve": true });
      if !workers.is_null() {
        command["workers"] = workers;
      }
      let result = Configuration::resolve(
        parse_config(json!({ "commands": [command, { "command": "fmt", "exts": ["md"] }] })),
        &Default::default(),
        &crate::environment::CanonicalizedPathBuf::new_for_testing("/"),
      );
      assert_eq!(result.diagnostics, vec![]);
      result
        .config
        .commands
        .iter()
        .map(|command| command.serve.map(|serve| serve.workers.get()))
        .collect::<Vec<_>>()
    };
    assert_eq!(commands(serde_json::Value::Null), vec![Some(1), None]);
    assert_eq!(commands(json!(3)), vec![Some(3), None]);
  }

  #[test]
  fn reports_what_a_served_command_cant_have() {
    for (command, property, message) in [
      (
        json!({ "command": "fmt", "exts": ["txt"], "workers": 2 }),
        "commands[0].workers",
        "Only a served command has it. Set \"serve\": true to serve the command.",
      ),
      (
        json!({ "command": "fmt", "exts": ["txt"], "serve": true, "stdin": false }),
        "commands[0].serve",
        "A served command gets the files on stdin, so \"stdin\" can't be false.",
      ),
      (
        json!({ "command": "fmt --path {{file_path}}", "exts": ["txt"], "serve": true }),
        "commands[0].command",
        "A served command formats every file, so its arguments and environment can't use {{file_path}}. It gets each file's path in its request.",
      ),
      (
        json!({ "command": { "executable": "fmt", "env": { "FILE": "{{file_path}}" } }, "exts": ["txt"], "serve": true }),
        "commands[0].command",
        "A served command formats every file, so its arguments and environment can't use {{file_path}}. It gets each file's path in its request.",
      ),
    ] {
      run_diagnostics_test(
        parse_config(json!({ "commands": [command] })),
        vec![ConfigurationDiagnostic {
          property_name: property.to_string(),
          message: message.to_string(),
        }],
      );
    }
    let result = Configuration::resolve(
      parse_config(json!({ "commands": [{ "command": "fmt --width {{line_width}}", "exts": ["txt"], "serve": true }] })),
      &Default::default(),
      &crate::environment::CanonicalizedPathBuf::new_for_testing("/"),
    );
    assert_eq!(result.diagnostics, vec![]);
  }

  #[test]
  fn resolves_a_command_and_setup_command_given_as_an_array_or_object() {
    let result = Configuration::resolve(
      parse_config(json!({
        "commands": [{
          "command": { "executable": "/opt/My Tools/fmt", "args": ["--emit", "", "a b"], "env": { "RUST_LOG": "warn" } },
          "setupCommand": ["rustup", "component", "add", "rustfmt"],
          "exts": ["rs"]
        }, {
          "command": ["my fmt", "--width", "{{line_width}}"],
          "setupCommand": { "executable": "rustup", "env": { "RUSTUP_TOOLCHAIN": "stable" } },
          "exts": ["txt"]
        }]
      })),
      &Default::default(),
      &crate::environment::CanonicalizedPathBuf::new_for_testing("/"),
    );
    assert_eq!(result.diagnostics, vec![]);
    let strings = |values: &[&str]| values.iter().map(|value| value.to_string()).collect::<Vec<_>>();
    let env = |name: &str, value: &str| BTreeMap::from([(name.to_string(), value.to_string())]);
    let [rust, text] = &result.config.commands[..] else {
      panic!("two commands");
    };
    assert_eq!(rust.executable, "/opt/My Tools/fmt");
    assert_eq!(rust.args, strings(&["--emit", "", "a b"]));
    assert_eq!(rust.env, env("RUST_LOG", "warn"));
    let setup = rust.setup_command.as_ref().unwrap();
    assert_eq!(
      (setup.executable.as_str(), &setup.args, &setup.env),
      ("rustup", &strings(&["component", "add", "rustfmt"]), &BTreeMap::new())
    );
    assert_eq!(
      (text.executable.as_str(), &text.args, &text.env),
      ("my fmt", &strings(&["--width", "{{line_width}}"]), &BTreeMap::new())
    );
    let setup = text.setup_command.as_ref().unwrap();
    assert_eq!(
      (setup.executable.as_str(), &setup.args, &setup.env),
      ("rustup", &Vec::new(), &env("RUSTUP_TOOLCHAIN", "stable"))
    );
  }

  #[test]
  fn reports_a_command_without_a_program() {
    for (command, property) in [
      (json!({ "command": [], "exts": ["txt"] }), "commands[0].command"),
      (json!({ "command": [""], "exts": ["txt"] }), "commands[0].command"),
      (
        json!({ "command": { "executable": "", "args": ["a"] }, "exts": ["txt"] }),
        "commands[0].command",
      ),
      (json!({ "command": "fmt", "setupCommand": [], "exts": ["txt"] }), "commands[0].setupCommand"),
      (
        json!({ "command": "fmt", "setupCommand": { "executable": "" }, "exts": ["txt"] }),
        "commands[0].setupCommand",
      ),
    ] {
      run_diagnostics_test(
        parse_config(json!({ "commands": [command] })),
        vec![ConfigurationDiagnostic {
          property_name: property.to_string(),
          message: "Expected to find a command name.".to_string(),
        }],
      );
    }
  }

  #[test]
  fn reports_an_unknown_variable_in_a_commands_environment() {
    let result = Configuration::resolve(
      parse_config(json!({ "commands": [{ "command": { "executable": "fmt", "env": { "WIDTH": "{{lineWidth}}" } }, "exts": ["txt"] }] })),
      &Default::default(),
      &crate::environment::CanonicalizedPathBuf::new_for_testing("/"),
    );
    assert_eq!(result.diagnostics.len(), 1, "{:?}", result.diagnostics);
    assert_eq!(result.diagnostics[0].property_name, "commands[0].command");
    assert!(
      result.diagnostics[0]
        .message
        .starts_with("Invalid template in environment variable 'WIDTH': Unknown variable '{{lineWidth}}'."),
      "{}",
      result.diagnostics[0].message
    );
  }

  #[test]
  fn a_commands_environment_is_part_of_its_resolved_configuration() {
    let serialized = |command: serde_json::Value| {
      let result = Configuration::resolve(
        parse_config(json!({ "commands": [{ "command": command, "setupCommand": command, "exts": ["txt"] }] })),
        &Default::default(),
        &crate::environment::CanonicalizedPathBuf::new_for_testing("/"),
      );
      assert_eq!(result.diagnostics, vec![]);
      serde_json::to_value(result.config).unwrap()
    };
    let with_env = |value: &str| serialized(json!({ "executable": "fmt", "env": { "A": value } }));
    assert_ne!(with_env("1"), with_env("2"));
    assert_eq!(with_env("1")["commands"][0]["env"], json!({ "A": "1" }));
    assert_eq!(with_env("1")["commands"][0]["setupCommand"]["env"], json!({ "A": "1" }));
    let line = serialized(json!("fmt"));
    assert_eq!(line, serialized(json!(["fmt"])));
    assert_eq!(line["commands"][0].get("env"), None);
  }

  #[track_caller]
  fn run_diagnostics_test(config: ConfigKeyMap, expected_diagnostics: Vec<ConfigurationDiagnostic>) {
    let result = Configuration::resolve(config, &Default::default(), &crate::environment::CanonicalizedPathBuf::new_for_testing("/"));
    assert_eq!(result.diagnostics, expected_diagnostics);
    assert!(!result.config.is_valid);
  }

  fn parse_config(value: serde_json::Value) -> ConfigKeyMap {
    serde_json::from_value(value).unwrap()
  }

  mod cache_key {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn default_cache_key() {
      let unresolved_config = parse_config(json!({
        "commands": [{
          "exts": ["txt"],
          "command": "1"
        }],
      }));
      let result = Configuration::resolve(
        unresolved_config,
        &Default::default(),
        &crate::environment::CanonicalizedPathBuf::new_for_testing("/"),
      );
      let config = result.config;
      assert!(result.diagnostics.is_empty());
      assert_eq!(config.cache_key, "0");
    }

    #[test]
    fn top_level_cache_key() {
      let unresolved_config = parse_config(json!({
        "cacheKey": "99",
        "commands": [{
          "exts": ["txt"],
          "command": "1"
        }],
      }));
      let result = Configuration::resolve(
        unresolved_config,
        &Default::default(),
        &crate::environment::CanonicalizedPathBuf::new_for_testing("/"),
      );
      assert!(result.diagnostics.is_empty());
      let config = result.config;
      assert_eq!(config.cache_key, "99");
    }

    #[test]
    fn top_level_cache_key_plus_command_cache_key_is_allowed() {
      let unresolved_config = parse_config(json!({
        "cacheKey": "99",
        "commands": [{
          "exts": ["txt"],
          "command": "1",
          "cacheKeyFiles": ["./src/plugins/implementations/builtin_exec/testdata/one-line.txt"]
        }],
      }));
      let result = Configuration::resolve(
        unresolved_config,
        &Default::default(),
        &crate::environment::CanonicalizedPathBuf::new_for_testing("/"),
      );
      assert!(result.config.is_valid);
      assert_eq!(result.diagnostics, vec![]);
      assert_eq!(result.config.cache_key, "99c7b3af761ad02238e72bf5a60c94be2f41eec6637ec3ec1bfa853a3a1fb91225");
    }

    #[test]
    fn command_cache_key_fails_if_file_does_not_exist() {
      let unresolved_config = parse_config(json!({
        "commands": [{
          "exts": ["txt"],
          "command": "1",
          "cacheKeyFiles": ["path/to/missing/file"]
        }],
      }));
      let result = Configuration::resolve(
        unresolved_config,
        &Default::default(),
        &crate::environment::CanonicalizedPathBuf::new_for_testing("/"),
      );
      assert!(!result.config.is_valid);
      assert_eq!(result.diagnostics.len(), 1);
      assert_eq!(result.diagnostics[0].property_name, "commands[0].cacheKeyFiles");
      assert!(result.diagnostics[0].message.starts_with("Unable to read file"));
    }

    #[test]
    fn command_cache_key_one_command_one_file() {
      let unresolved_config = parse_config(json!({
        "commands": [{
          "exts": ["txt"],
          "command": "1",
          "cacheKeyFiles": [
            "./src/plugins/implementations/builtin_exec/testdata/one-line.txt"
          ]
        }],
      }));
      let result = Configuration::resolve(
        unresolved_config,
        &Default::default(),
        &crate::environment::CanonicalizedPathBuf::new_for_testing("/"),
      );
      assert!(result.diagnostics.is_empty());
      let config = result.config;
      assert_eq!(config.cache_key, "c7b3af761ad02238e72bf5a60c94be2f41eec6637ec3ec1bfa853a3a1fb91225");
    }

    #[test]
    fn command_cache_key_one_command_multiple_files() {
      let unresolved_config = parse_config(json!({
        "commands": [{
          "exts": ["txt"],
          "command": "1",
          "cacheKeyFiles": [
            "./src/plugins/implementations/builtin_exec/testdata/one-line.txt",
            "./src/plugins/implementations/builtin_exec/testdata/multi-line.txt",
          ]
        }],
      }));
      let result = Configuration::resolve(
        unresolved_config,
        &Default::default(),
        &crate::environment::CanonicalizedPathBuf::new_for_testing("/"),
      );
      assert!(result.diagnostics.is_empty());
      let config = result.config;
      assert_eq!(config.cache_key, "4321f2e747210582553e6ad8ef5b866d87c357a039cd09cdbdab6ebe33517c1a");
    }

    #[test]
    fn command_cache_key_multiple_commands() {
      let unresolved_config = parse_config(json!({
        "commands": [
          {
            "exts": ["txt"],
            "command": "1",
            "cacheKeyFiles": [
              "./src/plugins/implementations/builtin_exec/testdata/one-line.txt",
              "./src/plugins/implementations/builtin_exec/testdata/multi-line.txt",
            ]
          },
          {
            "exts": ["txt"],
            "command": "2",
            "cacheKeyFiles": [
              "./src/plugins/implementations/builtin_exec/testdata/one-line.txt",
              "./src/plugins/implementations/builtin_exec/testdata/multi-line.txt",
            ]
          },
        ],
      }));
      let result = Configuration::resolve(
        unresolved_config,
        &Default::default(),
        &crate::environment::CanonicalizedPathBuf::new_for_testing("/"),
      );
      assert!(result.diagnostics.is_empty());
      let config = result.config;
      assert_eq!(config.cache_key, "51eaf161463bb6ba4957327330e27a80d039b7d2c0c27590ebdf844e7eca954a");
    }
  }
}
