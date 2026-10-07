use std::cell::Cell;
use std::collections::HashMap;
use std::io::Read;
use std::net::SocketAddr;
use std::net::ToSocketAddrs;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::mpsc;
use std::time::Instant;

use anyhow::Result;
use anyhow::bail;
use deno_terminal::colors;
use parking_lot::Mutex;
use url::Url;

use self::unsafe_certs::NoCertificateVerification;

use super::Logger;
use super::certs::get_root_cert_store;
use super::logging::ProgressBarStyle;
use super::logging::ProgressBars;
use super::no_proxy::NoProxy;
use crate::environment::DownloadedFile;
use crate::environment::response_too_large_error;

const MAX_RETRIES: u8 = 2;

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
enum AgentKind {
  Http,
  Https,
}

trait ProxyProvider {
  fn get_proxy(&self, kind: AgentKind) -> Option<&'static str>;
}

struct RealProxyUrlProvider;

impl ProxyProvider for RealProxyUrlProvider {
  fn get_proxy(&self, kind: AgentKind) -> Option<&'static str> {
    fn read_proxy_env_var(env_var_name: &str) -> Option<String> {
      // too much of a hassle to create a seam for the env var reading
      // and this struct is created before an env is created anyway
      #[allow(clippy::disallowed_methods)]
      std::env::var(env_var_name.to_uppercase())
        .ok()
        .or_else(|| std::env::var(env_var_name.to_lowercase()).ok())
        .filter(|v| !v.is_empty())
    }

    static HTTP_PROXY: OnceLock<Option<String>> = OnceLock::new();
    static HTTPS_PROXY: OnceLock<Option<String>> = OnceLock::new();

    match kind {
      AgentKind::Http => HTTP_PROXY.get_or_init(|| read_proxy_env_var("HTTP_PROXY")).as_deref(),
      AgentKind::Https => HTTPS_PROXY.get_or_init(|| read_proxy_env_var("HTTPS_PROXY")).as_deref(),
    }
  }
}

struct AgentStore<TProxyUrlProvider: ProxyProvider> {
  agents: Mutex<HashMap<(AgentKind, Option<&'static str>), ureq::Agent>>,
  logger: Arc<Logger>,
  no_proxy: NoProxy,
  proxy_url_provider: TProxyUrlProvider,
  resolver: BoundedResolver,
  unsafely_ignore_certificates: Option<UnsafelyIgnoreCertificates>,
}

impl<TProxyUrlProvider: ProxyProvider> AgentStore<TProxyUrlProvider> {
  pub fn get(&self, kind: AgentKind, url: &Url) -> Result<ureq::Agent> {
    let proxy = self.proxy_url_provider.get_proxy(kind);
    let proxy = proxy.filter(|_| match url.host_str() {
      Some(host) => !self.no_proxy.contains(host),
      None => true,
    });
    let key = (kind, proxy);
    let mut agents = self.agents.lock();
    let entry = agents.entry(key);
    Ok(match entry {
      std::collections::hash_map::Entry::Occupied(occupied_entry) => occupied_entry.get().clone(),
      std::collections::hash_map::Entry::Vacant(vacant_entry) => {
        // blocking the lock isn't too bad here because generally
        // there will only ever be one of these created ever
        let agent = self.build_agent(kind, proxy)?;
        vacant_entry.insert(agent.clone());
        agent
      }
    })
  }

  fn build_agent(&self, kind: AgentKind, proxy: Option<&str>) -> Result<ureq::Agent> {
    static INSTALLED_PROVIDER: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    let mut agent = ureq::AgentBuilder::new();
    if kind == AgentKind::Https {
      INSTALLED_PROVIDER.get_or_init(|| {
        if let Some(ignored) = &self.unsafely_ignore_certificates {
          log_warn!(
            self.logger,
            "{} Unsafely ignoring {} TLS certificates!",
            colors::yellow("Warning"),
            if ignored.0.is_empty() { "all" } else { "some" }
          );
        }
        let previous_provider = rustls::crypto::ring::default_provider().install_default();
        debug_assert!(previous_provider.is_ok());
      });

      #[allow(clippy::disallowed_methods)]
      let root_store = Arc::new(get_root_cert_store(&self.logger, &|env_var| std::env::var(env_var).ok(), &|file_path| {
        std::fs::read(file_path)
      })?);
      let mut config = rustls::ClientConfig::builder().with_root_certificates(root_store.clone()).with_no_client_auth();
      if let Some(unsafe_certificates) = &self.unsafely_ignore_certificates {
        config
          .dangerous()
          .set_certificate_verifier(Arc::new(NoCertificateVerification::new(unsafe_certificates.0.clone(), root_store)?));
      }
      agent = agent.tls_config(Arc::new(config));
    }
    agent = agent.redirects(0);
    if let Some(proxy) = proxy {
      agent = agent.proxy(ureq::Proxy::new(proxy)?);
    }
    agent = agent.resolver(self.resolver.clone());
    Ok(agent.build())
  }
}

// the deadline of the request being made on this thread, which its host
// lookups give up at (see `BoundedResolver`)
thread_local! {
  static REQUEST_DEADLINE: Cell<Option<Instant>> = const { Cell::new(None) };
}

/// Looks a host up, given as `host:port`.
type Lookup = Arc<dyn Fn(&str) -> std::io::Result<Vec<SocketAddr>> + Send + Sync>;

/// Looks hosts up for an agent's requests, giving up at the deadline of the
/// request being made on the calling thread (see
/// [`BoundedResolver::with_request_deadline`]).
///
/// A lookup itself can't be interrupted and ureq's timeout doesn't cover it,
/// so a request with a deadline does it on a thread of its own and stops
/// waiting for that thread at the deadline. That way a resolver that stalls
/// can't keep a download, and the blocking task it runs in, going past the
/// deadline. The lookup's thread goes on until the lookup gives up on its
/// own, but nothing waits for it.
#[derive(Clone)]
struct BoundedResolver {
  lookup: Lookup,
}

impl BoundedResolver {
  /// Looks hosts up with `lookup`.
  fn new(lookup: Lookup) -> Self {
    Self { lookup }
  }

  /// Looks hosts up the way the operating system does.
  fn system() -> Self {
    Self::new(Arc::new(|netloc: &str| netloc.to_socket_addrs().map(|addresses| addresses.collect())))
  }

  /// Runs `request` as the request on this thread with the deadline, which
  /// its host lookups give up at.
  fn with_request_deadline<T>(deadline: Option<Instant>, request: impl FnOnce() -> T) -> T {
    struct Restore(Option<Instant>);
    impl Drop for Restore {
      fn drop(&mut self) {
        REQUEST_DEADLINE.set(self.0);
      }
    }
    let _restore = Restore(REQUEST_DEADLINE.replace(deadline));
    request()
  }
}

impl ureq::Resolver for BoundedResolver {
  fn resolve(&self, netloc: &str) -> std::io::Result<Vec<SocketAddr>> {
    let Some(deadline) = REQUEST_DEADLINE.get() else {
      return (self.lookup)(netloc);
    };
    let timed_out = || std::io::Error::new(std::io::ErrorKind::TimedOut, format!("Looking up {} timed out.", netloc));
    let remaining = deadline
      .checked_duration_since(Instant::now())
      .filter(|remaining| !remaining.is_zero())
      .ok_or_else(timed_out)?;
    let (sender, receiver) = mpsc::channel();
    std::thread::Builder::new().name("dprint-host-lookup".to_string()).spawn({
      let lookup = self.lookup.clone();
      let netloc = netloc.to_string();
      // the receiver is gone once the request gave up on this
      move || drop(sender.send(lookup(&netloc)))
    })?;
    receiver.recv_timeout(remaining).unwrap_or_else(|_| Err(timed_out()))
  }
}

#[derive(Debug, Clone)]
pub struct UnsafelyIgnoreCertificates(Arc<Vec<String>>);

impl UnsafelyIgnoreCertificates {
  pub fn new(ic_allowlist: Vec<String>) -> Self {
    Self(Arc::new(ic_allowlist))
  }

  pub fn from_env() -> Option<Self> {
    let var = std::env::var_os("DPRINT_IGNORE_CERTS")?;
    if var == "1" {
      Some(Self::new(Vec::new()))
    } else {
      let var = var.to_str()?;
      Some(Self::new(var.split(",").map(|v| v.to_string()).collect()))
    }
  }
}

mod unsafe_certs {
  use std::net::IpAddr;
  use std::sync::Arc;

  use rustls::DigitallySignedStruct;
  use rustls::RootCertStore;
  use rustls::client::WebPkiServerVerifier;
  use rustls::client::danger::HandshakeSignatureValid;
  use rustls::client::danger::ServerCertVerified;
  use rustls::client::danger::ServerCertVerifier;
  use rustls::pki_types::ServerName;
  use rustls::server::VerifierBuilderError;

  // Below code copied and adapted from https://github.com/denoland/deno/blob/540fe7d9e46d6e734af1ce737adf90e8fc00dff8/ext/tls/lib.rs#L68
  // Copyright 2018-2025 the Deno authors. MIT license.

  #[derive(Debug)]
  pub struct NoCertificateVerification {
    ic_allowlist: Arc<Vec<String>>,
    default_verifier: Arc<WebPkiServerVerifier>,
  }

  impl NoCertificateVerification {
    pub fn new(ic_allowlist: Arc<Vec<String>>, root_cert_store: Arc<RootCertStore>) -> Result<Self, VerifierBuilderError> {
      Ok(Self {
        ic_allowlist,
        default_verifier: WebPkiServerVerifier::builder(root_cert_store).build()?,
      })
    }
  }

  impl ServerCertVerifier for NoCertificateVerification {
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
      self.default_verifier.supported_verify_schemes()
    }

    fn verify_server_cert(
      &self,
      end_entity: &rustls::pki_types::CertificateDer<'_>,
      intermediates: &[rustls::pki_types::CertificateDer<'_>],
      server_name: &rustls::pki_types::ServerName<'_>,
      ocsp_response: &[u8],
      now: rustls::pki_types::UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
      if self.ic_allowlist.is_empty() {
        return Ok(ServerCertVerified::assertion());
      }
      let dns_name_or_ip_address = match server_name {
        ServerName::DnsName(dns_name) => dns_name.as_ref().to_owned(),
        ServerName::IpAddress(ip_address) => Into::<IpAddr>::into(*ip_address).to_string(),
        _ => {
          // NOTE(bartlomieju): `ServerName` is a non-exhaustive enum
          // so we have this catch all errors here.
          return Err(rustls::Error::General("Unknown `ServerName` variant".to_string()));
        }
      };
      if self.ic_allowlist.contains(&dns_name_or_ip_address) {
        Ok(ServerCertVerified::assertion())
      } else {
        self
          .default_verifier
          .verify_server_cert(end_entity, intermediates, server_name, ocsp_response, now)
      }
    }

    fn verify_tls12_signature(
      &self,
      message: &[u8],
      cert: &rustls::pki_types::CertificateDer,
      dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
      if self.ic_allowlist.is_empty() {
        return Ok(HandshakeSignatureValid::assertion());
      }
      filter_invalid_encoding_err(self.default_verifier.verify_tls12_signature(message, cert, dss))
    }

    fn verify_tls13_signature(
      &self,
      message: &[u8],
      cert: &rustls::pki_types::CertificateDer,
      dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
      if self.ic_allowlist.is_empty() {
        return Ok(HandshakeSignatureValid::assertion());
      }
      filter_invalid_encoding_err(self.default_verifier.verify_tls13_signature(message, cert, dss))
    }
  }

  fn filter_invalid_encoding_err(to_be_filtered: Result<HandshakeSignatureValid, rustls::Error>) -> Result<HandshakeSignatureValid, rustls::Error> {
    match to_be_filtered {
      Err(rustls::Error::InvalidCertificate(rustls::CertificateError::BadEncoding)) => Ok(HandshakeSignatureValid::assertion()),
      res => res,
    }
  }
}

pub struct RealUrlDownloader {
  progress_bars: Option<Arc<ProgressBars>>,
  agent_store: AgentStore<RealProxyUrlProvider>,
  logger: Arc<Logger>,
}

impl RealUrlDownloader {
  pub fn new(
    progress_bars: Option<Arc<ProgressBars>>,
    logger: Arc<Logger>,
    no_proxy: NoProxy,
    unsafely_ignore_certificates: Option<UnsafelyIgnoreCertificates>,
  ) -> Result<Self> {
    Ok(Self {
      progress_bars,
      agent_store: AgentStore {
        agents: Default::default(),
        logger: logger.clone(),
        no_proxy,
        proxy_url_provider: RealProxyUrlProvider,
        unsafely_ignore_certificates,
        resolver: BoundedResolver::system(),
      },
      logger,
    })
  }

  /// Looks hosts up with `lookup` rather than the operating system.
  #[cfg(test)]
  fn with_lookup(mut self, lookup: Lookup) -> Self {
    self.agent_store.resolver = BoundedResolver::new(lookup);
    self
  }

  /// Downloads the file, giving up at the deadline when there's one, and on a
  /// response over `max_len` bytes when that's given (see
  /// `UrlDownloader::download_file_no_redirects`).
  pub fn download_with_auth(&self, url: &Url, auth: Option<&str>, deadline: Option<Instant>, max_len: Option<usize>) -> Result<Option<DownloadedFile>> {
    let agent = self.get_agent(url)?;
    self.download_with_retries(url, auth, deadline, max_len, &agent)
  }

  fn download_with_retries(
    &self,
    url: &Url,
    auth: Option<&str>,
    deadline: Option<Instant>,
    max_len: Option<usize>,
    agent: &ureq::Agent,
  ) -> Result<Option<DownloadedFile>> {
    let mut last_error = None;
    for retry_count in 0..(MAX_RETRIES + 1) {
      match self.inner_download(url, auth, retry_count, deadline, max_len, agent) {
        Ok(result) => return Ok(result),
        Err(attempt) => {
          if retry_count < MAX_RETRIES {
            log_debug!(self.logger, "Error downloading {} ({}/{}): {:#}", url, retry_count, MAX_RETRIES, attempt.error);
          }
          // retrying doesn't extend the deadline: an attempt that timed out
          // had what was left to it, so the deadline is reached, whatever
          // the timers say of the last few milliseconds
          let deadline_reached = deadline.is_some_and(|deadline| attempt.timed_out || Instant::now() >= deadline);
          last_error = Some(attempt.error);
          if deadline_reached {
            break;
          }
        }
      }
    }
    Err(last_error.unwrap())
  }

  #[cfg(test)]
  pub fn download_no_retries_for_testing(&self, url: &str) -> Result<Option<Vec<u8>>> {
    let url = Url::parse(url)?;
    let agent = self.get_agent(&url)?;
    Ok(
      self
        .inner_download(&url, None, 0, None, None, &agent)
        .map_err(|attempt| attempt.error)?
        .map(|r| r.content),
    )
  }

  fn get_agent(&self, url: &Url) -> Result<ureq::Agent> {
    let kind = match url.scheme() {
      "https" => AgentKind::Https,
      "http" => AgentKind::Http,
      _ => bail!("Not implemented url scheme: {}", url),
    };
    // this is expensive, but we're already in a blocking task here
    self.agent_store.get(kind, url)
  }

  fn inner_download(
    &self,
    url: &Url,
    auth: Option<&str>,
    retry_count: u8,
    deadline: Option<Instant>,
    max_len: Option<usize>,
    agent: &ureq::Agent,
  ) -> Result<Option<DownloadedFile>, AttemptError> {
    let mut request = agent.request_url("GET", url);
    if let Some(deadline) = deadline {
      // the whole request, reading the response included, gives up at the
      // deadline, and so does looking up the host (see `BoundedResolver`)
      match deadline.checked_duration_since(Instant::now()) {
        Some(remaining) if !remaining.is_zero() => request = request.timeout(remaining),
        _ => return Err(AttemptError::timed_out(anyhow::anyhow!("Error downloading {} - Timed out.", url))),
      }
    }
    if let Some(auth) = auth {
      request = request.set("Authorization", auth);
    }
    let resp = match BoundedResolver::with_request_deadline(deadline, || request.call().map_err(Box::new)) {
      Ok(resp) => resp,
      Err(err) if matches!(err.as_ref(), ureq::Error::Status(404, _)) => {
        return Ok(None);
      }
      Err(err) => {
        let timed_out = is_timeout(&err);
        let error = anyhow::anyhow!("Error downloading {} - Error: {:#}", url, err);
        return Err(AttemptError { error, timed_out });
      }
    };

    let status = resp.status();
    let headers: HashMap<String, String> = resp
      .headers_names()
      .into_iter()
      .filter_map(|name| resp.header(&name).map(|value| (name, value.to_string())))
      .collect();

    if (300..400).contains(&status) {
      return Ok(Some(DownloadedFile { headers, content: vec![] }));
    }

    let content_length = headers.get("content-length").and_then(|s| s.parse::<usize>().ok());
    // refused by what it says its length is, before any of it is reserved or read
    if let Some(max_len) = max_len
      && let Some(len) = content_length
      && len > max_len
    {
      return Err(response_too_large_error(url, Some(len), max_len).into());
    }
    let mut reader = resp.into_reader();
    let content = match read_response(
      url,
      retry_count,
      &mut reader,
      content_length.unwrap_or(0),
      max_len,
      self.progress_bars.as_deref(),
    ) {
      Ok(content) => content,
      Err(err) => {
        let timed_out = err.downcast_ref::<std::io::Error>().is_some_and(is_io_timeout);
        let error = anyhow::anyhow!("Error downloading {} - {:#}", url, err);
        return Err(AttemptError { error, timed_out });
      }
    };
    // or by how much of it there turns out to be, of which only a byte over
    // the limit was read
    if let Some(max_len) = max_len
      && content.len() > max_len
    {
      return Err(response_too_large_error(url, None, max_len).into());
    }
    Ok(Some(DownloadedFile { headers, content }))
  }
}

/// Reads the response, and at most a byte more than `max_len` when that's
/// given, so the caller can tell it's over the limit.
/// A failed attempt to download, and whether it timed out, which with a
/// deadline means the deadline is reached (the attempt's time limit is
/// what was left to it), so another attempt would only time out too.
struct AttemptError {
  error: anyhow::Error,
  timed_out: bool,
}

impl AttemptError {
  fn timed_out(error: anyhow::Error) -> Self {
    Self { error, timed_out: true }
  }
}

impl From<anyhow::Error> for AttemptError {
  fn from(error: anyhow::Error) -> Self {
    Self { error, timed_out: false }
  }
}

/// Whether a request failed by timing out: by the request's time limit, or
/// by the host lookup's (see [`BoundedResolver`]).
fn is_timeout(err: &ureq::Error) -> bool {
  let ureq::Error::Transport(transport) = err else {
    return false;
  };
  let mut source = std::error::Error::source(transport);
  while let Some(err) = source {
    if err.downcast_ref::<std::io::Error>().is_some_and(is_io_timeout) {
      return true;
    }
    source = err.source();
  }
  transport.kind() == ureq::ErrorKind::Io && transport.to_string().contains("timed out")
}

fn is_io_timeout(err: &std::io::Error) -> bool {
  matches!(err.kind(), std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock)
}

fn read_response(
  url: &Url,
  retry_count: u8,
  reader: &mut impl Read,
  total_size: usize,
  max_len: Option<usize>,
  progress_bars: Option<&ProgressBars>,
) -> Result<Vec<u8>> {
  let mut final_bytes = Vec::new();
  final_bytes.try_reserve_exact(total_size)?;
  let mut reader = reader.take(max_len.map_or(u64::MAX, |max_len| (max_len as u64).saturating_add(1)));
  if let Some(progress_bars) = &progress_bars {
    let mut buf: [u8; 512] = [0; 512]; // ensure progress bars update often
    let mut message = format!("Downloading {}", url);
    if retry_count > 0 {
      message.push_str(&format!(" (Retry {}/{})", retry_count, MAX_RETRIES))
    }
    let pb = progress_bars.add_progress(message, ProgressBarStyle::Download, total_size);
    loop {
      let bytes_read = reader.read(&mut buf)?;
      if bytes_read == 0 {
        break;
      }
      final_bytes.extend(&buf[..bytes_read]);
      pb.set_position(final_bytes.len());
    }
    pb.finish();
  } else {
    reader.read_to_end(&mut final_bytes)?;
  }
  Ok(final_bytes)
}

#[cfg(test)]
mod test {
  use dprint_core::owned_child::OwnedChild;
  use std::io::ErrorKind;
  use std::io::Read;
  use std::io::Write;
  use std::net::TcpListener;
  use std::net::TcpStream;
  use std::process::Command;
  use std::process::Stdio;
  use std::sync::Arc;
  use std::sync::atomic::AtomicUsize;
  use std::sync::atomic::Ordering;
  use std::time::Duration;
  use std::time::Instant;

  use crate::utils::LogLevel;
  use crate::utils::Logger;
  use crate::utils::LoggerOptions;
  use crate::utils::NoProxy;
  use crate::utils::url::ProxyProvider;

  use super::AgentStore;
  use super::BoundedResolver;
  use super::RealUrlDownloader;
  use super::read_response;

  #[test]
  fn test_agent_store() {
    struct TestProxyProvider;
    impl ProxyProvider for TestProxyProvider {
      fn get_proxy(&self, _kind: super::AgentKind) -> Option<&'static str> {
        Some("user:p@ssw0rd@localhost:9999")
      }
    }

    let logger = Arc::new(Logger::new(&LoggerOptions {
      initial_context_name: "test".to_string(),
      is_stdout_machine_readable: false,
      log_level: LogLevel::Debug,
    }));
    let agent_store = AgentStore {
      agents: Default::default(),
      logger,
      no_proxy: NoProxy::from_string("dprint.dev"),
      proxy_url_provider: TestProxyProvider,
      unsafely_ignore_certificates: None,
      resolver: BoundedResolver::system(),
    };

    let agent = agent_store.get(super::AgentKind::Http, &"http://example.com".parse().unwrap()).unwrap();
    let agent2 = agent_store.get(super::AgentKind::Http, &"http://other.com".parse().unwrap()).unwrap();
    assert_eq!(format!("{:?}", agent), format!("{:?}", agent2));
    assert!(format!("{:?}", agent).contains("p@ssw0rd"));

    let agent3 = agent_store.get(super::AgentKind::Http, &"http://dprint.dev".parse().unwrap()).unwrap();
    assert_ne!(format!("{:?}", agent), format!("{:?}", agent3));
    assert!(!format!("{:?}", agent3).contains("p@ssw0rd"));
  }

  #[test]
  fn unsafe_ignore_cert() {
    fn create_downloader(ignore_option: Option<Vec<String>>) -> RealUrlDownloader {
      RealUrlDownloader::new(
        None,
        Arc::new(Logger::new(&LoggerOptions {
          initial_context_name: "dprint".to_string(),
          is_stdout_machine_readable: true,
          log_level: LogLevel::Silent,
        })),
        NoProxy::from_string(""),
        ignore_option.map(|value| super::UnsafelyIgnoreCertificates(Arc::new(value))),
      )
      .unwrap()
    }

    let Some(_server) = start_deno_server() else {
      return; // ignore if the person running the test suite doesn't have Deno installed
    };

    // wait for the server to start
    {
      let downloader = create_downloader(Some(vec![]));
      for i in 1..=10 {
        let result = downloader.download_no_retries_for_testing("https://localhost:8063");
        if result.is_ok() {
          break;
        } else {
          std::thread::sleep(Duration::from_millis(10 * i));
        }
      }
    }

    // allow all
    {
      let downloader = create_downloader(Some(vec![]));
      let value = downloader.download_no_retries_for_testing("https://localhost:8063").unwrap().unwrap();
      assert_eq!(value, "Hi".as_bytes().to_vec());
    }
    // right host
    {
      let downloader = create_downloader(Some(vec!["localhost".to_string()]));
      let value = downloader.download_no_retries_for_testing("https://localhost:8063").unwrap().unwrap();
      assert_eq!(value, "Hi".as_bytes().to_vec());
    }
    // right ip
    {
      let downloader = create_downloader(Some(vec!["127.0.0.1".to_string()]));
      let value = downloader.download_no_retries_for_testing("https://127.0.0.1:8063").unwrap().unwrap();
      assert_eq!(value, "Hi".as_bytes().to_vec());
    }
    // not specified host
    {
      let downloader = create_downloader(Some(vec!["google.com".to_string()]));
      let result = downloader.download_no_retries_for_testing("https://localhost:8063");
      assert!(result.is_err());
    }
    // not specified ip
    {
      let downloader = create_downloader(Some(vec!["1.1.1.1".to_string()]));
      let result = downloader.download_no_retries_for_testing("https://localhost:8063");
      assert!(result.is_err());
    }
    // not configured, error
    {
      let downloader = create_downloader(None);
      let result = downloader.download_no_retries_for_testing("https://localhost:8063");
      assert!(result.is_err());
    }
  }

  fn create_silent_downloader() -> RealUrlDownloader {
    RealUrlDownloader::new(
      None,
      Arc::new(Logger::new(&LoggerOptions {
        initial_context_name: "dprint".to_string(),
        is_stdout_machine_readable: true,
        log_level: LogLevel::Silent,
      })),
      NoProxy::from_string(""),
      None,
    )
    .unwrap()
  }

  /// Starts a local server that does `respond` with each connection, and
  /// counts the connections.
  fn start_local_server(respond: fn(TcpStream)) -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/schema.json", listener.local_addr().unwrap());
    let connections = Arc::new(AtomicUsize::new(0));
    std::thread::spawn({
      let connections = connections.clone();
      move || {
        for stream in listener.incoming() {
          connections.fetch_add(1, Ordering::SeqCst);
          std::thread::spawn(move || respond(stream.unwrap()));
        }
      }
    });
    (url, connections)
  }

  /// Downloads the url with the deadline, failing the test when it doesn't
  /// give up soon after it rather than hanging.
  fn download_before(url: &str, deadline: Duration) -> (anyhow::Result<Option<Vec<u8>>>, Duration) {
    download_before_with(create_silent_downloader(), url, deadline)
  }

  fn download_before_with(downloader: RealUrlDownloader, url: &str, deadline: Duration) -> (anyhow::Result<Option<Vec<u8>>>, Duration) {
    let (sender, receiver) = std::sync::mpsc::channel();
    let url = url::Url::parse(url).unwrap();
    std::thread::spawn(move || {
      let start = Instant::now();
      let result = downloader.download_with_auth(&url, None, Some(start + deadline), None);
      sender.send((result.map(|file| file.map(|file| file.content)), start.elapsed())).unwrap();
    });
    receiver.recv_timeout(deadline + Duration::from_secs(10)).expect("the download didn't give up")
  }

  /// Downloads the url with the length limit, and gives the content or the
  /// error's text.
  fn download_at_most(url: &str, max_len: usize) -> Result<Option<Vec<u8>>, String> {
    let url = url::Url::parse(url).unwrap();
    create_silent_downloader()
      .download_with_auth(&url, None, None, Some(max_len))
      .map(|file| file.map(|file| file.content))
      .map_err(|err| err.to_string())
  }

  #[test]
  fn refuses_a_response_over_the_limit_rather_than_reading_it() {
    // what it says its length is, before any of it is read (or reserved)
    let (url, _) = start_local_server(|mut stream| {
      let mut request = [0; 1024];
      let _ = stream.read(&mut request);
      let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100000000\r\n\r\n{}");
    });
    assert_eq!(
      download_at_most(&url, 1000),
      Err(format!(
        "Error downloading {} - The response is 100000000 bytes, over the limit of 1000 bytes.",
        url
      ))
    );

    // what there turns out to be of it, when it doesn't say: read up to a
    // byte over the limit, and no further
    let (url, _) = start_local_server(|mut stream| {
      let mut request = [0; 1024];
      let _ = stream.read(&mut request);
      let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n");
      // until the download stops reading and closes the connection (or, were
      // it to read on, long after it should have stopped)
      let chunk = [b'x'; 1024];
      for _ in 0..64 * 1024 {
        if stream.write_all(&chunk).is_err() {
          break;
        }
      }
    });
    assert_eq!(
      download_at_most(&url, 100_000),
      Err(format!("Error downloading {} - The response is over the limit of 100000 bytes.", url))
    );

    // within it, it's read in full
    let (url, _) = start_local_server(|mut stream| {
      let mut request = [0; 1024];
      let _ = stream.read(&mut request);
      let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}");
    });
    assert_eq!(download_at_most(&url, 2), Ok(Some(b"{}".to_vec())));
  }

  #[test]
  fn reads_no_more_than_a_byte_over_the_limit() {
    let url = url::Url::parse("http://localhost/schema.json").unwrap();
    let mut endless = std::io::repeat(b'x').take(1_000_000);
    let content = read_response(&url, 0, &mut endless, 0, Some(100_000), None).unwrap();
    assert_eq!(content.len(), 100_001);
    // and all of it without a limit
    let mut endless = std::io::repeat(b'x').take(1_000_000);
    let content = read_response(&url, 0, &mut endless, 0, None, None).unwrap();
    assert_eq!(content.len(), 1_000_000);
  }

  /// Whether a download that gave up at `deadline` took that long. Its
  /// time limit is the socket's, whose timer can fire a few milliseconds
  /// early on Windows (its timers tick every 15.6 ms), so the lower bound
  /// has that much slack; the upper bounds don't.
  fn gave_up_at(elapsed: Duration, deadline: Duration) -> bool {
    const TIMER_SLACK: Duration = Duration::from_millis(50);
    elapsed + TIMER_SLACK >= deadline
  }

  /// A lookup that never finishes in time, counting how often it's asked.
  fn stalled_lookup(lookups: &Arc<AtomicUsize>) -> super::Lookup {
    let lookups = lookups.clone();
    Arc::new(move |_: &str| {
      lookups.fetch_add(1, Ordering::SeqCst);
      std::thread::sleep(Duration::from_secs(60));
      Err(std::io::Error::other("the lookup gave up"))
    })
  }

  #[test]
  fn a_host_lookup_gives_up_at_the_requests_deadline() {
    use ureq::Resolver;

    let lookups = Arc::new(AtomicUsize::new(0));
    let stalled = BoundedResolver::new(stalled_lookup(&lookups));
    let start = Instant::now();
    let err = BoundedResolver::with_request_deadline(Some(start + Duration::from_millis(200)), || stalled.resolve("stalled.invalid:80")).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
    assert_eq!(err.to_string(), "Looking up stalled.invalid:80 timed out.");
    assert!(start.elapsed() >= Duration::from_millis(200), "{:?}", start.elapsed());
    assert!(start.elapsed() < Duration::from_secs(3), "{:?}", start.elapsed());
    assert_eq!(lookups.load(Ordering::SeqCst), 1);
    // a deadline that passed doesn't start a lookup
    let err = BoundedResolver::with_request_deadline(Some(start), || stalled.resolve("stalled.invalid:80")).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
    assert_eq!(lookups.load(Ordering::SeqCst), 1);

    // one that finishes in time gives its addresses
    let address: std::net::SocketAddr = "127.0.0.1:80".parse().unwrap();
    let quick = BoundedResolver::new(Arc::new(move |_: &str| Ok(vec![address])));
    let addresses = BoundedResolver::with_request_deadline(Some(Instant::now() + Duration::from_secs(10)), || quick.resolve("quick.invalid:80"));
    assert_eq!(addresses.unwrap(), vec![address]);

    // a request without a deadline looks hosts up on its own thread, as is
    let thread = std::thread::current().id();
    let own_thread = BoundedResolver::new(Arc::new(move |_: &str| {
      if std::thread::current().id() == thread {
        Ok(Vec::new())
      } else {
        Err(std::io::Error::other("on another thread"))
      }
    }));
    assert!(own_thread.resolve("quick.invalid:80").is_ok());
    assert!(BoundedResolver::with_request_deadline(None, || own_thread.resolve("quick.invalid:80")).is_ok());
    // and the deadline only applies while the request runs
    BoundedResolver::with_request_deadline(Some(Instant::now()), || {});
    assert!(own_thread.resolve("quick.invalid:80").is_ok());
  }

  #[test]
  fn gives_up_on_a_host_lookup_that_stalls_at_the_deadline() {
    let lookups = Arc::new(AtomicUsize::new(0));
    let downloader = create_silent_downloader().with_lookup(stalled_lookup(&lookups));
    let (result, elapsed) = download_before_with(downloader, "http://stalled.invalid/schema.json", Duration::from_millis(500));
    let err = result.unwrap_err().to_string();
    assert!(err.starts_with("Error downloading http://stalled.invalid/schema.json"), "{}", err);
    assert!(err.contains("Looking up stalled.invalid:80 timed out."), "{}", err);
    assert!(gave_up_at(elapsed, Duration::from_millis(500)), "{:?}", elapsed);
    assert!(elapsed < Duration::from_secs(3), "{:?}", elapsed);
    // without retrying once the deadline passed
    assert_eq!(lookups.load(Ordering::SeqCst), 1);
  }

  #[test]
  fn doesnt_retry_an_attempt_that_timed_out_under_a_deadline() {
    // an attempt's time is what's left to the deadline, so once it times
    // out the deadline is reached, whatever the timers say of the last few
    // milliseconds (which once had a retry connect a second time)
    let lookups = Arc::new(AtomicUsize::new(0));
    let downloader = create_silent_downloader().with_lookup({
      let lookups = lookups.clone();
      Arc::new(move |netloc: &str| {
        lookups.fetch_add(1, Ordering::SeqCst);
        Err(std::io::Error::new(std::io::ErrorKind::TimedOut, format!("Looking up {} timed out.", netloc)))
      })
    });
    let (result, elapsed) = download_before_with(downloader, "http://timing-out.invalid/schema.json", Duration::from_secs(10));
    let err = result.unwrap_err().to_string();
    assert!(err.contains("Looking up timing-out.invalid:80 timed out."), "{}", err);
    assert!(elapsed < Duration::from_secs(3), "{:?}", elapsed);
    assert_eq!(lookups.load(Ordering::SeqCst), 1);

    // without a deadline, a timed out attempt is retried like any other
    let lookups = Arc::new(AtomicUsize::new(0));
    let downloader = create_silent_downloader().with_lookup({
      let lookups = lookups.clone();
      Arc::new(move |_: &str| {
        lookups.fetch_add(1, Ordering::SeqCst);
        Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "timed out"))
      })
    });
    let url = super::Url::parse("http://timing-out.invalid/schema.json").unwrap();
    assert!(downloader.download_with_auth(&url, None, None, None).is_err());
    assert_eq!(lookups.load(Ordering::SeqCst), usize::from(super::MAX_RETRIES) + 1);
  }

  #[test]
  fn gives_up_on_a_server_that_stops_responding_at_the_deadline() {
    // it accepts the connection and reads the request, then never responds
    let (url, connections) = start_local_server(|mut stream| {
      let mut request = [0; 1024];
      let _ = stream.read(&mut request);
      std::thread::sleep(Duration::from_secs(60));
    });
    let (result, elapsed) = download_before(&url, Duration::from_millis(500));
    let err = result.unwrap_err().to_string();
    assert!(err.starts_with(&format!("Error downloading {}", url)), "{}", err);
    assert!(gave_up_at(elapsed, Duration::from_millis(500)), "{:?}", elapsed);
    assert!(elapsed < Duration::from_secs(3), "{:?}", elapsed);
    // without retrying once the deadline passed
    assert_eq!(connections.load(Ordering::SeqCst), 1);
  }

  #[test]
  fn gives_up_on_a_response_that_keeps_trickling_in_at_the_deadline() {
    // the response arrives a byte at a time, which mustn't keep the download
    // going past the deadline
    let (url, _) = start_local_server(|mut stream| {
      let mut request = [0; 1024];
      let _ = stream.read(&mut request);
      let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100000\r\n\r\n");
      while stream.write_all(b" ").is_ok() {
        std::thread::sleep(Duration::from_millis(20));
      }
    });
    let (result, elapsed) = download_before(&url, Duration::from_millis(500));
    assert!(result.is_err());
    assert!(gave_up_at(elapsed, Duration::from_millis(500)), "{:?}", elapsed);
    assert!(elapsed < Duration::from_secs(3), "{:?}", elapsed);
  }

  #[test]
  fn downloads_before_the_deadline() {
    let (url, _) = start_local_server(|mut stream| {
      let mut request = [0; 1024];
      let _ = stream.read(&mut request);
      let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}");
    });
    let (result, _) = download_before(&url, Duration::from_secs(10));
    assert_eq!(result.unwrap(), Some(b"{}".to_vec()));
  }

  fn start_deno_server() -> Option<OwnedChild> {
    let cert = "-----BEGIN CERTIFICATE-----
MIIC+zCCAeOgAwIBAgIJAOFEwE15PYGsMA0GCSqGSIb3DQEBCwUAMBQxEjAQBgNV
BAMMCWxvY2FsaG9zdDAeFw0yNTAyMDEyMzE3MzFaFw0yNjAyMDEyMzE3MzFaMBQx
EjAQBgNVBAMMCWxvY2FsaG9zdDCCASIwDQYJKoZIhvcNAQEBBQADggEPADCCAQoC
ggEBAOeJ3ccDrg9MqBblIzEg+3J4DQJP2t1jHLapX/KjFY4tj1M5m9s9tNyRYDOk
4hhrXpWcOBJ3WvAt4MBgeP0rMP84j9CCH54i58SGJ8SZcvDGODjzwBpl1kks7oAT
CyftJlcpyY+oRcAFhKNz1WLLkm6gXiz9zv8KAd+tz9zlALdoafZteYiqSSwC9JpM
rkE908pJGvVkcpXZyQSxtNasB8W8Be3ZDj05z/dOugNtjssQqw3eGZlIFuIHrWmE
qvnz+VELd+14SgxWidf4QTtfvl1PFDbwysGBdu0sGeNnROTS9gILQDeIH4pbhk6z
L+HPAFYEONJuUTkbH+CQVcHw4BsCAwEAAaNQME4wHQYDVR0OBBYEFODfoAzFiSif
wMW//zOVH9cL8y/RMB8GA1UdIwQYMBaAFODfoAzFiSifwMW//zOVH9cL8y/RMAwG
A1UdEwQFMAMBAf8wDQYJKoZIhvcNAQELBQADggEBAEWXZTIvSObeigjVzQVLiu94
7J5e9ab6MCMsEoj0+F5ZoTnPqYyvp7wyTARZXw84xxKMink0MF9PZzQj7QgTaPJf
G44K4GihZIPcSe0dZ9xZ3xdOmZAVG7zG3JLr/z+Ii2QcWfFB+SrqXVMHtXQtpCo7
W+y72MIkho2wTcuZWNB+cPQXZIILVXFMrB+6zLFjg9+TwcBgnAZhmstZqw4E8FZN
DdxDL9/wuh+uAGgx5pLnpL8aeZoIiDl+FiQ3tI3YU/EE6YC0Q6ky1t1psOwsEWyr
p6EkSRnEWbe+XxT71f2xHp1HbA7CZoiQnN4yU3UPQEIfMq3zFJYKnlc9CRmHgns=
-----END CERTIFICATE-----
";
    let key = "-----BEGIN PRIVATE KEY-----
MIIEvwIBADANBgkqhkiG9w0BAQEFAASCBKkwggSlAgEAAoIBAQDnid3HA64PTKgW
5SMxIPtyeA0CT9rdYxy2qV/yoxWOLY9TOZvbPbTckWAzpOIYa16VnDgSd1rwLeDA
YHj9KzD/OI/Qgh+eIufEhifEmXLwxjg488AaZdZJLO6AEwsn7SZXKcmPqEXABYSj
c9Viy5JuoF4s/c7/CgHfrc/c5QC3aGn2bXmIqkksAvSaTK5BPdPKSRr1ZHKV2ckE
sbTWrAfFvAXt2Q49Oc/3TroDbY7LEKsN3hmZSBbiB61phKr58/lRC3fteEoMVonX
+EE7X75dTxQ28MrBgXbtLBnjZ0Tk0vYCC0A3iB+KW4ZOsy/hzwBWBDjSblE5Gx/g
kFXB8OAbAgMBAAECggEBAJeqblS7q1uoOf7tT3USBsN/sf3Osy4LizZ3kjsM6sS8
QUMh3F7rd7p3m82YduXKByX3M5+dATuMwckiKH6luS2lLkdFxVI/yROpUQlt/qWL
Ii7kM/TWulwqi3vnfYpExLWZ0MdCUZYrxyuOZ7uUX7IJaEcOZnYXZwzO/PbUJvj7
tGAOwIDHe9e/FYPbTQSErkbMui5loyloL6K7R/RKQWxcB3iWHNutdceXr8EdwiBw
Ac2LYkt4f+vkm2/8dIfwIwxvjNSBzl/AHYRGJbWbrrP4J7VKJyBn0mdgnPy4+BfM
RJIUJMRrYFCu3GPtC2IvEUsUJk7dVZ+HUxVYEQyXM5ECgYEA9AF1eh+S+WT5TUTI
iSgVUyNg1yFAb6hggCdAH1BmfvwZfWmyLL4WPrjAgSdls88J/HvJWyrLQlhk9Z0U
5JkKuClNYEFwTYmhvMVQ7mFDfsxUfUURvKSOjTaS5iI/z5jGB4R5DrxAgRkgoz3/
KHwi3hOPErrXA57IaCZw+FEeWEMCgYEA8uuFpbyW+hnTvljPHeC0gs1IBLGxCn0m
/AELmFRvTaCwHN/VrOtOU+SsY3f8meS9DRqlcG6aJkxvzRD2QcgOEn0dtP2KTEFC
/sTbolUw9QVP/IujAHpB6pUuCGxELcAYSJmzqpl4pSOG126a84OX/igda3zF51gp
BLWvVeASp0kCgYEAnJP/FdIDF4TDMeFMqi8NmB8guow89CnhWvtU+4M1cpFFriPQ
UUPdtHwMFBT6/2qBZwLsUFNiwX1FtBML4DGRHmJqo7T6YtdJ8X/REldZ35kxMn3L
Bvm1/Eoj9AfQWOAZW6OXp2wIHI/KUNas0QbvvQBiFEvPRCR1R9g7MC2lwk8CgYEA
koWxZVitkEmHyKZ0t0bUWplLuVkcuoDmxNY0kjtLr30e/SueDOEZq8yglpbHDGRG
C+NoqrprzHIKdZynjOIIauqAwqyzgG9U46sF95J/Jyt/JYtsVFtp6v70dywmq5nU
i+X50wsjFCirqsISQJO9WBYGONFX5cTtaOPV0GyJk9ECgYBJtfhIdA+DagWWe0kF
ejEnS6W1Hid3gK0vnDVL6Fws3GXSxifw+XeI+LzOFCHovc6eExWF1qxyRDwi96l3
SUHki7X8yemi+g10U4xJWZcQkbkivDuGLopt87f1BHmy/1O2pFmMwh7+cVQIpm1l
kGUMOx8j0U5fU8eSLECGi0FxBA==
-----END PRIVATE KEY-----
";
    let result = OwnedChild::spawn(
      Command::new("deno")
        .args([
          "eval".to_string(),
          format!("Deno.serve({{ port: 8063, cert: `{cert}`, key: `{key}` }}, req => new Response('Hi'));"),
        ])
        .stderr(Stdio::null())
        .stdout(Stdio::null()),
    );
    match result {
      Ok(child) => Some(child),
      Err(err) => {
        if err.kind() == ErrorKind::NotFound {
          None
        } else {
          panic!("Failed running Deno: {:#}", err);
        }
      }
    }
  }
}
