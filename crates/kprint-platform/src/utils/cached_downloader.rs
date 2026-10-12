use anyhow::Result;
use anyhow::anyhow;
use kprint_async_runtime::async_trait;
use std::cell::RefCell;
use std::collections::HashMap;
use url::Url;

use crate::environment::DownloadedFile;
use crate::environment::UrlDownloader;

type CachedDownloadResult = Result<Option<Vec<u8>>, String>;
/// The url and the length limit.
type CachedDownloadKey = (String, Option<usize>);

pub struct CachedDownloader<TInner: UrlDownloader> {
  inner: TInner,
  results: RefCell<HashMap<CachedDownloadKey, CachedDownloadResult>>,
}

impl<TInner: UrlDownloader> CachedDownloader<TInner> {
  pub fn new(inner: TInner) -> Self {
    Self {
      inner,
      results: Default::default(),
    }
  }
}

#[async_trait(?Send)]
impl<TInner: UrlDownloader> UrlDownloader for CachedDownloader<TInner> {
  async fn download_file_no_redirects(&self, url: &Url, max_len: Option<usize>) -> Result<Option<DownloadedFile>> {
    let key = (url.to_string(), max_len);
    {
      if let Some(result) = self.results.borrow().get(&key) {
        return match result {
          Ok(result) => Ok(result.clone().map(|content| DownloadedFile {
            headers: Default::default(),
            content,
          })),
          Err(err) => Err(anyhow!("{:#}", err)),
        };
      }
    }
    let result = self.inner.download_file_no_redirects(url, max_len).await;
    self.results.borrow_mut().insert(
      key,
      match &result {
        Ok(result) => Ok(result.as_ref().map(|r| r.content.clone())),
        Err(err) => Err(format!("{:#}", err)),
      },
    );
    result
  }
}

#[cfg(test)]
mod test {
  use super::*;
  use crate::environment::TestEnvironmentBuilder;
  use kprint_platform::environment::*;

  #[test]
  fn should_download_and_cache() {
    let mut builder = TestEnvironmentBuilder::new();
    let exists_url = Url::parse("http://localhost/test.txt").unwrap();
    let not_exists_url = Url::parse("http://localhost/non-existent.txt").unwrap();
    let environment = builder.add_remote_file(exists_url.as_str(), "1").build();
    environment.clone().run_in_runtime(async move {
      let downloader = CachedDownloader::new(environment.clone());

      // should cache when not exists
      assert!(downloader.download_file_no_redirects(&not_exists_url, None).await.as_ref().unwrap().is_none());
      environment.add_remote_file_bytes(not_exists_url.as_str(), Vec::new());
      assert!(downloader.download_file_no_redirects(&not_exists_url, None).await.as_ref().unwrap().is_none());

      // should get data and have it cached
      assert_eq!(
        downloader
          .download_file_no_redirects(&exists_url, None)
          .await
          .as_ref()
          .unwrap()
          .as_ref()
          .unwrap()
          .content,
        "1".as_bytes()
      );
      environment.add_remote_file_bytes(exists_url.as_str(), Vec::new());
      assert_eq!(
        downloader
          .download_file_no_redirects(&exists_url, None)
          .await
          .as_ref()
          .unwrap()
          .as_ref()
          .unwrap()
          .content,
        "1".as_bytes()
      );
    });
  }
}
