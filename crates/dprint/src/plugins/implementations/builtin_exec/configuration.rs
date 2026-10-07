use dprint_core::configuration::ConfigKeyMap;
use dprint_core::configuration::ConfigurationDiagnostic;
use dprint_core::configuration::GlobalConfiguration;
use dprint_core::configuration::RECOMMENDED_GLOBAL_CONFIGURATION;
use dprint_core::configuration::ResolveConfigurationResult;
use globset::GlobMatcher;
use serde::Serialize;
use serde::Serializer;
use sha2::Digest;
use sha2::Sha256;
use std::fs::read_to_string;
use std::path::Path;
use std::path::PathBuf;

use super::input::Associations;
use super::input::ExecCommandInput;
use super::input::ExecConfigInput;
use super::input::default_setup_timeout;
use super::input::default_stdin;
use super::input::default_timeout;
use super::template::validate_template;

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
  pub cwd: PathBuf,
  pub stdin: bool,
  #[serde(serialize_with = "serialize_glob")]
  pub associations: Option<GlobMatcher>,
  pub file_extensions: Vec<String>,
  pub file_names: Vec<String>,
  pub cache_key_files_hash: Option<String>,
  /// Command to run once before this command formats its first file.
  pub setup_command: Option<SetupCommand>,
}

/// A command run a single time before formatting begins (ex. to install a
/// toolchain so that parallel formatting doesn't race installing it).
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SetupCommand {
  pub executable: String,
  /// Executable arguments to add
  pub args: Vec<String>,
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

fn serialize_glob<S: Serializer>(value: &Option<GlobMatcher>, s: S) -> Result<S::Ok, S::Error> {
  match value {
    Some(value) => s.serialize_str(value.glob().glob()),
    None => s.serialize_none(),
  }
}

impl Configuration {
  /// Resolves the plugin's configuration: the values are read as what the
  /// configuration may hold (see [`ExecConfigInput`], which is also what its
  /// schema describes), and that is resolved to what formatting runs with.
  /// A value that isn't what it may be is a diagnostic saying where, and
  /// nothing more is resolved from it.
  pub fn resolve(config: ConfigKeyMap, global_config: &GlobalConfiguration) -> ResolveConfigurationResult<Configuration> {
    let (input, mut diagnostics) = match dprint_config_model::from_values::<ExecConfigInput>(config) {
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
          let (command_config, command_diagnostics) = resolve_command(command, input.cwd.as_deref());
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
fn resolve_command(command: ExecCommandInput, root_cwd: Option<&str>) -> (Option<CommandConfiguration>, Vec<ConfigurationDiagnostic>) {
  let names = ExecCommandInput::property_names();
  let mut diagnostics = Vec::new();
  let mut parts = split_command(&command.command);
  if parts.is_empty() {
    diagnostics.push(ConfigurationDiagnostic {
      property_name: names.command.clone(),
      message: "Expected to find a command name.".to_string(),
    });
    return (None, diagnostics);
  }

  for arg in parts.iter().skip(1) {
    if let Err(err) = validate_template(arg) {
      diagnostics.push(ConfigurationDiagnostic {
        property_name: names.command.clone(),
        message: format!("Invalid template in argument '{}': {}", arg, err),
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

  let setup_command = command.setup_command.and_then(|raw| resolve_setup_command(&raw, &mut diagnostics));

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
  let associations = associations.and_then(|glob| {
    let mut builder = globset::GlobBuilder::new(&glob);
    builder.case_insensitive(cfg!(windows));
    match builder.build() {
      Ok(glob) => Some(glob.compile_matcher()),
      Err(err) => {
        diagnostics.push(ConfigurationDiagnostic {
          message: format!("Error parsing associations glob: {:#}", err),
          property_name: names.associations.clone(),
        });
        None
      }
    }
  });

  let config = CommandConfiguration {
    executable: parts.remove(0),
    args: parts,
    setup_command,
    associations,
    cwd,
    stdin: command.stdin.unwrap_or_else(default_stdin),
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

/// Splits a command into its program and arguments the way the exec plugin
/// always has (with splitty 1.0.1):
///
/// - Parts are separated by spaces. Other whitespace (ex. a tab) is part of
///   a part.
/// - A part that starts with a quote (`"`) goes on to a quote followed by a
///   space or the end, and doesn't include those quotes, so it may contain
///   spaces. Without such a quote, it's the rest of the command as is.
/// - Quotes anywhere else are kept, and empty parts (ex. `""`) are left out.
pub fn split_command(command: &str) -> Vec<String> {
  let mut parts = Vec::new();
  let mut rest = command.trim_start_matches(' ');
  while !rest.is_empty() {
    let (part, after) = split_first_part(rest);
    if !part.is_empty() {
      parts.push(part.to_string());
    }
    rest = after.trim_start_matches(' ');
  }
  parts
}

/// The first part of a command that doesn't start with a space, and what's
/// after it.
fn split_first_part(text: &str) -> (&str, &str) {
  if let Some(quoted) = text.strip_prefix('"') {
    return match quoted.find("\" ") {
      Some(end) => (&quoted[..end], &quoted[end + 1..]),
      None => match quoted.strip_suffix('"') {
        Some(part) => (part, ""),
        // an opening quote without a closing one is kept, as is what follows
        None => (text, ""),
      },
    };
  }
  match text.find(' ') {
    Some(end) => (&text[..end], &text[end..]),
    None => (text, ""),
  }
}

fn resolve_setup_command(raw: &str, diagnostics: &mut Vec<ConfigurationDiagnostic>) -> Option<SetupCommand> {
  let mut parts = split_command(raw);
  if parts.is_empty() {
    diagnostics.push(ConfigurationDiagnostic {
      property_name: "setupCommand".to_string(),
      message: "Expected to find a command name.".to_string(),
    });
    return None;
  }
  Some(SetupCommand {
    executable: parts.remove(0),
    args: parts,
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
  use super::*;
  use dprint_core::configuration::ConfigKeyValue;
  use dprint_core::configuration::resolve_global_config;
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
    let config = Configuration::resolve(ConfigKeyMap::new(), &global_config).config;
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
    let result = Configuration::resolve(unresolved_config, &Default::default());
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
    let result = Configuration::resolve(unresolved_config, &Default::default());
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
    let mut config = Configuration::resolve(unresolved_config, &Default::default()).config;
    assert!(config.commands.remove(0).associations.is_some());

    let unresolved_config = parse_config(json!({
      "commands": [{
        "command": "command",
        "associations": []
      }],
    }));
    let mut config = Configuration::resolve(unresolved_config, &Default::default()).config;
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
    let result = Configuration::resolve(config, &Default::default());
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
      assert_eq!(split_command(command), *parts, "{:?}", command);
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
    let result = Configuration::resolve(unresolved_config, &Default::default());
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

  #[track_caller]
  fn run_diagnostics_test(config: ConfigKeyMap, expected_diagnostics: Vec<ConfigurationDiagnostic>) {
    let result = Configuration::resolve(config, &Default::default());
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
      let result = Configuration::resolve(unresolved_config, &Default::default());
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
      let result = Configuration::resolve(unresolved_config, &Default::default());
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
      let result = Configuration::resolve(unresolved_config, &Default::default());
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
      let result = Configuration::resolve(unresolved_config, &Default::default());
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
      let result = Configuration::resolve(unresolved_config, &Default::default());
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
      let result = Configuration::resolve(unresolved_config, &Default::default());
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
      let result = Configuration::resolve(unresolved_config, &Default::default());
      assert!(result.diagnostics.is_empty());
      let config = result.config;
      assert_eq!(config.cache_key, "51eaf161463bb6ba4957327330e27a80d039b7d2c0c27590ebdf844e7eca954a");
    }
  }
}
