/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

pub mod clean_stale;
mod extension;
mod file_tree;
mod io_handler;
mod subscriptions;

#[cfg(test)]
mod tests;

use std::collections::HashSet;
use std::collections::VecDeque;
use std::fmt::Formatter;
use std::pin::Pin;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::task::Context;
use std::task::Poll;

use allocative::Allocative;
use anyhow::Context as _;
use async_trait::async_trait;
use buck2_common::file_ops::FileMetadata;
use buck2_common::file_ops::TrackedFileDigest;
use buck2_common::liveliness_observer::LivelinessGuard;
use buck2_core::buck2_env;
use buck2_core::fs::project::ProjectRoot;
use buck2_core::fs::project_rel_path::ProjectRelativePath;
use buck2_core::fs::project_rel_path::ProjectRelativePathBuf;
use buck2_core::soft_error;
use buck2_data::error::ErrorTag;
use buck2_directory::directory::directory::Directory;
use buck2_directory::directory::directory_iterator::DirectoryIteratorPathStack;
use buck2_directory::directory::directory_ref::DirectoryRef;
use buck2_directory::directory::entry::DirectoryEntry;
use buck2_directory::directory::walk::unordered_entry_walk;
use buck2_error::AnyhowContextForError;
use buck2_error::BuckErrorContext;
use buck2_events::dispatch::current_span;
use buck2_events::dispatch::get_dispatcher;
use buck2_events::dispatch::get_dispatcher_opt;
use buck2_events::dispatch::with_dispatcher_async;
use buck2_events::dispatch::EventDispatcher;
use buck2_events::span::SpanId;
use buck2_execute::artifact_value::ArtifactValue;
use buck2_execute::digest_config::DigestConfig;
use buck2_execute::directory::ActionDirectory;
use buck2_execute::directory::ActionDirectoryEntry;
use buck2_execute::directory::ActionDirectoryMember;
use buck2_execute::directory::ActionDirectoryRef;
use buck2_execute::directory::ActionSharedDirectory;
use buck2_execute::execute::blocking::BlockingExecutor;
use buck2_execute::materialize::materializer::ArtifactNotMaterializedReason;
use buck2_execute::materialize::materializer::CasDownloadInfo;
use buck2_execute::materialize::materializer::CopiedArtifact;
use buck2_execute::materialize::materializer::DeclareMatchOutcome;
use buck2_execute::materialize::materializer::DeferredMaterializerExtensions;
use buck2_execute::materialize::materializer::HttpDownloadInfo;
use buck2_execute::materialize::materializer::MaterializationError;
use buck2_execute::materialize::materializer::Materializer;
use buck2_execute::materialize::materializer::WriteRequest;
use buck2_execute::output_size::OutputSize;
use buck2_execute::re::manager::ReConnectionManager;
use buck2_futures::cancellation::CancellationContext;
use buck2_http::HttpClient;
use buck2_util::threads::check_stack_overflow;
use buck2_util::threads::thread_spawn;
use buck2_wrapper_common::invocation_id::TraceId;
use chrono::DateTime;
use chrono::Duration;
use chrono::Utc;
use derivative::Derivative;
use derive_more::Display;
use dupe::Dupe;
use dupe::OptionDupedExt;
use futures::future;
use futures::future::BoxFuture;
use futures::future::FutureExt;
use futures::future::Shared;
use futures::future::TryFutureExt;
use futures::stream::BoxStream;
use futures::stream::FuturesOrdered;
use futures::stream::Stream;
use futures::stream::StreamExt;
use futures::Future;
use gazebo::prelude::*;
use itertools::Itertools;
use parking_lot::Mutex;
use pin_project::pin_project;
use tokio::runtime::Handle;
use tokio::sync::mpsc;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::oneshot;
use tokio::sync::oneshot::error::TryRecvError;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio::time::Interval;
use tracing::instrument;

use crate::materializers::deferred::clean_stale::CleanResult;
use crate::materializers::deferred::clean_stale::CleanStaleArtifactsCommand;
use crate::materializers::deferred::clean_stale::CleanStaleConfig;
use crate::materializers::deferred::extension::ExtensionCommand;
use crate::materializers::deferred::file_tree::FileTree;
use crate::materializers::deferred::io_handler::DefaultIoHandler;
use crate::materializers::deferred::io_handler::IoHandler;
use crate::materializers::deferred::subscriptions::MaterializerSubscriptionOperation;
use crate::materializers::deferred::subscriptions::MaterializerSubscriptions;
use crate::materializers::sqlite::MaterializerState;
use crate::materializers::sqlite::MaterializerStateSqliteDb;

/// Materializer implementation that defers materialization of declared
/// artifacts until they are needed (i.e. `ensure_materialized` is called).
///
/// # Important
///
/// This materializer defers both CAS fetches and local copies. Therefore, one
/// needs to be careful when choosing to call `ensure_materialized`.
/// Between `declare` and `ensure` calls, the local files could have changed.
///
/// This limits us to only "safely" using the materializer within the
/// computation of a build rule, and only to materialize inputs or outputs of
/// the rule, not random artifacts/paths. That's because:
/// - file changes before/after a build are handled by DICE, which invalidates
///   the outputs that depend on it. The materializer ends up having the wrong
///   information about these outputs. But because it's only used within the
///   build rules, the affected rule is recomputed and therefore has its
///   artifacts re-declared. So when `ensure` is called the materializer has
///   up-to-date information about the artifacts.
/// - file changes during a build are not properly supported by Buck and
///   treated as undefined behaviour, so there's no need to worry about them.
#[derive(Allocative)]
pub struct DeferredMaterializerAccessor<T: IoHandler + 'static> {
    /// Sender to emit commands to the command loop. See `MaterializerCommand`.
    #[allocative(skip)]
    command_sender: Arc<MaterializerSender<T>>,
    /// Handle of the command loop thread. Aborted on Drop.
    /// This thread serves as a queue for declare/ensure requests, making
    /// sure only one executes at a time and in the order they came in.
    /// TODO(rafaelc): aim to replace it with a simple mutex.
    #[allocative(skip)]
    command_thread: Option<std::thread::JoinHandle<()>>,
    /// Determines what to do on `try_materialize_final_artifact`: if true,
    /// materializes them, otherwise skips them.
    materialize_final_artifacts: bool,
    defer_write_actions: bool,

    io: Arc<T>,

    /// Tracked for logging purposes.
    materializer_state_info: buck2_data::MaterializerStateInfo,

    stats: Arc<DeferredMaterializerStats>,

    /// Logs verbose events about materializer to the event log when enabled.
    verbose_materializer_log: bool,
}

pub type DeferredMaterializer = DeferredMaterializerAccessor<DefaultIoHandler>;

impl<T: IoHandler> Drop for DeferredMaterializerAccessor<T> {
    fn drop(&mut self) {
        // We don't try to stop the underlying thread, since in practice when we drop the
        // DeferredMaterializer we are about to just terminate the process.
    }
}

/// Statistics we collect while operating the Deferred Materializer.
#[derive(Allocative, Default)]
pub struct DeferredMaterializerStats {
    declares: AtomicU64,
    declares_reused: AtomicU64,
}

fn access_time_update_max_buffer_size() -> anyhow::Result<usize> {
    buck2_env!("BUCK_ACCESS_TIME_UPDATE_MAX_BUFFER_SIZE", type=usize, default=5000)
}

pub struct DeferredMaterializerConfigs {
    pub materialize_final_artifacts: bool,
    pub defer_write_actions: bool,
    pub ttl_refresh: TtlRefreshConfiguration,
    pub update_access_times: AccessTimesUpdates,
    pub verbose_materializer_log: bool,
    pub clean_stale_config: Option<CleanStaleConfig>,
}

pub struct TtlRefreshConfiguration {
    pub frequency: std::time::Duration,
    pub min_ttl: Duration,
    pub enabled: bool,
}

#[derive(Clone, Copy, Debug, Dupe, PartialEq)]
pub enum AccessTimesUpdates {
    /// Flushes when the buffer is full and periodically
    Full,
    ///Flushes only when buffer is full
    Partial,
    /// Does not flush at all
    Disabled,
}

#[derive(Debug, buck2_error::Error)]
pub enum AccessTimesUpdatesError {
    #[error(
        "Invalid value for buckconfig `[buck2] update_access_times`. Got `{0}`. Expected one of `full`, `partial`  or `disabled`."
    )]
    InvalidValueForConfig(String),
}

impl AccessTimesUpdates {
    pub fn try_new_from_config_value(config_value: Option<&str>) -> anyhow::Result<Self> {
        match config_value {
            None | Some("") | Some("full") => Ok(AccessTimesUpdates::Full),
            Some("partial") => Ok(AccessTimesUpdates::Partial),
            Some("disabled") => Ok(AccessTimesUpdates::Disabled),
            Some(v) => Err(AccessTimesUpdatesError::InvalidValueForConfig(v.to_owned()).into()),
        }
    }
}

#[derive(Copy, Dupe, Clone)]
struct MaterializerCounters {
    sent: &'static AtomicUsize,
    received: &'static AtomicUsize,
}

impl MaterializerCounters {
    /// New counters. Note that this leaks the underlying data. See comments on MaterializerSender.
    fn leak_new() -> Self {
        Self {
            sent: Box::leak(Box::new(AtomicUsize::new(0))),
            received: Box::leak(Box::new(AtomicUsize::new(0))),
        }
    }

    fn ack_received(&self) {
        self.received.fetch_add(1, Ordering::Relaxed);
    }

    fn queue_size(&self) -> usize {
        self.sent
            .load(Ordering::Relaxed)
            .saturating_sub(self.received.load(Ordering::Relaxed))
    }
}

pub struct MaterializerSender<T: 'static> {
    /// High priority commands are processed in order.
    high_priority: mpsc::UnboundedSender<MaterializerCommand<T>>,
    /// Low priority commands are processed in order relative to each other, but high priority
    /// commands can be reordered ahead of them.
    low_priority: mpsc::UnboundedSender<LowPriorityMaterializerCommand>,
    counters: MaterializerCounters,
    /// Liveliness guard held while clean stale executes, dropped to interrupt clean.
    clean_guard: Mutex<Option<LivelinessGuard>>,
}

impl<T> MaterializerSender<T> {
    fn send(
        &self,
        command: MaterializerCommand<T>,
    ) -> Result<(), mpsc::error::SendError<MaterializerCommand<T>>> {
        *self.clean_guard.lock() = None;
        let res = self.high_priority.send(command);
        self.counters.sent.fetch_add(1, Ordering::Relaxed);
        res
    }

    fn send_low_priority(
        &self,
        command: LowPriorityMaterializerCommand,
    ) -> Result<(), mpsc::error::SendError<LowPriorityMaterializerCommand>> {
        let res = self.low_priority.send(command);
        self.counters.sent.fetch_add(1, Ordering::Relaxed);
        res
    }
}

struct MaterializerReceiver<T: 'static> {
    high_priority: mpsc::UnboundedReceiver<MaterializerCommand<T>>,
    low_priority: mpsc::UnboundedReceiver<LowPriorityMaterializerCommand>,
    counters: MaterializerCounters,
}

pub(crate) struct DeferredMaterializerCommandProcessor<T: 'static> {
    io: Arc<T>,
    sqlite_db: Option<MaterializerStateSqliteDb>,
    /// The runtime the deferred materializer will spawn futures on. This is normally the runtime
    /// used by the rest of Buck.
    rt: Handle,
    defer_write_actions: bool,
    log_buffer: LogBuffer,
    /// Keep track of artifact versions to avoid callbacks clobbering state if the state has moved
    /// forward.
    version_tracker: VersionTracker,
    /// Send messages back to the materializer.
    command_sender: Arc<MaterializerSender<T>>,
    /// The actual materializer state.
    tree: ArtifactTree,
    /// Active subscriptions
    subscriptions: MaterializerSubscriptions,
    /// History of refreshes. This *does* grow without bound, but considering the data is pretty
    /// small and we create it infrequently, that's fine.
    ttl_refresh_history: Vec<TtlRefreshHistoryEntry>,
    /// The current ttl_refresh instance, if any exists.
    ttl_refresh_instance: Option<oneshot::Receiver<(DateTime<Utc>, anyhow::Result<()>)>>,
    cancellations: &'static CancellationContext<'static>,
    stats: Arc<DeferredMaterializerStats>,
    access_times_buffer: Option<HashSet<ProjectRelativePathBuf>>,
    verbose_materializer_log: bool,
    daemon_dispatcher: EventDispatcher,
}

struct TtlRefreshHistoryEntry {
    at: DateTime<Utc>,
    outcome: Option<anyhow::Result<()>>,
}

// NOTE: This doesn't derive `Error` and that's on purpose.  We don't want to make it easy (or
// possible, in fact) to add  `context` to this SharedProcessingError and lose the variant.
#[derive(Debug, Clone, Dupe)]
pub enum SharedMaterializingError {
    Error(buck2_error::Error),
    NotFound {
        info: Arc<CasDownloadInfo>,
        debug: Arc<str>,
        directory: ActionDirectoryEntry<ActionSharedDirectory>,
    },
}

#[derive(buck2_error::Error, Debug)]
pub enum MaterializeEntryError {
    #[error(transparent)]
    Error(anyhow::Error),

    /// The artifact wasn't found. This typically means it expired in the CAS.
    #[error("Artifact not found (digest origin: {}, debug: {})", .info.origin.as_display_for_not_found(), .debug)]
    NotFound {
        info: Arc<CasDownloadInfo>,
        debug: Arc<str>,
        directory: ActionDirectoryEntry<ActionSharedDirectory>,
    },
}

impl From<anyhow::Error> for MaterializeEntryError {
    fn from(e: anyhow::Error) -> MaterializeEntryError {
        Self::Error(e)
    }
}

impl From<MaterializeEntryError> for SharedMaterializingError {
    fn from(e: MaterializeEntryError) -> SharedMaterializingError {
        match e {
            MaterializeEntryError::Error(e) => Self::Error(e.into()),
            MaterializeEntryError::NotFound {
                info,
                debug,
                directory,
            } => Self::NotFound {
                info,
                debug,
                directory,
            },
        }
    }
}

/// A future that is materializing on a separate task spawned by the materializer
type MaterializingFuture = Shared<BoxFuture<'static, Result<(), SharedMaterializingError>>>;
/// A future that is cleaning paths on a separate task spawned by the materializer
type CleaningFuture = Shared<BoxFuture<'static, buck2_error::Result<()>>>;

#[derive(Clone)]
enum ProcessingFuture {
    Materializing(MaterializingFuture),
    Cleaning(CleaningFuture),
}

/// Message taken by the `DeferredMaterializer`'s command loop.
enum MaterializerCommand<T: 'static> {
    // [Materializer trait methods -> Command thread]
    /// Takes a list of file paths, computes the materialized file paths of all
    /// of them, and sends the result through the oneshot.
    /// See `Materializer::get_materialized_file_paths` for more information.
    GetMaterializedFilePaths(
        Vec<ProjectRelativePathBuf>,
        oneshot::Sender<Vec<Result<ProjectRelativePathBuf, ArtifactNotMaterializedReason>>>,
    ),

    /// Declares that a set of artifacts already exist
    DeclareExisting(
        Vec<(ProjectRelativePathBuf, ArtifactValue)>,
        Option<SpanId>,
        Option<TraceId>,
    ),

    /// Declares an artifact: its path, value, and how to materialize it.
    Declare(
        ProjectRelativePathBuf,
        ArtifactValue,
        Box<ArtifactMaterializationMethod>, // Boxed to avoid growing all variants
        EventDispatcher,
    ),

    MatchArtifacts(
        Vec<(ProjectRelativePathBuf, ArtifactValue)>,
        oneshot::Sender<bool>,
    ),

    HasArtifact(ProjectRelativePathBuf, oneshot::Sender<bool>),

    /// Declares that given paths are no longer eligible to be materialized by this materializer.
    /// This typically should reflect a change made to the underlying filesystem, either because
    /// the file was created, or because it was removed..
    InvalidateFilePaths(
        Vec<ProjectRelativePathBuf>,
        oneshot::Sender<CleaningFuture>,
        EventDispatcher,
    ),

    /// Takes a list of artifact paths, and materializes all artifacts in the
    /// list that have been declared but not yet been materialized. When the
    /// materialization starts, a future is sent back through the provided
    /// Sender; this future will be resolved when the materialization
    /// concludes (whether successfully or not).
    Ensure(
        Vec<ProjectRelativePathBuf>,
        EventDispatcher,
        oneshot::Sender<BoxStream<'static, Result<(), MaterializationError>>>,
    ),

    Subscription(MaterializerSubscriptionOperation<T>),

    Extension(Box<dyn ExtensionCommand<T>>),

    /// Terminate command processor loop, used by tests
    #[allow(dead_code)]
    Abort,
}

impl<T> std::fmt::Debug for MaterializerCommand<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MaterializerCommand::GetMaterializedFilePaths(paths, _) => {
                write!(f, "GetMaterializedFilePaths({:?}, _)", paths,)
            }
            MaterializerCommand::DeclareExisting(paths, current_span, trace_id) => {
                write!(
                    f,
                    "DeclareExisting({:?}, {:?}, {:?})",
                    paths, current_span, trace_id
                )
            }
            MaterializerCommand::Declare(path, value, method, _dispatcher) => {
                write!(f, "Declare({:?}, {:?}, {:?})", path, value, method,)
            }
            MaterializerCommand::MatchArtifacts(paths, _) => {
                write!(f, "MatchArtifacts({:?})", paths)
            }
            MaterializerCommand::HasArtifact(path, _) => {
                write!(f, "HasArtifact({:?})", path)
            }
            MaterializerCommand::InvalidateFilePaths(paths, ..) => {
                write!(f, "InvalidateFilePaths({:?})", paths)
            }
            MaterializerCommand::Ensure(paths, _, _) => write!(f, "Ensure({:?}, _)", paths,),
            MaterializerCommand::Subscription(op) => write!(f, "Subscription({:?})", op,),
            MaterializerCommand::Extension(ext) => write!(f, "Extension({:?})", ext),
            MaterializerCommand::Abort => write!(f, "Abort"),
        }
    }
}

/// Materializer commands that can be reordered with regard to other commands.
#[derive(Debug)]
enum LowPriorityMaterializerCommand {
    /// [Materialization task -> Command thread]
    /// Notifies the command thread that an artifact was materialized. It takes
    /// the artifact path and the version that was materialized, such that if
    /// a newer version was declared during materialization - which should not
    /// happen under normal conditions - we can react accordingly.
    MaterializationFinished {
        path: ProjectRelativePathBuf,
        timestamp: DateTime<Utc>,
        version: Version,
        result: Result<(), SharedMaterializingError>,
    },

    CleanupFinished {
        path: ProjectRelativePathBuf,
        version: Version,
        result: Result<(), SharedMaterializingError>,
    },
}

/// Tree that stores materialization data for each artifact. Used internally by
/// the `DeferredMaterializer` to keep track of artifacts and how to
/// materialize them.
type ArtifactTree = FileTree<Box<ArtifactMaterializationData>>;

/// The Version of a processing future associated with an artifact. We use this to know if we can
/// clear the processing field when a callback is received, or if more work is expected.
#[derive(Eq, PartialEq, Copy, Clone, Dupe, Debug, Ord, PartialOrd, Display)]
pub struct Version(u64);

#[derive(Debug)]
struct VersionTracker(Version);

impl VersionTracker {
    fn new() -> Self {
        // Each Declare bumps the version, so that if an artifact is declared
        // a second time mid materialization of its previous version, we don't
        // incorrectly assume we materialized the latest version. We start with
        // 1 with because any disk state restored will start with version 0.
        Self(Version(1))
    }

    fn current(&self) -> Version {
        self.0
    }

    /// Increment the current version, return the previous  value
    fn next(&mut self) -> Version {
        let ret = self.current();
        self.0.0 += 1;
        ret
    }
}

pub struct ArtifactMaterializationData {
    /// Taken from `deps` of `ArtifactValue`. Used to materialize deps of the artifact.
    deps: Option<ActionSharedDirectory>,
    stage: ArtifactMaterializationStage,
    /// An optional future that may be processing something at the current path
    /// (for example, materializing or deleting). Any other future that needs to process
    /// this path would need to wait on the existing future to finish.
    /// TODO(scottcao): Turn this into a queue of pending futures.
    processing: Processing,
}

/// Represents a processing future + the version at which it was issued. When receiving
/// notifications about processing futures that finish, their changes are only applied if their
/// version is greater than the current version.
///
/// The version is an internal counter that is shared between the current processing_fut and
/// this data. When multiple operations are queued on a ArtifactMaterializationData, this
/// allows us to identify which one is current.
enum Processing {
    Done(Version),
    Active {
        future: ProcessingFuture,
        version: Version,
    },
}

impl Processing {
    fn current_version(&self) -> Version {
        match self {
            Self::Done(version) => *version,
            Self::Active { version, .. } => *version,
        }
    }

    fn into_future(self) -> Option<ProcessingFuture> {
        match self {
            Self::Done(..) => None,
            Self::Active { future, .. } => Some(future),
        }
    }
}

/// Fingerprint used to identify `ActionSharedDirectory`. We give it an explicit
/// alias because `TrackedFileDigest` can look confusing.
pub type ActionDirectoryFingerprint = TrackedFileDigest;

/// Metadata used to identify an artifact entry without all of its content. Stored on materialized
/// artifacts to check matching artifact optimizations. For `ActionSharedDirectory`, we use its fingerprint,
/// For everything else (files, symlinks, and external symlinks), we use `ActionDirectoryMember`
/// as is because it already holds the metadata we need.
#[derive(Clone, Dupe, Debug)]
pub struct ArtifactMetadata(pub ActionDirectoryEntry<DirectoryMetadata>);

#[derive(Clone, Dupe, Debug, Display)]
#[display("DirectoryMetadata(digest:{},size:{})", fingerprint, total_size)]
pub struct DirectoryMetadata {
    pub fingerprint: ActionDirectoryFingerprint,
    /// Size on disk, if the artifact is a directory.
    /// Storing separately from ArtifactMetadata to avoid calculating when
    /// checking matching artifacts.
    pub total_size: u64,
}

impl ArtifactMetadata {
    fn matches_entry(&self, entry: &ActionDirectoryEntry<ActionSharedDirectory>) -> bool {
        match (&self.0, entry) {
            (
                DirectoryEntry::Dir(DirectoryMetadata { fingerprint, .. }),
                DirectoryEntry::Dir(dir),
            ) => fingerprint == dir.fingerprint(),
            (DirectoryEntry::Leaf(l1), DirectoryEntry::Leaf(l2)) => {
                // In Windows, the 'executable bit' absence can cause Buck2 to re-download identical artifacts.
                // To avoid this, we exclude the executable bit from the comparison.
                if cfg!(windows) {
                    match (l1, l2) {
                        (
                            ActionDirectoryMember::File(meta1),
                            ActionDirectoryMember::File(meta2),
                        ) => return meta1.digest == meta2.digest,
                        _ => (),
                    }
                }
                l1 == l2
            }
            _ => false,
        }
    }

    fn new(entry: &ActionDirectoryEntry<ActionSharedDirectory>) -> Self {
        let new_entry = match entry {
            DirectoryEntry::Dir(dir) => DirectoryEntry::Dir(DirectoryMetadata {
                fingerprint: dir.fingerprint().dupe(),
                total_size: entry.calc_output_count_and_bytes().bytes,
            }),
            DirectoryEntry::Leaf(leaf) => DirectoryEntry::Leaf(leaf.dupe()),
        };
        Self(new_entry)
    }

    fn size(&self) -> u64 {
        match &self.0 {
            DirectoryEntry::Dir(dir) => dir.total_size,
            DirectoryEntry::Leaf(ActionDirectoryMember::File(file_metadata)) => {
                file_metadata.digest.size()
            }
            DirectoryEntry::Leaf(_) => 0,
        }
    }
}

enum ArtifactMaterializationStage {
    /// The artifact was declared, but the materialization hasn't started yet.
    /// If it did start but end with an error, it returns to this stage.
    /// When the the artifact was declared, we spawn a deletion future to delete
    /// all existing paths that conflict with the output paths.
    Declared {
        /// Taken from `entry` of `ArtifactValue`. Used to materialize the actual artifact.
        entry: ActionDirectoryEntry<ActionSharedDirectory>,
        method: Arc<ArtifactMaterializationMethod>,
    },
    /// This artifact was materialized
    Materialized {
        /// Once the artifact is materialized, we don't need the full entry anymore.
        /// We can throw away most of the entry and just keep some metadata used to
        /// check if materialized artifact matches declared artifact.
        metadata: ArtifactMetadata,
        /// Used to clean older artifacts from buck-out.
        last_access_time: DateTime<Utc>,
        /// Artifact declared by running daemon.
        /// Should not be deleted without invalidating DICE nodes, which currently
        /// means killing the daemon.
        active: bool,
    },
}

/// Different ways to materialize the files of an artifact. Some artifacts need
/// to be fetched from the CAS, others copied locally.
#[derive(Debug, Display)]
pub enum ArtifactMaterializationMethod {
    /// The files must be copied from a local path.
    #[display("local copy")]
    LocalCopy(
        /// A map `[dest => src]`, meaning that a file at
        /// `{artifact_path}/{dest}/{p}` needs to be copied from `{src}/{p}`.
        FileTree<ProjectRelativePathBuf>,
        /// Raw list of copied artifacts, as received in `declare_copy`.
        Vec<CopiedArtifact>,
    ),

    #[display("write")]
    Write(Arc<WriteFile>),

    /// The files must be fetched from the CAS.
    #[display("cas download (action: {})", info.origin)]
    CasDownload {
        /// The digest of the action that produced this output
        info: Arc<CasDownloadInfo>,
    },

    /// The file must be fetched over HTTP.
    #[display("http download ({})", info)]
    HttpDownload { info: HttpDownloadInfo },

    #[cfg(test)]
    Test,
}

trait MaterializationMethodToProto {
    fn to_proto(&self) -> buck2_data::MaterializationMethod;
}

impl MaterializationMethodToProto for ArtifactMaterializationMethod {
    fn to_proto(&self) -> buck2_data::MaterializationMethod {
        match self {
            ArtifactMaterializationMethod::LocalCopy { .. } => {
                buck2_data::MaterializationMethod::LocalCopy
            }
            ArtifactMaterializationMethod::CasDownload { .. } => {
                buck2_data::MaterializationMethod::CasDownload
            }
            ArtifactMaterializationMethod::Write { .. } => buck2_data::MaterializationMethod::Write,
            ArtifactMaterializationMethod::HttpDownload { .. } => {
                buck2_data::MaterializationMethod::HttpDownload
            }
            #[cfg(test)]
            ArtifactMaterializationMethod::Test => unimplemented!(),
        }
    }
}

#[async_trait]
impl<T: IoHandler + Allocative> Materializer for DeferredMaterializerAccessor<T> {
    fn name(&self) -> &str {
        "deferred"
    }

    async fn declare_existing(
        &self,
        artifacts: Vec<(ProjectRelativePathBuf, ArtifactValue)>,
    ) -> anyhow::Result<()> {
        let cmd = MaterializerCommand::DeclareExisting(
            artifacts,
            current_span(),
            get_dispatcher_opt().map(|d| d.trace_id().dupe()),
        );
        self.command_sender.send(cmd)?;
        Ok(())
    }

    async fn declare_copy_impl(
        &self,
        path: ProjectRelativePathBuf,
        value: ArtifactValue,
        srcs: Vec<CopiedArtifact>,
        _cancellations: &CancellationContext,
    ) -> anyhow::Result<()> {
        // TODO(rafaelc): get rid of this tree; it'd save a lot of memory.
        let mut srcs_tree = FileTree::new();
        for copied_artifact in srcs.iter() {
            let dest = copied_artifact.dest.strip_prefix(&path)?;

            {
                let mut walk = unordered_entry_walk(
                    copied_artifact
                        .dest_entry
                        .as_ref()
                        .map_dir(Directory::as_ref),
                );
                while let Some((path, entry)) = walk.next() {
                    if let DirectoryEntry::Leaf(ActionDirectoryMember::File(..)) = entry {
                        let path = path.get();
                        let dest_iter = dest.iter().chain(path.iter()).map(|f| f.to_owned());
                        let src = copied_artifact.src.join(&path);
                        srcs_tree.insert(dest_iter, src);
                    }
                }
            }
        }
        let cmd = MaterializerCommand::Declare(
            path,
            value,
            Box::new(ArtifactMaterializationMethod::LocalCopy(srcs_tree, srcs)),
            get_dispatcher(),
        );
        self.command_sender.send(cmd)?;
        Ok(())
    }

    async fn declare_cas_many_impl<'a, 'b>(
        &self,
        info: Arc<CasDownloadInfo>,
        artifacts: Vec<(ProjectRelativePathBuf, ArtifactValue)>,
        _cancellations: &CancellationContext,
    ) -> anyhow::Result<()> {
        for (path, value) in artifacts {
            let cmd = MaterializerCommand::Declare(
                path,
                value,
                Box::new(ArtifactMaterializationMethod::CasDownload { info: info.dupe() }),
                get_dispatcher(),
            );
            self.command_sender.send(cmd)?;
        }
        Ok(())
    }

    async fn declare_http(
        &self,
        path: ProjectRelativePathBuf,
        info: HttpDownloadInfo,
        _cancellations: &CancellationContext,
    ) -> anyhow::Result<()> {
        let cmd = MaterializerCommand::Declare(
            path,
            ArtifactValue::file(info.metadata.dupe()),
            Box::new(ArtifactMaterializationMethod::HttpDownload { info }),
            get_dispatcher(),
        );
        self.command_sender.send(cmd)?;

        Ok(())
    }

    async fn declare_write<'a>(
        &self,
        gen: Box<dyn FnOnce() -> anyhow::Result<Vec<WriteRequest>> + Send + 'a>,
    ) -> anyhow::Result<Vec<ArtifactValue>> {
        if !self.defer_write_actions {
            return self.io.immediate_write(gen).await;
        }

        let contents = gen()?;

        let mut paths = Vec::with_capacity(contents.len());
        let mut values = Vec::with_capacity(contents.len());
        let mut methods = Vec::with_capacity(contents.len());

        for WriteRequest {
            path,
            content,
            is_executable,
        } in contents
        {
            let digest = TrackedFileDigest::from_content(
                &content,
                self.io.digest_config().cas_digest_config(),
            );

            let meta = FileMetadata {
                digest,
                is_executable,
            };

            // NOTE: The zstd crate doesn't release extra capacity of its encoding buffer so it's
            // important to do so here (or the compressed Vec is the same capacity as the input!).
            let compressed_data = zstd::bulk::compress(&content, 0)
                .with_context(|| format!("Error compressing {} bytes", content.len()))?
                .into_boxed_slice();

            paths.push(path);
            values.push(ArtifactValue::file(meta));
            methods.push(ArtifactMaterializationMethod::Write(Arc::new(WriteFile {
                compressed_data,
                decompressed_size: content.len(),
                is_executable,
            })));
        }

        for (path, (value, method)) in std::iter::zip(paths, std::iter::zip(values.iter(), methods))
        {
            self.command_sender.send(MaterializerCommand::Declare(
                path,
                value.dupe(),
                Box::new(method),
                get_dispatcher(),
            ))?;
        }

        Ok(values)
    }

    async fn declare_match(
        &self,
        artifacts: Vec<(ProjectRelativePathBuf, ArtifactValue)>,
    ) -> anyhow::Result<DeclareMatchOutcome> {
        let (sender, recv) = oneshot::channel();

        self.command_sender
            .send(MaterializerCommand::MatchArtifacts(artifacts, sender))?;

        let is_match = recv
            .await
            .context("Recv'ing match future from command thread.")?;

        Ok(is_match.into())
    }

    async fn has_artifact_at(&self, path: ProjectRelativePathBuf) -> anyhow::Result<bool> {
        let (sender, recv) = oneshot::channel();

        self.command_sender
            .send(MaterializerCommand::HasArtifact(path, sender))?;

        let has_artifact = recv
            .await
            .context("Recv'ing match future from command thread.")?;

        Ok(has_artifact)
    }

    async fn invalidate_many(&self, paths: Vec<ProjectRelativePathBuf>) -> anyhow::Result<()> {
        let (sender, recv) = oneshot::channel();

        self.command_sender
            .send(MaterializerCommand::InvalidateFilePaths(
                paths,
                sender,
                get_dispatcher(),
            ))?;

        // Wait on future to finish before invalidation can continue.
        let invalidate_fut = recv.await?;
        invalidate_fut.await.map_err(anyhow::Error::from)
    }

    async fn materialize_many(
        &self,
        artifact_paths: Vec<ProjectRelativePathBuf>,
    ) -> anyhow::Result<BoxStream<'static, Result<(), MaterializationError>>> {
        let event_dispatcher = get_dispatcher();

        // TODO: display [materializing] in superconsole
        let (sender, recv) = oneshot::channel();
        self.command_sender
            .send(MaterializerCommand::Ensure(
                artifact_paths,
                event_dispatcher,
                sender,
            ))
            .context("Sending Ensure() command.")?;
        let materialization_fut = recv
            .await
            .context("Receiving materialization future from command thread.")?;
        Ok(materialization_fut)
    }

    async fn try_materialize_final_artifact(
        &self,
        artifact_path: ProjectRelativePathBuf,
    ) -> anyhow::Result<bool> {
        if self.materialize_final_artifacts {
            self.ensure_materialized(vec![artifact_path]).await?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    async fn get_materialized_file_paths(
        &self,
        paths: Vec<ProjectRelativePathBuf>,
    ) -> anyhow::Result<Vec<Result<ProjectRelativePathBuf, ArtifactNotMaterializedReason>>> {
        if paths.is_empty() {
            return Ok(Vec::new());
        }
        let (sender, recv) = oneshot::channel();
        self.command_sender
            .send(MaterializerCommand::GetMaterializedFilePaths(paths, sender))?;
        Ok(recv.await?)
    }

    fn as_deferred_materializer_extension(&self) -> Option<&dyn DeferredMaterializerExtensions> {
        Some(self as _)
    }

    fn log_materializer_state(&self, events: &EventDispatcher) {
        events.instant_event(self.materializer_state_info.clone())
    }

    fn add_snapshot_stats(&self, snapshot: &mut buck2_data::Snapshot) {
        snapshot.deferred_materializer_declares = self.stats.declares.load(Ordering::Relaxed);
        snapshot.deferred_materializer_declares_reused =
            self.stats.declares_reused.load(Ordering::Relaxed);
        snapshot.deferred_materializer_queue_size = self.command_sender.counters.queue_size() as _;
    }
}

impl DeferredMaterializerAccessor<DefaultIoHandler> {
    /// Spawns two threads (`materialization_loop` and `command_loop`).
    /// Creates and returns a new `DeferredMaterializer` that aborts those
    /// threads when dropped.
    pub fn new(
        fs: ProjectRoot,
        digest_config: DigestConfig,
        buck_out_path: ProjectRelativePathBuf,
        re_client_manager: Arc<ReConnectionManager>,
        io_executor: Arc<dyn BlockingExecutor>,
        configs: DeferredMaterializerConfigs,
        sqlite_db: Option<MaterializerStateSqliteDb>,
        sqlite_state: Option<MaterializerState>,
        http_client: HttpClient,
        daemon_dispatcher: EventDispatcher,
    ) -> anyhow::Result<Self> {
        let (high_priority_sender, high_priority_receiver) = mpsc::unbounded_channel();
        let (low_priority_sender, low_priority_receiver) = mpsc::unbounded_channel();

        let counters = MaterializerCounters::leak_new();

        let command_sender = Arc::new(MaterializerSender {
            high_priority: high_priority_sender,
            low_priority: low_priority_sender,
            counters,
            clean_guard: Mutex::new(None),
        });

        let command_receiver = MaterializerReceiver {
            high_priority: high_priority_receiver,
            low_priority: low_priority_receiver,
            counters,
        };

        let stats = Arc::new(DeferredMaterializerStats::default());

        let num_entries_from_sqlite = sqlite_state.as_ref().map_or(0, |s| s.len()) as u64;
        let materializer_state_info = buck2_data::MaterializerStateInfo {
            num_entries_from_sqlite,
        };
        let access_times_buffer =
            (!matches!(configs.update_access_times, AccessTimesUpdates::Disabled))
                .then(HashSet::new);

        let tree = ArtifactTree::initialize(sqlite_state);

        let io = Arc::new(DefaultIoHandler::new(
            fs,
            digest_config,
            buck_out_path,
            re_client_manager,
            io_executor,
            http_client,
        ));

        let command_processor = {
            let command_sender = command_sender.dupe();
            let rt = Handle::current();
            let stats = stats.dupe();
            let io = io.dupe();
            move |cancellations| DeferredMaterializerCommandProcessor {
                io,
                sqlite_db,
                rt,
                defer_write_actions: configs.defer_write_actions,
                log_buffer: LogBuffer::new(25),
                version_tracker: VersionTracker::new(),
                command_sender,
                tree,
                subscriptions: MaterializerSubscriptions::new(),
                ttl_refresh_history: Vec::new(),
                ttl_refresh_instance: None,
                cancellations,
                stats,
                access_times_buffer,
                verbose_materializer_log: configs.verbose_materializer_log,
                daemon_dispatcher,
            }
        };

        let access_time_update_max_buffer_size = access_time_update_max_buffer_size()?;

        let command_thread = thread_spawn("buck2-dm", {
            move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();

                let cancellations = CancellationContext::never_cancelled();

                rt.block_on(command_processor(cancellations).run(
                    command_receiver,
                    configs.ttl_refresh,
                    access_time_update_max_buffer_size,
                    configs.update_access_times,
                    configs.clean_stale_config,
                ));
            }
        })
        .context("Cannot start materializer thread")?;

        Ok(Self {
            command_thread: Some(command_thread),
            command_sender,
            materialize_final_artifacts: configs.materialize_final_artifacts,
            defer_write_actions: configs.defer_write_actions,
            io,
            materializer_state_info,
            stats,
            verbose_materializer_log: configs.verbose_materializer_log,
        })
    }
}

/// Simple ring buffer for tracking recent commands, to be shown on materializer error
#[derive(Clone)]
struct LogBuffer {
    inner: VecDeque<String>,
}

impl LogBuffer {
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: VecDeque::with_capacity(capacity),
        }
    }

    pub fn push(&mut self, item: String) {
        if self.inner.len() == self.inner.capacity() {
            self.inner.pop_front();
            self.inner.push_back(item);
        } else {
            self.inner.push_back(item);
        }
    }
}

impl std::fmt::Display for LogBuffer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.inner.iter().join("\n"))
    }
}

#[pin_project]
struct CommandStream<T: 'static> {
    high_priority: UnboundedReceiver<MaterializerCommand<T>>,
    low_priority: UnboundedReceiver<LowPriorityMaterializerCommand>,
    refresh_ttl_ticker: Option<Interval>,
    io_buffer_ticker: Interval,
    clean_stale_ticker: Option<Interval>,
    clean_stale_fut: Option<BoxFuture<'static, anyhow::Result<CleanResult>>>,
}

enum Op<T: 'static> {
    Command(MaterializerCommand<T>),
    LowPriorityCommand(LowPriorityMaterializerCommand),
    RefreshTtls,
    Tick,
    CleanStaleRequest,
}

impl<T: 'static> Stream for CommandStream<T> {
    type Item = Op<T>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.project();

        if let Poll::Ready(Some(e)) = this.high_priority.poll_recv(cx) {
            if let MaterializerCommand::Abort = e {
                return Poll::Ready(None);
            }
            return Poll::Ready(Some(Op::Command(e)));
        }

        if let Poll::Ready(Some(e)) = this.low_priority.poll_recv(cx) {
            return Poll::Ready(Some(Op::LowPriorityCommand(e)));
        }

        if let Some(ticker) = this.refresh_ttl_ticker.as_mut() {
            if ticker.poll_tick(cx).is_ready() {
                return Poll::Ready(Some(Op::RefreshTtls));
            }
        }

        if this.io_buffer_ticker.poll_tick(cx).is_ready() {
            return Poll::Ready(Some(Op::Tick));
        }

        // Ensure last clean completed before requesting a new one.
        if let Some(fut) = this.clean_stale_fut.as_mut() {
            if std::pin::pin!(fut).poll(cx).is_ready() {
                *this.clean_stale_fut = None;
            }
        } else if let Some(ticker) = this.clean_stale_ticker.as_mut() {
            if ticker.poll_tick(cx).is_ready() {
                return Poll::Ready(Some(Op::CleanStaleRequest));
            }
        }

        // We can never be done because we never drop the senders, so let's not bother.
        Poll::Pending
    }
}

#[derive(Copy, Clone, Dupe)]
enum MaterializeStack<'a> {
    Empty,
    Child(&'a MaterializeStack<'a>, &'a ProjectRelativePath),
}

impl<'a> Display for MaterializeStack<'a> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        if let MaterializeStack::Empty = self {
            return write!(f, "(empty)");
        }

        // Avoid recursion because we are fighting with stack overflow here,
        // and we do not want another stack overflow when producing error message.
        let mut stack = Vec::new();
        let mut current = *self;
        while let MaterializeStack::Child(parent, path) = current {
            stack.push(path);
            current = *parent;
        }
        write!(f, "{}", stack.iter().rev().join(" -> "))
    }
}

impl<T: IoHandler> DeferredMaterializerCommandProcessor<T> {
    fn spawn_from_rt<F>(rt: &Handle, f: F) -> JoinHandle<F::Output>
    where
        F: std::future::Future + Send + 'static,
        F::Output: Send + 'static,
    {
        // FIXME(JakobDegen): Ideally there wouldn't be a `None` case, but I don't know this code
        // well enough to be confident in removing it
        match get_dispatcher_opt() {
            Some(dispatcher) => rt.spawn(with_dispatcher_async(dispatcher, f)),
            None => rt.spawn(f),
        }
    }

    fn spawn<F>(&self, f: F) -> JoinHandle<F::Output>
    where
        F: std::future::Future + Send + 'static,
        F::Output: Send + 'static,
    {
        Self::spawn_from_rt(&self.rt, f)
    }

    /// Loop that runs for as long as the materializer is alive.
    ///
    /// It takes commands via the `Materializer` trait methods.
    async fn run(
        mut self,
        commands: MaterializerReceiver<T>,
        ttl_refresh: TtlRefreshConfiguration,
        access_time_update_max_buffer_size: usize,
        access_time_updates: AccessTimesUpdates,
        clean_stale_config: Option<CleanStaleConfig>,
    ) {
        let MaterializerReceiver {
            high_priority,
            low_priority,
            counters,
        } = commands;

        let refresh_ttl_ticker = if ttl_refresh.enabled {
            Some(tokio::time::interval_at(
                tokio::time::Instant::now() + ttl_refresh.frequency,
                ttl_refresh.frequency,
            ))
        } else {
            None
        };

        let clean_stale_ticker = clean_stale_config.as_ref().map(|clean_stale_config| {
            tokio::time::interval_at(
                tokio::time::Instant::now() + clean_stale_config.start_offset,
                clean_stale_config.clean_period,
            )
        });

        let io_buffer_ticker = tokio::time::interval(std::time::Duration::from_secs(5));

        let mut stream = CommandStream {
            high_priority,
            low_priority,
            refresh_ttl_ticker,
            io_buffer_ticker,
            clean_stale_ticker,
            clean_stale_fut: None,
        };

        while let Some(op) = stream.next().await {
            match op {
                Op::Command(command) => {
                    self.log_buffer.push(format!("{:?}", command));
                    self.process_one_command(command);
                    counters.ack_received();
                    self.flush_access_times(access_time_update_max_buffer_size);
                }
                Op::LowPriorityCommand(command) => {
                    self.log_buffer.push(format!("{:?}", command));
                    self.process_one_low_priority_command(command);
                    counters.ack_received();
                }
                Op::RefreshTtls => {
                    // It'd be neat to just implement this in the refresh_stream itself and simply
                    // have this loop implicitly drive it, but we can't do that as the stream's
                    // and_then callback would have to capture `&tree`. So, instead, we store the
                    // JoinHandle and just avoid scheduling more than one, though this means we'll
                    // just miss ticks if we do take longer than a tick to run.

                    self.poll_current_ttl_refresh();

                    if self.ttl_refresh_instance.is_none() {
                        let ttl_refresh = self
                            .io
                            .create_ttl_refresh(&self.tree, ttl_refresh.min_ttl)
                            .map(|fut| {
                                // We sue a channel here and not JoinHandle so we get blocking
                                // `try_recv`.
                                let (tx, rx) = oneshot::channel();

                                self.spawn(async {
                                    let res = fut.await;
                                    let _ignored = tx.send((Utc::now(), res));
                                });

                                rx
                            });

                        match ttl_refresh {
                            Some(ttl_refresh) => {
                                self.ttl_refresh_instance = Some(ttl_refresh);
                            }
                            None => self.ttl_refresh_history.push(TtlRefreshHistoryEntry {
                                at: Utc::now(),
                                outcome: None,
                            }),
                        }
                    }
                }
                Op::Tick => {
                    if matches!(access_time_updates, AccessTimesUpdates::Full) {
                        // Force a periodic flush.
                        self.flush_access_times(0);
                    };
                }
                Op::CleanStaleRequest => {
                    if let Some(config) = clean_stale_config.as_ref() {
                        let dispatcher = self.daemon_dispatcher.dupe();
                        let cmd = CleanStaleArtifactsCommand {
                            keep_since_time: chrono::Utc::now() - config.artifact_ttl,
                            dry_run: config.dry_run,
                            tracked_only: false,
                            dispatcher,
                        };
                        stream.clean_stale_fut = Some(cmd.create_clean_fut(&mut self, None));
                    } else {
                        // This should never happen
                        soft_error!(
                            "clean_stale_no_config",
                            anyhow::anyhow!("clean scheduled without being configured").into(),
                            quiet: true
                        )
                        .unwrap();
                    }
                }
            }
        }
    }

    fn process_one_command(&mut self, command: MaterializerCommand<T>) {
        match command {
            // Entry point for `get_materialized_file_paths` calls
            MaterializerCommand::GetMaterializedFilePaths(paths, result_sender) => {
                let result =
                    paths.into_map(|p| self.tree.file_contents_path(p, self.io.digest_config()));
                result_sender.send(result).ok();
            }
            MaterializerCommand::DeclareExisting(artifacts, ..) => {
                for (path, artifact) in artifacts {
                    self.declare_existing(&path, artifact);
                }
            }
            // Entry point for `declare_{copy|cas}` calls
            MaterializerCommand::Declare(path, value, method, event_dispatcher) => {
                self.maybe_log_command(&event_dispatcher, || {
                    buck2_data::materializer_command::Data::Declare(
                        buck2_data::materializer_command::Declare {
                            path: path.to_string(),
                        },
                    )
                });

                self.declare(&path, value, method);

                if self.subscriptions.should_materialize_eagerly(&path) {
                    self.materialize_artifact(&path, event_dispatcher);
                }
            }
            MaterializerCommand::MatchArtifacts(paths, sender) => {
                let all_matches = paths
                    .into_iter()
                    .all(|(path, value)| self.match_artifact(path, value));
                sender.send(all_matches).ok();
            }
            MaterializerCommand::HasArtifact(path, sender) => {
                sender.send(self.has_artifact(path)).ok();
            }
            MaterializerCommand::InvalidateFilePaths(paths, sender, event_dispatcher) => {
                tracing::trace!(
                    paths = ?paths,
                    "invalidate paths",
                );
                self.maybe_log_command(&event_dispatcher, || {
                    buck2_data::materializer_command::Data::InvalidateFilePaths(
                        buck2_data::materializer_command::InvalidateFilePaths {
                            paths: paths.iter().map(|p| p.to_string()).collect::<Vec<_>>(),
                        },
                    )
                });

                let existing_futs = self
                    .tree
                    .invalidate_paths_and_collect_futures(paths, self.sqlite_db.as_mut());

                // TODO: This probably shouldn't return a CleanFuture
                sender
                    .send(
                        async move {
                            join_all_existing_futs(existing_futs?)
                                .await
                                .map_err(buck2_error::Error::from)
                        }
                        .boxed()
                        .shared(),
                    )
                    .ok();
            }
            // Entry point for `ensure_materialized` calls
            MaterializerCommand::Ensure(paths, event_dispatcher, fut_sender) => {
                self.maybe_log_command(&event_dispatcher, || {
                    buck2_data::materializer_command::Data::Ensure(
                        buck2_data::materializer_command::Ensure {
                            paths: paths.iter().map(|p| p.to_string()).collect::<Vec<_>>(),
                        },
                    )
                });

                fut_sender
                    .send(self.materialize_many_artifacts(paths, event_dispatcher))
                    .ok();
            }
            MaterializerCommand::Subscription(sub) => sub.execute(self),
            MaterializerCommand::Extension(ext) => ext.execute(self),
            MaterializerCommand::Abort => unreachable!(),
        }
    }

    fn process_one_low_priority_command(&mut self, command: LowPriorityMaterializerCommand) {
        match command {
            // Materialization of artifact succeeded
            LowPriorityMaterializerCommand::MaterializationFinished {
                path,
                timestamp,
                version,
                result,
            } => {
                self.materialization_finished(path, timestamp, version, result);
            }
            LowPriorityMaterializerCommand::CleanupFinished {
                path,
                version,
                result,
            } => {
                self.tree.cleanup_finished(path, version, result);
            }
        }
    }

    /// Poll the current TTL refresh and remove it if it's done. Add the outcome to
    /// ttl_refresh_history.
    fn poll_current_ttl_refresh(&mut self) {
        self.ttl_refresh_instance = match self.ttl_refresh_instance.take() {
            Some(mut curr) => match curr.try_recv() {
                Ok((at, outcome)) => {
                    // Done
                    self.ttl_refresh_history.push(TtlRefreshHistoryEntry {
                        at,
                        outcome: Some(outcome),
                    });
                    None
                }
                Err(TryRecvError::Empty) => {
                    // Leave it alone.
                    Some(curr)
                }
                Err(TryRecvError::Closed) => {
                    // Shouldnt really happen unless Tokio is shutting down, but be safe.
                    self.ttl_refresh_history.push(TtlRefreshHistoryEntry {
                        at: Utc::now(),
                        outcome: Some(Err(anyhow::anyhow!("Shutdown"))),
                    });
                    None
                }
            },
            None => None,
        };
    }

    fn is_path_materialized(&self, path: &ProjectRelativePath) -> bool {
        match self.tree.prefix_get(&mut path.iter()) {
            None => false,
            Some(data) => {
                matches!(
                    data.stage,
                    ArtifactMaterializationStage::Materialized { .. }
                )
            }
        }
    }

    fn flush_access_times(&mut self, max_buffer_size: usize) -> String {
        if let Some(access_times_buffer) = self.access_times_buffer.as_mut() {
            let size = access_times_buffer.len();
            if size < max_buffer_size {
                return "Access times buffer is not full yet".to_owned();
            }

            let buffer = std::mem::take(access_times_buffer);
            let now = Instant::now();
            tracing::debug!("Flushing access times buffer");
            if let Some(sqlite_db) = self.sqlite_db.as_mut() {
                if let Err(e) = sqlite_db
                    .materializer_state_table()
                    .update_access_times(buffer.iter().collect::<Vec<_>>())
                {
                    soft_error!(
                        "materializer_materialize_error",
                        e.context(self.log_buffer.clone()).into(),
                        quiet: true
                    )
                    .unwrap();
                    return "Found error while updating access times in sqlite db".to_owned();
                }
            }
            return format!(
                "Finished flushing {} entries in {} ms",
                size,
                now.elapsed().as_millis(),
            );
        }
        "Access time updates are disabled. Consider removing `update_access_times = false` from your .buckconfig".to_owned()
    }

    fn materialize_many_artifacts(
        &mut self,
        paths: Vec<ProjectRelativePathBuf>,
        event_dispatcher: EventDispatcher,
    ) -> BoxStream<'static, Result<(), MaterializationError>> {
        let tasks = paths.into_iter().filter_map(|path| {
            self.materialize_artifact(path.as_ref(), event_dispatcher.dupe())
                .map(move |fut| {
                    fut.map_err(move |e| match e {
                        SharedMaterializingError::Error(source) => MaterializationError::Error {
                            path,
                            source: source.into(),
                        },
                        SharedMaterializingError::NotFound {
                            info,
                            debug,
                            directory,
                        } => MaterializationError::NotFound {
                            path,
                            info,
                            debug,
                            directory,
                        },
                    })
                })
        });

        tasks.collect::<FuturesOrdered<_>>().boxed()
    }

    fn declare_existing(&mut self, path: &ProjectRelativePath, value: ArtifactValue) {
        let metadata = ArtifactMetadata::new(value.entry());
        on_materialization(
            self.sqlite_db.as_mut(),
            &self.log_buffer,
            &self.subscriptions,
            path,
            &metadata,
            Utc::now(),
            "materializer_declare_existing_error",
        );

        self.tree.insert(
            path.iter().map(|f| f.to_owned()),
            Box::new(ArtifactMaterializationData {
                deps: value.deps().duped(),
                stage: ArtifactMaterializationStage::Materialized {
                    metadata,
                    last_access_time: Utc::now(),
                    active: true,
                },
                processing: Processing::Done(self.version_tracker.next()),
            }),
        );
    }

    fn declare(
        &mut self,
        path: &ProjectRelativePath,
        value: ArtifactValue,
        method: Box<ArtifactMaterializationMethod>,
    ) {
        self.stats.declares.fetch_add(1, Ordering::Relaxed);

        // Check if artifact to be declared is same as artifact that's already materialized.
        let mut path_iter = path.iter();
        if let Some(data) = self.tree.prefix_get_mut(&mut path_iter) {
            match &data.stage {
                ArtifactMaterializationStage::Materialized {
                    metadata,
                    last_access_time,
                    ..
                } => {
                    // NOTE: This is for testing performance when hitting mismatches with disk
                    // state. Unwrapping isn't ideal, but we can't report errors here.
                    let force_mismatch = buck2_env!(
                        "BUCK2_TEST_FORCE_DECLARE_MISMATCH",
                        bool,
                        applicability = testing
                    )
                    .unwrap();

                    if path_iter.next().is_none()
                        && metadata.matches_entry(value.entry())
                        && !force_mismatch
                    {
                        // In this case, the entry declared matches the already materialized
                        // entry on disk, so just update the deps field but leave
                        // the artifact as materialized.
                        tracing::trace!(
                            path = %path,
                            "already materialized, updating deps only",
                        );
                        let deps = value.deps().duped();
                        data.stage = ArtifactMaterializationStage::Materialized {
                            metadata: metadata.dupe(),
                            last_access_time: *last_access_time,
                            active: true,
                        };
                        data.deps = deps;

                        self.stats.declares_reused.fetch_add(1, Ordering::Relaxed);

                        return;
                    }
                }
                _ => {}
            }
        }

        // We don't have a matching artifact. Declare it.
        let version = self.version_tracker.next();

        tracing::trace!(
            path = %path,
            method = %method,
            value = %value.entry(),
            version = %version,
            "declare artifact",
        );

        // Always invalidate materializer state before actual deleting from filesystem
        // so there will never be a moment where artifact is deleted but materializer
        // thinks it still exists.
        let existing_futs = self
            .tree
            .invalidate_paths_and_collect_futures(vec![path.to_owned()], self.sqlite_db.as_mut());

        let existing_futs = ExistingFutures(existing_futs);

        let method = Arc::from(method);

        // Dispatch Write actions eagerly if possible. We can do this if no cleanup is required. We
        // also check that there are no deps, though for writes there should never be deps.

        // Gate this to not macs for now because we are seeing some instances of extremely slow I/O on macs.
        // This is a very hacky and temporary fix.
        // TODO(scottcao): Eagerly dispatch writes on a lower priority.
        let can_use_write_fast_path =
            !cfg!(target_os = "macos") && existing_futs.is_empty() && value.deps().is_none();

        let future = match &*method {
            ArtifactMaterializationMethod::Write(write) if can_use_write_fast_path => {
                let materialize = self.io.write(
                    path.to_owned(),
                    write.dupe(),
                    version,
                    self.command_sender.dupe(),
                    self.cancellations,
                );
                ProcessingFuture::Materializing(materialize.shared())
            }
            _ => ProcessingFuture::Cleaning(clean_path(
                &self.io,
                path.to_owned(),
                version,
                self.command_sender.dupe(),
                existing_futs,
                &self.rt,
                self.cancellations,
            )),
        };

        let data = Box::new(ArtifactMaterializationData {
            deps: value.deps().duped(),
            stage: ArtifactMaterializationStage::Declared {
                entry: value.entry().dupe(),
                method,
            },
            processing: Processing::Active { future, version },
        });
        self.tree.insert(path.iter().map(|f| f.to_owned()), data);
    }

    /// Check if artifact to be declared is same as artifact that's already materialized.
    #[instrument(level = "debug", skip(self), fields(path = %path, value = %value.entry()))]
    fn match_artifact(&mut self, path: ProjectRelativePathBuf, value: ArtifactValue) -> bool {
        let mut path_iter = path.iter();
        let data = match self.tree.prefix_get_mut(&mut path_iter) {
            Some(data) => data,
            None => {
                tracing::trace!("overlapping below");
                return false;
            }
        };

        // Something was declared above our path.
        if path_iter.next().is_some() {
            tracing::trace!("overlapping above");
            return false;
        }

        let is_match = match &data.stage {
            ArtifactMaterializationStage::Materialized { metadata, .. } => {
                let is_match = value.entry();
                tracing::trace!("materialized: found {}, is_match: {}", metadata.0, is_match);
                metadata.matches_entry(is_match)
            }
            ArtifactMaterializationStage::Declared { entry, .. } => {
                // NOTE: In theory, if something was declared here, we should probably be able to
                // just re-declare over it?
                let is_match = value.entry() == entry;
                tracing::trace!("declared: found {}, is_match: {}", entry, is_match);
                is_match
            }
        };

        // In practice, having a matching artifact with different deps isn't actually *possible*
        // right now, because the deps are derived from the artifact value and we'll always have
        // declared them before. But, if we have a local action cache and persist that as well as
        // materializer state across restarts, then eventually we could have a match with something
        // that hasn't had its deps populated yet (since the materializer state does not know about
        // deps).
        if is_match {
            if let Some(deps) = value.deps() {
                data.deps = Some(deps.dupe())
            }
        }

        is_match
    }

    fn has_artifact(&mut self, path: ProjectRelativePathBuf) -> bool {
        let mut path_iter = path.iter();
        let Some(data) = self.tree.prefix_get_mut(&mut path_iter) else {
            return false;
        };
        // Something was declared above our path.
        if path_iter.next().is_some() {
            return false;
        }

        match &mut data.stage {
            ArtifactMaterializationStage::Materialized {
                metadata: _,
                last_access_time,
                active,
            } => {
                // Treat this case much like a `declare_existing`
                *active = true;
                *last_access_time = Utc::now();
                if let Some(sqlite_db) = &mut self.sqlite_db {
                    if let Err(e) = sqlite_db
                        .materializer_state_table()
                        .update_access_times(vec![&path])
                    {
                        soft_error!("has_artifact_update_time", e.context(self.log_buffer.clone()).into(), quiet: true).unwrap();
                    }
                }
            }
            ArtifactMaterializationStage::Declared { .. } => {
                // Nothing to do here
            }
        }

        true
    }

    #[instrument(level = "debug", skip(self), fields(path = %path))]
    fn materialize_artifact(
        &mut self,
        path: &ProjectRelativePath,
        event_dispatcher: EventDispatcher,
    ) -> Option<MaterializingFuture> {
        self.materialize_artifact_recurse(MaterializeStack::Empty, path, event_dispatcher)
    }

    fn materialize_artifact_recurse(
        &mut self,
        stack: MaterializeStack<'_>,
        path: &ProjectRelativePath,
        event_dispatcher: EventDispatcher,
    ) -> Option<MaterializingFuture> {
        let stack = MaterializeStack::Child(&stack, path);
        // We only add context to outer error, because adding context to the future
        // is expensive. Errors in futures should add stack context themselves.
        match self.materialize_artifact_inner(stack, path, event_dispatcher) {
            Ok(res) => res,
            Err(e) => Some(
                future::err(SharedMaterializingError::Error(
                    e.context(format!("materializing {}", stack)).into(),
                ))
                .boxed()
                .shared(),
            ),
        }
    }

    fn materialize_artifact_inner(
        &mut self,
        stack: MaterializeStack<'_>,
        path: &ProjectRelativePath,
        event_dispatcher: EventDispatcher,
    ) -> anyhow::Result<Option<MaterializingFuture>> {
        // TODO(nga): rewrite without recursion or figure out why we overflow stack here.
        check_stack_overflow().tag(ErrorTag::ServerStackOverflow)?;

        // Get the data about the artifact, or return early if materializing/materialized
        let mut path_iter = path.iter();
        let data = match self.tree.prefix_get_mut(&mut path_iter) {
            // Never declared, nothing to do
            None => {
                tracing::debug!("not known");
                return Ok(None);
            }
            Some(data) => data,
        };

        let path = path.strip_suffix(path_iter.as_path()).unwrap();

        let cleaning_fut = match &data.processing {
            Processing::Active {
                future: ProcessingFuture::Cleaning(f),
                ..
            } => Some(f.clone()),
            Processing::Active {
                future: ProcessingFuture::Materializing(f),
                ..
            } => {
                tracing::debug!("join existing future");
                return Ok(Some(f.clone()));
            }
            Processing::Done(..) => None,
        };

        let deps = data.deps.dupe();
        let check_deps = deps.is_some();
        let entry_and_method = match &mut data.stage {
            ArtifactMaterializationStage::Declared { entry, method } => {
                Some((entry.dupe(), method.dupe()))
            }
            ArtifactMaterializationStage::Materialized {
                ref mut last_access_time,
                ..
            } => match check_deps {
                true => None,
                false => {
                    if let Some(ref mut buffer) = self.access_times_buffer.as_mut() {
                        // TODO (torozco): Why is it legal for something to be Materialized + Cleaning?
                        let timestamp = Utc::now();
                        *last_access_time = timestamp;

                        // NOTE (T142264535): We mostly expect that artifacts are always declared
                        // before they are materialized, but there's one case where that doesn't
                        // happen. In particular, when incremental actions execute, they will trigger
                        // materialization of outputs from a previous run. The artifact isn't really
                        // "active" (it's not an output that we'll use), but we do warn here (when we
                        // probably shouldn't).
                        //
                        // if !active {
                        //     tracing::warn!(path = %path, "Expected artifact to be marked active by declare")
                        // }
                        if buffer.insert(path.to_buf()) {
                            tracing::debug!(
                                "nothing to materialize, adding to access times buffer"
                            );
                        }
                    }

                    return Ok(None);
                }
            },
        };

        let version = self.version_tracker.next();

        tracing::debug!(
            has_entry_and_method = entry_and_method.is_some(),
            method = ?entry_and_method.as_ref().map(|(_, m)| m),
            has_deps = deps.is_some(),
            version = %version,
            cleaning = cleaning_fut.is_some(),
            "materialize artifact"
        );

        // If the artifact copies from other artifacts, we must materialize them first
        let deps_tasks = match entry_and_method.as_ref() {
            Some((_, m)) => match m.as_ref() {
                ArtifactMaterializationMethod::CasDownload { .. }
                | ArtifactMaterializationMethod::HttpDownload { .. }
                | ArtifactMaterializationMethod::Write { .. } => Vec::new(),
                ArtifactMaterializationMethod::LocalCopy(_, copied_artifacts) => copied_artifacts
                    .iter()
                    .filter_map(|a| {
                        self.materialize_artifact_recurse(
                            MaterializeStack::Child(&stack, path),
                            a.src.as_ref(),
                            event_dispatcher.dupe(),
                        )
                    })
                    .collect::<Vec<_>>(),
                #[cfg(test)]
                ArtifactMaterializationMethod::Test => Vec::new(),
            },
            _ => Vec::new(),
        };

        // The artifact might have symlinks pointing to other artifacts. We must
        // materialize them as well, to avoid dangling synlinks.
        let link_deps_tasks = match deps.as_ref() {
            None => Vec::new(),
            Some(deps) => self
                .tree
                .find_artifacts(deps)
                .into_iter()
                .filter_map(|p| {
                    self.materialize_artifact_recurse(
                        MaterializeStack::Child(&stack, path),
                        p.as_ref(),
                        event_dispatcher.dupe(),
                    )
                })
                .collect::<Vec<_>>(),
        };

        // Create a task to await deps and materialize ourselves
        let path_buf = path.to_buf();
        let path_buf_dup = path_buf.clone();
        let io = self.io.dupe();
        let command_sender = self.command_sender.dupe();
        let task = self
            .spawn(async move {
                let cancellations = CancellationContext::never_cancelled(); // spawned

                // Materialize the deps and this entry. This *must* happen in a try block because we
                // need to notify the materializer regardless of whether this succeeds or fails.

                let timestamp = Utc::now();
                let res: Result<(), SharedMaterializingError> = try {
                    // If there is an existing future trying to delete conflicting paths, we must wait for it
                    // to finish before we can start materialization.
                    if let Some(cleaning_fut) = cleaning_fut {
                        cleaning_fut
                        .await
                        .with_context(|| format!(
                            "Error waiting for a previous future to finish cleaning output path {}",
                            &path_buf
                        ))
                        .map_err(|e| SharedMaterializingError::Error(e.into()))?;
                    };

                    // In case this is a local copy, we first need to materialize the
                    // artifacts we are copying from, before we can copy them.
                    for t in deps_tasks {
                        t.await?;
                    }

                    if let Some((entry, method)) = entry_and_method {
                        let materialize = || {
                            io.materialize_entry(
                                path_buf.clone(),
                                method,
                                entry.dupe(),
                                event_dispatcher.dupe(),
                                cancellations,
                            )
                        };

                        // Windows symlinks need to be specified whether it is to a file or target. We rely on the
                        // target file existing to determine this. Ensure symlink targets exist before the entry
                        // is materialized for Windows. For non-Windows, do everything concurrently.
                        if cfg!(windows) {
                            for t in link_deps_tasks {
                                t.await?;
                            }
                            materialize().await?;
                        } else {
                            materialize().await?;
                            for t in link_deps_tasks {
                                t.await?;
                            }
                        }
                    } else {
                        for t in link_deps_tasks {
                            t.await?;
                        }
                    }
                };

                // Materialization finished, notify the command thread
                let _ignored = command_sender.send_low_priority(
                    LowPriorityMaterializerCommand::MaterializationFinished {
                        path: path_buf_dup,
                        timestamp,
                        version,
                        result: res.dupe(),
                    },
                );

                res
            })
            .map(|r| match r {
                Ok(r) => r,
                Err(e) => Err(SharedMaterializingError::Error(e.into())), // Turn the JoinError into a buck2_error::Error.
            })
            .boxed()
            .shared();

        let data = self.tree.prefix_get_mut(&mut path.iter()).unwrap();
        data.processing = Processing::Active {
            future: ProcessingFuture::Materializing(task.clone()),
            version,
        };

        Ok(Some(task))
    }

    #[instrument(level = "debug", skip(self, result), fields(path = %artifact_path))]
    fn materialization_finished(
        &mut self,
        artifact_path: ProjectRelativePathBuf,
        timestamp: DateTime<Utc>,
        version: Version,
        result: Result<(), SharedMaterializingError>,
    ) {
        match self.tree.prefix_get_mut(&mut artifact_path.iter()) {
            Some(info) => {
                if info.processing.current_version() > version {
                    // We can only unset the future if version matches.
                    // Otherwise, we may be unsetting a different future from a newer version.
                    tracing::debug!("version conflict");
                    return;
                }

                if result.is_err() {
                    let version = self.version_tracker.next();
                    match &info.stage {
                        ArtifactMaterializationStage::Materialized { .. } => {
                            tracing::debug!("artifact deps materialization failed, doing nothing");
                            // If already materialized, we only attempted to materialize deps, which means the error did
                            // not occur when materializing the artifact itself. There is no need to clean the artifact path
                            // and doing so will make the filesystem out of sync with materializer state.
                            info.processing = Processing::Done(version);
                        }
                        ArtifactMaterializationStage::Declared { .. } => {
                            tracing::debug!("materialization failed, redeclaring artifact");
                            // Even though materialization failed, something may have still materialized at artifact_path,
                            // so we need to delete anything at artifact_path before we ever retry materializing it.
                            // TODO(scottcao): Once command processor accepts an ArtifactTree instead of initializing one,
                            // add a test case to ensure this behavior.
                            let future = ProcessingFuture::Cleaning(clean_path(
                                &self.io,
                                artifact_path.clone(),
                                version,
                                self.command_sender.dupe(),
                                ExistingFutures::empty(),
                                &self.rt,
                                self.cancellations,
                            ));
                            info.processing = Processing::Active { future, version };
                        }
                    }
                } else {
                    tracing::debug!(has_deps = info.deps.is_some(), "transition to Materialized");
                    let new_stage = match &info.stage {
                        ArtifactMaterializationStage::Materialized { .. } => {
                            // This happens if deps = true. In this case, the entry itself was not
                            // materialized again, but its deps have been. We need to clear the
                            // waiting future regardless.
                            tracing::debug!("artifact is already materialized");
                            None
                        }
                        ArtifactMaterializationStage::Declared {
                            entry,
                            method: _method,
                        } => {
                            let metadata = ArtifactMetadata::new(entry);
                            // NOTE: We only insert this artifact if there isn't an in-progress cleanup
                            // future on this path.
                            on_materialization(
                                self.sqlite_db.as_mut(),
                                &self.log_buffer,
                                &self.subscriptions,
                                &artifact_path,
                                &metadata,
                                timestamp,
                                "materializer_finished_error",
                            );

                            Some(ArtifactMaterializationStage::Materialized {
                                metadata,
                                last_access_time: timestamp,
                                active: true,
                            })
                        }
                    };

                    if let Some(new_stage) = new_stage {
                        info.stage = new_stage;
                    }

                    info.processing = Processing::Done(version);
                }
            }
            None => {
                // NOTE: This can happen if a path got invalidated while it was being materialized.
                tracing::debug!("materialization_finished but path is vacant!")
            }
        }
    }

    fn maybe_log_command<F>(&self, event_dispatcher: &EventDispatcher, f: F)
    where
        F: FnOnce() -> buck2_data::materializer_command::Data,
    {
        if self.verbose_materializer_log {
            let data = Some(f());
            event_dispatcher.instant_event(buck2_data::MaterializerCommand { data });
        }
    }
}

/// Run callbacks for an artifact being materialized at `path`.
fn on_materialization(
    sqlite_db: Option<&mut MaterializerStateSqliteDb>,
    log_buffer: &LogBuffer,
    subscriptions: &MaterializerSubscriptions,
    path: &ProjectRelativePath,
    metadata: &ArtifactMetadata,
    timestamp: DateTime<Utc>,
    error_name: &'static str,
) {
    if let Some(sqlite_db) = sqlite_db {
        if let Err(e) = sqlite_db
            .materializer_state_table()
            .insert(path, metadata, timestamp)
        {
            soft_error!(error_name, e.context(log_buffer.clone()).into(), quiet: true).unwrap();
        }
    }

    subscriptions.on_materialization_finished(path);
}

impl ArtifactTree {
    fn initialize(sqlite_state: Option<MaterializerState>) -> Self {
        let mut tree = ArtifactTree::new();
        if let Some(sqlite_state) = sqlite_state {
            for (path, (metadata, last_access_time)) in sqlite_state.into_iter() {
                tree.insert(
                    path.iter().map(|f| f.to_owned()),
                    Box::new(ArtifactMaterializationData {
                        deps: None,
                        stage: ArtifactMaterializationStage::Materialized {
                            metadata,
                            last_access_time,
                            active: false,
                        },
                        processing: Processing::Done(Version(0)),
                    }),
                );
            }
        }
        tree
    }

    /// Given a path that's (possibly) not yet materialized, returns the path
    /// `contents_path` where its contents can be found. Returns Err if the
    /// contents cannot be found (ex. if it requires HTTP or CAS download)
    ///
    /// Note that the returned `contents_path` could be the same as `path`.
    #[instrument(level = "trace", skip(self), fields(path = %path))]
    fn file_contents_path(
        &self,
        path: ProjectRelativePathBuf,
        digest_config: DigestConfig,
    ) -> Result<ProjectRelativePathBuf, ArtifactNotMaterializedReason> {
        let mut path_iter = path.iter();
        let materialization_data = match self.prefix_get(&mut path_iter) {
            // Not in tree. Assume it's a source file that doesn't require materialization from materializer.
            None => return Ok(path),
            Some(data) => data,
        };
        let (entry, method) = match &materialization_data.stage {
            ArtifactMaterializationStage::Materialized { .. } => {
                return Ok(path);
            }
            ArtifactMaterializationStage::Declared { entry, method } => {
                (entry.dupe(), method.dupe())
            }
        };
        match method.as_ref() {
            ArtifactMaterializationMethod::CasDownload { info } => {
                let path_iter = path_iter.peekable();

                let root_entry: ActionDirectoryEntry<ActionSharedDirectory> = entry.dupe();
                let mut entry = Some(entry.as_ref());

                // Check if the path we are asking for exists in this entry.
                for name in path_iter {
                    entry = match entry {
                        Some(DirectoryEntry::Dir(d)) => d.get(name),
                        _ => break,
                    }
                }

                match entry {
                    Some(entry) => Err(ArtifactNotMaterializedReason::RequiresCasDownload {
                        path,
                        // TODO (@torozco): A nicer API to get an Immutable directory here.
                        entry: entry
                            .map_dir(|d| {
                                d.as_dyn()
                                    .to_builder()
                                    .fingerprint(digest_config.as_directory_serializer())
                            })
                            .map_leaf(|l| l.dupe()),
                        info: info.dupe(),
                    }),
                    None => Err(
                        ArtifactNotMaterializedReason::DeferredMaterializerCorruption {
                            path,
                            entry: root_entry,
                            info: info.dupe(),
                        },
                    ),
                }
            }
            ArtifactMaterializationMethod::HttpDownload { .. }
            | ArtifactMaterializationMethod::Write { .. } => {
                // TODO: Do the write directly to RE instead of materializing locally?
                Err(ArtifactNotMaterializedReason::RequiresMaterialization { path })
            }
            // TODO: also record and check materialized_files for LocalCopy
            ArtifactMaterializationMethod::LocalCopy(srcs, _) => {
                match srcs.prefix_get(&mut path_iter) {
                    None => Ok(path),
                    Some(src_path) => match path_iter.next() {
                        None => self.file_contents_path(src_path.clone(), digest_config),
                        // This is not supposed to be reachable, and if it's, there
                        // is a bug somewhere else. Panic to prevent the bug from
                        // propagating.
                        Some(part) => panic!(
                            "While getting materialized path of {:?}: path {:?} is a file, so subpath {:?} doesn't exist within.",
                            path, src_path, part,
                        ),
                    },
                }
            }
            #[cfg(test)]
            ArtifactMaterializationMethod::Test => unimplemented!(),
        }
    }

    #[instrument(level = "debug", skip(self, result), fields(path = %artifact_path))]
    fn cleanup_finished(
        &mut self,
        artifact_path: ProjectRelativePathBuf,
        version: Version,
        result: Result<(), SharedMaterializingError>,
    ) {
        match self
            .prefix_get_mut(&mut artifact_path.iter())
            .context("Path is vacant")
        {
            Ok(info) => {
                if info.processing.current_version() > version {
                    // We can only unset the future if version matches.
                    // Otherwise, we may be unsetting a different future from a newer version.
                    tracing::debug!("version conflict");
                    return;
                }

                if result.is_err() {
                    // Leave it alone, don't keep retrying.
                } else {
                    info.processing = Processing::Done(version);
                }
            }
            Err(e) => {
                // NOTE: This shouldn't normally happen?
                soft_error!("cleanup_finished_vacant", e.into(), quiet: true).unwrap();
            }
        }
    }

    /// Removes paths from tree and returns a pair of two vecs.
    /// First vec is a list of paths removed. Second vec is a list of
    /// pairs of removed paths to futures that haven't finished.
    fn invalidate_paths_and_collect_futures(
        &mut self,
        paths: Vec<ProjectRelativePathBuf>,
        sqlite_db: Option<&mut MaterializerStateSqliteDb>,
    ) -> anyhow::Result<Vec<(ProjectRelativePathBuf, ProcessingFuture)>> {
        let mut invalidated_paths = Vec::new();
        let mut futs = Vec::new();

        for path in paths {
            for (path, data) in self.remove_path(&path) {
                if let Some(processing_fut) = data.processing.into_future() {
                    futs.push((path.clone(), processing_fut));
                }
                invalidated_paths.push(path);
            }
        }

        #[cfg(test)]
        {
            for path in &invalidated_paths {
                if path.as_str() == "test/invalidate/failure" {
                    return Err(anyhow::anyhow!("Injected error"));
                }
            }
        }

        // We can invalidate the paths here even if materializations are currently running on
        // the underlying nodes, because when materialization finishes we'll check the version
        // number.
        if let Some(sqlite_db) = sqlite_db {
            sqlite_db
                .materializer_state_table()
                .delete(invalidated_paths)
                .context("Error invalidating paths in materializer state")?;
        }

        Ok(futs)
    }
}

enum FoundArtifact {
    /// Proper artifact.
    Found,
    /// Found a directory artifact with dependencies inside it.
    FoundForDir,
    // TODO(nga): figure the meaning of remaining. Are these bugs?
    /// Dependency dir not found in tree.
    DirNotFound,
    /// Leaf pointing to a dir.
    LeafPointsToDir,
}

impl<V: 'static> FileTree<V> {
    /// Finds all the paths in `deps` that are artifacts in `self`
    fn find_artifacts<D>(&self, deps: &D) -> Vec<ProjectRelativePathBuf>
    where
        D: ActionDirectory,
    {
        let mut artifacts = Vec::new();
        self.find_artifacts_impl(deps, |path, found| match found {
            FoundArtifact::Found | FoundArtifact::FoundForDir => {
                artifacts.push(path.to_buf());
            }
            FoundArtifact::DirNotFound | FoundArtifact::LeafPointsToDir => {}
        });
        artifacts
    }

    fn find_artifacts_for_debug<D>(&self, deps: &D) -> Vec<(ProjectRelativePathBuf, &'static str)>
    where
        D: ActionDirectory,
    {
        let mut result = Vec::new();
        self.find_artifacts_impl(deps, |path, found| {
            let found = match found {
                FoundArtifact::Found => "Found",
                FoundArtifact::FoundForDir => "FoundForDir",
                FoundArtifact::DirNotFound => "DirNotFound",
                FoundArtifact::LeafPointsToDir => "LeafPointsToDir",
            };
            result.push((path.to_buf(), found));
        });
        result
    }

    fn find_artifacts_impl<D>(
        &self,
        deps: &D,
        mut listener: impl FnMut(&ProjectRelativePath, FoundArtifact),
    ) where
        D: ActionDirectory,
    {
        fn walk_deps<'a, V, D>(
            tree: &FileTree<V>,
            entry: DirectoryEntry<D, &ActionDirectoryMember>,
            path: &mut ProjectRelativePathBuf,
            listener: &mut impl FnMut(&ProjectRelativePath, FoundArtifact),
        ) where
            D: ActionDirectoryRef<'a>,
        {
            match (tree, entry) {
                (FileTree::Data(_), DirectoryEntry::Leaf(_)) => {
                    listener(path, FoundArtifact::Found);
                }
                (FileTree::Data(_), DirectoryEntry::Dir(_)) => {
                    listener(path, FoundArtifact::FoundForDir);
                }
                (FileTree::Tree(tree_children), DirectoryEntry::Dir(d)) => {
                    // Not an artifact, but if entry is a directory we can search deeper within
                    for (name, child) in d.entries() {
                        path.push(name);
                        if let Some(subtree) = tree_children.get(name) {
                            walk_deps(subtree, child, path, listener);
                        } else {
                            listener(path, FoundArtifact::DirNotFound);
                        }
                        let popped = path.pop();
                        assert!(popped);
                    }
                }
                (FileTree::Tree(_), DirectoryEntry::Leaf(_)) => {
                    listener(path, FoundArtifact::LeafPointsToDir);
                }
            }
        }

        let mut path_buf = ProjectRelativePathBuf::default();
        walk_deps(
            self,
            DirectoryEntry::Dir(Directory::as_ref(deps)),
            &mut path_buf,
            &mut listener,
        );
        assert!(path_buf.is_empty());
    }

    /// Removes path from FileTree. Returns an iterator of pairs of path and entry removed
    /// from the tree.
    fn remove_path(
        &mut self,
        path: &ProjectRelativePath,
    ) -> Box<dyn Iterator<Item = (ProjectRelativePathBuf, V)>> {
        let mut path_iter = path.iter();
        let removed = self.remove(&mut path_iter);

        let mut path = path;
        // Rewind the `path` up to the entry we *actually* found.
        for _ in path_iter {
            path = path
                .parent()
                .expect("Path iterator cannot cause us to rewind past the last parent");
        }
        let path = path.to_owned();

        match removed {
            Some(tree) => Box::new(
                tree.into_iter_with_paths()
                    .map(move |(k, v)| ((path).join(k), v)),
            ),
            None => Box::new(std::iter::empty()),
        }
    }
}

/// Wait on all futures in `futs` to finish. Return Error for first future that failed
/// in the Vec.
async fn join_all_existing_futs(
    existing_futs: Vec<(ProjectRelativePathBuf, ProcessingFuture)>,
) -> buck2_error::Result<()> {
    // We can await inside a loop here because all ProcessingFuture's are spawned.
    for (path, fut) in existing_futs.into_iter() {
        match fut {
            ProcessingFuture::Materializing(f) => {
                // We don't care about errors from previous materializations.
                // We are trying to delete anything that has been materialized,
                // so these errors can be ignored.
                f.await.ok();
            }
            ProcessingFuture::Cleaning(f) => {
                f.await.with_context(|| {
                    format!(
                        "Error waiting for a previous future to finish cleaning output path {}",
                        path
                    )
                })?;
            }
        };
    }

    Ok(())
}

/// Spawns a future to clean output paths while waiting for any
/// pending future to finish.
fn clean_path<T: IoHandler>(
    io: &Arc<T>,
    path: ProjectRelativePathBuf,
    version: Version,
    command_sender: Arc<MaterializerSender<T>>,
    existing_futs: ExistingFutures,
    rt: &Handle,
    cancellations: &'static CancellationContext,
) -> CleaningFuture {
    if existing_futs.is_empty() {
        return io
            .clean_path(path, version, command_sender, cancellations)
            .shared();
    }

    DeferredMaterializerCommandProcessor::<T>::spawn_from_rt(rt, {
        let io = io.dupe();
        let cancellations = CancellationContext::never_cancelled();
        async move {
            join_all_existing_futs(existing_futs.into_result()?).await?;
            io.clean_path(path, version, command_sender, cancellations)
                .await
        }
    })
    .map(|r| match r {
        Ok(r) => r,
        Err(e) => Err(e.into()), // Turn the JoinError into a buck2_error::Error.
    })
    .boxed()
    .shared()
}

/// A wrapper type around the Result it contains. Used to expose some extra methods.
struct ExistingFutures(anyhow::Result<Vec<(ProjectRelativePathBuf, ProcessingFuture)>>);

impl ExistingFutures {
    fn is_empty(&self) -> bool {
        self.0.as_ref().map_or(false, |f| f.is_empty())
    }

    fn into_result(self) -> anyhow::Result<Vec<(ProjectRelativePathBuf, ProcessingFuture)>> {
        self.0
    }

    fn empty() -> Self {
        Self(Ok(Vec::new()))
    }
}

#[derive(Derivative)]
#[derivative(Debug)]
pub struct WriteFile {
    #[derivative(Debug = "ignore")]
    compressed_data: Box<[u8]>,
    decompressed_size: usize,
    is_executable: bool,
}
