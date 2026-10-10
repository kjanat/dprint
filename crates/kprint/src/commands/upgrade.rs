use std::path::Path;
use std::process::Command;
use std::process::Stdio;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use serde::Deserialize;
use url::Url;

use crate::environment::Environment;
use crate::environment::FilePermissions;
use crate::utils::LATEST_RELEASE_URL;
use crate::utils::extract_zip;
use crate::utils::get_running_pids_by_name;
use crate::utils::kill_process_by_id;

#[derive(Deserialize)]
struct Release {
  tag_name: String,
  assets: Vec<ReleaseAsset>,
}

#[derive(Deserialize)]
struct ReleaseAsset {
  name: String,
  browser_download_url: String,
}

// Note: To test `dprint upgrade`, you must do so manually at the moment.
// Update ./crates/kprint/Cargo.toml to have a version below the current
// released one, then run `./target/debug/dprint upgrade --log-level=debug`.

pub async fn upgrade<TEnvironment: Environment>(environment: &TEnvironment) -> Result<()> {
  let (_, release_file) = environment
    .download_file_err_404(&Url::parse(LATEST_RELEASE_URL)?, None)
    .await
    .context("Error fetching latest CLI release.")?;
  let release: Release = serde_json::from_slice(&release_file.content).context("Error reading latest CLI release.")?;
  let latest_version = &release.tag_name;
  let current_version = environment.cli_version();
  if current_version == latest_version.as_str() {
    log_stdout_info!(environment, "Already on latest version {}", latest_version);
    return Ok(());
  }

  log_stdout_info!(environment, "Upgrading from {} to {}...", current_version, latest_version);

  let exe_path = environment.current_exe()?;
  let mut components = exe_path.components().map(|c| c.as_os_str().to_string_lossy().to_lowercase()).peekable();
  while let Some(component) = components.next() {
    if component == "node_modules" {
      bail!("Cannot upgrade with `dprint upgrade` when the dprint executable is within a node_modules folder. Upgrade with npm instead.");
    } else if component == ".cargo" {
      bail!("It looks like you might have installed dprint with cargo install. Upgrade with cargo instead.");
    } else if component == "deno" && matches!(components.peek().map(|c| c.as_str()), Some("npm")) {
      bail!("It looks like you might have installed dprint with Deno. Upgrade by running the following instead: deno install -A -f npm:dprint");
    }
  }
  if exe_path.starts_with("/usr/local/Cellar/") {
    bail!("Cannot upgrade with `dprint upgrade` when the dprint executable is installed via Homebrew. Run `brew upgrade dprint` instead.");
  }

  let permissions = environment.file_permissions(&exe_path)?;

  if permissions.readonly() {
    bail!("You do not have write permission to {}", exe_path.display());
  }

  let arch = environment.cpu_arch();
  let os = environment.os();
  let zip_suffix = match os.as_str() {
    "linux" => "unknown-linux-gnu",
    "linux-musl" => "unknown-linux-musl",
    "macos" => "apple-darwin",
    "windows" => "pc-windows-msvc",
    _ => bail!("Not implemented operating system: {}", os),
  };
  let zip_filename = format!("dprint-{}-{}.zip", arch, zip_suffix);
  let asset = release
    .assets
    .iter()
    .find(|asset| asset.name == zip_filename)
    .with_context(|| format!("Release {} does not contain {}", latest_version, zip_filename))?;
  let zip_url = Url::parse(&asset.browser_download_url)?;

  let (_, zip_file) = environment.download_file_err_404(&zip_url, None).await?;
  let old_executable = exe_path.with_extension("old.exe");

  if !environment.is_real() {
    // kind of hard to test this with a test environment
    panic!("Need real environment.");
  }

  if cfg!(windows) {
    // on windows, we need to rename the current running executable
    // to something else in order to be able to replace it.
    environment.rename(&exe_path, &old_executable)?;
  } else {
    // on other platforms, we remove it first
    environment.remove_file(&exe_path)?;
  }

  let maybe_reinstall_message = "You may need to reinstall dprint from scratch. Sorry!";
  if let Err(err) = try_upgrade(&exe_path, &zip_file.content, permissions, environment) {
    if cfg!(windows) {
      // try to rename it back
      environment.rename(&old_executable, &exe_path).with_context(|| {
        format!(
          "Upgrade error: {:#}\nError upgrading and then error restoring. {}",
          err, maybe_reinstall_message
        )
      })?;
      bail!("Upgrade error: {:#}", err);
    } else {
      bail!("Upgrade error: {:#}\n{}", err, maybe_reinstall_message);
    }
  }

  // it would be nice if we could delete the old executable here on Windows,
  // but we need it in order to keep running the current executable
  log_stdout_info!(environment, "Upgraded to dprint {}", latest_version);

  Ok(())
}

fn try_upgrade(exe_path: &Path, zip_bytes: &[u8], permissions: FilePermissions, environment: &impl Environment) -> Result<()> {
  try_kill_other_dprint_processes(environment);
  extract_zip("Extracting zip...", zip_bytes, exe_path.parent().unwrap(), environment)?;
  environment.set_file_permissions(exe_path, permissions)?;
  validate_executable(exe_path).context("Error validating new executable.")?;
  Ok(())
}

fn validate_executable(path: &Path) -> Result<()> {
  let status = Command::new(path).stderr(Stdio::null()).stdout(Stdio::null()).arg("-v").status()?;
  if !status.success() {
    bail!("Status was not success.");
  }
  Ok(())
}

fn try_kill_other_dprint_processes(environment: &impl Environment) {
  let pids = match get_running_pids_by_name("dprint") {
    Ok(pids) => pids,
    Err(err) => {
      log_debug!(environment, "Error getting dprint processes. {:#}", err);
      return;
    }
  };
  let current_pid = std::process::id();
  for pid in pids {
    // it's important to not kill the current process obviously
    if pid != current_pid {
      log_debug!(environment, "Killing process with pid {}...", pid);
      if let Err(err) = kill_process_by_id(pid) {
        log_debug!(environment, "Error killing process with pid {}: {:#}", pid, err);
      }
    }
  }
}

#[cfg(test)]
mod test {
  use super::LATEST_RELEASE_URL;
  use crate::environment::FilePermissions;
  use crate::environment::TestEnvironment;
  use crate::environment::TestFilePermissions;

  use crate::test_helpers::run_test_cli;
  use kprint_platform::environment::*;

  #[test]
  fn should_not_upgrade_same_version() {
    let environment = TestEnvironment::new();
    environment.add_remote_file(LATEST_RELEASE_URL, r#"{ "tag_name": "0.0.0", "assets": [] }"#.as_bytes());
    run_test_cli(vec!["upgrade"], &environment).unwrap();
    assert_eq!(environment.take_stdout_messages(), vec!["Already on latest version 0.0.0"]);
  }

  #[test]
  fn should_upgrade_and_fail_readonly() {
    let environment = TestEnvironment::new();
    environment.mk_dir_all(environment.current_exe().unwrap().parent().unwrap()).unwrap();
    environment.write_file(environment.current_exe().unwrap(), "").unwrap();
    environment
      .set_file_permissions(
        environment.current_exe().unwrap(),
        FilePermissions::Test(TestFilePermissions { readonly: true }),
      )
      .unwrap();
    environment.add_remote_file(LATEST_RELEASE_URL, r#"{ "tag_name": "0.1.0", "assets": [] }"#.as_bytes());
    let err = run_test_cli(vec!["upgrade"], &environment).err().unwrap();
    assert_eq!(
      err.to_string(),
      format!("You do not have write permission to {}", environment.current_exe().unwrap().display())
    );
    assert_eq!(environment.take_stdout_messages(), vec!["Upgrading from 0.0.0 to 0.1.0..."]);
  }

  #[test]
  fn should_upgrade_and_fail_node_modules() {
    let environment = TestEnvironment::new();
    environment.add_remote_file(LATEST_RELEASE_URL, r#"{ "tag_name": "0.1.0", "assets": [] }"#.as_bytes());
    environment.set_current_exe_path("/test/node_modules/dprint/dprint");
    let err = run_test_cli(vec!["upgrade"], &environment).err().unwrap();
    assert_eq!(
      err.to_string(),
      "Cannot upgrade with `dprint upgrade` when the dprint executable is within a node_modules folder. Upgrade with npm instead.",
    );
    assert_eq!(environment.take_stdout_messages(), vec!["Upgrading from 0.0.0 to 0.1.0..."]);
  }

  #[test]
  fn should_upgrade_and_fail_homebrew() {
    let environment = TestEnvironment::new();
    environment.add_remote_file(LATEST_RELEASE_URL, r#"{ "tag_name": "0.1.0", "assets": [] }"#.as_bytes());
    environment.set_current_exe_path("/usr/local/Cellar/dprint");
    let err = run_test_cli(vec!["upgrade"], &environment).err().unwrap();
    assert_eq!(
      err.to_string(),
      "Cannot upgrade with `dprint upgrade` when the dprint executable is installed via Homebrew. Run `brew upgrade dprint` instead.",
    );
    assert_eq!(environment.take_stdout_messages(), vec!["Upgrading from 0.0.0 to 0.1.0..."]);
  }

  #[test]
  fn should_upgrade_and_fail_cargo_install() {
    let environment = TestEnvironment::new();
    environment.add_remote_file(LATEST_RELEASE_URL, r#"{ "tag_name": "0.1.0", "assets": [] }"#.as_bytes());
    environment.set_current_exe_path("/home/david/.cargo/dprint");
    let err = run_test_cli(vec!["upgrade"], &environment).err().unwrap();
    assert_eq!(
      err.to_string(),
      "It looks like you might have installed dprint with cargo install. Upgrade with cargo instead.",
    );
    assert_eq!(environment.take_stdout_messages(), vec!["Upgrading from 0.0.0 to 0.1.0..."]);
  }

  #[test]
  fn should_upgrade_and_fail_deno_install() {
    let environment = TestEnvironment::new();
    environment.add_remote_file(LATEST_RELEASE_URL, r#"{ "tag_name": "0.1.0", "assets": [] }"#.as_bytes());
    environment.set_current_exe_path("/usr/local/deno/npm/registry.npmjs.org/dprint/0.34.0/dprint");
    let err = run_test_cli(vec!["upgrade"], &environment).err().unwrap();
    assert_eq!(
      err.to_string(),
      concat!(
        "It looks like you might have installed dprint with Deno. ",
        "Upgrade by running the following instead: deno install -A -f npm:dprint",
      ),
    );
    assert_eq!(environment.take_stdout_messages(), vec!["Upgrading from 0.0.0 to 0.1.0..."]);
  }

  #[test]
  fn should_download_matching_asset_after_repo_rename() {
    for (arch, os, target) in [
      ("x86_64", "linux", "x86_64-unknown-linux-gnu"),
      ("aarch64", "linux", "aarch64-unknown-linux-gnu"),
      ("x86_64", "linux-musl", "x86_64-unknown-linux-musl"),
      ("aarch64", "linux-musl", "aarch64-unknown-linux-musl"),
      ("x86_64", "macos", "x86_64-apple-darwin"),
      ("aarch64", "macos", "aarch64-apple-darwin"),
      ("x86_64", "windows", "x86_64-pc-windows-msvc"),
      ("aarch64", "windows", "aarch64-pc-windows-msvc"),
    ] {
      let environment = TestEnvironment::new();
      let exe_path = environment.current_exe().unwrap();
      environment.mk_dir_all(exe_path.parent().unwrap()).unwrap();
      environment.write_file(&exe_path, "original executable").unwrap();
      environment.set_cpu_arch(arch);
      environment.set_os(os);
      let asset_name = format!("dprint-{target}.zip");
      let asset_url = format!("https://github.com/kjanat/renamed-repo/releases/download/0.1.0-kjanat/{asset_name}");
      environment.add_remote_file_bytes(
        LATEST_RELEASE_URL,
        serde_json::to_vec(&serde_json::json!({
          "tag_name": "0.1.0-kjanat",
          "assets": [
            { "name": "SHASUMS256.txt", "browser_download_url": "https://example.com/checksums" },
            { "name": "dprint-other-target.zip", "browser_download_url": "https://example.com/other-target" },
            { "name": asset_name, "browser_download_url": asset_url }
          ]
        }))
        .unwrap(),
      );
      // Stop before replacing the executable while verifying the chosen download URL.
      environment.add_remote_file_error(&asset_url, "asset download reached");
      let err = run_test_cli(vec!["upgrade"], &environment).unwrap_err();
      assert!(err.to_string().contains("asset download reached"), "{err:#}");
      assert_eq!(environment.remote_file_download_count(&asset_url), 1);
      assert_eq!(environment.remote_file_download_count("https://example.com/other-target"), 0);
      assert_eq!(environment.read_file(&exe_path).unwrap(), "original executable");
      assert_eq!(environment.take_stdout_messages(), vec!["Upgrading from 0.0.0 to 0.1.0-kjanat..."]);
    }
  }

  #[test]
  fn should_fail_when_release_has_no_matching_asset() {
    let environment = TestEnvironment::new();
    environment.mk_dir_all(environment.current_exe().unwrap().parent().unwrap()).unwrap();
    environment.write_file(environment.current_exe().unwrap(), "").unwrap();
    environment
      .set_file_permissions(environment.current_exe().unwrap(), FilePermissions::Test(Default::default()))
      .unwrap();
    environment.add_remote_file(LATEST_RELEASE_URL, r#"{ "tag_name": "0.1.0", "assets": [] }"#.as_bytes());
    environment.set_cpu_arch("x86_64");
    environment.set_os("linux");
    let err = run_test_cli(vec!["upgrade"], &environment).err().unwrap();
    assert_eq!(err.to_string(), "Release 0.1.0 does not contain dprint-x86_64-unknown-linux-gnu.zip");
    assert!(environment.path_exists(environment.current_exe().unwrap()));
    assert_eq!(environment.take_stdout_messages(), vec!["Upgrading from 0.0.0 to 0.1.0..."]);
  }
}
