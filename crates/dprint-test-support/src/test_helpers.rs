use anyhow::Result;
use once_cell::sync::Lazy;
use std::io::Write;
use std::path::PathBuf;
use sys_traits::FsCanonicalize;
use sys_traits::FsMetadata;
use sys_traits::FsRead;
use sys_traits::impls::RealSys;
// macro lifted from Deno's codebase
#[macro_export]
macro_rules! assert_contains {
  ($string:expr, $($test:expr),+ $(,)?) => {
    let string = &$string;
    if !($(string.contains($test))||+) {
      panic!("{:?} does not contain any of {:?}", string, [$($test),+]);
    }
  }
}

// this file should automatically be built when building the workspace
// These fixtures load before the mock environments that consume them exist.
pub static TEST_PROCESS_PLUGIN_PATH: Lazy<PathBuf> = Lazy::new(|| {
  let exe_name = if cfg!(windows) { "test-process-plugin.exe" } else { "test-process-plugin" };
  let profile_name = if cfg!(debug_assertions) { "debug" } else { "release" };
  let target_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target");
  assert!(RealSys.fs_exists_no_err(&target_dir));
  let file_path = target_dir.join(target_dir.join(env!("TARGET"))).join(profile_name).join(exe_name);
  let file_path = if RealSys.fs_exists_no_err(&file_path) {
    file_path
  } else {
    target_dir.join(profile_name).join(exe_name)
  };
  RealSys.fs_canonicalize(&file_path).unwrap_or_else(|err| {
    panic!(
      "Maybe run `cargo build` in the root of the repository?\n\nCould not canonicalize {}: {:#}",
      file_path.display(),
      err
    )
  })
});

// Regenerate this by running `./rebuild.sh` in /crates/test-plugin
pub static WASM_PLUGIN_BYTES: &[u8] = include_bytes!("../../test-plugin/test_plugin.wasm"); // 0.2.0
/// This is an old v3 interface Wasm plugin at 0.1.0
pub static WASM_PLUGIN_0_1_0_BYTES: &[u8] = include_bytes!("../../test-plugin/test_plugin_0_1_0.wasm");
// cache these so it only has to be done once across all tests
pub static PROCESS_PLUGIN_ZIP_BYTES: Lazy<Vec<u8>> = Lazy::new(|| {
  let buf: Vec<u8> = Vec::new();
  let w = std::io::Cursor::new(buf);
  let mut zip = zip::ZipWriter::new(w);
  let options = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
  zip
    .start_file(
      if cfg!(target_os = "windows") {
        "test-process-plugin.exe"
      } else {
        "test-process-plugin"
      },
      options,
    )
    .unwrap();
  let file_bytes = RealSys.fs_read(&*TEST_PROCESS_PLUGIN_PATH).unwrap();
  zip.write_all(&file_bytes).unwrap();
  zip.finish().unwrap().into_inner()
});
pub static PROCESS_PLUGIN_ZIP_CHECKSUM: Lazy<String> = Lazy::new(|| crate::utils::get_sha256_checksum(&PROCESS_PLUGIN_ZIP_BYTES));

/// Raw bytes of the test process plugin executable — used by tests that
/// stuff it into a per-platform npm tarball for the `pre_resolved_tarball`
/// path (npm-installed process plugins ship the executable inside the
/// tarball; dprint extracts the full tarball at setup time).
pub static PROCESS_PLUGIN_BINARY_BYTES: Lazy<Vec<u8>> = Lazy::new(|| RealSys.fs_read(&*TEST_PROCESS_PLUGIN_PATH).unwrap().into_owned());

/// Filename that a per-platform npm package would ship for the test process
/// plugin's executable (`test-process-plugin.exe` on Windows, otherwise
/// `test-process-plugin`).
pub fn process_plugin_binary_filename() -> &'static str {
  if cfg!(target_os = "windows") {
    "test-process-plugin.exe"
  } else {
    "test-process-plugin"
  }
}

/// Validates `instance` against `schema`, which must not refer to other
/// documents. The error says why it's invalid.
pub fn validate_with_schema(schema: &serde_json::Value, instance: &serde_json::Value) -> Result<(), String> {
  const URL: &str = "https://dprint.dev/test/schema.json";
  let mut schemas = boon::Schemas::new();
  let mut compiler = boon::Compiler::new();
  compiler.add_resource(URL, schema.clone()).unwrap();
  let index = compiler.compile(URL, &mut schemas).map_err(|err| format!("Invalid schema: {:#}", err)).unwrap();
  schemas.validate(instance, index).map_err(|err| format!("{:#}", err))
}

/// Builds a gzipped tar with the given (path, contents) entries. Paths must
/// share a single top-level directory (npm tarballs always wrap under `package/`).
pub fn create_test_npm_tarball(files: &[(&str, &[u8])]) -> Vec<u8> {
  let with_mode: Vec<_> = files.iter().map(|(p, c)| (*p, *c, 0o644u32)).collect();
  create_test_npm_tarball_with_modes(&with_mode)
}

/// Like `create_test_npm_tarball` but each entry carries its own unix mode.
pub fn create_test_npm_tarball_with_modes(files: &[(&str, &[u8], u32)]) -> Vec<u8> {
  build_tarball(files, |header, path| header.set_path(path).unwrap())
}

/// Like `create_test_npm_tarball` but writes path bytes directly, bypassing
/// the tar crate's `..` rejection in `set_path`. For exercising defenses
/// against malicious tarballs.
pub fn create_test_npm_tarball_raw_paths(files: &[(&str, &[u8])]) -> Vec<u8> {
  let with_mode: Vec<_> = files.iter().map(|(p, c)| (*p, *c, 0o644u32)).collect();
  build_tarball(&with_mode, |header, path| {
    let bytes = path.as_bytes();
    let name = &mut header.as_old_mut().name;
    if bytes.len() > name.len() {
      panic!(
        "raw tar path is too long for the legacy tar header name field: {} bytes > {} bytes: {}",
        bytes.len(),
        name.len(),
        path
      );
    }
    name.fill(0);
    name[..bytes.len()].copy_from_slice(bytes);
  })
}

fn build_tarball(files: &[(&str, &[u8], u32)], mut set_name: impl FnMut(&mut tar::Header, &str)) -> Vec<u8> {
  use flate2::Compression;
  use flate2::write::GzEncoder;

  let mut tar_builder = tar::Builder::new(Vec::new());
  for (path, contents, mode) in files {
    let mut header = tar::Header::new_gnu();
    set_name(&mut header, path);
    header.set_size(contents.len() as u64);
    header.set_mode(*mode);
    header.set_cksum();
    tar_builder.append(&header, *contents).unwrap();
  }
  let tar_bytes = tar_builder.into_inner().unwrap();

  let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
  std::io::Write::write_all(&mut encoder, &tar_bytes).unwrap();
  encoder.finish().unwrap()
}

pub fn get_test_wasm_plugin_checksum() -> String {
  crate::utils::get_sha256_checksum(WASM_PLUGIN_BYTES)
}

pub struct TestProcessPluginFile(String);

impl Default for TestProcessPluginFile {
  fn default() -> Self {
    TestProcessPluginFileBuilder::default().build()
  }
}

impl TestProcessPluginFile {
  pub fn checksum(&self) -> String {
    crate::utils::get_sha256_checksum(self.0.as_bytes())
  }

  pub fn text(&self) -> &str {
    self.0.as_ref()
  }
}

#[derive(Default)]
pub struct TestProcessPluginFileBuilder {
  schema_version: Option<u32>,
  name: Option<String>,
  version: Option<String>,
  zip_checksum: Option<String>,
}

impl TestProcessPluginFileBuilder {
  #[allow(unused)]
  pub fn schema_version(mut self, schema_version: u32) -> Self {
    self.schema_version = Some(schema_version);
    self
  }

  #[allow(unused)]
  pub fn name(mut self, name: &str) -> Self {
    self.name = Some(name.to_string());
    self
  }

  pub fn version(mut self, version: &str) -> Self {
    self.version = Some(version.to_string());
    self
  }

  pub fn zip_checksum(mut self, zip_checksum: &str) -> Self {
    self.zip_checksum = Some(zip_checksum.to_string());
    self
  }

  pub fn build(self) -> TestProcessPluginFile {
    TestProcessPluginFile(format!(
      r#"{{
  "schemaVersion": {0},
  "name": "{1}",
  "version": "{2}",
  "windows-x86_64": {{
      "reference": "https://github.com/dprint/test-process-plugin/releases/0.1.0/test-process-plugin.zip",
      "checksum": "{3}"
  }},
  "windows-aarch64": {{
      "reference": "https://github.com/dprint/test-process-plugin/releases/0.1.0/test-process-plugin.zip",
      "checksum": "{3}"
  }},
  "linux-aarch64": {{
      "reference": "https://github.com/dprint/test-process-plugin/releases/0.1.0/test-process-plugin.zip",
      "checksum": "{3}"
  }},
  "linux-x86_64": {{
      "reference": "https://github.com/dprint/test-process-plugin/releases/0.1.0/test-process-plugin.zip",
      "checksum": "{3}"
  }},
  "freebsd-x86_64": {{
      "reference": "https://github.com/dprint/test-process-plugin/releases/0.1.0/test-process-plugin.zip",
      "checksum": "{3}"
  }},
  "darwin-x86_64": {{
      "reference": "https://github.com/dprint/test-process-plugin/releases/0.1.0/test-process-plugin.zip",
      "checksum": "{3}"
  }},
  "darwin-aarch64": {{
      "reference": "https://github.com/dprint/test-process-plugin/releases/0.1.0/test-process-plugin.zip",
      "checksum": "{3}"
  }}
  }}"#,
      self.schema_version.unwrap_or(2),
      self.name.unwrap_or("test-process-plugin".to_string()),
      self.version.unwrap_or("0.1.0".to_string()),
      self.zip_checksum.unwrap_or(PROCESS_PLUGIN_ZIP_CHECKSUM.to_string())
    ))
  }
}

#[cfg(feature = "plugins")]
pub fn initialize_plugins(environment: &crate::environment::TestEnvironment) {
  use dprint_plugin_host::PluginCache;
  use dprint_plugin_host::PluginResolver;
  use std::rc::Rc;
  environment.run_in_runtime(async {
    // Match the old license-based fixture setup: missing or invalid config
    // leaves the fixture uninitialized, so tests can exercise that condition.
    let Ok(config) = dprint_config::resolution::resolve_config_from_args(&dprint_host_api::options::SessionOptions::default(), environment).await else {
      return;
    };
    let resolver = Rc::new(PluginResolver::new(environment.clone(), PluginCache::new(environment.clone())));
    for plugin in resolver.resolve_plugins(config.plugins.sources).await.unwrap() {
      plugin.initialize().await.unwrap().license_text().await.unwrap();
    }
    resolver.clear_and_shutdown_initialized().await;
  });
  environment.compile_cached_wasm_plugins();
  environment.clear_logs();
}
