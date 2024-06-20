/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

use std::io;
use std::mem;
use std::pin::Pin;
use std::process::Stdio;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::task::Context;
use std::task::Poll;

use anyhow::Context as _;
use async_compression::tokio::write::GzipEncoder;
use async_compression::tokio::write::ZstdEncoder;
use buck2_cli_proto::*;
use buck2_core::fs::paths::abs_norm_path::AbsNormPathBuf;
use buck2_core::fs::paths::abs_path::AbsPathBuf;
use buck2_core::fs::working_dir::WorkingDir;
use buck2_events::BuckEvent;
use buck2_util::cleanup_ctx::AsyncCleanupContext;
use buck2_wrapper_common::invocation_id::TraceId;
use futures::future::Future;
use futures::FutureExt;
use pin_project::pin_project;
use prost::Message;
use serde::Serialize;
use tokio::fs::OpenOptions;
use tokio::io::AsyncWrite;
use tokio::io::AsyncWriteExt;

use crate::file_names::get_logfile_name;
use crate::file_names::remove_old_logs;
use crate::read::EventLogPathBuf;
use crate::should_block_on_log_upload;
use crate::should_upload_log;
use crate::utils::Compression;
use crate::utils::Encoding;
use crate::utils::EventLogErrors;
use crate::utils::Invocation;
use crate::utils::LogMode;
use crate::utils::NoInference;
use crate::wait_for_child_and_log;
use crate::FutureChildOutput;

type EventLogWriter = Box<dyn AsyncWrite + Send + Sync + Unpin + 'static>;

mod counting_reader {
    use super::*;

    #[pin_project]
    pub struct CountingReader<T> {
        #[pin]
        pub(super) inner: T,
        pub(super) stats: Option<Arc<AtomicU64>>,
    }
}

use buck2_common::argv::SanitizedArgv;
use counting_reader::CountingReader;

use super::user_event_types::try_get_user_event;

impl<T> CountingReader<T> {
    fn new(inner: T, stats: Option<Arc<AtomicU64>>) -> Self {
        Self { inner, stats }
    }
}

impl<T> AsyncWrite for CountingReader<T>
where
    T: AsyncWrite,
{
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.project();
        let bytes = futures::ready!(this.inner.poll_write(cx, buf))?;
        if let Some(stats) = this.stats {
            stats.fetch_add(bytes as u64, Ordering::Relaxed);
        }

        Poll::Ready(Ok(bytes))
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::result::Result<(), std::io::Error>> {
        self.project().inner.poll_flush(cx)
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::result::Result<(), std::io::Error>> {
        self.project().inner.poll_shutdown(cx)
    }
}

#[derive(Eq, PartialEq, Copy, Clone)]
pub(crate) enum EventLogType {
    System,
    User,
}

struct NamedEventLogWriter {
    path: EventLogPathBuf,
    file: EventLogWriter,
    event_log_type: EventLogType,
    /// If this writing is done by a subprocess, that process's output, assuming we intend to wait
    /// for it to exit.
    process_to_wait_for: Option<FutureChildOutput>,
}

impl NamedEventLogWriter {
    fn new(
        path: EventLogPathBuf,
        file: impl AsyncWrite + std::marker::Send + std::marker::Unpin + std::marker::Sync + 'static,
        bytes_written: Option<Arc<AtomicU64>>,
        event_log_type: EventLogType,
        process_to_wait_for: Option<FutureChildOutput>,
    ) -> Self {
        let file = match path.encoding.compression {
            Compression::None => {
                Box::new(CountingReader::new(file, bytes_written)) as EventLogWriter
            }
            Compression::Gzip => Box::new(GzipEncoder::with_quality(
                CountingReader::new(file, bytes_written),
                async_compression::Level::Fastest,
            )) as EventLogWriter,
            Compression::Zstd => Box::new(ZstdEncoder::with_quality(
                CountingReader::new(file, bytes_written),
                async_compression::Level::Default,
            )) as EventLogWriter,
        };
        Self {
            path,
            file,
            event_log_type,
            process_to_wait_for,
        }
    }

    async fn flush(&mut self) -> anyhow::Result<()> {
        match self.file.flush().await {
            Ok(_) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => {
                // The subprocess exited with some kind of error. That is logged separately, so
                // here we just ignore it.
                Ok(())
            }
            Err(e) => Err(anyhow::Error::from(e).context(format!(
                "Error flushing log file at {}",
                self.path.path.display()
            ))),
        }
    }

    async fn shutdown(&mut self) {
        if let Err(e) = self.file.shutdown().await {
            tracing::warn!("Failed to flush log file at `{}`: {:#}", self.path.path, e);
        }
    }

    fn child(mut self) -> Option<FutureChildOutput> {
        self.process_to_wait_for.take()
    }

    fn serialize_event<'b, T>(&self, mut buf: &mut Vec<u8>, event: &T) -> anyhow::Result<()>
    where
        T: SerializeForLog + 'b,
    {
        match self.event_log_type {
            EventLogType::System => {
                match self.path.encoding.mode {
                    LogMode::Json => {
                        event.serialize_to_json(&mut buf)?;
                        buf.push(b'\n');
                    }
                    LogMode::Protobuf => event.serialize_to_protobuf_length_delimited(&mut buf)?,
                };
            }
            EventLogType::User => {
                if event.maybe_serialize_user_event(&mut buf)? {
                    buf.push(b'\n');
                }
            }
        }
        Ok(())
    }

    async fn write_all(&mut self, buf: &[u8]) -> anyhow::Result<()> {
        match self.file.write_all(buf).await {
            Ok(_) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => {
                // The subprocess exited with some kind of error. That is logged separately, so
                // here we just ignore it.
                Ok(())
            }
            Err(e) => Err(anyhow::Error::from(e).context("Failed to write event")),
        }
    }

    async fn write_events<'b, T, I>(
        &mut self,
        mut buf: &mut Vec<u8>,
        events: &I,
    ) -> Result<(), anyhow::Error>
    where
        T: SerializeForLog + 'b,
        I: IntoIterator<Item = &'b T> + Clone + 'b,
    {
        for event in events.clone() {
            self.serialize_event(&mut buf, event)?;
        }
        self.write_all(&buf).await?;
        Ok(())
    }
}

enum LogWriterState {
    Unopened {
        logdir: AbsNormPathBuf,
        extra_path: Option<AbsPathBuf>,
        extra_user_event_log_path: Option<AbsPathBuf>,
    },
    Opened {
        writers: Vec<NamedEventLogWriter>,
    },
    Closed,
}

pub struct WriteEventLog<'a> {
    state: LogWriterState,
    async_cleanup_context: Option<AsyncCleanupContext<'a>>,
    sanitized_argv: SanitizedArgv,
    command_name: String,
    working_dir: WorkingDir,
    /// Allocation cache. Must be cleaned before use.
    buf: Vec<u8>,
    log_size_counter_bytes: Option<Arc<AtomicU64>>,
    allow_vpnless: bool,
}

impl<'a> WriteEventLog<'a> {
    pub fn new(
        logdir: AbsNormPathBuf,
        working_dir: WorkingDir,
        extra_path: Option<AbsPathBuf>,
        extra_user_event_log_path: Option<AbsPathBuf>,
        sanitized_argv: SanitizedArgv,
        async_cleanup_context: AsyncCleanupContext<'a>,
        command_name: String,
        log_size_counter_bytes: Option<Arc<AtomicU64>>,
        allow_vpnless: bool,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            state: LogWriterState::Unopened {
                logdir,
                extra_path,
                extra_user_event_log_path,
            },
            async_cleanup_context: Some(async_cleanup_context),
            sanitized_argv,
            command_name,
            working_dir,
            buf: Vec::new(),
            log_size_counter_bytes,
            allow_vpnless,
        })
    }

    /// Get the command line arguments and cwd and serialize them for replaying later.
    async fn log_invocation(&mut self, trace_id: TraceId) -> anyhow::Result<()> {
        let command_line_args = self.sanitized_argv.argv.clone();
        let expanded_command_line_args = self.sanitized_argv.expanded_argv.clone();
        let invocation = Invocation {
            command_line_args,
            expanded_command_line_args,
            working_dir: self.working_dir.to_string(),
            trace_id,
        };
        self.write_ln(&[invocation]).await
    }

    async fn write_ln<'b, T, I>(&'b mut self, events: I) -> anyhow::Result<()>
    where
        T: SerializeForLog + 'b,
        I: IntoIterator<Item = &'b T> + Clone + 'b,
    {
        match &mut self.state {
            LogWriterState::Opened { writers, .. } => {
                for writer in writers {
                    self.buf.clear();

                    writer.write_events(&mut self.buf, &events).await?;

                    if self.buf.len() > 1_000_000 {
                        // Make sure we don't keep too much memory if encountered one large event.
                        self.buf = Vec::new();
                    }
                }
                Ok(())
            }
            LogWriterState::Unopened { .. } | LogWriterState::Closed => {
                self.buf.clear();
                if let Some(event) = events.into_iter().next() {
                    event.serialize_to_json(&mut self.buf)?;
                } else {
                    // Unreachable.
                }
                Err(EventLogErrors::LogNotOpen {
                    serialized_event: String::from_utf8(mem::take(&mut self.buf))
                        .context("Failed to serialize event for debug")?,
                }
                .into())
            }
        }
    }

    async fn ensure_log_writers_opened(&mut self, event: &BuckEvent) -> anyhow::Result<()> {
        let (logdir, maybe_extra_path, maybe_extra_user_event_log_path) = match &self.state {
            LogWriterState::Unopened {
                logdir,
                extra_path,
                extra_user_event_log_path,
            } => (logdir, extra_path, extra_user_event_log_path),
            LogWriterState::Opened { .. } => return Ok(()),
            LogWriterState::Closed => {
                return Err(anyhow::anyhow!("Received events after logs were closed"));
            }
        };
        tokio::fs::create_dir_all(logdir)
            .await
            .with_context(|| format!("Error creating event log directory: `{}`", logdir))?;
        remove_old_logs(logdir).await;

        let encoding = Encoding::PROTO_ZSTD;
        let file_name = &get_logfile_name(event, encoding, &self.command_name)?;
        let path = EventLogPathBuf {
            path: logdir.as_abs_path().join(file_name),
            encoding,
        };
        let writer = start_persist_event_log_subprocess(
            path,
            event.trace_id()?.clone(),
            self.log_size_counter_bytes.clone(),
            self.allow_vpnless,
        )
        .await?;
        let mut writers = vec![writer];

        // Also open the user's log file, if any as provided, with no encoding.
        if let Some(extra_path) = maybe_extra_path {
            writers.push(
                open_event_log_for_writing(
                    EventLogPathBuf::infer_opt(extra_path.clone())?.unwrap_or_else(
                        |NoInference(path)| EventLogPathBuf {
                            path,
                            encoding: Encoding::JSON_GZIP,
                        },
                    ),
                    self.log_size_counter_bytes.clone(),
                    EventLogType::System,
                )
                .await?,
            );
        }

        // Also open the user's simple log file, if any as provided, json-line formatted with no compression if no extensions are detected.
        if let Some(extra_user_event_log_path) = maybe_extra_user_event_log_path {
            writers.push(
                open_event_log_for_writing(
                    EventLogPathBuf::infer_opt(extra_user_event_log_path.clone())?.unwrap_or_else(
                        |NoInference(path)| EventLogPathBuf {
                            path,
                            encoding: Encoding::JSON,
                        },
                    ),
                    self.log_size_counter_bytes.clone(),
                    EventLogType::User,
                )
                .await?,
            );
        }

        self.state = LogWriterState::Opened { writers };
        self.log_invocation(event.trace_id()?).await
    }

    pub fn exit(&mut self) -> impl Future<Output = ()> + 'static + Send + Sync {
        // Shut down writers, flush all our files before exiting.
        let state = std::mem::replace(&mut self.state, LogWriterState::Closed);

        async move {
            let mut writers = match state {
                LogWriterState::Opened { writers } => writers,
                LogWriterState::Unopened { .. } | LogWriterState::Closed => {
                    // Nothing to do in this case, though this should be unreachable
                    // since we just did a write_ln.
                    return;
                }
            };

            for writer in writers.iter_mut() {
                writer.shutdown().await
            }

            // NOTE: We call `into_iter()` here and that implicitly drops the `writer.file`, which
            // is necessary for an actual `close` call to be send to the child FD (it is a bit of
            // an odd behavior in Tokio that `shutdown` doesn't do that).
            let futs = writers
                .into_iter()
                .filter_map(|w| w.child())
                .map(|proc| wait_for_child_and_log(proc, "Event Log"));

            futures::future::join_all(futs).await;
        }
    }
}

impl<'a> Drop for WriteEventLog<'a> {
    fn drop(&mut self) {
        let exit = self.exit();
        match self.async_cleanup_context.as_ref() {
            Some(async_cleanup_context) => {
                async_cleanup_context.register("event log upload", exit.boxed());
            }
            None => (),
        }
    }
}

async fn start_persist_event_log_subprocess(
    path: EventLogPathBuf,
    trace_id: TraceId,
    bytes_written: Option<Arc<AtomicU64>>,
    allow_vpnless: bool,
) -> anyhow::Result<NamedEventLogWriter> {
    let current_exe = std::env::current_exe().context("No current_exe")?;
    let mut command = buck2_util::process::async_background_command(current_exe);
    // @oss-disable: #[cfg(unix)]
    #[cfg(all(tokio_unstable, unix))] // @oss-enable
    {
        // Ensure that if we get CTRL-C, the persist-event-logs process does not get it.
        command.process_group(0);
    }
    let manifold_name = &format!("{}{}", trace_id, path.extension());
    // TODO T184566736: detach subprocess
    command
        .args(["debug", "persist-event-logs"])
        .args(["--manifold-name", manifold_name])
        .args(["--local-path".as_ref(), path.path.as_os_str()])
        .args(["--trace-id", &trace_id.to_string()]);
    if !should_upload_log()? {
        command.arg("--no-upload");
    };
    if allow_vpnless {
        command.arg("--allow-vpnless");
    }
    command.stdout(Stdio::null()).stdin(Stdio::piped());

    let block = should_block_on_log_upload()?;
    if block {
        command.stderr(Stdio::piped());
    } else {
        command.stderr(Stdio::null());
    }

    let mut child = command.spawn().with_context(|| {
        format!(
            "Failed to open event log subprocess for writing at `{}`",
            path.path.display()
        )
    })?;
    let pipe = child.stdin.take().expect("stdin was piped");

    // Only spawn this if we are going to wait.
    let process_to_wait_for = if block {
        Some(FutureChildOutput::new(child))
    } else {
        None
    };

    Ok(NamedEventLogWriter::new(
        path,
        pipe,
        bytes_written,
        EventLogType::System,
        process_to_wait_for,
    ))
}

async fn open_event_log_for_writing(
    path: EventLogPathBuf,
    bytes_written: Option<Arc<AtomicU64>>,
    event_log_type: EventLogType,
) -> anyhow::Result<NamedEventLogWriter> {
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path.path)
        .await
        .with_context(|| {
            format!(
                "Failed to open event log for writing at `{}`",
                path.path.display()
            )
        })?;

    Ok(NamedEventLogWriter::new(
        path,
        file,
        bytes_written,
        event_log_type,
        None,
    ))
}

impl<'a> WriteEventLog<'a> {
    pub async fn write_events(&mut self, events: &[Arc<BuckEvent>]) -> anyhow::Result<()> {
        let mut event_refs = Vec::new();
        let mut first = true;
        for event in events {
            if first {
                self.ensure_log_writers_opened(event).await?;
                first = false;
            }

            event_refs.push(StreamValueForWrite::Event(event.event()));
        }

        if event_refs.is_empty() {
            return Ok(());
        }

        self.write_ln(&event_refs).await
    }

    pub async fn write_result(
        &mut self,
        result: &buck2_cli_proto::CommandResult,
    ) -> anyhow::Result<()> {
        match &self.state {
            LogWriterState::Opened { .. } | LogWriterState::Closed => {}
            LogWriterState::Unopened { .. } => {
                // This is a bit wonky. We can receive a CommandResult before we opened log files
                // if the command crashed before it started. That can happen if the daemon
                // initialization is what fails, since we need the daemon to initialize in order to
                // access request metadata, which we need for the command start event. To keep
                // things simple, just tolerate this happening.
                return Ok(());
            }
        }

        let event = StreamValueForWrite::Result(result);

        self.write_ln(&[event]).await
    }

    pub async fn flush_files(&mut self) -> anyhow::Result<()> {
        let writers = match &mut self.state {
            LogWriterState::Opened { writers } => writers,
            LogWriterState::Unopened { .. } | LogWriterState::Closed => return Ok(()),
        };

        for writer in writers {
            writer.flush().await?;
        }

        Ok(())
    }
}

pub(crate) trait SerializeForLog {
    fn serialize_to_json(&self, buf: &mut Vec<u8>) -> anyhow::Result<()>;
    fn serialize_to_protobuf_length_delimited(&self, buf: &mut Vec<u8>) -> anyhow::Result<()>;
    fn maybe_serialize_user_event(&self, buf: &mut Vec<u8>) -> anyhow::Result<bool>;
}

impl SerializeForLog for Invocation {
    fn serialize_to_json(&self, buf: &mut Vec<u8>) -> anyhow::Result<()> {
        serde_json::to_writer(buf, &self).context("Failed to serialize event")
    }

    fn serialize_to_protobuf_length_delimited(&self, buf: &mut Vec<u8>) -> anyhow::Result<()> {
        let invocation = buck2_data::Invocation {
            command_line_args: self.command_line_args.clone(),
            expanded_command_line_args: self.expanded_command_line_args.clone(),
            working_dir: self.working_dir.clone(),
            trace_id: Some(self.trace_id.to_string()),
        };
        invocation.encode_length_delimited(buf)?;
        Ok(())
    }

    // Always log invocation record to user event log for `buck2 log show` compatibility
    fn maybe_serialize_user_event(&self, buf: &mut Vec<u8>) -> anyhow::Result<bool> {
        serde_json::to_writer(buf, &self).context("Failed to serialize event")?;
        Ok(true)
    }
}

#[derive(Serialize)]
pub enum StreamValueForWrite<'a> {
    Result(&'a CommandResult),
    Event(&'a buck2_data::BuckEvent),
}

impl<'a> SerializeForLog for StreamValueForWrite<'a> {
    fn serialize_to_json(&self, buf: &mut Vec<u8>) -> anyhow::Result<()> {
        serde_json::to_writer(buf, &self).context("Failed to serialize event")
    }

    fn serialize_to_protobuf_length_delimited(&self, buf: &mut Vec<u8>) -> anyhow::Result<()> {
        // We use `CommandProgressForWrite` here to avoid cloning `BuckEvent`.
        // `CommandProgressForWrite` serialization is bitwise identical to `CommandProgress`.
        // See the protobuf spec
        // https://developers.google.com/protocol-buffers/docs/encoding#length-types
        // for the details about protobuf wire format.
        let progress = match self {
            Self::Event(e) => command_progress_for_write::Progress::Event(e.encode_to_vec()),
            Self::Result(res) => command_progress_for_write::Progress::Result((*res).clone()),
        };
        let stream_val = buck2_cli_proto::CommandProgressForWrite {
            progress: Some(progress),
        };
        stream_val.encode_length_delimited(buf)?;
        Ok(())
    }

    fn maybe_serialize_user_event(&self, buf: &mut Vec<u8>) -> anyhow::Result<bool> {
        if let StreamValueForWrite::Event(event) = self {
            if let Some(user_event) = try_get_user_event(event)? {
                serde_json::to_writer(buf, &user_event).context("Failed to serialize event")?;
                return Ok(true);
            }
        }

        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use std::time::SystemTime;

    use buck2_data::LoadBuildFileStart;
    use buck2_data::SpanStartEvent;
    use buck2_events::span::SpanId;
    use futures::TryStreamExt;
    use tempfile::TempDir;

    use super::*;
    use crate::stream_value::StreamValue;

    impl WriteEventLog<'static> {
        async fn new_test(log: EventLogPathBuf) -> anyhow::Result<Self> {
            Ok(Self {
                state: LogWriterState::Opened {
                    writers: vec![
                        open_event_log_for_writing(log, None, EventLogType::System).await?,
                    ],
                },
                sanitized_argv: SanitizedArgv {
                    argv: vec!["buck2".to_owned()],
                    expanded_argv: vec!["buck2".to_owned()],
                },
                async_cleanup_context: None,
                command_name: "testtest".to_owned(),
                working_dir: WorkingDir::current_dir()?,
                buf: Vec::new(),
                log_size_counter_bytes: None,
                allow_vpnless: false,
            })
        }
    }

    fn make_event() -> BuckEvent {
        BuckEvent::new(
            SystemTime::now(),
            TraceId::new(),
            Some(SpanId::next()),
            None,
            buck2_data::buck_event::Data::SpanStart(SpanStartEvent {
                data: Some(buck2_data::span_start_event::Data::Load(
                    LoadBuildFileStart {
                        module_id: "foo".to_owned(),
                        cell: "bar".to_owned(),
                    },
                )),
            }),
        )
    }

    #[tokio::test]
    async fn test_protobuf_decoding_gzip() -> anyhow::Result<()> {
        test_protobuf_decoding(Encoding::PROTO_GZIP).await
    }

    #[tokio::test]
    async fn test_protobuf_decoding_zstd() -> anyhow::Result<()> {
        test_protobuf_decoding(Encoding::PROTO_ZSTD).await
    }

    async fn test_protobuf_decoding(encoding: Encoding) -> anyhow::Result<()> {
        //Create log dir
        let tmp_dir = TempDir::new()?;

        //Create mock event
        let event = make_event();

        // Create event log
        let log = EventLogPathBuf {
            path: AbsPathBuf::try_from(tmp_dir.path().join("log")).unwrap(),
            encoding,
        };

        let mut write_event_log = WriteEventLog::new_test(log.clone()).await?;

        //Log event
        let value = StreamValueForWrite::Event(event.event());
        write_event_log.log_invocation(event.trace_id()?).await?;
        write_event_log.write_ln(&[value]).await?;
        write_event_log.exit().await;

        //Get and decode log
        let (_invocation, mut events) = log.unpack_stream().await?;

        //Get event
        let retrieved_event = match events.try_next().await?.expect("Failed getting log") {
            StreamValue::Event(e) => BuckEvent::try_from(e),
            _ => panic!("expected event"),
        }?;

        //Assert it's the same event created in the beginning
        assert_eq!(retrieved_event.timestamp(), event.timestamp());
        assert_eq!(
            retrieved_event.trace_id().unwrap(),
            event.trace_id().unwrap()
        );
        assert_eq!(retrieved_event.span_id().unwrap(), event.span_id().unwrap());
        assert_eq!(retrieved_event.data(), event.data());

        assert!(
            events.try_next().await.unwrap().is_none(),
            "expecting no more events"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_tick_makes_valid_log_zstd() -> anyhow::Result<()> {
        test_tick_makes_valid_log(Encoding::PROTO_ZSTD).await
    }

    async fn test_tick_makes_valid_log(encoding: Encoding) -> anyhow::Result<()> {
        if cfg!(windows) {
            // Do not want to deal with exclusivity issues on Windows.
            return Ok(());
        }

        let tmp_dir = TempDir::new()?;

        let log = EventLogPathBuf {
            path: AbsPathBuf::try_from(tmp_dir.path().join("test_tick_makes_valid_log.pb.gz"))
                .unwrap(),
            encoding,
        };

        let mut write_event_log = WriteEventLog::new_test(log.clone()).await?;

        let event = make_event();
        let value = StreamValueForWrite::Event(event.event());
        write_event_log.log_invocation(event.trace_id()?).await?;
        write_event_log.write_ln(&[value]).await?;

        assert!(
            log.unpack_stream().await.is_err(),
            "Sanity check: gzip was not flushed, so the log is invalid"
        );

        // Now flush the gzip stream.
        write_event_log.flush_files().await?;

        // Do not close the log, and open it.
        let (_invocation, mut events) = log.unpack_stream().await?;

        let retrieved_event = match events.try_next().await?.expect("Failed getting log") {
            StreamValue::Event(e) => BuckEvent::try_from(e).unwrap(),
            _ => panic!("expecting event"),
        };

        assert_eq!(retrieved_event.timestamp(), event.timestamp());
        assert_eq!(
            retrieved_event.trace_id().unwrap(),
            event.trace_id().unwrap()
        );
        assert_eq!(retrieved_event.span_id(), event.span_id());
        assert_eq!(retrieved_event.data(), event.data());

        match encoding.compression {
            Compression::Gzip => {
                // TODO(nga): `tick` does not write gzip footer, so even after `tick`
                //   generated file is not a valid gzip file.
                // assert!(events.try_next().await.unwrap().is_none(), "expecting no more events");
                assert!(events.try_next().await.is_err());
            }
            Compression::Zstd => {
                assert!(
                    events.try_next().await.unwrap().is_none(),
                    "expecting no more events"
                );
            }
            Compression::None => unreachable!(),
        }

        Ok(())
    }

    #[test]
    fn test_stream_value_serialize_to_protobuf_length_delimited() {
        let event = make_event();
        let mut actual = Vec::new();
        StreamValueForWrite::Event(event.event())
            .serialize_to_protobuf_length_delimited(&mut actual)
            .unwrap();
        let expected = buck2_cli_proto::CommandProgress {
            progress: Some(command_progress::Progress::Event(event.into())),
        }
        .encode_length_delimited_to_vec();
        assert_eq!(expected, actual);
    }
}
