mod command;
pub mod input;
pub mod legacy;
pub use command::split_command;
pub use legacy::COMMANDS_RELEASE as EXEC_COMMANDS_RELEASE;
pub use legacy::is_exec_plugin_reference;
pub use legacy::knows_exec_plugin_commands;
/// What a configuration's plugins can list to name the built-in exec.
pub const BUILT_IN_REFERENCE_NAME: &str = "exec";

/// The reference `BUILT_IN_REFERENCE_NAME` stands for: the exec plugin release
/// whose configuration the built-in reads, which is also what runs with
/// `DPRINT_BUILTIN_EXEC=0`.
pub const BUILT_IN_REFERENCE: &str = "npm:@dprint/exec@0.7.3/plugin.json";

/// Whether dprint serves the reference with its built-in exec: every reference
/// to the exec plugin, in any release and with any checksum, unless
/// `DPRINT_BUILTIN_EXEC=0` asks for the process plugin it names.
pub fn is_builtin_exec_reference(environment: &impl kprint_platform::environment::EnvironmentVariables, reference: &crate::PluginSourceReference) -> bool {
  legacy::is_exec_plugin_reference(reference) && environment.env_var("DPRINT_BUILTIN_EXEC").is_none_or(|value| value != "0")
}
