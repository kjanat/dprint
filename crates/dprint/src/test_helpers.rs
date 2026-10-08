use std::cell::RefCell;
use std::rc::Rc;

use anyhow::Result;
use deno_terminal::colors;
use thiserror::Error;

use crate::AppError;
use crate::arg_parser::parse_args;
use crate::environment::TestEnvironment;
use crate::plugins::PluginCache;
use crate::plugins::PluginResolver;
use crate::run_cli::run_cli;
use crate::utils::TestStdInReader;

pub use dprint_test_support::assert_contains;
pub use dprint_test_support::test_helpers::*;

#[derive(Debug, Error)]
#[error("{inner:#}")]
pub struct TestAppError {
  asserted_exit_code: RefCell<bool>,
  inner: AppError,
}

impl TestAppError {
  #[track_caller]
  pub fn assert_exit_code(&self, exit_code: i32) {
    self.asserted_exit_code.replace(true);
    assert_eq!(self.inner.exit_code, exit_code);
  }
}

impl From<AppError> for TestAppError {
  fn from(inner: AppError) -> Self {
    Self {
      asserted_exit_code: Default::default(),
      inner,
    }
  }
}

impl From<anyhow::Error> for TestAppError {
  fn from(inner: anyhow::Error) -> Self {
    Self {
      asserted_exit_code: Default::default(),
      inner: inner.into(),
    }
  }
}

impl Drop for TestAppError {
  fn drop(&mut self) {
    if std::thread::panicking() || self.inner.exit_code <= 1 {
      return;
    }
    if !*self.asserted_exit_code.borrow() {
      panic!("Exit code must be asserted. Was: {}", self.inner.exit_code);
    }
  }
}

pub fn run_test_cli(args: Vec<&str>, environment: &TestEnvironment) -> Result<(), TestAppError> {
  run_test_cli_with_stdin(args, environment, TestStdInReader::default())
}

pub fn run_test_cli_with_stdin(args: Vec<&str>, environment: &TestEnvironment, stdin_reader: TestStdInReader) -> Result<(), TestAppError> {
  let mut args: Vec<String> = args.into_iter().map(String::from).collect();
  args.insert(0, String::from(""));
  let plugin_cache = PluginCache::new(environment.clone());
  let plugin_resolver = Rc::new(PluginResolver::new(environment.clone(), plugin_cache));
  let args = parse_args(args, stdin_reader).map_err(Into::<AppError>::into)?;
  environment.set_stdout_machine_readable(args.is_stdout_machine_readable());
  environment.set_log_level(args.log_level);

  environment.run_in_runtime({
    let environment = environment.clone();
    async move {
      let result = run_cli(&args, &environment, &plugin_resolver).await;
      plugin_resolver.clear_and_shutdown_initialized().await;
      Ok(result?)
    }
  })
}

pub fn get_singular_formatted_text() -> String {
  format!("Formatted {} file.", colors::bold("1"))
}

pub fn get_plural_formatted_text(count: usize) -> String {
  format!("Formatted {} files.", colors::bold(count.to_string()))
}

pub fn get_singular_check_text() -> String {
  format!("Found {} not formatted file. Run {} to fix.", colors::bold("1"), colors::bold("dprint fmt"))
}

pub fn get_plural_check_text(count: usize) -> String {
  format!(
    "Found {} not formatted files. Run {} to fix.",
    colors::bold(count.to_string()),
    colors::bold("dprint fmt")
  )
}

pub fn get_expected_help_text() -> &'static str {
  concat!(
    "dprint ",
    env!("CARGO_PKG_VERSION"),
    r#"
Copyright 2019 by David Sherret

Auto-formats source code based on the specified plugins.

USAGE:
    dprint <SUBCOMMAND> [OPTIONS] [--] [files/directories/patterns]...

SUBCOMMANDS:
  init               Initializes a configuration file in the current directory, or adds plugins to an existing one.
  add                Adds a plugin to the configuration file.
  fmt                Formats the source files and writes the result to the file system.
  check              Checks for any files that haven't been formatted.
  config             Functionality related to the configuration file.
  file-paths         Prints the resolved file paths for the plugins based on the args and configuration.
  resolved-config    Prints the resolved configuration for the plugins based on the args and configuration.
  schema             Prints a JSON schema of the configuration file, including the plugins' configuration.
  incremental-state  Prints the state used to determine whether the incremental cache would be invalidated.
  format-times       Prints the amount of time it takes to format each file. Use this for debugging.
  clear-cache        Deletes the plugin cache directory.
  upgrade            Upgrades the dprint executable.
  completions        Generate shell completions script for dprint
  license            Outputs the software license.
  lsp                Starts up a language server for formatting files.

More details at `dprint help <SUBCOMMAND>`

OPTIONS:
  -c, --config [<config>]             Path or url to JSON or TOML configuration file, the configuration text itself (a `{...}` object or TOML), or `-` to read it from stdin. Defaults to dprint.json(c), .dprint.json(c), dprint.toml or .dprint.toml in current or ancestor directory when not provided.
      --config-discovery[=<BOOLEAN>]  Sets the config discovery mode. Set to `false` to completely disable, `ignore-descendants` to avoid finding config files in child directories, or `global` to only use the global config file.
      --plugins <urls/files>...       List of urls or file paths of plugins to use. This overrides what is specified in the config file.
  -L, --log-level <log-level>         Set log level [default: info] [possible values: debug, info, warn, error, silent]

ENVIRONMENT VARIABLES:
  DPRINT_MAX_THREADS   Limit the number of threads dprint uses for
                       formatting (ex. DPRINT_MAX_THREADS=4).
  DPRINT_MAX_PLUGIN_COMPILES
                       The most Wasm plugins a run compiles to native code
                       before it formats (default 50).
  DPRINT_WASM_FORMAT_ENGINE
                       How Wasm plugins without native code format: auto
                       (default), interpreter or native.
  DPRINT_CACHE_DIR     Directory to store the dprint cache. Note that this
                       directory may be periodically deleted by the CLI.
  DPRINT_CONFIG_DIR    Global config directory to store a global dprint.json file.
                       Defaults to the dprint sub folder in the system configuration
                       directory.
  DPRINT_CONFIG_DISCOVERY
                       Sets the config discovery mode. Set to "false"/"0" to disable
                       or "global" to always use the global config file.
  DPRINT_CERT          Load certificate authority from PEM encoded file.
  DPRINT_TLS_CA_STORE  Comma-separated list of order dependent certificate stores.
                       Possible values: "mozilla" and "system".
                       Defaults to "mozilla,system".
  DPRINT_IGNORE_CERTS  Unsafe way to get dprint to ignore certificates. Specify 1
                       to ignore all certificates or a comma separated list of specific
                       hosts to ignore (ex. dprint.dev,localhost,[::],127.0.0.1)
  DPRINT_EDITOR        Editor used for editing config files.
  DPRINT_GLOBAL_GITIGNORE
                       Set to "1" to also respect git's global excludes file
                       (core.excludesFile). Disabled by default.
  HTTPS_PROXY          Proxy to use when downloading plugins or configuration
                       files (also supports HTTP_PROXY and NO_PROXY).
  NO_COLOR             Disables coloured output.
  FORCE_COLOR          Forces coloured output, even when NO_COLOR is set.

GETTING STARTED:
  1. Navigate to the root directory of a code repository.
  2. Run `dprint init` to create a dprint.json file in that directory.
  3. Modify configuration file if necessary.
  4. Run `dprint fmt` or `dprint check`.

EXAMPLES:
  Write formatted files to file system:

    dprint fmt

  Check for files that haven't been formatted:

    dprint check

  Specify path to config file other than the default:

    dprint fmt --config path/to/config/dprint.json

  Provide the configuration instead of a path to it:

    dprint fmt --config '{ "excludes": ["dist"], "plugins": ["..."] }'
    dprint fmt --config <(cat path/to/config/dprint.json)
    dprint fmt --config <<<'{ "excludes": ["dist"], "plugins": ["..."] }'

  Search for files using the specified paths or file patterns:

    dprint fmt "**/*.{ts,tsx,js,jsx,json}"
"#
  )
}
