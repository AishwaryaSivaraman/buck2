/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

mod default;
pub(crate) mod fmt;
mod resolve_alias;
mod streaming;
use std::fs::File;
use std::io::BufWriter;
use std::io::Write;

use anyhow::Context as _;
use async_trait::async_trait;
use buck2_cli_proto::targets_request;
use buck2_cli_proto::targets_request::TargetHashGraphType;
use buck2_cli_proto::TargetsRequest;
use buck2_cli_proto::TargetsResponse;
use buck2_common::dice::cells::HasCellResolver;
use buck2_common::pattern::parse_from_cli::parse_patterns_from_cli_args;
use buck2_core::pattern::pattern_type::TargetPatternExtra;
use buck2_error::internal_error;
use buck2_error::BuckErrorContext;
use buck2_server_ctx::ctx::ServerCommandContextTrait;
use buck2_server_ctx::global_cfg_options::global_cfg_options_from_client_context;
use buck2_server_ctx::partial_result_dispatcher::PartialResultDispatcher;
use buck2_server_ctx::template::run_server_command;
use buck2_server_ctx::template::ServerCommandTemplate;
use dice::DiceTransaction;

use crate::commands::targets::default::targets_batch;
use crate::commands::targets::default::TargetHashOptions;
use crate::commands::targets::fmt::create_formatter;
use crate::commands::targets::resolve_alias::targets_resolve_aliases;
use crate::commands::targets::streaming::targets_streaming;

enum OutputType {
    Stdout,
    File,
}

fn outputter<'a, W: Write + Send + 'a>(
    request: &TargetsRequest,
    stdout: W,
) -> anyhow::Result<(OutputType, Box<dyn Write + Send + 'a>)> {
    match &request.output {
        None => Ok((OutputType::Stdout, Box::new(stdout))),
        Some(file) => {
            let file =
                BufWriter::new(File::create(file).with_context(|| {
                    format!("Failed to open file `{file}` for `targets` output ")
                })?);
            Ok((OutputType::File, Box::new(file)))
        }
    }
}

pub(crate) async fn targets_command(
    server_ctx: &dyn ServerCommandContextTrait,
    partial_result_dispatcher: PartialResultDispatcher<buck2_cli_proto::StdoutBytes>,
    req: TargetsRequest,
) -> anyhow::Result<TargetsResponse> {
    run_server_command(
        TargetsServerCommand { req },
        server_ctx,
        partial_result_dispatcher,
    )
    .await
}

struct TargetsServerCommand {
    req: TargetsRequest,
}

#[async_trait]
impl ServerCommandTemplate for TargetsServerCommand {
    type StartEvent = buck2_data::TargetsCommandStart;
    type EndEvent = buck2_data::TargetsCommandEnd;
    type Response = TargetsResponse;
    type PartialResult = buck2_cli_proto::StdoutBytes;

    async fn command(
        &self,
        server_ctx: &dyn ServerCommandContextTrait,
        mut partial_result_dispatcher: PartialResultDispatcher<Self::PartialResult>,
        dice: DiceTransaction,
    ) -> anyhow::Result<Self::Response> {
        targets(
            server_ctx,
            &mut partial_result_dispatcher.as_writer(),
            dice,
            &self.req,
        )
        .await
    }

    fn is_success(&self, _response: &Self::Response) -> bool {
        // No response if we failed.
        true
    }

    fn end_event(&self, _response: &buck2_error::Result<Self::Response>) -> Self::EndEvent {
        buck2_data::TargetsCommandEnd {
            unresolved_target_patterns: self
                .req
                .target_patterns
                .iter()
                .map(|p| buck2_data::TargetPattern { value: p.clone() })
                .collect(),
        }
    }
}

async fn targets(
    server_ctx: &dyn ServerCommandContextTrait,
    stdout: &mut (impl Write + Send),
    mut dice: DiceTransaction,
    request: &TargetsRequest,
) -> anyhow::Result<TargetsResponse> {
    let cwd = server_ctx.working_dir();
    let cell_resolver = dice.get_cell_resolver().await?;
    let parsed_target_patterns = parse_patterns_from_cli_args::<TargetPatternExtra>(
        &mut dice,
        &request.target_patterns,
        cwd,
    )
    .await?;

    let (output_type, mut output) = outputter(request, stdout)?;

    let response = match &request.targets {
        Some(targets_request::Targets::ResolveAlias(_)) => {
            targets_resolve_aliases(dice, request, parsed_target_patterns).await?
        }
        Some(targets_request::Targets::Other(other)) => {
            if other.streaming {
                let formatter = create_formatter(request, other)?;
                let hashing = match TargetHashGraphType::from_i32(other.target_hash_graph_type)
                    .expect("buck cli should send valid target hash graph type")
                {
                    TargetHashGraphType::None => None,
                    _ => Some(other.target_hash_use_fast_hash),
                };

                let res = targets_streaming(
                    server_ctx,
                    dice,
                    formatter,
                    &mut output,
                    parsed_target_patterns,
                    other.keep_going,
                    other.cached,
                    other.imports,
                    hashing,
                    request.concurrency.as_ref().map(|x| x.concurrency as usize),
                )
                .await;
                // Make sure we always flush the outputter, even on failure, as we may have partially written to it
                output.flush()?;
                res?
            } else {
                let formatter = create_formatter(request, other)?;
                let global_cfg_options = global_cfg_options_from_client_context(
                    request
                        .target_cfg
                        .as_ref()
                        .internal_error("target_cfg must be set")?,
                    server_ctx,
                    &mut dice,
                )
                .await?;
                let fs = server_ctx.project_root();
                targets_batch(
                    server_ctx,
                    dice,
                    &*formatter,
                    parsed_target_patterns,
                    &global_cfg_options,
                    TargetHashOptions::new(other, &cell_resolver, fs)?,
                    other.keep_going,
                )
                .await?
            }
        }
        None => return Err(internal_error!("Missing field in proto request")),
    };

    let buffer = match output_type {
        OutputType::Stdout => response.serialized_targets_output,
        OutputType::File => {
            output.write_all(response.serialized_targets_output.as_bytes())?;
            String::new()
        }
    };
    let response = TargetsResponse {
        error_count: response.error_count,
        serialized_targets_output: buffer,
    };
    output.flush()?;
    Ok(response)
}
