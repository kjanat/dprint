use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use deno_terminal::colors;
use std::ffi::OsString;
use std::path::Path;
use std::path::PathBuf;

use crate::arg_parser::CliArgs;
use crate::arg_parser::ConfigArg;
use crate::arg_parser::ConfigDiscovery;
use crate::arg_parser::SubCommand;
use crate::environment::CanonicalizedPathBuf;
use crate::environment::Environment;
use crate::environment::PathKind;
use crate::utils::PathSource;
use crate::utils::ResolvedFilePathWithText;
use crate::utils::ResolvedFilePathWithTextRef;
use crate::utils::resolve_path_source_to_file_with_cache;
use crate::utils::resolve_url_or_file_path_to_path_source;

pub static POSSIBLE_CONFIG_FILE_NAMES: [&str; 4] = ["dprint.json", "dprint.jsonc", ".dprint.json", ".dprint.jsonc"];

#[derive(Debug)]
pub struct ResolvedConfigPathWithText {
  pub source: PathSource,
  pub is_first_download: bool,
  pub content: String,
  pub base_path: CanonicalizedPathBuf,
  pub is_global_config: bool,
}

impl ResolvedConfigPathWithText {
  pub fn as_file_path_with_text_ref(&self) -> ResolvedFilePathWithTextRef<'_> {
    ResolvedFilePathWithTextRef {
      content: &self.content,
      source: &self.source,
    }
  }
}

pub async fn resolve_main_config_path_and_bytes<TEnvironment: Environment>(
  args: &CliArgs,
  environment: &TEnvironment,
) -> Result<Option<ResolvedConfigPathWithText>> {
  fn get_default_paths(args: &CliArgs, environment: &impl Environment) -> Result<Option<ResolvedConfigPathWithText>> {
    let start_search_dir = get_start_search_directory(args, environment)?;
    let maybe_config_file = get_config_file_in_dir(&start_search_dir, environment)?;

    if let Some((path, content)) = maybe_config_file {
      let path = environment.canonicalize(path)?;
      Ok(Some(ResolvedConfigPathWithText {
        source: PathSource::new_local(path),
        is_first_download: false,
        content,
        base_path: start_search_dir,
        is_global_config: false,
      }))
    } else {
      get_default_config_file_in_ancestor_directories(environment, environment.cwd().as_ref())
    }
  }

  fn get_start_search_directory(args: &CliArgs, environment: &impl Environment) -> std::io::Result<CanonicalizedPathBuf> {
    if let SubCommand::StdInFmt(command) = &args.sub_command {
      // When formatting via stdin, resolve the config file based on the
      // file path provided to the command. This is done for people who
      // format files in their editor.
      if environment.is_absolute_path(&command.file_name_or_path)
        && let Some(parent) = PathBuf::from(&command.file_name_or_path).parent()
      {
        return environment.canonicalize(parent);
      }
    }

    Ok(environment.cwd())
  }

  let config_discovery = args.config_discovery(environment);
  if let Some(config) = &args.config {
    let base_path = environment.cwd();
    let resolved_file = match config {
      ConfigArg::Text(_) => read_config_arg(config, &base_path, args, environment).await?,
      // a value that opens like a json object but was resolved as a path is
      // rarely what was meant, so say so rather than only naming the path
      ConfigArg::PathOrUrl(value) => read_config_arg(config, &base_path, args, environment)
        .await
        .map_err(|err| add_config_text_hint(err, value))?,
    };
    Ok(Some(ResolvedConfigPathWithText {
      content: resolved_file.content,
      source: resolved_file.source,
      is_first_download: resolved_file.is_first_download,
      base_path,
      is_global_config: false,
    }))
  } else if matches!(config_discovery, ConfigDiscovery::Global) {
    resolve_global_config_path_or_error(environment).map(Some)
  } else if config_discovery.traverse_ancestors()
    && let Some(path) = get_default_paths(args, environment)?
  {
    Ok(Some(path))
  } else if matches!(config_discovery, ConfigDiscovery::Default)
    && args.plugins.is_empty()
    && let ResolveGlobalConfigPathResult::Found(path) = resolve_global_config_path_and_text_detail(environment)?
  {
    Ok(Some(path))
  } else {
    Ok(None)
  }
}

async fn read_config_arg<TEnvironment: Environment>(
  config: &ConfigArg,
  base_path: &CanonicalizedPathBuf,
  args: &CliArgs,
  environment: &TEnvironment,
) -> Result<ResolvedFilePathWithText> {
  // work out where the configuration comes from before reading any of it:
  // reading a pipe blocks until it's written to, so a sub command that can't
  // accept one has to turn it away first
  let config_source = resolve_config_arg_source(config, base_path, environment)?;
  if let Some(display) = config_source.virtual_display()
    && sub_command_needs_config_file(&args.sub_command)
  {
    bail!("{}", config_needs_file_message(display));
  }

  Ok(match config_source {
    ConfigArgSource::Text { text, display } => ResolvedFilePathWithText {
      content: text,
      source: virtual_config_source(base_path, &display),
      is_first_download: false,
    },
    ConfigArgSource::Stream { path, display } => {
      log_debug!(environment, "Reading the config from a stream at {}", display);
      let bytes = environment.read_file_bytes(&path)?;
      let content = String::from_utf8(bytes).with_context(|| format!("Failed converting '{}' to string.", display))?;
      ResolvedFilePathWithText {
        content,
        source: virtual_config_source(base_path, &display),
        is_first_download: false,
      }
    }
    ConfigArgSource::File(path_source) => resolve_path_source_to_file_with_cache(path_source, environment).await?.into_text()?,
  })
}

/// Whether the sub command needs a configuration file rather than one-shot
/// text: it either writes the file back, or runs long enough to read it again.
pub fn sub_command_needs_config_file(sub_command: &SubCommand) -> bool {
  matches!(sub_command, SubCommand::Config(_) | SubCommand::EditorService(_) | SubCommand::Lsp)
}

/// Told to a sub command that was handed configuration it can't use.
pub fn config_needs_file_message(display: &str) -> String {
  format!(
    concat!(
      "Cannot use the configuration provided by --config ({}) with this sub command because it needs a configuration ",
      "file it can read again or write back to. Specify a file path instead (ex. --config dprint.json)."
    ),
    display,
  )
}

/// Explains the text heuristic when a value that opens like a json object was
/// resolved as a path and didn't work out.
fn add_config_text_hint(err: anyhow::Error, config: &str) -> anyhow::Error {
  if !config.trim_start().starts_with('{') {
    return err;
  }
  anyhow::anyhow!(
    concat!(
      "{:#}\n\nThe --config value is only read as the configuration itself when it starts with `{{` and ends with ",
      "`}}`. Use --config - to read the configuration from stdin instead."
    ),
    err,
  )
}

/// Where the configuration named by `--config` is going to come from, worked
/// out before any of it is read.
enum ConfigArgSource {
  /// Text provided inline or already read from stdin.
  Text { text: String, display: String },
  /// A regular file or a url, read the usual way.
  File(PathSource),
  /// A pipe: a fifo, or something like the `/dev/fd/63` of a `<(...)` process
  /// substitution that can be read but not canonicalized.
  Stream { path: PathBuf, display: String },
}

impl ConfigArgSource {
  /// How to describe the configuration when it didn't come from a file dprint
  /// could also write back to, or `None` when it did.
  fn virtual_display(&self) -> Option<&str> {
    match self {
      ConfigArgSource::Text { display, .. } | ConfigArgSource::Stream { display, .. } => Some(display),
      ConfigArgSource::File(_) => None,
    }
  }
}

fn resolve_config_arg_source(config: &ConfigArg, cwd: &CanonicalizedPathBuf, environment: &impl Environment) -> Result<ConfigArgSource> {
  let config = match config {
    ConfigArg::Text(config) => {
      return Ok(ConfigArgSource::Text {
        text: config.text.clone(),
        display: config.origin.clone(),
      });
    }
    ConfigArg::PathOrUrl(config) => config,
  };

  match resolve_url_or_file_path_to_path_source(config, &PathSource::new_local(cwd.clone()), environment) {
    Ok(path_source) => Ok(match path_source.maybe_local_path() {
      // a fifo has an ordinary path that canonicalizes, but its text came from
      // whatever wrote to it rather than from that directory
      Some(path) if is_stream_path(environment, path.as_ref()) => ConfigArgSource::Stream {
        display: path.display().to_string(),
        path: path.as_ref().to_path_buf(),
      },
      _ => ConfigArgSource::File(path_source),
    }),
    // a pipe with no path of its own (the `/dev/fd/63` of a process substitution)
    // can be stat'd and read, but not canonicalized. A regular file that can't
    // be canonicalized (ex. on an unusual file system) isn't a stream though,
    // so it keeps the error rather than losing its directory
    Err(err) => match resolve_uncanonicalized_local_path(config, cwd, environment) {
      Some(path) if is_stream_path(environment, &path) => Ok(ConfigArgSource::Stream {
        display: path.display().to_string(),
        path,
      }),
      _ => Err(err),
    },
  }
}

/// Whether the path names something to read as a stream rather than an ordinary
/// file. A directory isn't a regular file either, but it isn't a stream: it
/// should keep failing on the read the way it always has.
pub fn is_stream_path(environment: &impl Environment, path: &Path) -> bool {
  environment.path_exists(path) && !environment.path_is_file(path) && !matches!(environment.path_kind(path), Some(PathKind::Dir))
}

/// The source used for configuration text that didn't come from a file
/// dprint opened by path (provided inline, on stdin, or through a pipe).
///
/// There's no configuration directory in that case, so relative paths within
/// the configuration (`extends`, plugin paths and `${configDir}`) resolve
/// against the current working directory, as if the configuration were a
/// file sitting in it.
fn virtual_config_source(cwd: &CanonicalizedPathBuf, origin: &str) -> PathSource {
  PathSource::new_local_virtual(cwd.join_panic_relative(VIRTUAL_CONFIG_FILE_NAME), origin.to_string())
}

/// A name that can't collide with a real configuration file in the directory,
/// since nothing should read or write it.
const VIRTUAL_CONFIG_FILE_NAME: &str = "<config>";

/// Makes a local `--config` value absolute without canonicalizing it, or
/// `None` when it doesn't name a local path at all.
fn resolve_uncanonicalized_local_path(config: &str, cwd: &CanonicalizedPathBuf, environment: &impl Environment) -> Option<PathBuf> {
  if let Some(rest) = config.strip_prefix("~/") {
    return Some(environment.get_home_dir()?.join(rest));
  }
  if let Ok(url) = url::Url::parse(config) {
    // a single letter scheme is a Windows drive rather than a url (ex. `C:/config.json`)
    if url.scheme().len() > 1 {
      return if url.scheme() == "file" { url.to_file_path().ok() } else { None };
    }
  }
  Some(cwd.join(config))
}

fn resolve_global_config_path_or_error(environment: &impl Environment) -> Result<ResolvedConfigPathWithText> {
  match resolve_global_config_path_and_text_detail(environment)? {
    ResolveGlobalConfigPathResult::Found(resolved_config_path) => Ok(resolved_config_path),
    ResolveGlobalConfigPathResult::NotFound => anyhow::bail!("Could not find global dprint.json file. Create one with `dprint init --global`"),
    ResolveGlobalConfigPathResult::FailedResolvingSystemDir(err) => Err(anyhow::Error::from(err).context(concat!(
      "Could not find system config directory. ",
      "Maybe specify the DPRINT_CONFIG_DIR environment ",
      "variable to say where to store the global dprint configuration file."
    ))),
  }
}

pub fn resolve_global_config_path_and_text(environment: &impl Environment) -> std::io::Result<Option<ResolvedConfigPathWithText>> {
  match resolve_global_config_path_and_text_detail(environment)? {
    ResolveGlobalConfigPathResult::Found(resolved_config_text) => Ok(Some(resolved_config_text)),
    ResolveGlobalConfigPathResult::NotFound | ResolveGlobalConfigPathResult::FailedResolvingSystemDir { .. } => Ok(None),
  }
}

enum ResolveGlobalConfigPathResult {
  Found(ResolvedConfigPathWithText),
  NotFound,
  FailedResolvingSystemDir(std::io::Error),
}

fn resolve_global_config_path_and_text_detail(environment: &impl Environment) -> std::io::Result<ResolveGlobalConfigPathResult> {
  let global_folder = match resolve_global_config_dir(environment) {
    Ok(dir) => dir,
    Err(err) => return Ok(ResolveGlobalConfigPathResult::FailedResolvingSystemDir(err)),
  };
  for name in ["dprint.jsonc", "dprint.json"] {
    let file_path = global_folder.join_panic_relative(name);
    if let Some(content) = environment.maybe_read_file(&file_path)? {
      return Ok(ResolveGlobalConfigPathResult::Found(ResolvedConfigPathWithText {
        source: PathSource::new_local(file_path),
        is_first_download: false,
        content,
        base_path: environment.cwd(),
        is_global_config: true,
      }));
    }
  }
  Ok(ResolveGlobalConfigPathResult::NotFound)
}

pub fn resolve_global_config_dir(environment: &impl Environment) -> std::io::Result<CanonicalizedPathBuf> {
  if let Some(folder) = resolve_env_var_folder(environment, "DPRINT_CONFIG_DIR")
    && let Ok(folder) = resolve_or_create_folder(environment, &folder).inspect_err(|err| {
      log_warn!(
        environment,
        "{} Could not resolve DPRINT_CONFIG_DIR value '{}'. Falling back to system configuration directory.",
        colors::yellow("Warning"),
        folder.display(),
      );
      log_debug!(environment, "Reason: {:#}", err);
    })
  {
    Ok(folder)
  } else {
    match resolve_system_config_dir(environment) {
      Some(dir) => resolve_or_create_folder(environment, dir.join("dprint")),
      None => Err(std::io::Error::new(std::io::ErrorKind::NotFound, "Could not find system config directory.")),
    }
  }
}

fn resolve_system_config_dir(environment: &impl Environment) -> Option<PathBuf> {
  if environment.os() == "macos" {
    resolve_mac_system_config_dir(environment)
  } else {
    environment.get_config_dir()
  }
}

/// For macOS, use XDG_CONFIG_HOME if available, otherwise check if the system config directory
/// exists, but fall back to $HOME/.config if it doesn't
fn resolve_mac_system_config_dir(environment: &impl Environment) -> Option<PathBuf> {
  // first, try XDG_CONFIG_HOME
  if let Some(xdg_config_home) = resolve_env_var_folder(environment, "XDG_CONFIG_HOME") {
    return Some(PathBuf::from(xdg_config_home));
  }

  // second, check if the system config directory exists
  if let Some(config_dir) = environment.get_config_dir() {
    // check if the dprint sub dir exists
    let dprint_config = config_dir.join("dprint");
    if environment.path_exists(&dprint_config) {
      return Some(config_dir);
    }
  }

  // fall back and prefer $HOME/.config
  if let Some(home_dir) = environment.get_home_dir() {
    return Some(home_dir.join(".config"));
  }

  None
}

fn resolve_env_var_folder(environment: &impl Environment, name: &str) -> Option<OsString> {
  environment.env_var(name).filter(|f| !f.is_empty())
}

fn resolve_or_create_folder(environment: &impl Environment, path: impl AsRef<Path>) -> std::io::Result<CanonicalizedPathBuf> {
  match environment.canonicalize(path.as_ref()) {
    Ok(path) => Ok(path),
    Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
      if let (Some(parent), Some(filename)) = (path.as_ref().parent(), path.as_ref().file_name()) {
        _ = environment.mk_dir_all(parent);
        Ok(environment.canonicalize(parent)?.join_panic_relative(filename.to_string_lossy()))
      } else {
        Err(err)
      }
    }
    Err(err) => Err(err),
  }
}

pub fn get_default_config_file_in_ancestor_directories(environment: &impl Environment, start_dir: &Path) -> Result<Option<ResolvedConfigPathWithText>> {
  for ancestor_dir in start_dir.ancestors() {
    if let Some((ancestor_config_path, content)) = get_config_file_in_dir(ancestor_dir, environment)? {
      return Ok(Some(ResolvedConfigPathWithText {
        source: PathSource::new_local(environment.canonicalize(ancestor_config_path)?),
        is_first_download: false,
        content,
        base_path: environment.canonicalize(ancestor_dir)?,
        is_global_config: false,
      }));
    }
  }

  Ok(None)
}

fn get_config_file_in_dir(dir: impl AsRef<Path>, environment: &impl Environment) -> std::io::Result<Option<(PathBuf, String)>> {
  for file_name in &POSSIBLE_CONFIG_FILE_NAMES {
    let config_path = dir.as_ref().join(file_name);
    if let Some(text) = environment.maybe_read_file(&config_path)? {
      return Ok(Some((config_path, text)));
    }
  }
  Ok(None)
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::environment::TestEnvironment;

  #[test]
  fn test_sub_command_needs_config_file() {
    // these either write the configuration file back or run long enough to
    // read it again, so one-shot text and pipes are no good to them
    assert!(sub_command_needs_config_file(&SubCommand::Config(crate::arg_parser::ConfigSubCommand::Edit)));
    assert!(sub_command_needs_config_file(&SubCommand::Lsp));
    assert!(sub_command_needs_config_file(&SubCommand::EditorService(
      crate::arg_parser::EditorServiceSubCommand { parent_pid: 1 }
    )));
    assert!(!sub_command_needs_config_file(&SubCommand::EditorInfo));
    assert!(!sub_command_needs_config_file(&SubCommand::Version));
  }

  #[test]
  fn test_resolve_system_config_dir_macos_with_xdg_config_home() {
    let env = TestEnvironment::new();
    env.set_os("macos");
    env.set_env_var("XDG_CONFIG_HOME", Some("/custom/xdg/config"));

    let result = resolve_system_config_dir(&env);
    assert_eq!(result, Some(PathBuf::from("/custom/xdg/config")));
  }

  #[test]
  fn test_resolve_system_config_dir_macos_with_empty_xdg_config_home() {
    let env = TestEnvironment::new();
    env.set_os("macos");
    env.set_env_var("XDG_CONFIG_HOME", Some(""));

    // Empty XDG_CONFIG_HOME should be ignored, fall back to $HOME/.config
    let result = resolve_system_config_dir(&env);
    assert_eq!(result, Some(PathBuf::from("/home/.config")));
  }

  #[test]
  fn test_resolve_system_config_dir_macos_with_existing_dprint_dir() {
    let env = TestEnvironment::new();
    env.set_os("macos");
    env.mk_dir_all("/config/dprint").unwrap();

    let result = resolve_system_config_dir(&env);
    assert_eq!(result, Some(PathBuf::from("/config")));
  }

  #[test]
  fn test_resolve_system_config_dir_macos_without_existing_dprint_dir() {
    let env = TestEnvironment::new();
    env.set_os("macos");
    // Don't create /config/dprint

    // Should fall back to $HOME/.config
    let result = resolve_system_config_dir(&env);
    assert_eq!(result, Some(PathBuf::from("/home/.config")));
  }

  #[test]
  fn test_resolve_system_config_dir_macos_priority_xdg_over_existing() {
    let env = TestEnvironment::new();
    env.set_os("macos");
    env.set_env_var("XDG_CONFIG_HOME", Some("/custom/xdg"));
    env.mk_dir_all("/config/dprint").unwrap();

    // XDG_CONFIG_HOME should take priority even if dprint dir exists
    let result = resolve_system_config_dir(&env);
    assert_eq!(result, Some(PathBuf::from("/custom/xdg")));
  }

  #[test]
  fn test_resolve_system_config_dir_linux() {
    let env = TestEnvironment::new();
    env.set_os("linux");

    // Non-macOS should use config_dir
    let result = resolve_system_config_dir(&env);
    assert_eq!(result, Some(PathBuf::from("/config")));
  }

  #[test]
  fn test_resolve_system_config_dir_windows() {
    let env = TestEnvironment::new();
    env.set_os("windows");

    // Non-macOS should use config_dir
    let result = resolve_system_config_dir(&env);
    assert_eq!(result, Some(PathBuf::from("/config")));
  }

  #[test]
  fn test_resolve_mac_system_config_dir_xdg_priority() {
    let env = TestEnvironment::new();
    env.set_env_var("XDG_CONFIG_HOME", Some("/xdg"));
    env.mk_dir_all("/config/dprint").unwrap();

    let result = resolve_mac_system_config_dir(&env);
    assert_eq!(result, Some(PathBuf::from("/xdg")));
  }

  #[test]
  fn test_resolve_mac_system_config_dir_existing_dprint_priority() {
    let env = TestEnvironment::new();
    env.mk_dir_all("/config/dprint").unwrap();

    let result = resolve_mac_system_config_dir(&env);
    assert_eq!(result, Some(PathBuf::from("/config")));
  }

  #[test]
  fn test_resolve_mac_system_config_dir_home_fallback() {
    let env = TestEnvironment::new();
    // No XDG_CONFIG_HOME, no existing dprint dir

    let result = resolve_mac_system_config_dir(&env);
    assert_eq!(result, Some(PathBuf::from("/home/.config")));
  }

  #[test]
  fn test_resolve_env_var_folder_with_value() {
    let env = TestEnvironment::new();
    env.set_env_var("TEST_VAR", Some("/some/path"));

    let result = resolve_env_var_folder(&env, "TEST_VAR");
    assert_eq!(result, Some(OsString::from("/some/path")));
  }

  #[test]
  fn test_resolve_env_var_folder_with_empty_value() {
    let env = TestEnvironment::new();
    env.set_env_var("TEST_VAR", Some(""));

    let result = resolve_env_var_folder(&env, "TEST_VAR");
    assert_eq!(result, None);
  }

  #[test]
  fn test_resolve_env_var_folder_not_set() {
    let env = TestEnvironment::new();

    let result = resolve_env_var_folder(&env, "NONEXISTENT_VAR");
    assert_eq!(result, None);
  }
}
