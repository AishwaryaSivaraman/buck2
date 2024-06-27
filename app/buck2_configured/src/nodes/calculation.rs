/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

//! Calculations relating to 'TargetNode's that runs on Dice

use std::iter;
use std::sync::Arc;

use allocative::Allocative;
use anyhow::Context;
use async_trait::async_trait;
use buck2_build_api::actions::execute::dice_data::HasFallbackExecutorConfig;
use buck2_build_api::transition::TRANSITION_ATTRS_PROVIDER;
use buck2_build_api::transition::TRANSITION_CALCULATION;
use buck2_common::dice::cycles::CycleGuard;
use buck2_core::cells::name::CellName;
use buck2_core::configuration::compatibility::IncompatiblePlatformReason;
use buck2_core::configuration::compatibility::IncompatiblePlatformReasonCause;
use buck2_core::configuration::compatibility::MaybeCompatible;
use buck2_core::configuration::data::ConfigurationData;
use buck2_core::configuration::pair::ConfigurationNoExec;
use buck2_core::configuration::pair::ConfigurationWithExec;
use buck2_core::configuration::transition::applied::TransitionApplied;
use buck2_core::configuration::transition::id::TransitionId;
use buck2_core::execution_types::execution::ExecutionPlatform;
use buck2_core::execution_types::execution::ExecutionPlatformResolution;
use buck2_core::plugins::PluginKind;
use buck2_core::plugins::PluginKindSet;
use buck2_core::plugins::PluginListElemKind;
use buck2_core::plugins::PluginLists;
use buck2_core::provider::label::ConfiguredProvidersLabel;
use buck2_core::provider::label::ProvidersLabel;
use buck2_core::target::configured_target_label::ConfiguredTargetLabel;
use buck2_core::target::label::label::TargetLabel;
use buck2_error::internal_error;
use buck2_error::AnyhowContextForError;
use buck2_error::BuckErrorContext;
use buck2_futures::cancellation::CancellationContext;
use buck2_node::attrs::configuration_context::AttrConfigurationContext;
use buck2_node::attrs::configuration_context::AttrConfigurationContextImpl;
use buck2_node::attrs::configuration_context::PlatformConfigurationError;
use buck2_node::attrs::configured_attr::ConfiguredAttr;
use buck2_node::attrs::configured_traversal::ConfiguredAttrTraversal;
use buck2_node::attrs::display::AttrDisplayWithContextExt;
use buck2_node::attrs::inspect_options::AttrInspectOptions;
use buck2_node::attrs::internal::EXEC_COMPATIBLE_WITH_ATTRIBUTE_FIELD;
use buck2_node::attrs::internal::LEGACY_TARGET_COMPATIBLE_WITH_ATTRIBUTE_FIELD;
use buck2_node::attrs::internal::TARGET_COMPATIBLE_WITH_ATTRIBUTE_FIELD;
use buck2_node::configuration::resolved::ConfigurationSettingKey;
use buck2_node::configuration::resolved::ResolvedConfiguration;
use buck2_node::configuration::resolved::ResolvedConfigurationSettings;
use buck2_node::configuration::toolchain_constraints::ToolchainConstraints;
use buck2_node::execution::GetExecutionPlatforms;
use buck2_node::nodes::configured::ConfiguredTargetNode;
use buck2_node::nodes::configured_frontend::ConfiguredTargetNodeCalculation;
use buck2_node::nodes::configured_frontend::ConfiguredTargetNodeCalculationImpl;
use buck2_node::nodes::configured_frontend::CONFIGURED_TARGET_NODE_CALCULATION;
use buck2_node::nodes::frontend::TargetGraphCalculation;
use buck2_node::nodes::unconfigured::TargetNode;
use buck2_node::nodes::unconfigured::TargetNodeRef;
use buck2_node::visibility::VisibilityError;
use derive_more::Display;
use dice::DiceComputations;
use dice::Key;
use dupe::Dupe;
use futures::FutureExt;
use starlark_map::ordered_map::OrderedMap;
use starlark_map::small_map::SmallMap;
use starlark_map::small_set::SmallSet;

use crate::calculation::ConfiguredGraphCycleDescriptor;
use crate::configuration::calculation::ConfigurationCalculation;
use crate::target::TargetConfiguredTargetLabel;

#[derive(Debug, buck2_error::Error)]
enum NodeCalculationError {
    #[error("expected `{0}` attribute to be a list but got `{1}`")]
    TargetCompatibleNotList(String, String),
    #[error(
        "`{0}` had both `{}` and `{}` attributes. It should only have one.",
        TARGET_COMPATIBLE_WITH_ATTRIBUTE_FIELD,
        LEGACY_TARGET_COMPATIBLE_WITH_ATTRIBUTE_FIELD
    )]
    BothTargetCompatibleWith(String),
    #[error(
        "Target {0} configuration transitioned\n\
        old: {1}\n\
        new: {2}\n\
        but attribute: {3}\n\
        resolved with old configuration to: {4}\n\
        resolved with new configuration to: {5}"
    )]
    TransitionAttrIncompatibleChange(
        TargetLabel,
        ConfigurationData,
        ConfigurationData,
        String,
        String,
        String,
    ),
}

enum CompatibilityConstraints {
    Any(ConfiguredAttr),
    All(ConfiguredAttr),
}

async fn compute_platform_cfgs(
    ctx: &mut DiceComputations<'_>,
    node: TargetNodeRef<'_>,
) -> anyhow::Result<OrderedMap<TargetLabel, ConfigurationData>> {
    let mut platform_map = OrderedMap::new();
    for platform_target in node.platform_deps() {
        let config = ctx.get_platform_configuration(platform_target).await?;
        platform_map.insert(platform_target.dupe(), config);
    }

    Ok(platform_map)
}

async fn legacy_execution_platform(
    ctx: &DiceComputations<'_>,
    cfg: &ConfigurationNoExec,
) -> ExecutionPlatform {
    ExecutionPlatform::legacy_execution_platform(
        ctx.get_fallback_executor_config().clone(),
        cfg.dupe(),
    )
}

#[derive(Debug, buck2_error::Error)]
enum ToolchainDepError {
    #[error("Can't find toolchain_dep execution platform using configuration `{0}`")]
    ToolchainDepMissingPlatform(ConfigurationData),
    #[error("Target `{0}` was used as a toolchain_dep, but is not a toolchain rule")]
    NonToolchainRuleUsedAsToolchainDep(TargetLabel),
    #[error("Target `{0}` was used not as a toolchain_dep, but is a toolchain rule")]
    ToolchainRuleUsedAsNormalDep(TargetLabel),
    #[error("Target `{0}` has a transition_dep, which is not permitted on a toolchain rule")]
    ToolchainTransitionDep(TargetLabel),
}

#[derive(Debug, buck2_error::Error)]
enum PluginDepError {
    #[error("Plugin dep `{0}` is a toolchain rule")]
    PluginDepIsToolchainRule(TargetLabel),
}

pub async fn find_execution_platform_by_configuration(
    ctx: &mut DiceComputations<'_>,
    exec_cfg: &ConfigurationData,
    cfg: &ConfigurationData,
) -> buck2_error::Result<ExecutionPlatform> {
    match ctx.get_execution_platforms().await? {
        Some(platforms) if exec_cfg != &ConfigurationData::unbound_exec() => {
            for c in platforms.candidates() {
                if c.cfg() == exec_cfg {
                    return Ok(c.dupe());
                }
            }
            Err(buck2_error::Error::new(
                ToolchainDepError::ToolchainDepMissingPlatform(exec_cfg.dupe()),
            ))
        }
        _ => Ok(legacy_execution_platform(ctx, &ConfigurationNoExec::new(cfg.dupe())).await),
    }
}

pub struct ExecutionPlatformConstraints {
    exec_deps: Arc<[TargetLabel]>,
    toolchain_deps: Arc<[TargetConfiguredTargetLabel]>,
    exec_compatible_with: Arc<[ConfigurationSettingKey]>,
}

impl ExecutionPlatformConstraints {
    pub fn new_constraints(
        exec_deps: Arc<[TargetLabel]>,
        toolchain_deps: Arc<[TargetConfiguredTargetLabel]>,
        exec_compatible_with: Arc<[ConfigurationSettingKey]>,
    ) -> Self {
        Self {
            exec_deps,
            toolchain_deps,
            exec_compatible_with,
        }
    }

    fn new(
        node: TargetNodeRef,
        gathered_deps: &GatheredDeps,
        cfg_ctx: &(dyn AttrConfigurationContext + Sync),
    ) -> buck2_error::Result<Self> {
        let exec_compatible_with: Arc<[_]> = if let Some(a) = node.attr_or_none(
            EXEC_COMPATIBLE_WITH_ATTRIBUTE_FIELD,
            AttrInspectOptions::All,
        ) {
            let configured_attr = a.configure(cfg_ctx).with_context(|| {
                format!(
                    "Error configuring attribute `{}` to resolve execution platform",
                    a.name
                )
            })?;
            ConfiguredTargetNode::attr_as_target_compatible_with(configured_attr.value)
                .map(|label| {
                    label.with_context(|| {
                        format!("attribute `{}`", EXEC_COMPATIBLE_WITH_ATTRIBUTE_FIELD)
                    })
                })
                .collect::<Result<_, _>>()?
        } else {
            Arc::new([])
        };

        Ok(Self::new_constraints(
            gathered_deps
                .exec_deps
                .iter()
                .map(|c| c.0.target().unconfigured().dupe())
                .collect(),
            gathered_deps
                .toolchain_deps
                .iter()
                .map(|c| c.dupe())
                .collect(),
            exec_compatible_with,
        ))
    }

    async fn toolchain_allows(
        &self,
        ctx: &mut DiceComputations<'_>,
    ) -> buck2_error::Result<Arc<[ToolchainConstraints]>> {
        // We could merge these constraints together, but the time to do that
        // probably outweighs the benefits given there are likely to only be a few
        // execution platforms to test.
        let mut result = Vec::with_capacity(self.toolchain_deps.len());
        for x in self.toolchain_deps.iter() {
            result.push(execution_platforms_for_toolchain(ctx, x.dupe()).await?)
        }
        Ok(result.into())
    }

    async fn one(
        self,
        ctx: &mut DiceComputations<'_>,
        node: TargetNodeRef<'_>,
    ) -> buck2_error::Result<ExecutionPlatformResolution> {
        let toolchain_allows = self.toolchain_allows(ctx).await?;
        ctx.resolve_execution_platform_from_constraints(
            node.label().pkg().cell_name(),
            self.exec_compatible_with,
            self.exec_deps,
            toolchain_allows,
        )
        .await
    }

    pub async fn one_for_cell(
        self,
        ctx: &mut DiceComputations<'_>,
        cell: CellName,
    ) -> buck2_error::Result<ExecutionPlatformResolution> {
        let toolchain_allows = self.toolchain_allows(ctx).await?;
        ctx.resolve_execution_platform_from_constraints(
            cell,
            self.exec_compatible_with,
            self.exec_deps,
            toolchain_allows,
        )
        .await
    }
}

async fn execution_platforms_for_toolchain(
    ctx: &mut DiceComputations<'_>,
    target: TargetConfiguredTargetLabel,
) -> buck2_error::Result<ToolchainConstraints> {
    #[derive(Clone, Display, Debug, Dupe, Eq, Hash, PartialEq, Allocative)]
    struct ExecutionPlatformsForToolchainKey(TargetConfiguredTargetLabel);

    #[async_trait]
    impl Key for ExecutionPlatformsForToolchainKey {
        type Value = buck2_error::Result<ToolchainConstraints>;
        async fn compute(
            &self,
            ctx: &mut DiceComputations,
            _cancellation: &CancellationContext,
        ) -> Self::Value {
            let node = ctx.get_target_node(self.0.unconfigured()).await?;
            if node.transition_deps().next().is_some() {
                // We could actually check this when defining the rule, but a bit of a corner
                // case, and much simpler to do so here.
                return Err(buck2_error::Error::new(
                    ToolchainDepError::ToolchainTransitionDep(self.0.unconfigured().dupe()),
                ));
            }
            let resolved_configuration = &ctx
                .get_resolved_configuration(
                    self.0.cfg(),
                    self.0.pkg().cell_name(),
                    node.get_configuration_deps(),
                )
                .await?;
            let platform_cfgs = compute_platform_cfgs(ctx, node.as_ref()).await?;
            // We don't really need `resolved_transitions` here:
            // `Traversal` declared above ignores transitioned dependencies.
            // But we pass `resolved_transitions` here to prevent breakages in the future
            // if something here changes.
            let resolved_transitions = OrderedMap::new();
            let cfg_ctx = AttrConfigurationContextImpl::new(
                resolved_configuration,
                ConfigurationNoExec::unbound_exec(),
                &resolved_transitions,
                &platform_cfgs,
            );
            let (gathered_deps, errors_and_incompats) =
                gather_deps(&self.0, node.as_ref(), &cfg_ctx, ctx).await?;
            if let Some(ret) = errors_and_incompats.finalize() {
                // Statically assert that we hit one of the `?`s
                enum Void {}
                let _: Void = ret?.require_compatible()?;
            }
            let constraints =
                ExecutionPlatformConstraints::new(node.as_ref(), &gathered_deps, &cfg_ctx)?;
            let toolchain_allows = constraints.toolchain_allows(ctx).await?;
            Ok(ToolchainConstraints::new(
                &constraints.exec_deps,
                &constraints.exec_compatible_with,
                &toolchain_allows,
            ))
        }

        fn equality(x: &Self::Value, y: &Self::Value) -> bool {
            match (x, y) {
                (Ok(x), Ok(y)) => x == y,
                _ => false,
            }
        }
    }

    ctx.compute(&ExecutionPlatformsForToolchainKey(target))
        .await?
}

pub async fn get_execution_platform_toolchain_dep(
    ctx: &mut DiceComputations<'_>,
    target_label: &TargetConfiguredTargetLabel,
    target_node: TargetNodeRef<'_>,
) -> buck2_error::Result<MaybeCompatible<ExecutionPlatformResolution>> {
    assert!(target_node.is_toolchain_rule());
    let target_cfg = target_label.cfg();
    let target_cell = target_node.label().pkg().cell_name();
    let resolved_configuration = ctx
        .get_resolved_configuration(
            target_cfg,
            target_cell,
            target_node.get_configuration_deps(),
        )
        .await?;
    if target_node.transition_deps().next().is_some() {
        Err(buck2_error::Error::new(
            ToolchainDepError::ToolchainTransitionDep(target_label.unconfigured().dupe()),
        ))
    } else {
        let platform_cfgs = compute_platform_cfgs(ctx, target_node).await?;
        let resolved_transitions = OrderedMap::new();
        let cfg_ctx = AttrConfigurationContextImpl::new(
            &resolved_configuration,
            ConfigurationNoExec::unbound_exec(),
            &resolved_transitions,
            &platform_cfgs,
        );
        let (gathered_deps, errors_and_incompats) =
            gather_deps(target_label, target_node, &cfg_ctx, ctx).await?;
        if let Some(ret) = errors_and_incompats.finalize() {
            return ret.map_err(Into::into);
        }
        Ok(MaybeCompatible::Compatible(
            resolve_execution_platform(
                ctx,
                target_node,
                &resolved_configuration,
                &gathered_deps,
                &cfg_ctx,
            )
            .await?,
        ))
    }
}

async fn resolve_execution_platform(
    ctx: &mut DiceComputations<'_>,
    node: TargetNodeRef<'_>,
    resolved_configuration: &ResolvedConfiguration,
    gathered_deps: &GatheredDeps,
    cfg_ctx: &(dyn AttrConfigurationContext + Sync),
) -> buck2_error::Result<ExecutionPlatformResolution> {
    // If no execution platforms are configured, we fall back to the legacy execution
    // platform behavior. We currently only support legacy execution platforms. That behavior is that there is a
    // single executor config (the fallback config) and the execution platform is in the same
    // configuration as the target.
    // The non-none case will be handled when we invoke the resolve_execution_platform() on ctx below, the none
    // case can't be handled there because we don't pass the full configuration into it.
    if ctx.get_execution_platforms().await?.is_none() {
        return Ok(ExecutionPlatformResolution::new(
            Some(legacy_execution_platform(ctx, resolved_configuration.cfg()).await),
            Vec::new(),
        ));
    };

    let constraints = ExecutionPlatformConstraints::new(node, gathered_deps, cfg_ctx)?;
    constraints.one(ctx, node).await
}

fn unpack_target_compatible_with_attr(
    target_node: TargetNodeRef,
    resolved_cfg: &ResolvedConfiguration,
    attr_name: &str,
) -> anyhow::Result<Option<ConfiguredAttr>> {
    let attr = target_node.attr_or_none(attr_name, AttrInspectOptions::All);
    let attr = match attr {
        Some(attr) => attr,
        None => return Ok(None),
    };

    struct AttrConfigurationContextToResolveCompatibleWith<'c> {
        resolved_cfg: &'c ResolvedConfiguration,
    }

    impl<'c> AttrConfigurationContext for AttrConfigurationContextToResolveCompatibleWith<'c> {
        fn resolved_cfg_settings(&self) -> &ResolvedConfigurationSettings {
            self.resolved_cfg.settings()
        }

        fn cfg(&self) -> ConfigurationNoExec {
            self.resolved_cfg.cfg().dupe()
        }

        fn exec_cfg(&self) -> anyhow::Result<ConfigurationNoExec> {
            Err(internal_error!(
                "exec_cfg() is not needed to resolve `{}` or `{}`",
                TARGET_COMPATIBLE_WITH_ATTRIBUTE_FIELD,
                LEGACY_TARGET_COMPATIBLE_WITH_ATTRIBUTE_FIELD
            ))
        }

        fn toolchain_cfg(&self) -> ConfigurationWithExec {
            unreachable!()
        }

        fn platform_cfg(&self, _label: &TargetLabel) -> anyhow::Result<ConfigurationData> {
            unreachable!(
                "platform_cfg() is not needed to resolve `{}` or `{}`",
                TARGET_COMPATIBLE_WITH_ATTRIBUTE_FIELD,
                LEGACY_TARGET_COMPATIBLE_WITH_ATTRIBUTE_FIELD
            )
        }

        fn resolved_transitions(
            &self,
        ) -> anyhow::Result<&OrderedMap<Arc<TransitionId>, Arc<TransitionApplied>>> {
            Err(internal_error!(
                "resolved_transitions() is not needed to resolve `{}` or `{}`",
                TARGET_COMPATIBLE_WITH_ATTRIBUTE_FIELD,
                LEGACY_TARGET_COMPATIBLE_WITH_ATTRIBUTE_FIELD
            ))
        }
    }

    let attr = attr
        .configure(&AttrConfigurationContextToResolveCompatibleWith { resolved_cfg })
        .with_context(|| format!("Error configuring attribute `{}`", attr_name))?;

    match attr.value.unpack_list() {
        Some(values) => {
            if !values.is_empty() {
                Ok(Some(attr.value))
            } else {
                Ok(None)
            }
        }
        None => Err(NodeCalculationError::TargetCompatibleNotList(
            attr.name.to_owned(),
            attr.value.as_display_no_ctx().to_string(),
        )
        .into()),
    }
}

fn check_compatible(
    target_label: &ConfiguredTargetLabel,
    target_node: TargetNodeRef,
    resolved_cfg: &ResolvedConfiguration,
) -> anyhow::Result<MaybeCompatible<()>> {
    let target_compatible_with = unpack_target_compatible_with_attr(
        target_node,
        resolved_cfg,
        TARGET_COMPATIBLE_WITH_ATTRIBUTE_FIELD,
    )?;
    let legacy_compatible_with = unpack_target_compatible_with_attr(
        target_node,
        resolved_cfg,
        LEGACY_TARGET_COMPATIBLE_WITH_ATTRIBUTE_FIELD,
    )?;

    let compatibility_constraints = match (target_compatible_with, legacy_compatible_with) {
        (None, None) => return Ok(MaybeCompatible::Compatible(())),
        (Some(..), Some(..)) => {
            return Err(
                NodeCalculationError::BothTargetCompatibleWith(target_label.to_string()).into(),
            );
        }
        (Some(target_compatible_with), None) => {
            CompatibilityConstraints::All(target_compatible_with)
        }
        (None, Some(legacy_compatible_with)) => {
            CompatibilityConstraints::Any(legacy_compatible_with)
        }
    };

    // We are compatible if the list of target expressions is empty,
    // OR if we match ANY expression in the list of attributes.
    let check_compatibility = |attr| -> anyhow::Result<(Vec<_>, Vec<_>)> {
        let mut left = Vec::new();
        let mut right = Vec::new();
        for label in ConfiguredTargetNode::attr_as_target_compatible_with(attr) {
            let label = label?;
            match resolved_cfg.settings().setting_matches(&label) {
                Some(_) => left.push(label),
                None => right.push(label),
            }
        }

        Ok((left, right))
    };

    // We only record the first incompatibility, for either ANY or ALL.
    // TODO(cjhopman): Should we report _all_ the things that are incompatible?
    let incompatible_target = match compatibility_constraints {
        CompatibilityConstraints::Any(attr) => {
            let (compatible, incompatible) = check_compatibility(attr).with_context(|| {
                format!(
                    "attribute `{}`",
                    LEGACY_TARGET_COMPATIBLE_WITH_ATTRIBUTE_FIELD
                )
            })?;
            let incompatible = incompatible.into_iter().next();
            match (compatible.is_empty(), incompatible.into_iter().next()) {
                (false, _) | (true, None) => {
                    return Ok(MaybeCompatible::Compatible(()));
                }
                (true, Some(v)) => v,
            }
        }
        CompatibilityConstraints::All(attr) => {
            let (_compatible, incompatible) = check_compatibility(attr).with_context(|| {
                format!("attribute `{}`", TARGET_COMPATIBLE_WITH_ATTRIBUTE_FIELD)
            })?;
            match incompatible.into_iter().next() {
                Some(label) => label,
                None => {
                    return Ok(MaybeCompatible::Compatible(()));
                }
            }
        }
    };
    Ok(MaybeCompatible::Incompatible(Arc::new(
        IncompatiblePlatformReason {
            target: target_label.dupe(),
            cause: IncompatiblePlatformReasonCause::UnsatisfiedConfig(incompatible_target.0),
        },
    )))
}

/// Ideally, we would check this much earlier. However, that turns out to be a bit tricky to
/// implement. Naively implementing this check on unconfigured nodes doesn't work because it results
/// in dice cycles when there are cycles in the unconfigured graph.
async fn check_plugin_deps(
    ctx: &mut DiceComputations<'_>,
    target_label: &ConfiguredTargetLabel,
    plugin_deps: &PluginLists,
) -> anyhow::Result<()> {
    for (_, dep_label, elem_kind) in plugin_deps.iter() {
        if *elem_kind == PluginListElemKind::Direct {
            let dep_node = ctx
                .get_target_node(dep_label)
                .await
                .with_context(|| format!("looking up unconfigured target node `{}`", dep_label))?;
            if dep_node.is_toolchain_rule() {
                return Err(PluginDepError::PluginDepIsToolchainRule(dep_label.dupe()).into());
            }
            if !dep_node.is_visible_to(target_label.unconfigured())? {
                return Err(VisibilityError::NotVisibleTo(
                    dep_label.dupe(),
                    target_label.unconfigured().dupe(),
                )
                .into());
            }
        }
    }
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CheckVisibility {
    Yes,
    No,
}

#[derive(Default)]
struct ErrorsAndIncompatibilities {
    errs: Vec<anyhow::Error>,
    incompats: Vec<Arc<IncompatiblePlatformReason>>,
}

impl ErrorsAndIncompatibilities {
    pub fn unpack_dep_into(
        &mut self,
        target_label: &TargetConfiguredTargetLabel,
        result: anyhow::Result<MaybeCompatible<ConfiguredTargetNode>>,
        check_visibility: CheckVisibility,
        list: &mut Vec<ConfiguredTargetNode>,
    ) {
        list.extend(self.unpack_dep(target_label, result, check_visibility));
    }

    fn unpack_dep(
        &mut self,
        target_label: &TargetConfiguredTargetLabel,
        result: anyhow::Result<MaybeCompatible<ConfiguredTargetNode>>,
        check_visibility: CheckVisibility,
    ) -> Option<ConfiguredTargetNode> {
        match result {
            Err(e) => {
                self.errs.push(e);
            }
            Ok(MaybeCompatible::Incompatible(reason)) => {
                self.incompats.push(Arc::new(IncompatiblePlatformReason {
                    target: target_label.inner().dupe(),
                    cause: IncompatiblePlatformReasonCause::Dependency(reason.dupe()),
                }));
            }
            Ok(MaybeCompatible::Compatible(dep)) => {
                if CheckVisibility::No == check_visibility {
                    return Some(dep);
                }
                match dep.is_visible_to(target_label.unconfigured()) {
                    Ok(true) => {
                        return Some(dep);
                    }
                    Ok(false) => {
                        self.errs.push(
                            VisibilityError::NotVisibleTo(
                                dep.label().unconfigured().dupe(),
                                target_label.unconfigured().dupe(),
                            )
                            .into(),
                        );
                    }
                    Err(e) => {
                        self.errs.push(e);
                    }
                }
            }
        }
        None
    }

    /// Returns an error/incompatibility to return, if any, and `None` otherwise
    pub fn finalize<T>(mut self) -> Option<anyhow::Result<MaybeCompatible<T>>> {
        // FIXME(JakobDegen): Report all incompatibilities
        if let Some(incompat) = self.incompats.pop() {
            return Some(Ok(MaybeCompatible::Incompatible(incompat)));
        }
        if let Some(err) = self.errs.pop() {
            return Some(Err(err));
        }
        None
    }
}

#[derive(Default)]
struct GatheredDeps {
    deps: Vec<ConfiguredTargetNode>,
    exec_deps: SmallMap<ConfiguredProvidersLabel, CheckVisibility>,
    toolchain_deps: SmallSet<TargetConfiguredTargetLabel>,
    plugin_lists: PluginLists,
}

async fn gather_deps(
    target_label: &TargetConfiguredTargetLabel,
    target_node: TargetNodeRef<'_>,
    attr_cfg_ctx: &(dyn AttrConfigurationContext + Sync),
    ctx: &mut DiceComputations<'_>,
) -> anyhow::Result<(GatheredDeps, ErrorsAndIncompatibilities)> {
    #[derive(Default)]
    struct Traversal {
        deps: OrderedMap<ConfiguredProvidersLabel, SmallSet<PluginKindSet>>,
        exec_deps: SmallMap<ConfiguredProvidersLabel, CheckVisibility>,
        toolchain_deps: SmallSet<TargetConfiguredTargetLabel>,
        plugin_lists: PluginLists,
    }

    impl ConfiguredAttrTraversal for Traversal {
        fn dep(&mut self, dep: &ConfiguredProvidersLabel) -> anyhow::Result<()> {
            self.deps.entry(dep.clone()).or_insert_with(SmallSet::new);
            Ok(())
        }

        fn dep_with_plugins(
            &mut self,
            dep: &ConfiguredProvidersLabel,
            plugin_kinds: &PluginKindSet,
        ) -> anyhow::Result<()> {
            self.deps
                .entry(dep.clone())
                .or_insert_with(SmallSet::new)
                .insert(plugin_kinds.dupe());
            Ok(())
        }

        fn exec_dep(&mut self, dep: &ConfiguredProvidersLabel) -> anyhow::Result<()> {
            self.exec_deps.insert(dep.clone(), CheckVisibility::Yes);
            Ok(())
        }

        fn toolchain_dep(&mut self, dep: &ConfiguredProvidersLabel) -> anyhow::Result<()> {
            self.toolchain_deps
                .insert(TargetConfiguredTargetLabel::new_without_exec_cfg(
                    dep.target().dupe(),
                ));
            Ok(())
        }

        fn plugin_dep(&mut self, dep: &TargetLabel, kind: &PluginKind) -> anyhow::Result<()> {
            self.plugin_lists
                .insert(kind.dupe(), dep.dupe(), PluginListElemKind::Direct);
            Ok(())
        }
    }

    let mut traversal = Traversal::default();
    for a in target_node.attrs(AttrInspectOptions::All) {
        let configured_attr = a.configure(attr_cfg_ctx)?;
        configured_attr.traverse(target_node.label().pkg(), &mut traversal)?;
    }

    let dep_results = CycleGuard::<ConfiguredGraphCycleDescriptor>::new(ctx)?
        .guard_this(ctx.compute_join(traversal.deps.iter(), |ctx, v| {
            async move { ctx.get_configured_target_node(v.0.target()).await }.boxed()
        }))
        .await
        .into_result(ctx)
        .await??;

    let mut plugin_lists = traversal.plugin_lists;
    let mut deps = Vec::new();
    let mut errors_and_incompats = ErrorsAndIncompatibilities::default();
    for (res, (_, plugin_kind_sets)) in dep_results.into_iter().zip(traversal.deps) {
        let Some(dep) = errors_and_incompats.unpack_dep(target_label, res, CheckVisibility::Yes)
        else {
            continue;
        };

        if !plugin_kind_sets.is_empty() {
            for (kind, plugins) in dep.plugin_lists().iter_by_kind() {
                let Some(should_propagate) = plugin_kind_sets
                    .iter()
                    .filter_map(|set| set.get(kind))
                    .reduce(std::ops::BitOr::bitor)
                else {
                    continue;
                };
                let should_propagate = if should_propagate {
                    PluginListElemKind::Propagate
                } else {
                    PluginListElemKind::NoPropagate
                };
                for (target, elem_kind) in plugins {
                    if *elem_kind != PluginListElemKind::NoPropagate {
                        plugin_lists.insert(kind.dupe(), target.dupe(), should_propagate);
                    }
                }
            }
        }

        deps.push(dep);
    }

    let mut exec_deps = traversal.exec_deps;
    for kind in target_node.uses_plugins() {
        for plugin_label in plugin_lists.iter_for_kind(kind).map(|(target, _)| {
            attr_cfg_ctx.configure_exec_target(&ProvidersLabel::default_for(target.dupe()))
        }) {
            exec_deps
                .entry(plugin_label?)
                .or_insert(CheckVisibility::No);
        }
    }

    Ok((
        GatheredDeps {
            deps,
            exec_deps,
            toolchain_deps: traversal.toolchain_deps,
            plugin_lists,
        },
        errors_and_incompats,
    ))
}

/// Resolves configured attributes of target node needed to compute transitions
async fn resolve_transition_attrs<'a>(
    transitions: impl Iterator<Item = &TransitionId>,
    target_node: &'a TargetNode,
    resolved_cfg: &ResolvedConfiguration,
    platform_cfgs: &OrderedMap<TargetLabel, ConfigurationData>,
    ctx: &mut DiceComputations<'_>,
) -> anyhow::Result<OrderedMap<&'a str, Arc<ConfiguredAttr>>> {
    struct AttrConfigurationContextToResolveTransitionAttrs<'c> {
        resolved_cfg: &'c ResolvedConfiguration,
        toolchain_cfg: ConfigurationWithExec,
        platform_cfgs: &'c OrderedMap<TargetLabel, ConfigurationData>,
    }

    impl<'c> AttrConfigurationContext for AttrConfigurationContextToResolveTransitionAttrs<'c> {
        fn resolved_cfg_settings(&self) -> &ResolvedConfigurationSettings {
            self.resolved_cfg.settings()
        }

        fn cfg(&self) -> ConfigurationNoExec {
            self.resolved_cfg.cfg().dupe()
        }

        fn exec_cfg(&self) -> anyhow::Result<ConfigurationNoExec> {
            Err(internal_error!(
                "exec_cfg() is not needed in pre transition attribute resolution."
            ))
        }

        fn toolchain_cfg(&self) -> ConfigurationWithExec {
            self.toolchain_cfg.dupe()
        }

        fn platform_cfg(&self, label: &TargetLabel) -> anyhow::Result<ConfigurationData> {
            match self.platform_cfgs.get(label) {
                Some(configuration) => Ok(configuration.dupe()),
                None => Err(PlatformConfigurationError::UnknownPlatformTarget(label.dupe()).into()),
            }
        }

        fn resolved_transitions(
            &self,
        ) -> anyhow::Result<&OrderedMap<Arc<TransitionId>, Arc<TransitionApplied>>> {
            Err(internal_error!(
                "resolved_transitions() can't be used before transition execution."
            ))
        }
    }

    let cfg_ctx = AttrConfigurationContextToResolveTransitionAttrs {
        resolved_cfg,
        platform_cfgs,
        toolchain_cfg: resolved_cfg
            .cfg()
            .make_toolchain(&ConfigurationNoExec::unbound_exec()),
    };
    let mut result = OrderedMap::default();
    for tr in transitions {
        let attrs = TRANSITION_ATTRS_PROVIDER
            .get()?
            .transition_attrs(ctx, &tr)
            .await?;
        if let Some(attrs) = attrs {
            for attr in attrs.as_ref() {
                // Multiple outgoing transitions may refer the same attribute.
                if result.contains_key(attr.as_str()) {
                    continue;
                }

                if let Some(coerced_attr) = target_node.attr(&attr, AttrInspectOptions::All)? {
                    let configured_attr = coerced_attr.configure(&cfg_ctx)?;
                    if let Some(old_val) =
                        result.insert(configured_attr.name, Arc::new(configured_attr.value))
                    {
                        return Err(internal_error!(
                            "Found duplicated value `{}` for attr `{}` on target `{}`",
                            &old_val.as_display_no_ctx(),
                            attr,
                            target_node.label()
                        ));
                    }
                }
            }
        }
    }
    Ok(result)
}

/// Verifies if configured node's attributes are equal to the same attributes configured with pre-transition configuration.
/// Only check attributes used in transition.
fn verify_transitioned_attrs<'a>(
    // Attributes resolved with pre-transition configuration
    pre_transition_attrs: &OrderedMap<&'a str, Arc<ConfiguredAttr>>,
    pre_transition_config: &ConfigurationData,
    node: &ConfiguredTargetNode,
) -> anyhow::Result<()> {
    for (attr, attr_value) in pre_transition_attrs {
        let transition_configured_attr = node
            .get(attr, AttrInspectOptions::All)
            .with_internal_error(|| {
                format!(
                    "Attr {} was not found in transition for target {}",
                    attr,
                    node.label(),
                )
            })?;
        if &transition_configured_attr.value != attr_value.as_ref() {
            return Err(NodeCalculationError::TransitionAttrIncompatibleChange(
                node.label().unconfigured().dupe(),
                pre_transition_config.dupe(),
                node.label().cfg().dupe(),
                attr.to_string(),
                attr_value.as_display_no_ctx().to_string(),
                transition_configured_attr
                    .value
                    .as_display_no_ctx()
                    .to_string(),
            )
            .into());
        }
    }
    Ok(())
}

/// Compute configured target node ignoring transition for this node.
async fn compute_configured_target_node_no_transition(
    target_label: &ConfiguredTargetLabel,
    target_node: TargetNode,
    ctx: &mut DiceComputations<'_>,
) -> anyhow::Result<MaybeCompatible<ConfiguredTargetNode>> {
    let partial_target_label =
        &TargetConfiguredTargetLabel::new_without_exec_cfg(target_label.dupe());
    let target_cfg = target_label.cfg();
    let target_cell = target_node.label().pkg().cell_name();
    let resolved_configuration = ctx
        .get_resolved_configuration(
            target_cfg,
            target_cell,
            target_node.get_configuration_deps(),
        )
        .await
        .with_context(|| format!("Error resolving configuration deps of `{}`", target_label))?;

    // Must check for compatibility before evaluating non-compatibility attributes.
    if let MaybeCompatible::Incompatible(reason) =
        check_compatible(target_label, target_node.as_ref(), &resolved_configuration)?
    {
        return Ok(MaybeCompatible::Incompatible(reason));
    }

    let platform_cfgs = compute_platform_cfgs(ctx, target_node.as_ref()).await?;

    let mut resolved_transitions = OrderedMap::new();
    let attrs = resolve_transition_attrs(
        target_node.transition_deps().map(|(_, tr)| tr.as_ref()),
        &target_node,
        &resolved_configuration,
        &platform_cfgs,
        ctx,
    )
    .await?;
    for (_dep, tr) in target_node.transition_deps() {
        let resolved_cfg = TRANSITION_CALCULATION
            .get()?
            .apply_transition(ctx, &attrs, target_cfg, tr)
            .await?;
        resolved_transitions.insert(tr.dupe(), resolved_cfg);
    }

    // We need to collect deps and to ensure that all attrs can be successfully
    // configured so that we don't need to support propagate configuration errors on attr access.
    let attr_cfg_ctx = AttrConfigurationContextImpl::new(
        &resolved_configuration,
        // We have not yet done exec platform resolution so for now we just use `unbound_exec`
        // here. We only use this when collecting exec deps and toolchain deps. In both of those
        // cases, we replace the exec cfg later on in this function with the "proper" exec cfg.
        ConfigurationNoExec::unbound_exec(),
        &resolved_transitions,
        &platform_cfgs,
    );
    let (gathered_deps, mut errors_and_incompats) = gather_deps(
        partial_target_label,
        target_node.as_ref(),
        &attr_cfg_ctx,
        ctx,
    )
    .await?;

    check_plugin_deps(ctx, target_label, &gathered_deps.plugin_lists).await?;

    let execution_platform_resolution = if target_cfg.is_unbound() {
        // The unbound configuration is used when evaluation configuration nodes.
        // That evaluation is
        // (1) part of execution platform resolution and
        // (2) isn't allowed to do execution
        // And so we use an "unspecified" execution platform to avoid cycles and cause any attempts at execution to fail.
        ExecutionPlatformResolution::unspecified()
    } else if let Some(exec_cfg) = target_label.exec_cfg() {
        // The label was produced by a toolchain_dep, so we use the execution platform of our parent
        // We need to convert that to an execution platform, so just find the one with the same configuration.
        ExecutionPlatformResolution::new(
            Some(
                find_execution_platform_by_configuration(
                    ctx,
                    exec_cfg,
                    resolved_configuration.cfg().cfg(),
                )
                .await?,
            ),
            Vec::new(),
        )
    } else {
        resolve_execution_platform(
            ctx,
            target_node.as_ref(),
            &resolved_configuration,
            &gathered_deps,
            &attr_cfg_ctx,
        )
        .await?
    };
    let execution_platform = execution_platform_resolution.cfg();

    // We now need to replace the dummy exec config we used above with the real one

    let execution_platform = &execution_platform;
    let toolchain_deps = &gathered_deps.toolchain_deps;
    let exec_deps = &gathered_deps.exec_deps;

    let get_toolchain_deps = DiceComputations::declare_closure(move |ctx| {
        async move {
            ctx.compute_join(
                toolchain_deps,
                |ctx, target: &TargetConfiguredTargetLabel| {
                    async move {
                        ctx.get_configured_target_node(
                            &target.with_exec_cfg(execution_platform.cfg().dupe()),
                        )
                        .await
                    }
                    .boxed()
                },
            )
            .await
        }
        .boxed()
    });

    let get_exec_deps = DiceComputations::declare_closure(|ctx| {
        async move {
            ctx.compute_join(exec_deps, |ctx, (target, check_visibility)| {
                async move {
                    (
                        ctx.get_configured_target_node(
                            &target
                                .target()
                                .unconfigured()
                                .configure_pair(execution_platform.cfg_pair().dupe()),
                        )
                        .await,
                        *check_visibility,
                    )
                }
                .boxed()
            })
            .await
        }
        .boxed()
    });

    let (toolchain_dep_results, exec_dep_results): (Vec<_>, Vec<_>) =
        CycleGuard::<ConfiguredGraphCycleDescriptor>::new(&ctx)?
            .guard_this(ctx.compute2(get_toolchain_deps, get_exec_deps))
            .await
            .into_result(ctx)
            .await??;

    let mut deps = gathered_deps.deps;
    let mut exec_deps = Vec::with_capacity(gathered_deps.exec_deps.len());

    for dep in toolchain_dep_results {
        errors_and_incompats.unpack_dep_into(
            partial_target_label,
            dep,
            CheckVisibility::Yes,
            &mut deps,
        );
    }
    for (dep, check_visibility) in exec_dep_results {
        errors_and_incompats.unpack_dep_into(
            partial_target_label,
            dep,
            check_visibility,
            &mut exec_deps,
        );
    }

    if let Some(ret) = errors_and_incompats.finalize() {
        return ret;
    }

    Ok(MaybeCompatible::Compatible(ConfiguredTargetNode::new(
        target_label.dupe(),
        target_node.dupe(),
        resolved_configuration,
        resolved_transitions,
        execution_platform_resolution,
        deps,
        exec_deps,
        platform_cfgs,
        gathered_deps.plugin_lists,
    )))
}

/// Compute configured target node after transition is applied to the target.
///
/// This function creates two node: transitioned node and a forward node.
/// Forward node is returned.
async fn compute_configured_target_node_with_transition(
    key: &ConfiguredTransitionedNodeKey,
    ctx: &mut DiceComputations<'_>,
) -> anyhow::Result<MaybeCompatible<ConfiguredTargetNode>> {
    assert_eq!(
        key.forward.unconfigured(),
        key.transitioned.unconfigured(),
        "Transition can be done only to the nodes with different configuration; \
               this valid case was ruled out earlier"
    );
    assert_ne!(
        key.forward, key.transitioned,
        "Transition can only happen to a node with the same unconfigured target"
    );

    let target_node = ctx.get_target_node(key.transitioned.unconfigured()).await?;
    let transitioned_node =
        compute_configured_target_node_no_transition(&key.transitioned, target_node.dupe(), ctx)
            .await?;
    transitioned_node.try_map(|transitioned_node| {
        ConfiguredTargetNode::new_forward(key.forward.dupe(), transitioned_node)
    })
}

async fn compute_configured_target_node(
    key: &ConfiguredTargetNodeKey,
    ctx: &mut DiceComputations<'_>,
) -> anyhow::Result<MaybeCompatible<ConfiguredTargetNode>> {
    let target_node = ctx
        .get_target_node(key.0.unconfigured())
        .await
        .with_context(|| {
            format!(
                "looking up unconfigured target node `{}`",
                key.0.unconfigured()
            )
        })?;

    match key.0.exec_cfg() {
        None if target_node.is_toolchain_rule() => {
            return Err(ToolchainDepError::ToolchainRuleUsedAsNormalDep(
                key.0.unconfigured().dupe(),
            )
            .into());
        }
        Some(_) if !target_node.is_toolchain_rule() => {
            return Err(ToolchainDepError::NonToolchainRuleUsedAsToolchainDep(
                key.0.unconfigured().dupe(),
            )
            .into());
        }
        _ => {}
    }

    if let Some(transition_id) = &target_node.rule.cfg {
        compute_configured_forward_target_node(key, &target_node, transition_id, ctx).await
    } else {
        // We are not caching `ConfiguredTransitionedNodeKey` because this is cheap,
        // and no need to fetch `target_node` again.
        compute_configured_target_node_no_transition(&key.0.dupe(), target_node, ctx).await
    }
}

async fn compute_configured_forward_target_node(
    key: &ConfiguredTargetNodeKey,
    target_node: &TargetNode,
    transition_id: &TransitionId,
    ctx: &mut DiceComputations<'_>,
) -> anyhow::Result<MaybeCompatible<ConfiguredTargetNode>> {
    #[async_trait]
    impl Key for ConfiguredTransitionedNodeKey {
        type Value = buck2_error::Result<MaybeCompatible<ConfiguredTargetNode>>;

        async fn compute(
            &self,
            ctx: &mut DiceComputations,
            _cancellation: &CancellationContext,
        ) -> buck2_error::Result<MaybeCompatible<ConfiguredTargetNode>> {
            compute_configured_target_node_with_transition(self, ctx)
                .await
                .map_err(buck2_error::Error::from)
        }

        fn equality(x: &Self::Value, y: &Self::Value) -> bool {
            if let (Ok(x), Ok(y)) = (x, y) {
                x == y
            } else {
                false
            }
        }
    }

    let target_label_before_transition = &key.0;
    let platform_cfgs = compute_platform_cfgs(ctx, target_node.as_ref()).await?;
    let resolved_configuration = ctx
        .get_resolved_configuration(
            target_label_before_transition.cfg(),
            target_node.label().pkg().cell_name(),
            target_node.get_configuration_deps(),
        )
        .await
        .with_context(|| {
            format!(
                "Error resolving configuration deps of `{}`",
                target_label_before_transition
            )
        })?;

    let attrs = resolve_transition_attrs(
        iter::once(transition_id),
        target_node,
        &resolved_configuration,
        &platform_cfgs,
        ctx,
    )
    .await?;

    let cfg = TRANSITION_CALCULATION
        .get()?
        .apply_transition(
            ctx,
            &attrs,
            target_label_before_transition.cfg(),
            transition_id,
        )
        .await?;
    let target_label_after_transition = target_label_before_transition
        .unconfigured()
        .configure(cfg.single()?.dupe());

    if &target_label_after_transition == target_label_before_transition {
        // Transitioned to identical configured target, no need to create a forward node.
        compute_configured_target_node_no_transition(
            target_label_before_transition,
            target_node.dupe(),
            ctx,
        )
        .await
    } else {
        let configured_target_node = ctx
            .compute(&ConfiguredTransitionedNodeKey {
                forward: target_label_before_transition.dupe(),
                transitioned: target_label_after_transition,
            })
            .await??;

        if let MaybeCompatible::Compatible(configured_target_node) = &configured_target_node {
            let forward_target_node = configured_target_node
                .forward_target()
                .internal_error("must be forward node")?;

            verify_transitioned_attrs(
                &attrs,
                resolved_configuration.cfg().cfg(),
                forward_target_node,
            )?;
        }

        Ok(configured_target_node)
    }
}

#[derive(Clone, Dupe, Display, Debug, Eq, Hash, PartialEq, Allocative)]
pub struct ConfiguredTargetNodeKey(pub ConfiguredTargetLabel);

/// Similar to [`ConfiguredTargetNodeKey`], but used when the target
/// is transitioned to different configuration because rule definition requires it.
#[derive(Clone, Dupe, Display, Debug, Eq, Hash, PartialEq, Allocative)]
#[display(fmt = "ConfiguredTransitionedNodeKey({}, {})", forward, transitioned)]
pub struct ConfiguredTransitionedNodeKey {
    /// Forward node label.
    forward: ConfiguredTargetLabel,
    /// Transitional node label.
    transitioned: ConfiguredTargetLabel,
}

struct ConfiguredTargetNodeCalculationInstance;

pub(crate) fn init_configured_target_node_calculation() {
    CONFIGURED_TARGET_NODE_CALCULATION.init(&ConfiguredTargetNodeCalculationInstance);
}

#[derive(Debug, Allocative, Eq, PartialEq)]
struct LookingUpConfiguredNodeContext {
    target: ConfiguredTargetLabel,
    len: usize,
    rest: Option<Arc<Self>>,
}

impl buck2_error::TypedContext for LookingUpConfiguredNodeContext {
    fn eq(&self, other: &dyn buck2_error::TypedContext) -> bool {
        match (other as &dyn std::any::Any).downcast_ref::<Self>() {
            Some(v) => self == v,
            None => false,
        }
    }
}

impl LookingUpConfiguredNodeContext {
    fn new(target: ConfiguredTargetLabel, parent: Option<Arc<Self>>) -> Self {
        let (len, rest) = match parent {
            Some(v) => (v.len + 1, Some(v.clone())),
            None => (1, None),
        };
        Self { target, len, rest }
    }

    fn add_context<T>(res: anyhow::Result<T>, target: ConfiguredTargetLabel) -> anyhow::Result<T> {
        res.compute_context(
            |parent_ctx: Arc<Self>| Self::new(target.clone(), Some(parent_ctx)),
            || Self::new(target.clone(), None),
        )
    }
}

impl std::fmt::Display for LookingUpConfiguredNodeContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.len == 1 {
            write!(f, "Error looking up configured node {}", &self.target)?;
        } else {
            writeln!(
                f,
                "Error in configured node dependency, dependency chain follows (-> indicates depends on, ^ indicates same configuration as previous):"
            )?;

            let mut curr = self;
            let mut prev_cfg = None;
            let mut is_first = true;

            loop {
                f.write_str("    ")?;
                if is_first {
                    f.write_str("   ")?;
                } else {
                    f.write_str("-> ")?;
                }

                write!(f, "{}", curr.target.unconfigured())?;
                let cfg = Some(curr.target.cfg());
                f.write_str(" (")?;
                if cfg == prev_cfg {
                    f.write_str("^")?;
                } else {
                    std::fmt::Display::fmt(curr.target.cfg(), f)?;
                }
                f.write_str(")\n")?;
                is_first = false;
                prev_cfg = Some(curr.target.cfg());
                match &curr.rest {
                    Some(v) => curr = &**v,
                    None => break,
                }
            }
        }
        Ok(())
    }
}

#[async_trait]
impl ConfiguredTargetNodeCalculationImpl for ConfiguredTargetNodeCalculationInstance {
    async fn get_configured_target_node(
        &self,
        ctx: &mut DiceComputations<'_>,
        target: &ConfiguredTargetLabel,
    ) -> anyhow::Result<MaybeCompatible<ConfiguredTargetNode>> {
        #[async_trait]
        impl Key for ConfiguredTargetNodeKey {
            type Value = buck2_error::Result<MaybeCompatible<ConfiguredTargetNode>>;
            async fn compute(
                &self,
                ctx: &mut DiceComputations,
                _cancellation: &CancellationContext,
            ) -> Self::Value {
                let res = compute_configured_target_node(self, ctx).await;
                Ok(LookingUpConfiguredNodeContext::add_context(
                    res,
                    self.0.dupe(),
                )?)
            }

            fn equality(x: &Self::Value, y: &Self::Value) -> bool {
                match (x, y) {
                    (Ok(x), Ok(y)) => x == y,
                    _ => false,
                }
            }
        }

        ctx.compute(&ConfiguredTargetNodeKey(target.dupe()))
            .await?
            .map_err(anyhow::Error::from)
    }
}
