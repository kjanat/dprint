//! Chooses how a Wasm plugin formats: interpreted, or compiled to native code.
//!
//! Compiling a plugin costs time in proportion to its module's size, and it
//! pays off when interpreting what the plugin formats would take longer.
//! Before a run formats anything, it adds up the bytes of the files each
//! plugin will format and chooses with `choose` (see `resolution.rs`). A
//! plugin compiled before formats natively, as loading its native code takes
//! milliseconds.
//!
//! The numbers are from formatting and compiling the plugins of
//! plugins.dprint.dev with wasmtime 43 (Cranelift) and wasmi 2:
//!
//! | plugin | module | compile, one thread | interpreted | native |
//! | --- | --- | --- | --- | --- |
//! | typescript 0.96.1 | 4.2 MB | 3.64s | 5.8 µs/byte | 0.74 µs/byte |
//! | json 0.23.0 | 0.5 MB | 0.63s | 2.1 µs/byte | 0.28 µs/byte |
//! | markdown 0.23.0 | 0.7 MB | 0.73s | 0.36 µs/byte | 0.07 µs/byte |

use std::path::Path;
use std::time::Duration;

use serde::Deserialize;
use serde::Serialize;

use crate::environment::PluginEnvironment as Environment;

/// How a plugin formats.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormatEngine {
  Interpreter,
  Native,
}

/// The CPU time Cranelift takes to compile one byte of a module: 0.80 to
/// 1.26 µs for the plugins measured.
const COMPILE_NANOS_PER_MODULE_BYTE: u64 = 900;

/// The CPU time the interpreter takes to format one byte, for a plugin that
/// hasn't formatted in the interpreter yet. Plugins differ a lot (0.36 to
/// 5.8 µs a byte), so after a run a plugin's own rate is kept (see
/// `FormatRate`). This is about the geometric mean of the measured ones,
/// which keeps the cost of a wrong guess about the same either way.
const DEFAULT_INTERPRETED_NANOS_PER_BYTE: u64 = 1600;

/// How many times faster native code formats than the interpreter: 5.4 to
/// 7.8 times for the plugins measured. Taking the low end only makes the
/// choice compile a little later.
const NATIVE_SPEEDUP: u64 = 6;

/// How every Wasm plugin without native code formats, when
/// `DPRINT_WASM_FORMAT_ENGINE` is set to `interpreter` or `native`.
pub fn forced(environment: &impl Environment) -> Option<FormatEngine> {
  let value = environment.env_var("DPRINT_WASM_FORMAT_ENGINE")?;
  match value.to_str().map(str::trim) {
    Some("interpreter") => Some(FormatEngine::Interpreter),
    Some("native") => Some(FormatEngine::Native),
    Some("auto" | "") => None,
    _ => {
      log_warn!(
        environment,
        "Ignoring DPRINT_WASM_FORMAT_ENGINE={}: expected auto, interpreter or native.",
        value.to_string_lossy()
      );
      None
    }
  }
}

/// How a plugin formats `bytes_to_format` bytes when it has no native code:
/// compiled when interpreting the bytes would take longer than compiling
/// the module. `rate` is the plugin's measured rate in the interpreter, if
/// it has one.
pub fn choose(module_len: u64, bytes_to_format: u64, rate: Option<&FormatRate>) -> FormatEngine {
  let interpreting = bytes_to_format.saturating_mul(interpreted_nanos_per_byte(rate));
  let saved = interpreting - interpreting / NATIVE_SPEEDUP;
  if saved > compile_nanos(module_len) {
    FormatEngine::Native
  } else {
    FormatEngine::Interpreter
  }
}

/// About how long compiling a module of `module_len` bytes takes, in CPU
/// time.
pub fn compile_time(module_len: u64) -> Duration {
  Duration::from_nanos(compile_nanos(module_len))
}

fn compile_nanos(module_len: u64) -> u64 {
  module_len.saturating_mul(COMPILE_NANOS_PER_MODULE_BYTE)
}

fn interpreted_nanos_per_byte(rate: Option<&FormatRate>) -> u64 {
  rate.and_then(FormatRate::nanos_per_byte).unwrap_or(DEFAULT_INTERPRETED_NANOS_PER_BYTE)
}

/// How fast a plugin formatted in the interpreter: the bytes it formatted
/// and the time that took. It's kept per build of the plugin, next to its
/// module.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FormatRate {
  pub bytes: u64,
  pub nanos: u64,
}

impl FormatRate {
  /// Once this many bytes are counted, older runs count half, so the rate
  /// follows the files the plugin formats now.
  const MAX_BYTES: u64 = 64 * 1024 * 1024;

  /// The rate needs this many bytes before it's used, as the first files a
  /// plugin formats also pay for translating its functions.
  const MIN_BYTES: u64 = 64 * 1024;

  pub fn nanos_per_byte(&self) -> Option<u64> {
    (self.bytes >= Self::MIN_BYTES).then(|| self.nanos / self.bytes)
  }

  /// Adds what a run formatted.
  pub fn add(&mut self, other: FormatRate) {
    self.bytes = self.bytes.saturating_add(other.bytes);
    self.nanos = self.nanos.saturating_add(other.nanos);
    while self.bytes > Self::MAX_BYTES {
      self.bytes /= 2;
      self.nanos /= 2;
    }
  }
}

/// The rate kept at `path`, if there is one.
pub fn read_rate(environment: &impl Environment, path: &Path) -> Option<FormatRate> {
  serde_json::from_slice(&environment.read_file_bytes(path).ok()?).ok()
}

/// Adds what this run formatted to the rate kept at `path`. This is best
/// effort: it only steers a later run's choice.
pub fn add_to_rate(environment: &impl Environment, path: &Path, run: FormatRate) {
  let mut rate = read_rate(environment, path).unwrap_or_default();
  rate.add(run);
  let Ok(bytes) = serde_json::to_vec(&rate) else {
    return;
  };
  if let Err(err) = environment.atomic_write_file_bytes(path, &bytes) {
    log_debug!(environment, "Error writing {}: {:#}", path.display(), err);
  }
}

#[cfg(test)]
mod test {
  use super::*;

  const MB: u64 = 1024 * 1024;

  #[test]
  fn compiles_once_interpreting_would_take_longer() {
    // a 4 MB module compiles in about 3.8s
    assert_eq!(choose(4 * MB, 0, None), FormatEngine::Interpreter);
    assert_eq!(choose(4 * MB, 100 * 1024, None), FormatEngine::Interpreter);
    // compiling saves about 2.8s of interpreting 2 MB
    assert_eq!(choose(4 * MB, 2 * MB, None), FormatEngine::Interpreter);
    // and about 5.6s of interpreting 4 MB
    assert_eq!(choose(4 * MB, 4 * MB, None), FormatEngine::Native);
  }

  #[test]
  fn a_plugin_slow_in_the_interpreter_compiles_sooner() {
    let slow = FormatRate { bytes: MB, nanos: MB * 5800 };
    assert_eq!(choose(4 * MB, MB, Some(&slow)), FormatEngine::Native);
    let fast = FormatRate { bytes: MB, nanos: MB * 360 };
    assert_eq!(choose(4 * MB, 4 * MB, Some(&fast)), FormatEngine::Interpreter);
  }

  #[test]
  fn a_rate_of_few_bytes_is_not_used() {
    let few = FormatRate {
      bytes: 1024,
      nanos: 1024 * 100_000,
    };
    assert_eq!(few.nanos_per_byte(), None);
    assert_eq!(choose(4 * MB, MB, Some(&few)), choose(4 * MB, MB, None));
  }

  #[test]
  fn older_runs_count_less_once_the_rate_is_large() {
    let mut rate = FormatRate {
      bytes: 60 * MB,
      nanos: 60 * MB * 1000,
    };
    rate.add(FormatRate {
      bytes: 10 * MB,
      nanos: 10 * MB * 3000,
    });
    assert!(rate.bytes <= FormatRate::MAX_BYTES);
    // (60 * 1000 + 10 * 3000) / 70
    assert_eq!(rate.nanos_per_byte(), Some(1285));
  }
}
