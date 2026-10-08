#[allow(clippy::disallowed_methods)]
fn main() {
  println!("cargo:rustc-env=TARGET={}", std::env::var("TARGET").unwrap());
  println!("cargo:rustc-env=RUSTC_VERSION_TEXT={}", get_rustc_version());
  set_wasm_backend_cfg();
}

// Select the wasm backend. wasmtime's Cranelift backend has native codegen for
// x86_64, aarch64, riscv64 and s390x. On powerpc64/loongarch64 it has no native
// backend, and in the Android sandbox the signal-based trap handling that native
// code relies on is unavailable, so those targets compile to wasmtime's portable
// Pulley bytecode and interpret it instead (see the `use_pulley` cfg in
// crates/dprint/src/plugins/implementations/wasm/load_instance.rs).
#[allow(clippy::disallowed_methods)]
fn set_wasm_backend_cfg() {
  println!("cargo:rustc-check-cfg=cfg(use_pulley)");
  let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
  let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
  let use_pulley = matches!(arch.as_str(), "powerpc64" | "loongarch64") || os == "android";
  if use_pulley {
    println!("cargo:rustc-cfg=use_pulley");
  }
}

fn get_rustc_version() -> String {
  let output = std::process::Command::new("rustc").arg("-V").output().unwrap();
  String::from_utf8(output.stdout).unwrap().trim().to_string()
}
