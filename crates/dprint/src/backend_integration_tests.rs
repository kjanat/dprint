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
