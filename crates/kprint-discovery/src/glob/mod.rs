#[allow(clippy::module_inception)]
mod glob;
mod glob_matcher;
mod glob_pattern;
mod glob_utils;
mod scan;

pub use glob::*;
pub use glob_matcher::*;
pub use glob_pattern::*;
pub use glob_utils::*;
// for the git index scan, which is unix only (see `git_index`)
#[cfg(unix)]
pub(crate) use scan::DirScanGitIgnore;
#[cfg(unix)]
pub(crate) use scan::DirScanOptions;
#[cfg(unix)]
pub(crate) use scan::DirScanOutput;
#[cfg(unix)]
pub(crate) use scan::walk_dir;
