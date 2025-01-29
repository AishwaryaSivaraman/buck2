/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

use std::borrow::Cow;
use std::collections::HashMap;
use std::io::Write;

use buck2_client_ctx::client_ctx::ClientCommandContext;
use buck2_client_ctx::common::BuckArgMatches;
use buck2_client_ctx::exit_result::ClientIoError;
use buck2_client_ctx::exit_result::ExitResult;
use buck2_data::re_platform::Property;
use buck2_error::conversion::from_any_with_tag;
use buck2_error::BuckErrorContext;
use buck2_event_log::stream_value::StreamValue;
use buck2_event_observer::fmt_duration;
use buck2_event_observer::what_ran;
use buck2_event_observer::what_ran::CommandReproducer;
use buck2_event_observer::what_ran::WhatRanOptions;
use buck2_event_observer::what_ran::WhatRanOutputCommand;
use buck2_event_observer::what_ran::WhatRanOutputCommandExtra;
use buck2_event_observer::what_ran::WhatRanOutputWriter;
use buck2_event_observer::what_ran::WhatRanRelevantAction;
use buck2_event_observer::what_ran::WhatRanState;
use buck2_events::span::SpanId;
use futures::stream::Stream;
use futures::TryStreamExt;
use indexmap::IndexMap;

use crate::commands::log::options::EventLogOptions;
use crate::commands::log::transform_format;
use crate::commands::log::LogCommandOutputFormat;
use crate::commands::log::LogCommandOutputFormatWithWriter;
use crate::commands::log::OutputFormatWithWriter;
/// Output everything Buck2 ran from selected invocation.
///
/// The output is presented as a series of tab-delimited records with the following structure:
///
/// The reason for executing a given command. That's either to build or to test.
///
/// The identity of this command. This will include the target that ran required it.
///
/// The executor for this command. This will either be RE or local.
///
/// Details to reproduce it. For RE, that's the action digest. For local, the command.
///
///
/// To reproduce an action that ran on RE, use the following command then follow the instructions.
/// The DIGEST is of the form `hash:size`.
///
/// frecli cas download-action DIGEST
///
///
/// To reproduce an action that ran locally, make sure your working directory is the project root
/// (if unsure, use `buck2 root --kind project` to find it), then run the command. The command is
/// already shell-quoted.
#[derive(Debug, clap::Parser)]
pub struct WhatRanCommand {
    #[clap(flatten)]
    pub common: WhatRanCommandCommon,

    /// Show only commands that failed
    #[clap(long, conflicts_with = "incomplete")]
    pub failed: bool,

    /// Show only commands that were not completed.
    /// That is command were running if buck2 process was killed,
    /// or command currently running if buck2 is running build now.
    #[clap(long)]
    pub incomplete: bool,

    /// Show also std_err from commands that are run.
    /// If the command fails before completing, we display "<command did not finish executing>".
    /// If it finishes but there is no error, we display "<stderr is empty>".
    /// Otherwise, std_err is shown. For JSON, we show raw values and null for non-completion.
    #[clap(long, conflicts_with = "incomplete")]
    pub show_std_err: bool,

    /// Omit commands if their std_err is empty
    #[clap(long, conflicts_with = "incomplete", requires = "show_std_err")]
    pub omit_empty_std_err: bool,
}

#[derive(Debug, clap::Parser)]
pub struct WhatRanCommandCommon {
    #[clap(flatten)]
    event_log: EventLogOptions,

    #[clap(
        long = "format",
        help = "Which output format to use for this command",
        default_value = "tabulated",
        ignore_case = true,
        value_enum
    )]
    output: LogCommandOutputFormat,

    #[clap(flatten)]
    options: WhatRanOptions,
}

struct WhatRanCommandOptions {
    options: WhatRanOptions,

    /// Print commands only if they failed.
    failed: bool,

    /// Print commands only if they did not finish.
    incomplete: bool,
}

impl WhatRanCommand {
    pub fn exec(self, _matches: BuckArgMatches<'_>, ctx: ClientCommandContext<'_>) -> ExitResult {
        let Self {
            common:
                WhatRanCommandCommon {
                    event_log,
                    output,
                    options,
                },
            failed,
            incomplete,
            show_std_err,
            omit_empty_std_err,
        } = self;
        buck2_client_ctx::stdio::print_with_writer::<buck2_error::Error, _>(|w| {
            let mut output = OutputFormatWithWriter {
                format: transform_format(output, w),
                include_std_err: show_std_err,
                omit_empty_std_err,
            };
            ctx.instant_command_no_log("log-what-ran", |ctx| async move {
                let log_path = event_log.get(&ctx).await?;

                let (invocation, events) = log_path.unpack_stream().await?;

                buck2_client_ctx::eprintln!(
                    "Showing commands from: {}{}",
                    invocation.display_command_line(),
                    if options.filter_category.is_some() {
                        ", filtered by action category"
                    } else {
                        ""
                    }
                )?;

                let options = WhatRanCommandOptions {
                    options,
                    failed,
                    incomplete,
                };
                WhatRanCommandState::execute(events, &mut output, &options).await?;
                buck2_error::Ok(())
            })
        })?;
        ExitResult::success()
    }
}

#[allow(clippy::vec_box)]
struct WhatRanEntry {
    /// Known to be a WhatRanRelevantAction.
    event: Box<buck2_data::BuckEvent>,

    /// Known to be a CommandReproducer.
    reproducers: Vec<Box<buck2_data::BuckEvent>>,
}

impl WhatRanEntry {
    fn emit_what_ran_entry(
        &self,
        output: &mut impl WhatRanOutputWriter,
        data: &Option<buck2_data::span_end_event::Data>,
        options: &WhatRanCommandOptions,
    ) -> Result<(), ClientIoError> {
        let action = WhatRanRelevantAction::from_buck_data(
            self.event
                .data
                .as_ref()
                .buck_error_context("Checked above")?,
        );
        let options_regex = what_ran::WhatRanOptionsRegex::from_options(&options.options)?;
        for repro in self.reproducers.iter() {
            what_ran::emit_what_ran_entry(
                action,
                CommandReproducer::from_buck_data(
                    repro.data.as_ref().buck_error_context("Checked above")?,
                    options_regex.options,
                )
                .buck_error_context("Checked above")?,
                data,
                output,
                &options_regex,
            )?;
        }
        Ok(())
    }
}

/// The state for a WhatRan command. This is all the events we have seen that are
/// we have seen that are WhatRanRelevantActions, and the CommandReproducer associated with them.
#[derive(Default)]
pub struct WhatRanCommandState {
    /// Maps action spans to their details.
    known_actions: HashMap<SpanId, WhatRanEntry>,
}

impl WhatRanState for WhatRanCommandState {
    fn get(&self, span_id: SpanId) -> Option<WhatRanRelevantAction<'_>> {
        self.known_actions
            .get(&span_id)
            .and_then(|e| e.event.data.as_ref())
            .and_then(WhatRanRelevantAction::from_buck_data)
    }
}

impl WhatRanCommandState {
    async fn execute(
        mut events: impl Stream<Item = buck2_error::Result<StreamValue>> + Unpin + Send,
        output: &mut impl WhatRanOutputWriter,
        options: &WhatRanCommandOptions,
    ) -> Result<(), ClientIoError> {
        let mut cmd = Self::default();

        while let Some(event) = events.try_next().await? {
            match event {
                StreamValue::Event(event) => cmd.event(event, output, options)?,
                _ => {}
            }
        }

        // emit remaining
        for (_, entry) in cmd.known_actions.iter() {
            if should_emit_unfinished_action(options) {
                entry.emit_what_ran_entry(output, &None, options)?;
            }
        }
        Ok(())
    }

    /// Receive a new event. We store it if it's relevant and emmit them latter.
    /// Note that in practice we don't expect the event to be *both* relevant to emit *and* a
    /// WhatRanRelevantAction, but it doesn't hurt to always check both.
    fn event(
        &mut self,
        event: Box<buck2_data::BuckEvent>,
        output: &mut impl WhatRanOutputWriter,
        options: &WhatRanCommandOptions,
    ) -> buck2_error::Result<()> {
        if let Some(data) = &event.data {
            if WhatRanRelevantAction::from_buck_data(data).is_some() {
                self.known_actions.insert(
                    SpanId::from_u64(event.span_id)?,
                    WhatRanEntry {
                        event,
                        reproducers: Default::default(),
                    },
                );
                return Ok(());
            }

            if CommandReproducer::from_buck_data(data, &options.options).is_some() {
                if let Some(parent_id) = SpanId::from_u64_opt(event.parent_id) {
                    if let Some(entry) = self.known_actions.get_mut(&parent_id) {
                        entry.reproducers.push(event);
                    }
                }
                return Ok(());
            }

            match data {
                buck2_data::buck_event::Data::SpanEnd(span) => {
                    if let Some(entry) =
                        self.known_actions.remove(&SpanId::from_u64(event.span_id)?)
                    {
                        if should_emit_finished_action(&span.data, options) {
                            entry.emit_what_ran_entry(output, &span.data, options)?;
                        }
                    }
                }
                _ => {}
            }
        }

        Ok(())
    }
}

fn should_emit_finished_action(
    data: &Option<buck2_data::span_end_event::Data>,
    options: &WhatRanCommandOptions,
) -> bool {
    if options.incomplete {
        return false;
    }

    match data {
        Some(buck2_data::span_end_event::Data::ActionExecution(action)) => {
            action.failed || !options.failed
        }
        _ => !options.failed, // This is dead code (this span can only be ActionExecution End given
                              // its ID must match an ActionExecution start).
    }
}

fn should_emit_unfinished_action(options: &WhatRanCommandOptions) -> bool {
    !options.failed // We don't know if it failed or not.
}

/// An output that writes to stdout in a tabulated format.
impl WhatRanOutputWriter for OutputFormatWithWriter<'_> {
    fn emit_command(&mut self, command: WhatRanOutputCommand<'_>) -> buck2_error::Result<()> {
        if self.include_std_err
            && self.omit_empty_std_err
            && command.std_err.map_or(false, |s| s.is_empty())
        {
            return Ok(());
        }
        let std_err_formatted = if self.include_std_err {
            Some(command.std_err.map_or_else(
                || "<command did not finish executing>",
                |std_err| {
                    if std_err.is_empty() {
                        "<std_err is empty>"
                    } else {
                        std_err
                    }
                },
            ))
        } else {
            None
        };

        match &mut self.format {
            LogCommandOutputFormatWithWriter::Tabulated(w) => {
                w.write_all(format!("{}\n", command.as_tabulated_reproducer()).as_bytes())?;
                if let Some(std_err) = std_err_formatted {
                    write!(
                        w,
                        "{}{}",
                        std_err,
                        if std_err.ends_with('\n') { "" } else { "\n" }
                    )?;
                }
                Ok(())
            }
            LogCommandOutputFormatWithWriter::Json(w) => {
                let reproducer = match command.repro {
                    CommandReproducer::CacheQuery(cache_hit) => JsonReproducer::CacheQuery {
                        digest: &cache_hit.action_digest,
                    },
                    CommandReproducer::CacheHit(cache_hit) => match cache_hit.cache_type() {
                        buck2_data::CacheType::ActionCache => JsonReproducer::Cache {
                            digest: &cache_hit.action_digest,
                            action_key: cache_hit.action_key.as_deref(),
                        },
                        buck2_data::CacheType::RemoteDepFileCache => {
                            JsonReproducer::ReDepFileCache {
                                digest: &cache_hit.action_digest,
                                action_key: cache_hit.action_key.as_deref(),
                            }
                        }
                    },
                    CommandReproducer::ReExecute(re_execute) => JsonReproducer::Re {
                        digest: &re_execute.action_digest,
                        platform_properties: into_index_map(&re_execute.platform),
                        action_key: re_execute.action_key.as_deref(),
                    },
                    CommandReproducer::LocalExecute(local_execute) => JsonReproducer::Local {
                        command: local_execute.command.as_ref().map_or_else(
                            || Cow::Owned(Vec::new()),
                            |command| Cow::Borrowed(command.argv.as_ref()),
                        ),
                        env: local_execute
                            .command
                            .as_ref()
                            .into_iter()
                            .flat_map(|command| command.env.iter())
                            .map(|entry| (entry.key.as_ref(), entry.value.as_ref()))
                            .collect(),
                    },
                    CommandReproducer::WorkerExecute(worker_execute) => JsonReproducer::Worker {
                        command: worker_execute.command.as_ref().map_or_else(
                            || Cow::Owned(Vec::new()),
                            |command| Cow::Borrowed(command.argv.as_ref()),
                        ),
                        env: worker_execute
                            .command
                            .as_ref()
                            .into_iter()
                            .flat_map(|command| command.env.iter())
                            .map(|entry| (entry.key.as_ref(), entry.value.as_ref()))
                            .collect(),
                    },
                    // TODO(ctolliday): use the worker_id as the `identity`, and add it to worker execution events.
                    // Currently the identity is the first target that used the worker, which might be misleading.
                    CommandReproducer::WorkerInit(worker_init) => JsonReproducer::WorkerInit {
                        command: worker_init.command.as_ref().map_or_else(
                            || Cow::Owned(Vec::new()),
                            |command| Cow::Borrowed(command.argv.as_ref()),
                        ),
                        env: worker_init
                            .command
                            .as_ref()
                            .into_iter()
                            .flat_map(|command| command.env.iter())
                            .map(|entry| (entry.key.as_ref(), entry.value.as_ref()))
                            .collect(),
                    },
                };
                let std_err = if self.include_std_err {
                    Some(command.std_err.unwrap_or("null"))
                } else {
                    None
                };

                let command = JsonCommand {
                    reason: command.reason,
                    identity: command.identity,
                    reproducer,
                    duration: command
                        .duration
                        .map(|duration| fmt_duration::fmt_duration(duration, 1.0)),
                    extra: command.extra.map(Into::into),
                    std_err,
                };
                serde_json::to_writer(w.by_ref(), &command)?;
                w.write_all("\n".as_bytes())?;
                Ok(())
            }
            LogCommandOutputFormatWithWriter::Csv(writer) => {
                #[derive(serde::Serialize)]
                struct Record<'a> {
                    reason: &'a str,
                    identity: &'a str,
                    executor: String,
                    reproducer: String,
                    #[serde(skip_serializing_if = "Option::is_none")]
                    std_err: Option<&'a str>,
                }
                writer
                    .serialize(Record {
                        reason: command.reason,
                        identity: command.identity,
                        executor: command.repro.executor(),
                        reproducer: command.repro.to_string(),
                        std_err: std_err_formatted,
                    })
                    .map_err(|e| from_any_with_tag(e, buck2_error::ErrorTag::Tier0))?;
                Ok(())
            }
        }
    }
}

fn into_index_map(platform: &Option<buck2_data::RePlatform>) -> IndexMap<&str, &str> {
    platform.as_ref().map_or_else(IndexMap::new, |p| {
        p.properties
            .iter()
            .map(|Property { name, value }| (name.as_ref(), value.as_ref()))
            .collect()
    })
}

#[derive(serde::Serialize)]
struct JsonCommand<'a> {
    reason: &'a str,
    identity: &'a str,
    reproducer: JsonReproducer<'a>,
    #[serde(skip_serializing_if = "Option::is_none")]
    duration: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    extra: Option<JsonExtra<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    std_err: Option<&'a str>,
}

mod json_reproducer {
    #![allow(clippy::ref_option_ref)] // within Serialize

    use super::*;

    #[derive(serde::Serialize)]
    #[serde(tag = "executor", content = "details")]
    pub enum JsonReproducer<'a> {
        CacheQuery {
            digest: &'a str,
        },
        Cache {
            digest: &'a str,
            #[serde(skip_serializing_if = "Option::is_none")]
            action_key: Option<&'a str>,
        },
        ReDepFileCache {
            digest: &'a str,
            #[serde(skip_serializing_if = "Option::is_none")]
            action_key: Option<&'a str>,
        },
        Re {
            digest: &'a str,
            platform_properties: IndexMap<&'a str, &'a str>,
            #[serde(skip_serializing_if = "Option::is_none")]
            action_key: Option<&'a str>,
        },
        Local {
            command: Cow<'a, [String]>,
            env: IndexMap<&'a str, &'a str>,
        },
        Worker {
            command: Cow<'a, [String]>,
            env: IndexMap<&'a str, &'a str>,
        },
        WorkerInit {
            command: Cow<'a, [String]>,
            env: IndexMap<&'a str, &'a str>,
        },
    }
}

use json_reproducer::JsonReproducer;

#[derive(serde::Serialize)]
#[serde(rename_all = "lowercase")]
enum JsonExtra<'a> {
    TestCases(&'a [String]),
}

impl<'a> From<WhatRanOutputCommandExtra<'a>> for JsonExtra<'a> {
    fn from(extra: WhatRanOutputCommandExtra<'a>) -> JsonExtra<'a> {
        match extra {
            WhatRanOutputCommandExtra::TestCases(cases) => JsonExtra::TestCases(cases),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_base_command() -> JsonCommand<'static> {
        let command = Cow::Owned(vec!["some".to_owned(), "command".to_owned()]);
        let mut env = IndexMap::new();
        env.insert("KEY", "val");

        JsonCommand {
            reason: "test.run",
            identity: "some/target",
            reproducer: JsonReproducer::Local { command, env },
            duration: Some("1".to_owned()),
            extra: None,
            std_err: None,
        }
    }

    fn make_base_command_in_re() -> JsonCommand<'static> {
        JsonCommand {
            reason: "test.run",
            identity: "some/target",
            reproducer: JsonReproducer::Re {
                digest: "placeholder",
                platform_properties: indexmap::indexmap! {
                    "platform" => "linux-remote-execution"
                },
                action_key: None,
            },
            duration: Some("1".to_owned()),
            extra: None,
            std_err: None,
        }
    }

    #[test]
    fn serialize_what_ran_command_no_extr() -> buck2_error::Result<()> {
        let command = make_base_command();

        let expected = r#"{
  "reason": "test.run",
  "identity": "some/target",
  "reproducer": {
    "executor": "Local",
    "details": {
      "command": [
        "some",
        "command"
      ],
      "env": {
        "KEY": "val"
      }
    }
  },
  "duration": "1"
}"#;
        assert_eq!(expected, serde_json::to_string_pretty(&command)?);
        Ok(())
    }

    #[test]
    fn serialize_what_ran_command_with_extra() -> buck2_error::Result<()> {
        let mut command = make_base_command();
        let cases = &["case".to_owned()];
        command.extra = Some(JsonExtra::TestCases(cases));

        let expected = r#"{
  "reason": "test.run",
  "identity": "some/target",
  "reproducer": {
    "executor": "Local",
    "details": {
      "command": [
        "some",
        "command"
      ],
      "env": {
        "KEY": "val"
      }
    }
  },
  "duration": "1",
  "extra": {
    "testcases": [
      "case"
    ]
  }
}"#;
        assert_eq!(expected, serde_json::to_string_pretty(&command)?);
        Ok(())
    }

    #[test]
    fn serialize_what_ran_command_in_re() -> buck2_error::Result<()> {
        let command = make_base_command_in_re();

        let expected = r#"{
  "reason": "test.run",
  "identity": "some/target",
  "reproducer": {
    "executor": "Re",
    "details": {
      "digest": "placeholder",
      "platform_properties": {
        "platform": "linux-remote-execution"
      }
    }
  },
  "duration": "1"
}"#;
        assert_eq!(expected, serde_json::to_string_pretty(&command)?);
        Ok(())
    }
}
