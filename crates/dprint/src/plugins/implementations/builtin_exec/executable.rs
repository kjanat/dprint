use std::ffi::OsStr;
use std::path::Path;
use std::path::PathBuf;

/// What Windows uses when the PATHEXT environment variable isn't set.
const DEFAULT_PATH_EXT: &str = ".COM;.EXE;.BAT;.CMD";

/// Resolves a command's executable to the file it refers to.
///
/// On Windows, Rust's `Command` only looks for `<name>.exe` on the PATH, so
/// commands installed as `.cmd` or `.bat` shims (ex. anything installed with
/// `npm install -g`) aren't found. This searches the PATHEXT extensions like
/// the Windows shell does. Elsewhere, and when nothing matches, the executable
/// is returned as given.
// commands run on the real system, so this looks at the real file system
#[allow(clippy::disallowed_methods)]
pub fn resolve_executable(executable: &str, cwd: &Path) -> PathBuf {
  #[cfg(windows)]
  {
    let path_var = std::env::var_os("PATH");
    let path_ext_var = std::env::var_os("PATHEXT");
    if let Some(path) = find_with_path_ext(executable, cwd, path_var.as_deref(), path_ext_var.as_deref(), &|path| path.is_file()) {
      return path;
    }
  }
  let _ = cwd;
  PathBuf::from(executable)
}

/// Finds the file a command's executable refers to the way the Windows shell
/// does (see `resolve_executable`), with the PATH, PATHEXT and working
/// directory given: a name is looked for on the PATH, a path from the working
/// directory.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn find_with_path_ext(
  executable: &str,
  cwd: &Path,
  path_var: Option<&OsStr>,
  path_ext_var: Option<&OsStr>,
  is_file: &dyn Fn(&Path) -> bool,
) -> Option<PathBuf> {
  let extensions = path_ext_var
    .and_then(|value| value.to_str())
    .unwrap_or(DEFAULT_PATH_EXT)
    .split(';')
    .filter(|ext| ext.starts_with('.') && ext.len() > 1)
    // file names on Windows are case insensitive, and lowercase reads better
    .map(|ext| ext.to_ascii_lowercase())
    .collect::<Vec<_>>();
  let find = |base: PathBuf| -> Option<PathBuf> {
    // a name that already has one of the extensions is tried as is first
    let has_path_ext = base
      .extension()
      .and_then(|ext| ext.to_str())
      .is_some_and(|ext| extensions.iter().any(|path_ext| path_ext[1..].eq_ignore_ascii_case(ext)));
    if has_path_ext && is_file(&base) {
      return Some(base);
    }
    extensions.iter().find_map(|ext| {
      let mut candidate = base.clone().into_os_string();
      candidate.push(ext);
      let candidate = PathBuf::from(candidate);
      is_file(&candidate).then_some(candidate)
    })
  };
  let path = Path::new(executable);
  if path.is_absolute() || path.components().count() > 1 {
    // a path, which is relative to the command's working directory
    find(cwd.join(path))
  } else {
    // a name, searched for on the PATH. like Rust's `Command` and unlike the
    // Windows shell, not in the working directory, so a repository can't
    // provide a command that's run instead of the installed one
    std::env::split_paths(path_var?).find_map(|dir| find(dir.join(path)))
  }
}

#[cfg(test)]
mod test {
  use std::ffi::OsString;

  use super::*;

  /// The path with forward slashes, so the tests read the same on every platform.
  fn normalize(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
  }

  /// Finds the executable among the given files, which are matched ignoring
  /// case like on Windows.
  fn find(executable: &str, files: &[&str], path_ext: Option<&str>) -> Option<String> {
    let path_var = std::env::join_paths(["/first", "/second"]).unwrap();
    find_with_path_ext(
      executable,
      Path::new("/cwd"),
      Some(&path_var),
      path_ext.map(OsString::from).as_deref(),
      &|path| files.iter().any(|file| file.eq_ignore_ascii_case(&normalize(path))),
    )
    .map(|path| normalize(&path))
  }

  #[test]
  fn finds_shims_through_path_ext() {
    assert_eq!(find("tombi", &["/second/tombi.cmd"], None), Some("/second/tombi.cmd".to_string()));
    // the first PATH entry with a match wins
    assert_eq!(
      find("tombi", &["/first/tombi.cmd", "/second/tombi.exe"], None),
      Some("/first/tombi.cmd".to_string())
    );
    // then the first PATHEXT extension
    assert_eq!(
      find("tombi", &["/first/tombi.cmd", "/first/tombi.exe"], None),
      Some("/first/tombi.exe".to_string())
    );
    // which is matched ignoring case
    assert_eq!(find("tombi", &["/first/TOMBI.CMD"], None), Some("/first/tombi.cmd".to_string()));
  }

  #[test]
  fn uses_the_path_ext_variable() {
    assert_eq!(find("fmt", &["/first/fmt.ps1"], Some(".EXE;.PS1")), Some("/first/fmt.ps1".to_string()));
    assert_eq!(find("fmt", &["/first/fmt.ps1"], None), None);
  }

  #[test]
  fn tries_a_name_with_an_extension_as_is_first() {
    assert_eq!(find("tombi.cmd", &["/first/tombi.cmd"], None), Some("/first/tombi.cmd".to_string()));
    assert_eq!(find("TOMBI.CMD", &["/first/tombi.cmd"], None), Some("/first/TOMBI.CMD".to_string()));
    // an extension that isn't in PATHEXT is part of the name
    assert_eq!(find("fmt.v2", &["/first/fmt.v2.exe"], None), Some("/first/fmt.v2.exe".to_string()));
  }

  #[test]
  fn resolves_paths_from_the_cwd_and_not_names() {
    assert_eq!(
      find("./node_modules/.bin/prettier", &["/cwd/./node_modules/.bin/prettier.cmd"], None),
      Some("/cwd/./node_modules/.bin/prettier.cmd".to_string())
    );
    // a name isn't looked for in the cwd
    assert_eq!(find("tombi", &["/cwd/tombi.cmd"], None), None);
  }

  #[test]
  fn returns_the_executable_when_not_found() {
    assert_eq!(find("missing", &[], None), None);
    assert_eq!(resolve_executable("missing", Path::new("/cwd")), PathBuf::from("missing"));
  }
}
