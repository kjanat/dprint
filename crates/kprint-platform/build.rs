// Cargo supplies the target triple to build scripts before an environment exists.
#[allow(clippy::disallowed_methods)]
fn main() {
  println!("cargo:rustc-env=TARGET={}", std::env::var("TARGET").unwrap());
}
