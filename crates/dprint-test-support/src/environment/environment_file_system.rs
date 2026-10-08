use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use tree_fucker::CancellationToken;
use tree_fucker::CaseSensitivity;
use tree_fucker::Ceilings;
use tree_fucker::Continuation;
use tree_fucker::Crossing;
use tree_fucker::DirectoryListing;
use tree_fucker::DomainCapabilities;
use tree_fucker::DomainCaseSensitivity;
use tree_fucker::DomainIdentity;
use tree_fucker::DomainKey;
use tree_fucker::Enrichment;
use tree_fucker::EnrichmentBatch;
use tree_fucker::EntryInfo;
use tree_fucker::EntryKind;
use tree_fucker::FileSystem;
use tree_fucker::FsCapabilities;
use tree_fucker::FsError;
use tree_fucker::Lease;
use tree_fucker::ListingSession;
use tree_fucker::Metadata;
use tree_fucker::MetadataFields;
use tree_fucker::Observation;
use tree_fucker::ProbeResult;
use tree_fucker::RelativePath;
use tree_fucker::SessionCost;
use tree_fucker::SessionOutcome;
use tree_fucker::WatchId;
use tree_fucker::WatcherKind;
use tree_fucker::WatcherSink;

use super::DirEntry;
use super::PathKind;
use dprint_platform::environment::*;

/// Scannable file system backed by an environment's `dir_info`.
///
/// Used for environments that aren't the real file system, such as the test
/// environment's in-memory one. Everything is one storage domain, and every
/// listed entry has a known kind.
pub struct EnvironmentFileSystem<TEnvironment: FileSystemEnvironment> {
  environment: TEnvironment,
}

impl<TEnvironment: FileSystemEnvironment> EnvironmentFileSystem<TEnvironment> {
  pub fn new(environment: TEnvironment) -> Self {
    Self { environment }
  }
}

fn domain() -> ProbeResult {
  ProbeResult {
    identity: DomainIdentity::Known(DomainKey::declared(1)),
    capabilities: DomainCapabilities {
      case: DomainCaseSensitivity::Sensitive,
      ..DomainCapabilities::inline()
    },
    is_domain_root: false,
    crossed: Crossing::NotCrossed,
    directory_case: None,
  }
}

fn directory() -> EntryInfo {
  EntryInfo {
    kind: EntryKind::Directory,
    metadata: Metadata::default(),
    identity: None,
  }
}

impl<TEnvironment: FileSystemEnvironment> FileSystem for EnvironmentFileSystem<TEnvironment> {
  fn capabilities(&self) -> FsCapabilities {
    FsCapabilities {
      case: CaseSensitivity::Sensitive,
      watcher: WatcherKind::None,
    }
  }

  fn canonicalize(&self, root: &Path) -> Result<PathBuf, FsError> {
    Ok(self.environment.canonicalize(root)?.into_path_buf())
  }

  fn resolve_domain(&self, _root: &Path, _path: &RelativePath, _parent: Option<&ProbeResult>) -> Result<ProbeResult, FsError> {
    Ok(domain())
  }

  fn metadata(&self, root: &Path, path: &RelativePath) -> Result<EntryInfo, FsError> {
    let kind = match self.environment.path_kind(path.under(root)) {
      Some(PathKind::Dir) => EntryKind::Directory,
      Some(PathKind::File) => EntryKind::File,
      Some(PathKind::Symlink) => EntryKind::Symlink,
      None => return Err(FsError::NotFound),
    };
    Ok(EntryInfo {
      kind,
      metadata: Metadata::default(),
      identity: None,
    })
  }

  fn open_listing(&self, root: &Path, path: &RelativePath, ceilings: Ceilings, _cancel: CancellationToken) -> Box<dyn ListingSession> {
    Box::new(EnvironmentListing {
      environment: self.environment.clone(),
      directory: path.under(root),
      ceilings,
    })
  }

  fn enrich(&self, _root: &Path, _path: &RelativePath, _batch: &EnrichmentBatch) -> Result<Enrichment, FsError> {
    Err(FsError::Unsupported("the environment lists no metadata".to_string()))
  }

  fn watch(&self, _root: &Path, _path: &RelativePath, _recursive: bool, _sink: Arc<dyn WatcherSink>) -> Result<WatchId, FsError> {
    Err(FsError::Unsupported("the environment has no watcher".to_string()))
  }

  fn unwatch(&self, _watch: WatchId) {}
}

/// Lists a whole directory in a single step.
struct EnvironmentListing<TEnvironment: FileSystemEnvironment> {
  environment: TEnvironment,
  directory: PathBuf,
  ceilings: Ceilings,
}

impl<TEnvironment: FileSystemEnvironment> ListingSession for EnvironmentListing<TEnvironment> {
  fn resume(self: Box<Self>, _lease: Lease) -> (Continuation, SessionCost) {
    let mut cost = SessionCost {
      listing_operations: 1,
      ..SessionCost::default()
    };
    let entries = match self.environment.dir_info(&self.directory) {
      Ok(entries) => entries,
      Err(err) => return (Continuation::Finished(SessionOutcome::Failed(err.into())), cost),
    };
    let entries = entries
      .into_iter()
      .filter_map(|entry| {
        let (path, kind) = match entry {
          DirEntry::Directory(path) => (path, EntryKind::Directory),
          DirEntry::File { path, .. } => (path, EntryKind::File),
        };
        let name = path.file_name()?.to_os_string();
        Some(tree_fucker::DirEntry::new(
          name,
          Observation::resolved(EntryInfo {
            kind,
            metadata: Metadata::default(),
            identity: None,
          }),
        ))
      })
      .collect::<Vec<_>>();
    cost.entries_enumerated = entries.len() as u64;
    cost.bytes = entries.iter().map(|entry| tree_fucker::entry_bytes(&entry.name)).sum();
    if let Some(limited) = self.ceilings.exceeded_by(entries.len(), cost.bytes) {
      return (Continuation::Finished(SessionOutcome::ResourceLimited(limited)), cost);
    }
    let listing = DirectoryListing {
      directory: directory(),
      entries,
      supplied_fields: MetadataFields::NONE,
      domain: Box::new(domain()),
      anchor: None,
    };
    (Continuation::Finished(SessionOutcome::Complete(listing)), cost)
  }
}
