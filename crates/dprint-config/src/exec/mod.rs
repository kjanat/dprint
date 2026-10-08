mod command;
pub mod input;
pub mod legacy;
pub use command::split_command;
pub use legacy::COMMANDS_RELEASE as EXEC_COMMANDS_RELEASE;
pub use legacy::is_exec_plugin_reference;
pub use legacy::knows_exec_plugin_commands;
pub fn exec_command_program(command: &str) -> Option<String> {
  split_command(command).into_iter().next()
}
pub fn is_builtin_exec_reference(environment: &impl dprint_platform::environment::EnvironmentVariables, reference: &crate::PluginSourceReference) -> bool {
  legacy::served_release(reference).is_ok() && environment.env_var("DPRINT_BUILTIN_EXEC").is_none_or(|value| value != "0")
}
