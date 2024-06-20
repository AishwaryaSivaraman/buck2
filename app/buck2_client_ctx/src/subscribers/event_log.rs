/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

use std::sync::atomic::AtomicU64;
use std::sync::Arc;

use async_trait::async_trait;
use buck2_common::argv::SanitizedArgv;
use buck2_core::fs::paths::abs_norm_path::AbsNormPathBuf;
use buck2_core::fs::paths::abs_path::AbsPathBuf;
use buck2_core::fs::working_dir::WorkingDir;
use buck2_event_log::write::WriteEventLog;
use buck2_events::BuckEvent;
use buck2_util::cleanup_ctx::AsyncCleanupContext;
use futures::FutureExt;

use crate::subscribers::subscriber::EventSubscriber;
use crate::subscribers::subscriber::Tick;

/// This EventLog lets us to events emitted by Buck and log them to a file. The events are
/// serialized as JSON and logged one per line.
pub(crate) struct EventLog<'a> {
    async_cleanup_context: Option<AsyncCleanupContext<'a>>,
    writer: WriteEventLog,
}

impl<'a> EventLog<'a> {
    pub(crate) fn new(
        logdir: AbsNormPathBuf,
        working_dir: WorkingDir,
        extra_path: Option<AbsPathBuf>,
        extra_user_event_log_path: Option<AbsPathBuf>,
        sanitized_argv: SanitizedArgv,
        async_cleanup_context: AsyncCleanupContext<'a>,
        command_name: String,
        log_size_counter_bytes: Option<Arc<AtomicU64>>,
        allow_vpnless: bool,
    ) -> anyhow::Result<EventLog> {
        Ok(Self {
            async_cleanup_context: Some(async_cleanup_context),
            writer: WriteEventLog::new(
                logdir,
                working_dir,
                extra_path,
                extra_user_event_log_path,
                sanitized_argv,
                command_name,
                log_size_counter_bytes,
                allow_vpnless,
            )?,
        })
    }
}

#[async_trait]
impl<'a> EventSubscriber for EventLog<'a> {
    async fn handle_events(&mut self, events: &[Arc<BuckEvent>]) -> anyhow::Result<()> {
        self.writer.write_events(events).await
    }

    async fn handle_tailer_stderr(&mut self, _stderr: &str) -> anyhow::Result<()> {
        // TODO(nga): currently we mostly ignore buckd stderr.
        //   It is very important to investigate crashes of buckd.
        //
        //   We attach truncated log to Scuba since D53337966
        //   (although we probably shouldn't do that).
        //
        //   Regardless of that we should do either or both of the following:
        //   - write it to event log if it is interesting (e.g. crash)
        //   - upload it to manifold unconditionally as a separate file
        //     (but only relevant part, since command start)
        Ok(())
    }

    async fn handle_command_result(
        &mut self,
        result: &buck2_cli_proto::CommandResult,
    ) -> anyhow::Result<()> {
        self.writer.write_result(result).await
    }

    /// Flush all log files during on tick to avoid buffering data in memory which we might lose if
    /// we hit an error.
    async fn tick(&mut self, _tick: &Tick) -> anyhow::Result<()> {
        self.writer.flush_files().await
    }

    async fn exit(&mut self) -> anyhow::Result<()> {
        self.writer.exit().await;
        Ok(())
    }
}

impl<'a> Drop for EventLog<'a> {
    fn drop(&mut self) {
        let exit = self.writer.exit();
        match self.async_cleanup_context.as_ref() {
            Some(async_cleanup_context) => {
                async_cleanup_context.register("event log upload", exit.boxed());
            }
            None => (),
        }
    }
}
