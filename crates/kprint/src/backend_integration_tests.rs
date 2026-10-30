#[cfg(unix)]
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
#[cfg(unix)]
#[test]
fn formats_with_built_in_exec_for_any_release_or_checksum() {
  // no plugin files are served, so any of these would fail if it tried to
  // download the plugin it names
  for reference in [
    "https://plugins.dprint.dev/exec-0.7.3.json@0000000000000000000000000000000000000000000000000000000000000000",
    "https://plugins.dprint.dev/exec-0.5.0.json",
    "https://plugins.dprint.dev/dprint/dprint-plugin-exec/latest.json",
    "npm:@dprint/exec@0.8.0/plugin.json@abc",
    "npm:@dprint/exec",
  ] {
    let environment = TestEnvironmentBuilder::new()
      .with_default_config(|config_file| {
        config_file
          .add_plugin(reference)
          .add_config_section("exec", r#"{ "commands": [{ "command": "tr a-z A-Z", "exts": ["txt"] }] }"#);
      })
      .write_file("/file.txt", "text\n")
      .build();
    run_test_cli(vec!["fmt", "/file.txt"], &environment).unwrap();
    assert_eq!(environment.read_file("/file.txt").unwrap(), "TEXT\n", "{}", reference);
    assert_eq!(environment.take_stdout_messages(), vec![crate::test_helpers::get_singular_formatted_text()]);
  }
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

#[cfg(unix)]
#[test]
fn builtin_exec_formats_the_files_a_command_associates() {
  let environment = TestEnvironmentBuilder::new()
    .with_default_config(|config| {
      config.add_config_section("exec", r#"{"commands":[{"command":"tr a-z A-Z","associations":"**/*.data"}]}"#);
    })
    .write_file("/file.data", "text\n")
    .write_file("/file.other", "text\n")
    .build();
  run_test_cli(vec!["fmt"], &environment).unwrap();
  assert_eq!(environment.read_file("/file.data").unwrap(), "TEXT\n");
  assert_eq!(environment.read_file("/file.other").unwrap(), "text\n");
  environment.take_stdout_messages();
}

#[cfg(unix)]
#[test]
fn builtin_exec_matches_a_commands_associations_relative_to_the_config() {
  let environment = TestEnvironmentBuilder::new()
    .with_default_config(|config| {
      config.add_config_section("exec", r#"{"commands":[{"command":"tr a-z A-Z","associations":"docs/*.data"}]}"#);
    })
    .write_file("/docs/file.data", "text\n")
    .write_file("/file.data", "text\n")
    .build();
  run_test_cli(vec!["fmt"], &environment).unwrap();
  assert_eq!(environment.read_file("/docs/file.data").unwrap(), "TEXT\n");
  assert_eq!(environment.read_file("/file.data").unwrap(), "text\n");
  environment.take_stdout_messages();
}

#[cfg(unix)]
#[test]
fn builtin_exec_formats_the_files_a_command_associates_after_their_formatter() {
  let environment = TestEnvironmentBuilder::with_initialized_remote_wasm_plugin()
    .with_default_config(|config| {
      config
        .add_remote_wasm_plugin()
        .add_config_section("test-plugin", r#"{"ending":"DONE"}"#)
        .add_config_section("exec", r#"{"commands":[{"command":"tr a-z A-Z","associations":"**/*.txt"}]}"#);
    })
    .write_file("/file.txt", "text")
    .build();
  run_test_cli(vec!["fmt", "/file.txt"], &environment).unwrap();
  assert_eq!(environment.read_file("/file.txt").unwrap(), "TEXT_DONE");
  environment.take_stdout_messages();
}

#[cfg(unix)]
#[test]
fn builtin_exec_only_runs_a_missing_program_for_its_files() {
  let environment = TestEnvironmentBuilder::new()
    .with_default_config(|config| {
      config.add_config_section(
        "exec",
        r#"{"commands":[{"command":"kprint-missing-formatter","exts":["rs"]},{"command":"tr a-z A-Z","exts":["txt"]}]}"#,
      );
    })
    .write_file("/file.txt", "text\n")
    .write_file("/file.rs", "text\n")
    .build();
  run_test_cli(vec!["fmt", "/file.txt"], &environment).unwrap();
  assert_eq!(environment.read_file("/file.txt").unwrap(), "TEXT\n");
  environment.take_stdout_messages();
  run_test_cli(vec!["fmt", "/file.rs"], &environment).unwrap_err();
  let errors = environment.take_stderr_messages().join("\n");
  assert!(errors.contains("Cannot start formatter process 'kprint-missing-formatter' (in "), "{}", errors);
  assert_eq!(environment.read_file("/file.rs").unwrap(), "text\n");
}

#[cfg(unix)]
#[test]
fn names_the_plugins_that_keep_changing_each_others_output() {
  let environment = TestEnvironmentBuilder::with_initialized_remote_wasm_plugin()
    .with_default_config(|config| {
      config
        .add_remote_wasm_plugin()
        .add_config_section("exec", r#"{"commands":[{"command":"tr a-z A-Z","associations":"**/*.txt"}]}"#);
    })
    .write_file("/file.txt", "text")
    .build();
  run_test_cli(vec!["fmt", "/file.txt"], &environment).unwrap_err().assert_exit_code(1);
  assert_eq!(
    environment.take_stderr_messages(),
    vec![
      "Error formatting /file.txt. Message: Formatting not stable. Bailed after 5 tries. test-plugin and exec keep changing each other's output.".to_string()
    ]
  );
  assert_eq!(environment.read_file("/file.txt").unwrap(), "text");
}

#[cfg(unix)]
#[test]
fn formats_a_plugins_embedded_code_with_the_plugins_it_names() {
  let run = |test_plugin_config: &str| {
    let environment = TestEnvironmentBuilder::with_initialized_remote_wasm_and_process_plugin()
      .with_default_config(|config| {
        config
          .add_remote_wasm_plugin()
          .add_remote_process_plugin()
          .add_config_section("test-plugin", test_plugin_config)
          .add_config_section("exec", r#"{"commands":[{"command":"tr a-z A-Z","exts":["txt_ps"]}]}"#);
      })
      .write_file("/file.txt", "plugin: text")
      .write_file("/skip.txt", "plugin: text")
      .build();
    run_test_cli(vec!["fmt", "/file.txt", "/skip.txt"], &environment).unwrap();
    environment.take_stdout_messages();
    (environment.read_file("/file.txt").unwrap(), environment.read_file("/skip.txt").unwrap())
  };
  let both = |text: &str| (text.to_string(), text.to_string());
  assert_eq!(run("{}"), both("plugin: text_formatted_process_formatted"));
  assert_eq!(run(r#"{"embedded":{"txt_ps":["exec"]}}"#), both("plugin: TEXT_formatted"));
  assert_eq!(run(r#"{"embedded":{".TXT_PS":[]}}"#), both("plugin: text_formatted"));
  assert_eq!(
    run(r#"{"embedded":{"txt_ps":["exec"]},"overrides":[{"files":"/skip.txt","embedded":{"txt_ps":[]}}]}"#),
    ("plugin: TEXT_formatted".to_string(), "plugin: text_formatted".to_string())
  );
}

#[test]
fn reports_an_unknown_plugin_for_embedded_code() {
  let environment = TestEnvironmentBuilder::with_initialized_remote_wasm_plugin()
    .with_default_config(|config| {
      config
        .add_remote_wasm_plugin()
        .add_config_section("test-plugin", r#"{"embedded":{"cs":["csharp"]}}"#);
    })
    .write_file("/file.txt", "text")
    .build();
  let error = run_test_cli(vec!["fmt", "/file.txt"], &environment).unwrap_err();
  error.assert_exit_code(12);
  assert!(
    error
      .to_string()
      .contains("test-plugin.embedded.cs: Unknown plugin 'csharp'. Expected the configuration key of a plugin in the configuration (ex. \"typescript\")."),
    "{}",
    error
  );
}
