/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

use buck2_node::nodes::configured::ConfiguredTargetNode;
use buck2_node::nodes::unconfigured::TargetNode;
use starlark::environment::GlobalsBuilder;
use starlark::starlark_module;
use starlark::values::starlark_value_as_type::StarlarkValueAsType;

use super::nodes::action::StarlarkActionQueryNode;
use crate::bxl::starlark_defs::analysis_result::StarlarkAnalysisResult;
use crate::bxl::starlark_defs::aquery::StarlarkAQueryCtx;
use crate::bxl::starlark_defs::artifacts::EnsuredArtifact;
use crate::bxl::starlark_defs::audit::StarlarkAuditCtx;
use crate::bxl::starlark_defs::build_result::StarlarkBxlBuildResult;
use crate::bxl::starlark_defs::cli_args::CliArgs;
use crate::bxl::starlark_defs::context::actions::BxlActions;
use crate::bxl::starlark_defs::context::fs::BxlFilesystem;
use crate::bxl::starlark_defs::context::output::OutputStream;
use crate::bxl::starlark_defs::context::BxlContext;
use crate::bxl::starlark_defs::cquery::StarlarkCQueryCtx;
use crate::bxl::starlark_defs::file_set::StarlarkFileNode;
use crate::bxl::starlark_defs::nodes::configured::StarlarkConfiguredTargetNode;
use crate::bxl::starlark_defs::nodes::configured::StarlarkLazyResolvedAttrs;
use crate::bxl::starlark_defs::nodes::unconfigured::StarlarkTargetNode;
use crate::bxl::starlark_defs::target_universe::StarlarkTargetUniverse;
use crate::bxl::starlark_defs::targetset::StarlarkTargetSet;
use crate::bxl::starlark_defs::uquery::StarlarkUQueryCtx;

#[starlark_module]
pub(crate) fn register_bxl_type_names_in_bxl_namespace(globals: &mut GlobalsBuilder) {
    const CliArgs: StarlarkValueAsType<CliArgs> = StarlarkValueAsType::new();
    const Context: StarlarkValueAsType<BxlContext> = StarlarkValueAsType::new();
    const AuditContext: StarlarkValueAsType<StarlarkAuditCtx> = StarlarkValueAsType::new();
    const AqueryContext: StarlarkValueAsType<StarlarkAQueryCtx> = StarlarkValueAsType::new();
    const CqueryContext: StarlarkValueAsType<StarlarkCQueryCtx> = StarlarkValueAsType::new();
    const UqueryContext: StarlarkValueAsType<StarlarkUQueryCtx> = StarlarkValueAsType::new();
    const Actions: StarlarkValueAsType<BxlActions> = StarlarkValueAsType::new();
    const Filesystem: StarlarkValueAsType<BxlFilesystem> = StarlarkValueAsType::new();
    const BuildResult: StarlarkValueAsType<StarlarkBxlBuildResult> = StarlarkValueAsType::new();
    const AnalysisResult: StarlarkValueAsType<StarlarkAnalysisResult> = StarlarkValueAsType::new();
    const EnsuredArtifact: StarlarkValueAsType<EnsuredArtifact> = StarlarkValueAsType::new();
    const FileNode: StarlarkValueAsType<StarlarkFileNode> = StarlarkValueAsType::new();
    const ActionQueryNode: StarlarkValueAsType<StarlarkActionQueryNode> =
        StarlarkValueAsType::new();
    // TODO(nero): TargetNode is a deprecated alias.
    const TargetNode: StarlarkValueAsType<StarlarkTargetNode> = StarlarkValueAsType::new();
    const UnconfiguredTargetNode: StarlarkValueAsType<StarlarkTargetNode> =
        StarlarkValueAsType::new();
    const ConfiguredTargetNode: StarlarkValueAsType<StarlarkConfiguredTargetNode> =
        StarlarkValueAsType::new();
    const LazyResolvedAttrs: StarlarkValueAsType<StarlarkLazyResolvedAttrs> =
        StarlarkValueAsType::new();
    const TargetSet: StarlarkValueAsType<StarlarkTargetSet<TargetNode>> =
        StarlarkValueAsType::new();
    const ConfiguredTargetSet: StarlarkValueAsType<StarlarkTargetSet<ConfiguredTargetNode>> =
        StarlarkValueAsType::new();
    const TargetUniverse: StarlarkValueAsType<StarlarkTargetUniverse> = StarlarkValueAsType::new();
    const OutputStream: StarlarkValueAsType<OutputStream> = StarlarkValueAsType::new();
}
