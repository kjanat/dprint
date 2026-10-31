#![deny(clippy::print_stderr)]
#![deny(clippy::print_stdout)]
#![deny(clippy::unused_async)]

use anyhow::Result;
use environment::RealEnvironmentOptions;
use kprint::arg_parser;
use kprint::environment;
use kprint::plugins;
use kprint::run_cli;
use kprint::utils;
use kprint_process_plugin::setup_exit_process_panic_hook;
use run_cli::AppError;
use std::rc::Rc;
use utils::LogLevel;
use utils::RealStdInReader;

fn main() {
  setup_exit_process_panic_hook();
  // dprint compiles wasm plugins in a child process of itself so it can kill a
  // compile that stalls (see compile_worker.rs)
  if std::env::args_os().nth(1).is_some_and(|arg| arg == plugins::WASM_COMPILE_WORKER_ARG) {
    let args = std::env::args_os().skip(2).collect::<Vec<_>>();
    std::process::exit(plugins::run_wasm_compile_worker(&args));
  }
  #[cfg(unix)]
  kill_owned_children_on_termination();
  // wasm plugins run on blocking threads and execute wasm on the native stack,
  // so give blocking threads a stack large enough (see WASM_PLUGIN_THREAD_STACK_SIZE).
  let rt = tokio::runtime::Builder::new_current_thread()
    .enable_time()
    .thread_stack_size(crate::plugins::WASM_PLUGIN_THREAD_STACK_SIZE)
    .build()
    .unwrap();
  rt.block_on(async move {
    match run().await {
      Ok(_) => {}
      Err((err, log_level)) => {
        if log_level != LogLevel::Silent {
          let result = format!("{:#}", err.inner);
          #[allow(clippy::print_stderr)]
          if !result.is_empty() {
            eprintln!("{}", result);
          }
        }
        // exiting skips destructors, so kill the owned children here
        kprint_owned_child::kill_all_owned_children();
        std::process::exit(err.exit_code);
      }
    }
  });
}

/// Child processes run in process groups of their own (see `OwnedChild`), so
/// a terminal's Ctrl+C no longer reaches them. Kill them when dprint is
/// interrupted or terminated, then end the way the signal would have.
#[cfg(unix)]
fn kill_owned_children_on_termination() {
  use signal_hook::consts::signal::SIGHUP;
  use signal_hook::consts::signal::SIGINT;
  use signal_hook::consts::signal::SIGQUIT;
  use signal_hook::consts::signal::SIGTERM;

  let Ok(mut signals) = signal_hook::iterator::Signals::new([SIGINT, SIGTERM, SIGHUP, SIGQUIT]) else {
    return;
  };
  std::thread::spawn(move || {
    if let Some(signal) = signals.forever().next() {
      kprint_owned_child::kill_all_owned_children();
      let _ = signal_hook::low_level::emulate_default_handler(signal);
      std::process::exit(128 + signal);
    }
  });
}

async fn run() -> Result<(), (AppError, LogLevel)> {
  let args = arg_parser::parse_args(std::env::args().collect(), RealStdInReader).map_err(|err| (err.into(), LogLevel::Info))?;

  let environment = kprint::environment::create_real_environment(RealEnvironmentOptions {
    log_level: args.log_level,
    is_stdout_machine_readable: args.is_stdout_machine_readable(),
  })
  .map_err(|err| (err.into(), args.log_level))?;
  let plugin_cache = plugins::PluginCache::new(environment.clone());
  let plugin_resolver = Rc::new(plugins::PluginResolver::new(environment.clone(), plugin_cache));

  let result = run_cli::run_cli(&args, &environment, &plugin_resolver).await;
  plugin_resolver.clear_and_shutdown_initialized().await;
  result.map_err(|err| (err.into(), args.log_level))
}
