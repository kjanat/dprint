use crate::environment::PlatformEnvironment as Environment;
use anyhow::Result;
use anyhow::anyhow;
use serde_json::Value;
use url::Url;

// The permanent repository ID keeps release discovery working after repository renames.
pub const LATEST_RELEASE_URL: &str = "https://api.github.com/repositories/1092062077/releases/latest";

pub async fn is_out_of_date(environment: &(impl Environment + crate::environment::ApplicationEnvironment)) -> Option<String> {
  log_debug!(environment, "Checking if CLI out of date...");
  match latest_cli_version(environment).await {
    Ok(latest_version) => {
      let current_version = environment.cli_version();
      if current_version == latest_version {
        log_debug!(environment, "CLI version matched.");
        None
      } else {
        log_debug!(environment, "Current version: {}\nLatest version: {}", current_version, latest_version);
        Some(latest_version)
      }
    }
    Err(err) => {
      log_debug!(environment, "Error fetching CLI version: {:#}", err);
      None
    }
  }
}

pub async fn latest_cli_version(environment: &impl Environment) -> Result<String> {
  let (_, file) = environment.download_file_err_404(&Url::parse(LATEST_RELEASE_URL)?, None).await?;
  let data: Value = serde_json::from_slice(&file.content)?;
  let obj = data.as_object().ok_or_else(|| anyhow!("Root was not object."))?;
  let version = obj.get("tag_name").ok_or_else(|| anyhow!("Could not find release tag."))?;
  Ok(version.as_str().ok_or_else(|| anyhow!("release tag was not a string."))?.to_string())
}

#[cfg(test)]
mod test {
  use crate::environment::TestEnvironmentBuilder;

  use super::*;

  #[test]
  fn gets_latest_cli_version_valid() {
    let environment = TestEnvironmentBuilder::new()
      .add_remote_file(LATEST_RELEASE_URL, r#"{ "tag_name": "0.1.0-kjanat" }"#)
      .build();
    environment.clone().run_in_runtime(async move {
      assert_eq!(latest_cli_version(&environment).await.unwrap(), "0.1.0-kjanat");
    });
  }

  #[test]
  fn gets_latest_cli_version_if_out_of_date() {
    let environment = TestEnvironmentBuilder::new()
      .add_remote_file(LATEST_RELEASE_URL, r#"{ "tag_name": "2.2.1" }"#)
      .build();
    environment.clone().run_in_runtime(async move {
      assert_eq!(is_out_of_date(&environment).await, Some("2.2.1".to_string()));
    });
  }

  #[test]
  fn gets_if_not_out_of_date() {
    let environment = TestEnvironmentBuilder::new()
      .add_remote_file(LATEST_RELEASE_URL, r#"{ "tag_name": "0.0.0" }"#)
      .build();
    environment.clone().run_in_runtime(async move {
      assert_eq!(is_out_of_date(&environment).await, None);
    });
  }

  #[test]
  fn is_out_of_date_invalid() {
    let environment = TestEnvironmentBuilder::new().add_remote_file(LATEST_RELEASE_URL, r#"{}"#).build();
    environment.clone().run_in_runtime(async move {
      assert_eq!(is_out_of_date(&environment).await, None);
    });
  }

  #[test]
  fn is_out_of_date_err() {
    let environment = TestEnvironmentBuilder::new().build();
    environment.add_remote_file_error(LATEST_RELEASE_URL, r#"err"#);
    environment.clone().run_in_runtime(async move {
      assert_eq!(is_out_of_date(&environment).await, None);
    });
  }
}
