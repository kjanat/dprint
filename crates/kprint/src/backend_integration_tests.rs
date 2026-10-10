use crate::environment::FileSystemEnvironment;
use crate::environment::TestEnvironmentBuilder;
use crate::test_helpers::run_test_cli;
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

#[cfg(unix)]
#[test]
fn builtin_exec_activates_from_json_and_toml_without_a_plugin_reference() {
  for (path, config) in [
    ("/dprint.json", r#"{"exec":{"commands":[{"command":"tr a-z A-Z","exts":["txt"]}]}}"#),
    ("/dprint.toml", "[[exec.commands]]\ncommand = \"tr a-z A-Z\"\nexts = [\"txt\"]\n"),
  ] {
    let environment = TestEnvironmentBuilder::new().write_file(path, config).write_file("/file.txt", "text\n").build();
    // Check must report a formatting difference, not an unknown exec table.
    let error = run_test_cli(vec!["check", "/file.txt"], &environment).unwrap_err();
    error.assert_exit_code(20);
    environment.take_stdout_messages();
    environment.take_stderr_messages();
    run_test_cli(vec!["fmt", "/file.txt"], &environment).unwrap();
    assert_eq!(environment.read_file("/file.txt").unwrap(), "TEXT\n");
    environment.take_stdout_messages();
    run_test_cli(vec!["check", "/file.txt"], &environment).unwrap();
    environment.take_stdout_messages();
  }
}

#[test]
fn builtin_exec_has_resolved_config_and_schema_without_a_plugin_reference() {
  let environment = TestEnvironmentBuilder::new()
    .with_default_config(|config| {
      config.add_config_section("exec", r#"{"commands":[{"command":"formatter","exts":["txt"]}]}"#);
    })
    .build();
  // Direct host configuration does not depend on the legacy substitution switch.
  environment.set_env_var("DPRINT_BUILTIN_EXEC", Some("0"));
  run_test_cli(vec!["output-resolved-config"], &environment).unwrap();
  let resolved: serde_json::Value = serde_json::from_str(&environment.take_stdout_messages()[0]).unwrap();
  assert_eq!(resolved["exec"]["commands"].as_array().unwrap().len(), 1);
  run_test_cli(vec!["schema"], &environment).unwrap();
  let schema: serde_json::Value = serde_json::from_str(&environment.take_stdout_messages()[0]).unwrap();
  assert!(schema["properties"].get("exec").is_some());
}

#[cfg(unix)]
#[test]
fn builtin_exec_inherits_local_commands_without_a_plugin_reference() {
  let environment = TestEnvironmentBuilder::new()
    .write_file("/dprint.toml", "extends = \"./commands.toml\"\n")
    .write_file("/commands.toml", "[[exec.commands]]\ncommand = \"tr a-z A-Z\"\nexts = [\"txt\"]\n")
    .write_file("/file.txt", "text\n")
    .build();
  run_test_cli(vec!["fmt", "/file.txt"], &environment).unwrap();
  assert_eq!(environment.read_file("/file.txt").unwrap(), "TEXT\n");
  environment.take_stdout_messages();
}

#[test]
fn builtin_exec_rejects_options_outside_the_host_contract() {
  let environment = TestEnvironmentBuilder::new()
    .with_default_config(|config| {
      config.add_config_section("exec", r#"{"commands":[{"command":"formatter","exts":["txt"],"shell":"bash"}]}"#);
    })
    .build();
  run_test_cli(vec!["output-resolved-config"], &environment).unwrap_err().assert_exit_code(1);
  let diagnostics = environment.take_stderr_messages().join("\n");
  assert!(diagnostics.contains("unknown field `shell`"), "{}", diagnostics);
}

#[cfg(unix)]
#[test]
fn builtin_exec_filters_remote_commands_without_a_plugin_reference() {
  for (permission, expected) in [("false", "text\n"), ("[\"tr\"]", "TEXT\n")] {
    let environment = TestEnvironmentBuilder::new()
      .write_file(
        "/dprint.json",
        format!(r#"{{"extends":"https://example.com/config.json","exec":{{"playWithFire":{permission}}}}}"#),
      )
      .add_remote_file(
        "https://example.com/config.json",
        r#"{"exec":{"commands":[{"command":"tr a-z A-Z","exts":["txt"]}],"playWithFire":true}}"#,
      )
      .write_file("/file.txt", "text\n")
      .build();
    let result = run_test_cli(vec!["fmt", "/file.txt"], &environment);
    if permission != "false" {
      result.unwrap();
    } else {
      result.unwrap_err().assert_exit_code(14);
    }
    assert_eq!(environment.read_file("/file.txt").unwrap(), expected);
    environment.take_stdout_messages();
    environment.take_stderr_messages();
  }
}
