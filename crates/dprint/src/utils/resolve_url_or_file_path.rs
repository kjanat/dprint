use std::time::UNIX_EPOCH;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use url::Url;

use super::PathSource;
use crate::cache::CacheEntry;
use crate::cache::HeadersMap;
use crate::cache::HttpCache;
use crate::environment::Environment;
use crate::utils::RemotePathSource;

/// How long a downloaded remote file is used before its URL is checked for
/// changes. Remote configuration is often pinned to a branch (ex.
/// `https://cdn.jsdelivr.net/gh/user/repo@main/dprint.json`) whose content
/// changes over time, so a cached copy can't be used forever.
pub const REMOTE_FILE_MAX_AGE_SECS: u64 = 60 * 60;

#[derive(Debug, Clone)]
pub struct ResolvedFilePathWithBytes {
  pub source: PathSource,
  /// Whether the file was downloaded rather than served from the cache. This
  /// is false when a stale cache entry was checked for changes and had none.
  pub is_first_download: bool,
  pub content: Vec<u8>,
}

impl ResolvedFilePathWithBytes {
  pub fn into_text(self) -> Result<ResolvedFilePathWithText> {
    let content = String::from_utf8(self.content).with_context(|| format!("Failed converting '{}' to string.", self.source.display()))?;
    Ok(ResolvedFilePathWithText {
      source: self.source,
      content,
      is_first_download: self.is_first_download,
    })
  }
}

#[derive(Debug, Clone)]
pub struct ResolvedFilePathWithText {
  pub source: PathSource,
  pub is_first_download: bool,
  pub content: String,
}

impl ResolvedFilePathWithText {
  pub fn as_ref(&self) -> ResolvedFilePathWithTextRef<'_> {
    ResolvedFilePathWithTextRef {
      source: &self.source,
      content: &self.content,
    }
  }
}

#[derive(Debug, Clone, Copy)]
pub struct ResolvedFilePathWithTextRef<'a> {
  pub source: &'a PathSource,
  pub content: &'a str,
}

pub async fn resolve_url_or_file_path_to_file_with_cache<TEnvironment: Environment>(
  url_or_file_path: &str,
  base: &PathSource,
  environment: &TEnvironment,
) -> Result<ResolvedFilePathWithBytes> {
  let path_source = resolve_url_or_file_path_to_path_source(url_or_file_path, base, environment)?;

  match &path_source {
    PathSource::Remote(remote_path_source) => resolve_url_to_file_with_cache(&remote_path_source.url, environment).await,
    PathSource::Local(local_path_source) => {
      let content = environment.read_file_bytes(&local_path_source.path)?;
      Ok(ResolvedFilePathWithBytes {
        source: path_source,
        is_first_download: false,
        content,
      })
    }
    PathSource::Npm(_) => bail!("Cannot resolve npm specifier as a URL or file path"),
  }
}

async fn resolve_url_to_file_with_cache<TEnvironment: Environment>(url: &Url, environment: &TEnvironment) -> Result<ResolvedFilePathWithBytes> {
  const MAX_REDIRECTS: usize = 10;

  enum CachedFile {
    Redirect(Url),
    Content(ResolvedFilePathWithBytes),
  }

  fn use_cache_entry(current_url: &Url, entry: CacheEntry, is_first_download: bool) -> Result<CachedFile> {
    if let Some(location) = entry.metadata.headers.get("location") {
      // cached redirect — follow it
      return Ok(CachedFile::Redirect(current_url.join(location)?));
    }
    // cached content
    let resolved_url = Url::parse(&entry.metadata.url).unwrap_or_else(|_| current_url.clone());
    Ok(CachedFile::Content(ResolvedFilePathWithBytes {
      source: PathSource::Remote(RemotePathSource { url: resolved_url }),
      is_first_download,
      content: entry.content,
    }))
  }

  let cache = HttpCache::new(environment.clone(), environment.get_cache_dir().join("remote"));
  let now_secs = environment.sys_time_now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
  let mut current_url = url.clone();
  // Redirects downloaded on the way to the content. They're only written to the
  // cache once the content is reached so that a failure part way through a
  // re-checked chain leaves the previously cached chain intact to fall back to.
  let mut pending_redirects: Vec<(Url, HeadersMap)> = Vec::new();
  let write_pending_redirects = |pending_redirects: &[(Url, HeadersMap)]| {
    for (redirect_url, headers) in pending_redirects {
      // ignore errors
      _ = cache.set(redirect_url, headers.clone(), &[]);
    }
  };
  // The previously cached entry of the last re-checked url whose response now
  // leads somewhere else (a redirect with a new target, or content that became
  // a redirect), to fall back to when the new target can't be downloaded.
  let mut previous_chain: Option<(Url, CacheEntry)> = None;
  // Whether a re-checked redirect now points somewhere else. The content it
  // leads to is then new for this configuration even when it was already cached.
  let mut chain_changed = false;

  for _ in 0..=MAX_REDIRECTS {
    let key = cache.cache_item_key(&current_url)?;

    // check cache
    let mut stale_entry = None;
    if let Some(entry) = cache.get(&key)? {
      if is_cache_entry_fresh(&entry, now_secs) {
        match use_cache_entry(&current_url, entry, chain_changed)? {
          CachedFile::Redirect(location) => {
            current_url = location;
            continue;
          }
          CachedFile::Content(file) => {
            write_pending_redirects(&pending_redirects);
            return Ok(file);
          }
        }
      }
      log_debug!(environment, "Checking for changes: {}", current_url);
      stale_entry = Some(entry);
    }

    // download
    let result = match environment.download_file_no_redirects(&current_url, None).await {
      Ok(Some(result)) => result,
      Ok(None) => bail!("Error downloading {} - 404 Not Found", url),
      Err(err) => {
        // keep using the previously downloaded file when checking for changes
        // fails (ex. offline). Nothing is written, so it's checked again next time.
        let cached_file = match stale_entry {
          Some(entry) => use_cache_entry(&current_url, entry, chain_changed)?,
          None => match previous_chain.take() {
            Some((previous_url, entry)) => {
              chain_changed = false;
              use_cache_entry(&previous_url, entry, false)?
            }
            None => return Err(err),
          },
        };
        log_warn!(
          environment,
          "Using the cached version of {} because checking it for changes failed. {:#}",
          url,
          err
        );
        pending_redirects.clear();
        match cached_file {
          CachedFile::Redirect(location) => {
            current_url = location;
            continue;
          }
          CachedFile::Content(file) => return Ok(file),
        }
      }
    };

    // follow redirect
    if let Some(location) = result.headers.get("location") {
      let location = current_url.join(location)?;
      if let Some(entry) = stale_entry {
        let previous_location = entry.metadata.headers.get("location").map(|l| current_url.join(l)).transpose()?;
        if previous_location.as_ref() != Some(&location) {
          chain_changed = true;
          previous_chain = Some((current_url.clone(), entry));
        }
      }
      pending_redirects.push((current_url, result.headers));
      current_url = location;
      continue;
    }

    let is_changed = chain_changed
      || match &stale_entry {
        Some(entry) => entry.content != result.content,
        None => true,
      };

    // cache the response and ignore errors
    write_pending_redirects(&pending_redirects);
    _ = cache.set(&current_url, result.headers.clone(), &result.content);

    return Ok(ResolvedFilePathWithBytes {
      source: PathSource::Remote(RemotePathSource { url: current_url }),
      is_first_download: is_changed,
      content: result.content,
    });
  }

  bail!("Too many redirects for {}", url)
}

/// A cached remote file is used without checking its URL for changes while it's
/// younger than its freshness lifetime.
fn is_cache_entry_fresh(entry: &CacheEntry, now_secs: u64) -> bool {
  match entry.metadata.time {
    Some(time) => now_secs.saturating_sub(time) < freshness_lifetime_secs(&entry.metadata.headers),
    // cached by a version of dprint that didn't record the time
    None => false,
  }
}

/// How long a cached response is used before it's checked for changes. A
/// response marked `Cache-Control: immutable` (ex. a CDN URL pinned to a tag or
/// commit) promises not to change for its `max-age`, so it's trusted for that
/// long when that's longer than the default.
fn freshness_lifetime_secs(headers: &HeadersMap) -> u64 {
  let directives = || {
    headers
      .iter()
      .filter(|(name, _)| name.eq_ignore_ascii_case("cache-control"))
      .flat_map(|(_, value)| value.split(','))
      .map(|directive| directive.trim())
  };
  if !directives().any(|directive| directive.eq_ignore_ascii_case("immutable")) {
    return REMOTE_FILE_MAX_AGE_SECS;
  }
  let max_age = directives().find_map(|directive| {
    let (name, value) = directive.split_once('=')?;
    if name.trim().eq_ignore_ascii_case("max-age") {
      value.trim().trim_matches('"').parse::<u64>().ok()
    } else {
      None
    }
  });
  std::cmp::max(max_age.unwrap_or(0), REMOTE_FILE_MAX_AGE_SECS)
}

pub async fn fetch_file_or_url_bytes(url_or_file_path: &PathSource, environment: &impl Environment) -> Result<Vec<u8>> {
  match url_or_file_path {
    PathSource::Remote(path_source) => Ok(environment.download_file_err_404(&path_source.url, None).await?.1.content),
    PathSource::Local(path_source) => Ok(environment.read_file_bytes(&path_source.path)?),
    PathSource::Npm(_) => bail!("Cannot fetch bytes directly for an npm specifier"),
  }
}

pub fn resolve_url_or_file_path_to_path_source(url_or_file_path: &str, base: &PathSource, environment: &impl Environment) -> Result<PathSource> {
  if let Some(url) = try_parse_url(url_or_file_path) {
    if url.cannot_be_a_base() {
      // relative url
      if let PathSource::Remote(remote_base) = base {
        let url = remote_base.url.join(url_or_file_path)?;
        return Ok(PathSource::new_remote(url));
      }
    } else {
      // handle file urls (ex. file:///C:/some/folder/file.json)
      if url.scheme() == "file" {
        match url.to_file_path() {
          Ok(file_path) => return Ok(PathSource::new_local(environment.canonicalize(file_path)?)),
          Err(()) => bail!("Problem converting file url `{}` to file path.", url_or_file_path),
        }
      }
      return Ok(PathSource::new_remote(url));
    }
  } else if let Some(rest) = url_or_file_path.strip_prefix("~/") {
    // handle home directory
    match environment.get_home_dir() {
      Some(home_dir) => {
        let path = if rest.is_empty() {
          home_dir
        } else {
          environment.canonicalize(home_dir.join(rest))?
        };
        return Ok(PathSource::new_local(path));
      }
      None => bail!("Failed to get home directory path"),
    }
  }

  Ok(match base {
    PathSource::Remote(remote_base) => {
      let url = remote_base.url.join(url_or_file_path)?;
      PathSource::new_remote(url)
    }
    PathSource::Local(local_base) => PathSource::new_local(environment.canonicalize(local_base.path.join(url_or_file_path))?),
    PathSource::Npm(_) => bail!("Cannot resolve a relative path against an npm specifier"),
  })
}

fn try_parse_url(url_or_file_path: &str) -> Option<Url> {
  if is_absolute_windows_file_path(url_or_file_path) {
    return None;
  }

  Url::parse(url_or_file_path).ok()
}

fn is_absolute_windows_file_path(value: &str) -> bool {
  let chars = value.chars().collect::<Vec<_>>();
  return is_alpha(&chars, 0) && matches!(chars.get(1), Some(':')) && is_slash(&chars, 2) && !is_slash(&chars, 3);

  fn is_alpha(chars: &[char], index: usize) -> bool {
    chars.get(index).map(|c| c.is_alphabetic()).unwrap_or(false)
  }

  fn is_slash(chars: &[char], index: usize) -> bool {
    chars.get(index).map(|c| matches!(c, '/' | '\\')).unwrap_or(false)
  }
}

#[cfg(test)]
mod tests {
  use std::path::Path;

  use crate::environment::CanonicalizedPathBuf;
  use crate::environment::TestEnvironment;
  use pretty_assertions::assert_eq;

  use super::super::PathSource;
  use super::*;

  #[test]
  fn should_resolve_a_url() {
    let environment = TestEnvironment::new();
    environment.add_remote_file("https://dprint.dev/test.json", "t".as_bytes());
    environment.clone().run_in_runtime(async move {
      let base = PathSource::new_local(CanonicalizedPathBuf::new_for_testing("/"));
      let url = "https://dprint.dev/test.json";
      let result = resolve_url_or_file_path_to_file_with_cache(url, &base, &environment).await.unwrap();
      assert_eq!(result.source.is_remote(), true);
      assert_eq!(result.is_first_download, true);
      assert_eq!(result.content, "t".as_bytes());

      // should get a second time from the cache
      let result = resolve_url_or_file_path_to_file_with_cache(url, &base, &environment).await.unwrap();
      assert_eq!(result.source.is_remote(), true);
      assert_eq!(result.is_first_download, false);
      assert_eq!(result.content, "t".as_bytes());
    });
  }

  #[test]
  fn should_resolve_a_relative_path_to_base_url() {
    let environment = TestEnvironment::new();
    environment.add_remote_file("https://dprint.dev/asdf/test/test.json", "t".as_bytes());
    environment.clone().run_in_runtime(async move {
      let base = PathSource::new_remote(Url::parse("https://dprint.dev/asdf/").unwrap());
      let result = resolve_url_or_file_path_to_file_with_cache("test/test.json", &base, &environment)
        .await
        .unwrap();
      assert_eq!(result.source.is_remote(), true);
      assert_eq!(result.source.unwrap_remote().url.as_str(), "https://dprint.dev/asdf/test/test.json");
      assert_eq!(result.content, "t".as_bytes());
    });
  }

  #[cfg(windows)]
  #[test]
  fn should_resolve_a_file_url_on_windows() {
    let environment = TestEnvironment::new();
    environment.mk_dir_all("C:\\test").unwrap();
    environment.write_file("C:\\test\\test.json", "{}").unwrap();
    environment.clone().run_in_runtime(async move {
      use crate::environment::CanonicalizedPathBuf;

      let base = PathSource::new_local(CanonicalizedPathBuf::new_for_testing("V:\\"));
      let result = resolve_url_or_file_path_to_file_with_cache("file://C:/test/test.json", &base, &environment)
        .await
        .unwrap();
      assert_eq!(result.source.is_local(), true);
      assert_eq!(result.source.unwrap_local().path, CanonicalizedPathBuf::new_for_testing("C:\\test\\test.json"));
    });
  }

  #[cfg(unix)]
  #[test]
  fn should_resolve_a_file_url_on_unix() {
    let environment = TestEnvironment::new();
    environment.mk_dir_all("/test").unwrap();
    environment.write_file("/test/test.json", "{}").unwrap();
    environment.clone().run_in_runtime(async move {
      let base = PathSource::new_local(CanonicalizedPathBuf::new_for_testing("/"));
      let result = resolve_url_or_file_path_to_file_with_cache("file:///test/test.json", &base, &environment)
        .await
        .unwrap();
      assert_eq!(result.source.is_local(), true);
      assert_eq!(result.source.unwrap_local().path, CanonicalizedPathBuf::new_for_testing("/test/test.json"));
    });
  }

  #[cfg(windows)]
  #[test]
  fn should_resolve_an_absolute_path_on_windows() {
    let environment = TestEnvironment::new();
    environment.mk_dir_all("C:\\test").unwrap();
    environment.write_file("C:\\test\\test.json", "{}").unwrap();
    environment.clone().run_in_runtime(async move {
      let base = PathSource::new_local(CanonicalizedPathBuf::new_for_testing("V:\\"));
      let result = resolve_url_or_file_path_to_file_with_cache("C:\\test\\test.json", &base, &environment)
        .await
        .unwrap();
      assert_eq!(result.source.is_local(), true);
      assert_eq!(result.source.unwrap_local().path, CanonicalizedPathBuf::new_for_testing("C:\\test\\test.json"));
    });
  }

  #[cfg(windows)]
  #[test]
  fn should_resolve_an_absolute_path_on_windows_using_forward_slashes() {
    let environment = TestEnvironment::new();
    environment.mk_dir_all("C:\\test").unwrap();
    environment.write_file("C:\\test\\test.json", "{}").unwrap();
    environment.clone().run_in_runtime(async move {
      let base = PathSource::new_local(CanonicalizedPathBuf::new_for_testing("V:\\"));
      let result = resolve_url_or_file_path_to_file_with_cache("C:/test/test.json", &base, &environment)
        .await
        .unwrap();
      assert_eq!(result.source.is_local(), true);
      assert_eq!(result.source.unwrap_local().path, CanonicalizedPathBuf::new_for_testing("C:\\test\\test.json"));
    });
  }

  #[test]
  fn should_resolve_a_relative_file_path() {
    let environment = TestEnvironment::new();
    environment.mk_dir_all("/test").unwrap();
    environment.write_file("/test/test.json", "{}").unwrap();
    environment.clone().run_in_runtime(async move {
      let base = PathSource::new_local(CanonicalizedPathBuf::new_for_testing("/"));
      let result = resolve_url_or_file_path_to_file_with_cache("test/test.json", &base, &environment)
        .await
        .unwrap();
      assert_eq!(result.source.is_local(), true);
      assert_eq!(result.source.unwrap_local().path, CanonicalizedPathBuf::new_for_testing("/test/test.json"));
    });
  }

  #[test]
  fn should_resolve_a_file_path_relative_to_base_path() {
    let environment = TestEnvironment::new();
    environment.mk_dir_all("/other/test").unwrap();
    environment.write_file("/other/test/test.json", "{}").unwrap();
    environment.clone().run_in_runtime(async move {
      let base = PathSource::new_local(CanonicalizedPathBuf::new_for_testing("/other"));
      let result = resolve_url_or_file_path_to_file_with_cache("test/test.json", &base, &environment)
        .await
        .unwrap();
      assert_eq!(result.source.is_local(), true);
      assert_eq!(
        result.source.unwrap_local().path,
        CanonicalizedPathBuf::new_for_testing("/other/test/test.json")
      );
    });
  }

  #[test]
  fn should_error_when_url_cannot_be_resolved() {
    let environment = TestEnvironment::new();
    environment.clone().run_in_runtime(async move {
      let base = PathSource::new_local(CanonicalizedPathBuf::new_for_testing("/other"));
      let err = resolve_url_or_file_path_to_file_with_cache("https://dprint.dev/test.json", &base, &environment)
        .await
        .err()
        .unwrap();
      assert_eq!(err.to_string(), "Error downloading https://dprint.dev/test.json - 404 Not Found");
    });
  }

  #[test]
  fn should_resolve_url_using_redirected_url() {
    let environment = TestEnvironment::new();
    environment.add_remote_file("https://cdn.example.com/v1/plugin.json", "content".as_bytes());
    environment.add_remote_file_redirect("https://example.com/plugin.json", "https://cdn.example.com/v1/plugin.json");
    environment.clone().run_in_runtime(async move {
      let base = PathSource::new_local(CanonicalizedPathBuf::new_for_testing("/"));
      let result = resolve_url_or_file_path_to_file_with_cache("https://example.com/plugin.json", &base, &environment)
        .await
        .unwrap();
      assert_eq!(result.source.is_remote(), true);
      assert_eq!(result.is_first_download, true);
      assert_eq!(result.content, "content".as_bytes());
      // the resolved path source should use the redirected URL
      assert_eq!(
        result.source,
        PathSource::new_remote(Url::parse("https://cdn.example.com/v1/plugin.json").unwrap())
      );
      // relative paths should resolve against the redirected URL
      let relative_result = resolve_url_or_file_path_to_path_source("downloads/plugin.zip", &result.source.parent(), &environment).unwrap();
      assert_eq!(
        relative_result,
        PathSource::new_remote(Url::parse("https://cdn.example.com/v1/downloads/plugin.zip").unwrap())
      );

      // should get from cache on second request and still have correct redirect URL
      let result2 = resolve_url_or_file_path_to_file_with_cache("https://example.com/plugin.json", &base, &environment)
        .await
        .unwrap();
      assert_eq!(result2.is_first_download, false);
      assert_eq!(
        result2.source,
        PathSource::new_remote(Url::parse("https://cdn.example.com/v1/plugin.json").unwrap())
      );
    });
  }

  #[test]
  fn should_check_for_changes_once_cache_entry_is_stale() {
    const START: u64 = 1_000_000;
    let environment = TestEnvironment::new();
    let url = "https://dprint.dev/test.json";
    environment.add_remote_file(url, "1".as_bytes());
    environment.set_fs_time(START);
    environment.clone().run_in_runtime(async move {
      let base = PathSource::new_local(CanonicalizedPathBuf::new_for_testing("/"));
      let result = resolve_url_or_file_path_to_file_with_cache(url, &base, &environment).await.unwrap();
      assert_eq!(result.content, "1".as_bytes());
      assert_eq!(result.is_first_download, true);

      // a fresh cache entry is used even though the remote file changed
      environment.add_remote_file(url, "2".as_bytes());
      environment.set_fs_time(START + REMOTE_FILE_MAX_AGE_SECS - 1);
      let result = resolve_url_or_file_path_to_file_with_cache(url, &base, &environment).await.unwrap();
      assert_eq!(result.content, "1".as_bytes());
      assert_eq!(result.is_first_download, false);

      // once stale, the url is checked for changes
      environment.set_fs_time(START + REMOTE_FILE_MAX_AGE_SECS);
      let result = resolve_url_or_file_path_to_file_with_cache(url, &base, &environment).await.unwrap();
      assert_eq!(result.content, "2".as_bytes());
      assert_eq!(result.is_first_download, true);

      // checking an unchanged file is not a first download
      environment.set_fs_time(START + REMOTE_FILE_MAX_AGE_SECS * 2);
      let result = resolve_url_or_file_path_to_file_with_cache(url, &base, &environment).await.unwrap();
      assert_eq!(result.content, "2".as_bytes());
      assert_eq!(result.is_first_download, false);

      // the check made the entry fresh again
      environment.add_remote_file(url, "3".as_bytes());
      environment.set_fs_time(START + REMOTE_FILE_MAX_AGE_SECS * 3 - 1);
      let result = resolve_url_or_file_path_to_file_with_cache(url, &base, &environment).await.unwrap();
      assert_eq!(result.content, "2".as_bytes());
      assert_eq!(result.is_first_download, false);

      environment.set_fs_time(START + REMOTE_FILE_MAX_AGE_SECS * 3);
      let result = resolve_url_or_file_path_to_file_with_cache(url, &base, &environment).await.unwrap();
      assert_eq!(result.content, "3".as_bytes());
      assert_eq!(result.is_first_download, true);
      assert_eq!(environment.take_stderr_messages(), Vec::<String>::new());
    });
  }

  #[test]
  fn should_use_stale_cache_entry_when_checking_for_changes_fails() {
    const START: u64 = 1_000_000;
    let environment = TestEnvironment::new();
    let url = "https://dprint.dev/test.json";
    environment.add_remote_file(url, "1".as_bytes());
    environment.set_fs_time(START);
    environment.clone().run_in_runtime(async move {
      let base = PathSource::new_local(CanonicalizedPathBuf::new_for_testing("/"));
      let result = resolve_url_or_file_path_to_file_with_cache(url, &base, &environment).await.unwrap();
      assert_eq!(result.content, "1".as_bytes());

      environment.add_remote_file_error(url, "network down");
      environment.set_fs_time(START + REMOTE_FILE_MAX_AGE_SECS);
      let result = resolve_url_or_file_path_to_file_with_cache(url, &base, &environment).await.unwrap();
      assert_eq!(result.content, "1".as_bytes());
      assert_eq!(result.is_first_download, false);
      assert_eq!(result.source, PathSource::new_remote(Url::parse(url).unwrap()));
      assert_eq!(
        environment.take_stderr_messages(),
        vec!["Using the cached version of https://dprint.dev/test.json because checking it for changes failed. network down".to_string()]
      );

      // the failure doesn't refresh the entry, so it's checked again next time
      environment.add_remote_file(url, "2".as_bytes());
      let result = resolve_url_or_file_path_to_file_with_cache(url, &base, &environment).await.unwrap();
      assert_eq!(result.content, "2".as_bytes());
      assert_eq!(result.is_first_download, true);
    });
  }

  #[test]
  fn should_error_when_stale_remote_file_no_longer_exists() {
    const START: u64 = 1_000_000;
    let environment = TestEnvironment::new();
    let url = "https://dprint.dev/test.json";
    environment.add_remote_file(url, "1".as_bytes());
    environment.set_fs_time(START);
    environment.clone().run_in_runtime(async move {
      let base = PathSource::new_local(CanonicalizedPathBuf::new_for_testing("/"));
      resolve_url_or_file_path_to_file_with_cache(url, &base, &environment).await.unwrap();

      environment.remove_remote_file(url);
      environment.set_fs_time(START + REMOTE_FILE_MAX_AGE_SECS);
      let err = resolve_url_or_file_path_to_file_with_cache(url, &base, &environment).await.err().unwrap();
      assert_eq!(err.to_string(), "Error downloading https://dprint.dev/test.json - 404 Not Found");
    });
  }

  #[test]
  fn should_check_stale_redirect_for_changes() {
    const START: u64 = 1_000_000;
    let environment = TestEnvironment::new();
    let url = "https://example.com/plugin.json";
    environment.add_remote_file("https://cdn.example.com/v1/plugin.json", "v1".as_bytes());
    environment.add_remote_file("https://cdn.example.com/v2/plugin.json", "v2".as_bytes());
    environment.add_remote_file_redirect(url, "https://cdn.example.com/v1/plugin.json");
    environment.set_fs_time(START);
    environment.clone().run_in_runtime(async move {
      let base = PathSource::new_local(CanonicalizedPathBuf::new_for_testing("/"));
      let result = resolve_url_or_file_path_to_file_with_cache(url, &base, &environment).await.unwrap();
      assert_eq!(result.content, "v1".as_bytes());

      // the cached redirect is used while fresh
      environment.add_remote_file_redirect(url, "https://cdn.example.com/v2/plugin.json");
      environment.set_fs_time(START + REMOTE_FILE_MAX_AGE_SECS - 1);
      let result = resolve_url_or_file_path_to_file_with_cache(url, &base, &environment).await.unwrap();
      assert_eq!(result.content, "v1".as_bytes());
      assert_eq!(result.is_first_download, false);

      // then checked for changes
      environment.set_fs_time(START + REMOTE_FILE_MAX_AGE_SECS);
      let result = resolve_url_or_file_path_to_file_with_cache(url, &base, &environment).await.unwrap();
      assert_eq!(result.content, "v2".as_bytes());
      assert_eq!(result.is_first_download, true);
      assert_eq!(
        result.source,
        PathSource::new_remote(Url::parse("https://cdn.example.com/v2/plugin.json").unwrap())
      );
    });
  }

  #[test]
  fn should_get_if_cache_entry_is_fresh() {
    use crate::cache::SerializedCachedUrlMetadata;

    fn entry(headers: &[(&str, &str)], time: Option<u64>) -> CacheEntry {
      CacheEntry {
        metadata: SerializedCachedUrlMetadata {
          headers: headers.iter().map(|(name, value)| (name.to_string(), value.to_string())).collect(),
          url: "https://dprint.dev/test.json".to_string(),
          time,
        },
        content: Vec::new(),
      }
    }

    const YEAR: u64 = 31_536_000;
    let now = YEAR * 2;
    assert!(is_cache_entry_fresh(&entry(&[], Some(now)), now));
    assert!(is_cache_entry_fresh(&entry(&[], Some(now - REMOTE_FILE_MAX_AGE_SECS + 1)), now));
    assert!(!is_cache_entry_fresh(&entry(&[], Some(now - REMOTE_FILE_MAX_AGE_SECS)), now));
    // cached by an older dprint without a time
    assert!(!is_cache_entry_fresh(&entry(&[], None), now));
    // clock went backwards
    assert!(is_cache_entry_fresh(&entry(&[], Some(now + 10)), now));
    // immutable responses are trusted for their max-age
    let immutable = &[("cache-control", "public, max-age=31536000, s-maxage=31536000, immutable")];
    assert!(is_cache_entry_fresh(&entry(immutable, Some(now - YEAR + 1)), now));
    assert!(!is_cache_entry_fresh(&entry(immutable, Some(now - YEAR)), now));
    assert!(!is_cache_entry_fresh(&entry(immutable, None), now));
    assert!(is_cache_entry_fresh(
      &entry(&[("Cache-Control", "IMMUTABLE, Max-Age=\"31536000\"")], Some(now - YEAR + 1)),
      now
    ));
    // but never for less than the default
    let short_immutable = &[("cache-control", "max-age=60, immutable")];
    assert!(is_cache_entry_fresh(&entry(short_immutable, Some(now - REMOTE_FILE_MAX_AGE_SECS + 1)), now));
    assert!(!is_cache_entry_fresh(&entry(short_immutable, Some(now - REMOTE_FILE_MAX_AGE_SECS)), now));
    assert!(!is_cache_entry_fresh(
      &entry(&[("cache-control", "immutable")], Some(now - REMOTE_FILE_MAX_AGE_SECS)),
      now
    ));
    // max-age alone isn't trusted
    assert!(!is_cache_entry_fresh(
      &entry(
        &[("cache-control", "public, max-age=604800, s-maxage=43200")],
        Some(now - REMOTE_FILE_MAX_AGE_SECS)
      ),
      now
    ));
    assert!(!is_cache_entry_fresh(
      &entry(&[("cache-control", "not-immutable")], Some(now - REMOTE_FILE_MAX_AGE_SECS)),
      now
    ));
  }

  #[test]
  fn should_count_changed_redirect_to_cached_target_as_first_download() {
    const START: u64 = 1_000_000;
    let environment = TestEnvironment::new();
    let url = "https://example.com/plugin.json";
    let v1_url = "https://cdn.example.com/v1/plugin.json";
    let v2_url = "https://cdn.example.com/v2/plugin.json";
    environment.add_remote_file(v1_url, "v1".as_bytes());
    environment.add_remote_file(v2_url, "v2".as_bytes());
    environment.add_remote_file_redirect(url, v1_url);
    environment.set_fs_time(START);
    environment.clone().run_in_runtime(async move {
      let base = PathSource::new_local(CanonicalizedPathBuf::new_for_testing("/"));
      let result = resolve_url_or_file_path_to_file_with_cache(url, &base, &environment).await.unwrap();
      assert_eq!(result.content, "v1".as_bytes());

      // the new target is cached and fresh, but it's new for this url
      environment.set_fs_time(START + REMOTE_FILE_MAX_AGE_SECS - 1);
      let result = resolve_url_or_file_path_to_file_with_cache(v2_url, &base, &environment).await.unwrap();
      assert_eq!(result.is_first_download, true);
      environment.add_remote_file_redirect(url, v2_url);
      environment.set_fs_time(START + REMOTE_FILE_MAX_AGE_SECS);
      let result = resolve_url_or_file_path_to_file_with_cache(url, &base, &environment).await.unwrap();
      assert_eq!(result.content, "v2".as_bytes());
      assert_eq!(result.is_first_download, true);
      assert_eq!(result.source, PathSource::new_remote(Url::parse(v2_url).unwrap()));

      // the re-checked redirect was cached, so the remote redirect isn't consulted again
      environment.add_remote_file_redirect(url, "https://cdn.example.com/v3/plugin.json");
      let result = resolve_url_or_file_path_to_file_with_cache(url, &base, &environment).await.unwrap();
      assert_eq!(result.content, "v2".as_bytes());
      assert_eq!(result.is_first_download, false);
      assert_eq!(environment.take_stderr_messages(), Vec::<String>::new());
    });
  }

  #[test]
  fn should_use_previous_redirect_chain_when_new_target_fails() {
    const START: u64 = 1_000_000;
    let environment = TestEnvironment::new();
    let url = "https://example.com/plugin.json";
    let v1_url = "https://cdn.example.com/v1/plugin.json";
    let v2_url = "https://cdn.example.com/v2/plugin.json";
    environment.add_remote_file(v1_url, "v1".as_bytes());
    environment.add_remote_file_redirect(url, v1_url);
    environment.set_fs_time(START);
    environment.clone().run_in_runtime(async move {
      let base = PathSource::new_local(CanonicalizedPathBuf::new_for_testing("/"));
      resolve_url_or_file_path_to_file_with_cache(url, &base, &environment).await.unwrap();

      // the redirect now points at a target that can't be downloaded
      environment.add_remote_file_redirect(url, v2_url);
      environment.add_remote_file_error(v2_url, "network down");
      environment.set_fs_time(START + REMOTE_FILE_MAX_AGE_SECS);
      let result = resolve_url_or_file_path_to_file_with_cache(url, &base, &environment).await.unwrap();
      assert_eq!(result.content, "v1".as_bytes());
      assert_eq!(result.is_first_download, false);
      assert_eq!(result.source, PathSource::new_remote(Url::parse(v1_url).unwrap()));
      assert_eq!(
        environment.take_stderr_messages(),
        vec!["Using the cached version of https://example.com/plugin.json because checking it for changes failed. network down".to_string()]
      );

      // also when the previous target can't be re-checked either
      environment.add_remote_file_error(v1_url, "network down");
      environment.set_fs_time(START + REMOTE_FILE_MAX_AGE_SECS * 2);
      let result = resolve_url_or_file_path_to_file_with_cache(url, &base, &environment).await.unwrap();
      assert_eq!(result.content, "v1".as_bytes());
      assert_eq!(result.is_first_download, false);
      assert_eq!(environment.take_stderr_messages().len(), 2);

      // the new redirect wasn't cached, so it's picked up once its target is available
      environment.add_remote_file(v2_url, "v2".as_bytes());
      let result = resolve_url_or_file_path_to_file_with_cache(url, &base, &environment).await.unwrap();
      assert_eq!(result.content, "v2".as_bytes());
      assert_eq!(result.is_first_download, true);
      assert_eq!(result.source, PathSource::new_remote(Url::parse(v2_url).unwrap()));
      assert_eq!(environment.take_stderr_messages(), Vec::<String>::new());
    });
  }

  #[test]
  fn should_use_previous_content_when_it_becomes_a_redirect_whose_target_fails() {
    const START: u64 = 1_000_000;
    let environment = TestEnvironment::new();
    let url = "https://example.com/plugin.json";
    let v2_url = "https://cdn.example.com/v2/plugin.json";
    environment.add_remote_file(url, "v1".as_bytes());
    environment.set_fs_time(START);
    environment.clone().run_in_runtime(async move {
      let base = PathSource::new_local(CanonicalizedPathBuf::new_for_testing("/"));
      resolve_url_or_file_path_to_file_with_cache(url, &base, &environment).await.unwrap();

      // the url now redirects to a target that can't be downloaded
      environment.add_remote_file_redirect(url, v2_url);
      environment.add_remote_file_error(v2_url, "network down");
      environment.set_fs_time(START + REMOTE_FILE_MAX_AGE_SECS);
      let result = resolve_url_or_file_path_to_file_with_cache(url, &base, &environment).await.unwrap();
      assert_eq!(result.content, "v1".as_bytes());
      assert_eq!(result.is_first_download, false);
      assert_eq!(result.source, PathSource::new_remote(Url::parse(url).unwrap()));
      assert_eq!(
        environment.take_stderr_messages(),
        vec!["Using the cached version of https://example.com/plugin.json because checking it for changes failed. network down".to_string()]
      );

      // then the redirect is followed once its target is available
      environment.add_remote_file(v2_url, "v2".as_bytes());
      let result = resolve_url_or_file_path_to_file_with_cache(url, &base, &environment).await.unwrap();
      assert_eq!(result.content, "v2".as_bytes());
      assert_eq!(result.is_first_download, true);
      assert_eq!(result.source, PathSource::new_remote(Url::parse(v2_url).unwrap()));
      assert_eq!(environment.take_stderr_messages(), Vec::<String>::new());
    });
  }

  #[test]
  fn should_get_if_absolute_windows_file_path() {
    assert!(is_absolute_windows_file_path("C:/test"));
    assert!(is_absolute_windows_file_path("C:\\test"));
    assert!(!is_absolute_windows_file_path("C://test"));
    assert!(!is_absolute_windows_file_path("C:\\\\test"));
  }

  #[test]
  fn should_resolve_home_dir() {
    let environment = TestEnvironment::new();
    environment.clone().run_in_runtime(async move {
      let base = PathSource::new_local(CanonicalizedPathBuf::new_for_testing("/other"));
      let cases = [
        ("~/file.json", "/home/file.json"),
        ("~/other/file.json", "/home/other/file.json"),
        ("~/a/file.json", "/home/a/file.json"),
      ];
      for (input, expected) in cases {
        environment.mk_dir_all(Path::new(expected).parent().unwrap()).unwrap();
        environment.write_file(expected, "").unwrap();
        let result = resolve_url_or_file_path_to_file_with_cache(input, &base, &environment).await.unwrap();
        assert_eq!(result.source.is_local(), true);
        assert_eq!(result.source.unwrap_local().path, CanonicalizedPathBuf::new_for_testing(expected));
      }
    });
  }
}
