use std::sync::Arc;

use anyhow::Result;
use dprint_core::configuration::ConfigurationDiagnostic;
use indexmap::IndexMap;
use thiserror::Error;

use super::FormatConfig;
use super::InitializedPlugin;
use crate::environment::Environment;
use crate::utils::PathSource;

#[derive(Debug, Error)]
#[error("[{}]: Error initializing from configuration file. Had {} diagnostic(s).", .plugin_name, .diagnostic_count)]
pub struct OutputPluginConfigDiagnosticsError {
  pub plugin_name: String,
  pub diagnostic_count: usize,
}

/// `property_origins` are the configuration files the plugin's configuration
/// properties are from, when that's not the configuration file being
/// resolved.
pub async fn output_plugin_config_diagnostics<TEnvironment: Environment>(
  plugin_name: &str,
  plugin: &dyn InitializedPlugin,
  format_config: Arc<FormatConfig>,
  property_origins: &IndexMap<String, PathSource>,
  environment: &TEnvironment,
) -> Result<Result<(), OutputPluginConfigDiagnosticsError>> {
  let mut diagnostic_count = 0;

  for diagnostic in plugin.config_diagnostics(format_config).await? {
    log_warn!(environment, "[{}]: {}", plugin_name, describe_config_diagnostic(&diagnostic, property_origins));
    diagnostic_count += 1;
  }

  if diagnostic_count > 0 {
    Ok(Err(OutputPluginConfigDiagnosticsError {
      plugin_name: plugin_name.to_string(),
      diagnostic_count,
    }))
  } else {
    Ok(Ok(()))
  }
}

/// A configuration diagnostic, followed by the configuration file its
/// property is from when that's in `property_origins`.
pub fn describe_config_diagnostic(diagnostic: &ConfigurationDiagnostic, property_origins: &IndexMap<String, PathSource>) -> String {
  match property_origins.get(&diagnostic.property_name) {
    Some(source) => format!("{}\n    at {}", diagnostic, source.display()),
    None => diagnostic.to_string(),
  }
}
