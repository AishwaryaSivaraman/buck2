/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

use std::collections::HashMap;
use std::fmt::Debug;
use std::mem;
use std::sync::Arc;

use allocative::Allocative;
use anyhow::Context;
use async_trait::async_trait;
use buck2_analysis::analysis::calculation::get_rule_spec;
use buck2_analysis::analysis::env::RuleAnalysisAttrResolutionContext;
use buck2_analysis::analysis::env::RuleSpec;
use buck2_artifact::artifact::artifact_type::Artifact;
use buck2_build_api::analysis::anon_promises_dyn::AnonPromisesDyn;
use buck2_build_api::analysis::anon_targets_registry::AnonTargetsRegistryDyn;
use buck2_build_api::analysis::anon_targets_registry::ANON_TARGET_REGISTRY_NEW;
use buck2_build_api::analysis::registry::AnalysisRegistry;
use buck2_build_api::analysis::AnalysisResult;
use buck2_build_api::artifact_groups::promise::PromiseArtifact;
use buck2_build_api::artifact_groups::promise::PromiseArtifactId;
use buck2_build_api::artifact_groups::promise::PromiseArtifactResolveError;
use buck2_build_api::deferred::calculation::EVAL_ANON_TARGET;
use buck2_build_api::deferred::calculation::GET_PROMISED_ARTIFACT;
use buck2_build_api::interpreter::rule_defs::artifact::starlark_artifact_like::ValueAsArtifactLike;
use buck2_build_api::interpreter::rule_defs::context::AnalysisContext;
use buck2_build_api::interpreter::rule_defs::plugins::AnalysisPlugins;
use buck2_build_api::interpreter::rule_defs::provider::collection::FrozenProviderCollectionValue;
use buck2_build_api::interpreter::rule_defs::provider::collection::ProviderCollection;
use buck2_configured::nodes::calculation::find_execution_platform_by_configuration;
use buck2_core::base_deferred_key::BaseDeferredKey;
use buck2_core::base_deferred_key::BaseDeferredKeyDyn;
use buck2_core::cells::name::CellName;
use buck2_core::cells::paths::CellRelativePath;
use buck2_core::execution_types::execution::ExecutionPlatformResolution;
use buck2_core::package::PackageLabel;
use buck2_core::pattern::pattern::lex_target_pattern;
use buck2_core::pattern::pattern::PatternData;
use buck2_core::pattern::pattern_type::TargetPatternExtra;
use buck2_core::target::label::label::TargetLabel;
use buck2_core::target::name::TargetNameRef;
use buck2_core::unsafe_send_future::UnsafeSendFuture;
use buck2_error::BuckErrorContext;
use buck2_events::dispatch::get_dispatcher;
use buck2_events::dispatch::span_async;
use buck2_execute::digest_config::HasDigestConfig;
use buck2_futures::cancellation::CancellationContext;
use buck2_interpreter::dice::starlark_provider::with_starlark_eval_provider;
use buck2_interpreter::error::BuckStarlarkError;
use buck2_interpreter::print_handler::EventDispatcherPrintHandler;
use buck2_interpreter::soft_error::Buck2StarlarkSoftErrorHandler;
use buck2_interpreter::starlark_profiler::profiler::StarlarkProfilerOpt;
use buck2_interpreter::starlark_promise::StarlarkPromise;
use buck2_interpreter::types::configured_providers_label::StarlarkConfiguredProvidersLabel;
use buck2_interpreter_for_build::rule::FrozenRuleCallable;
use buck2_node::attrs::attr_type::AttrType;
use buck2_node::attrs::coerced_attr::CoercedAttr;
use buck2_node::attrs::internal::internal_attrs;
use buck2_util::arc_str::ArcStr;
use derive_more::Display;
use dice::DiceComputations;
use dice::Key;
use dupe::Dupe;
use futures::future::BoxFuture;
use futures::FutureExt;
use starlark::any::AnyLifetime;
use starlark::any::ProvidesStaticType;
use starlark::codemap::FileSpan;
use starlark::environment::Module;
use starlark::eval::Evaluator;
use starlark::values::dict::UnpackDictEntries;
use starlark::values::structs::AllocStruct;
use starlark::values::Trace;
use starlark::values::UnpackValue;
use starlark::values::Value;
use starlark::values::ValueTyped;
use starlark::StarlarkResultExt;
use starlark_map::ordered_map::OrderedMap;
use starlark_map::small_map::SmallMap;

use crate::anon_promises::AnonPromises;
use crate::anon_target_attr::AnonTargetAttr;
use crate::anon_target_attr_coerce::AnonTargetAttrTypeCoerce;
use crate::anon_target_attr_resolve::AnonTargetAttrResolution;
use crate::anon_target_attr_resolve::AnonTargetAttrResolutionContext;
use crate::anon_target_attr_resolve::AnonTargetDependents;
use crate::anon_target_node::AnonTarget;
use crate::promise_artifacts::PromiseArtifactRegistry;

#[derive(Debug, Trace, Allocative, ProvidesStaticType)]
pub struct AnonTargetsRegistry<'v> {
    // We inherit the execution platform of our parent
    execution_platform: ExecutionPlatformResolution,
    promises: AnonPromises<'v>,
    promise_artifact_registry: PromiseArtifactRegistry,
}

#[derive(Debug, buck2_error::Error)]
pub enum AnonTargetsError {
    #[error("Not allowed to call `anon_targets` in this context")]
    AssertNoPromisesFailed,
    #[error(
        "Invalid `name` attribute, must be a label or a string, got `{value}` of type `{typ}`"
    )]
    InvalidNameType { typ: String, value: String },
    #[error("`name` attribute must be a valid target label, got `{0}`")]
    NotTargetLabel(String),
    #[error("Unknown attribute `{0}`")]
    UnknownAttribute(String),
    #[error("Internal attribute `{0}` not allowed as argument to `anon_targets`")]
    InternalAttribute(String),
    #[error("Missing attribute `{0}`")]
    MissingAttribute(String),
    #[error("Query macros are not supported")]
    QueryMacroNotSupported,
}

#[derive(Hash, Eq, PartialEq, Clone, Dupe, Debug, Display, Trace, Allocative)]
pub(crate) struct AnonTargetKey(pub(crate) Arc<AnonTarget>);

#[async_trait]
impl Key for AnonTargetKey {
    type Value = buck2_error::Result<AnalysisResult>;

    async fn compute(
        &self,
        ctx: &mut DiceComputations,
        _cancellation: &CancellationContext,
    ) -> Self::Value {
        Ok(self.run_analysis(ctx).await?)
    }

    fn equality(_: &Self::Value, _: &Self::Value) -> bool {
        false
    }
}

impl AnonTargetKey {
    fn downcast(key: Arc<dyn BaseDeferredKeyDyn>) -> anyhow::Result<Self> {
        Ok(AnonTargetKey(
            key.into_any()
                .downcast()
                .ok()
                .internal_error("Expecting AnonTarget")?,
        ))
    }

    pub(crate) fn new<'v>(
        execution_platform: &ExecutionPlatformResolution,
        rule: ValueTyped<'v, FrozenRuleCallable>,
        attributes: UnpackDictEntries<&'v str, Value<'v>>,
    ) -> anyhow::Result<Self> {
        let mut name = None;
        let internal_attrs = internal_attrs();

        let entries = attributes.entries;
        let attrs_spec = rule.attributes();
        let mut attrs = OrderedMap::with_capacity(attrs_spec.len());

        let anon_attr_ctx = AnonAttrCtx::new(execution_platform);

        for (k, v) in entries {
            if k == "name" {
                name = Some(Self::coerce_name(v)?);
            } else if internal_attrs.contains_key(k) {
                return Err(AnonTargetsError::InternalAttribute(k.to_owned()).into());
            } else {
                let attr = attrs_spec
                    .attribute(k)
                    .ok_or_else(|| AnonTargetsError::UnknownAttribute(k.to_owned()))?;
                attrs.insert(
                    k.to_owned(),
                    Self::coerce_to_anon_target_attr(attr.coercer(), v, &anon_attr_ctx)
                        .with_context(|| format!("Error coercing attribute `{}`", k))?,
                );
            }
        }
        for (k, _, a) in attrs_spec.attr_specs() {
            if !attrs.contains_key(k) && !internal_attrs.contains_key(k) {
                if let Some(x) = a.default() {
                    attrs.insert(
                        k.to_owned(),
                        Self::coerced_to_anon_target_attr(x, a.coercer())?,
                    );
                } else {
                    return Err(AnonTargetsError::MissingAttribute(k.to_owned()).into());
                }
            }
        }

        // We need to ensure there is a "name" attribute which corresponds to something we can turn in to a label.
        // If there isn't a good one, make something up
        let name = match name {
            None => Self::create_name(&rule.rule_type().name)?,
            Some(name) => name,
        };

        Ok(Self(Arc::new(AnonTarget::new(
            rule.rule_type().dupe(),
            name,
            attrs.into(),
            execution_platform.cfg().dupe(),
        ))))
    }

    /// We need to parse a TargetLabel from a String, but it doesn't matter if the pieces aren't
    /// valid targets in the context of this build (e.g. if the package really exists),
    /// just that it is syntactically valid.
    fn parse_target_label(x: &str) -> anyhow::Result<TargetLabel> {
        let err = || AnonTargetsError::NotTargetLabel(x.to_owned());
        let lex = lex_target_pattern::<TargetPatternExtra>(x, false).with_context(err)?;
        // TODO(nga): `CellName` contract requires it refers to declared cell name.
        //   This `unchecked_new` violates it.
        let cell =
            CellName::unchecked_new(lex.cell_alias.filter(|a| !a.is_empty()).unwrap_or("anon"))?;
        match lex.pattern.reject_ambiguity()? {
            PatternData::TargetInPackage {
                package,
                target_name,
                extra: TargetPatternExtra,
            } => Ok(TargetLabel::new(
                PackageLabel::new(cell, CellRelativePath::new(package)),
                target_name.as_ref(),
            )),
            _ => Err(err().into()),
        }
    }

    fn create_name(rule_name: &str) -> anyhow::Result<TargetLabel> {
        // TODO(nga): this creates non-existing cell reference.
        let cell_name = CellName::unchecked_new("anon")?;
        let pkg = PackageLabel::new(cell_name, CellRelativePath::empty());
        Ok(TargetLabel::new(pkg, TargetNameRef::new(rule_name)?))
    }

    fn coerce_name(x: Value) -> anyhow::Result<TargetLabel> {
        if let Some(x) = StarlarkConfiguredProvidersLabel::from_value(x) {
            Ok(x.label().target().unconfigured().dupe())
        } else if let Some(x) = x.unpack_str() {
            Self::parse_target_label(x)
        } else {
            Err(AnonTargetsError::InvalidNameType {
                typ: x.get_type().to_owned(),
                value: x.to_string(),
            }
            .into())
        }
    }

    fn coerce_to_anon_target_attr(
        attr: &AttrType,
        x: Value,
        ctx: &AnonAttrCtx,
    ) -> anyhow::Result<AnonTargetAttr> {
        attr.coerce_item(ctx, x)
    }

    fn coerced_to_anon_target_attr(
        x: &CoercedAttr,
        ty: &AttrType,
    ) -> anyhow::Result<AnonTargetAttr> {
        AnonTargetAttr::from_coerced_attr(x, ty)
    }

    pub(crate) async fn resolve(
        &self,
        dice: &mut DiceComputations<'_>,
    ) -> anyhow::Result<AnalysisResult> {
        Ok(dice.compute(self).await??)
    }

    fn run_analysis<'a>(
        &'a self,
        dice: &'a mut DiceComputations<'_>,
    ) -> BoxFuture<'a, anyhow::Result<AnalysisResult>> {
        let fut = async move { self.run_analysis_impl(dice).await };
        Box::pin(unsafe { UnsafeSendFuture::new_encapsulates_starlark(fut) })
    }

    async fn run_analysis_impl(
        &self,
        dice: &mut DiceComputations<'_>,
    ) -> anyhow::Result<AnalysisResult> {
        let dependents = AnonTargetDependents::get_dependents(self)?;
        let dependents_analyses = dependents.get_analysis_results(dice).await?;

        let exec_resolution = ExecutionPlatformResolution::new(
            Some(
                find_execution_platform_by_configuration(
                    dice,
                    self.0.exec_cfg().cfg(),
                    self.0.exec_cfg().cfg(),
                )
                .await?,
            ),
            Vec::new(),
        );

        let rule_impl = get_rule_spec(dice, self.0.rule_type()).await?;
        let env = Module::new();
        let print = EventDispatcherPrintHandler(get_dispatcher());

        span_async(
            buck2_data::AnalysisStart {
                target: Some(self.0.as_proto().into()),
                rule: self.0.rule_type().to_string(),
            },
            async move {
                let (dice, mut eval, ctx, list_res) = with_starlark_eval_provider(
                    dice,
                    &mut StarlarkProfilerOpt::disabled(),
                    format!("anon_analysis:{}", self),
                    |provider, dice| {
                        let (mut eval, _) = provider.make(&env)?;
                        eval.set_print_handler(&print);
                        eval.set_soft_error_handler(&Buck2StarlarkSoftErrorHandler);

                        // No attributes are allowed to contain macros or other stuff, so an empty resolution context works
                        let rule_analysis_attr_resolution_ctx = RuleAnalysisAttrResolutionContext {
                            module: &env,
                            dep_analysis_results: dependents_analyses.dep_analysis_results,
                            query_results: HashMap::new(),
                            execution_platform_resolution: exec_resolution.clone(),
                        };

                        let resolution_ctx = AnonTargetAttrResolutionContext {
                            promised_artifacts_map: dependents_analyses.promised_artifacts,
                            rule_analysis_attr_resolution_ctx,
                        };

                        let mut resolved_attrs = Vec::with_capacity(self.0.attrs().len());
                        for (name, attr) in self.0.attrs().iter() {
                            resolved_attrs.push((
                                name,
                                attr.resolve_single(self.0.name().pkg(), &resolution_ctx)?,
                            ));
                        }
                        let attributes = env
                            .heap()
                            .alloc_typed_unchecked(AllocStruct(resolved_attrs))
                            .cast();

                        let registry = AnalysisRegistry::new_from_owner(
                            BaseDeferredKey::AnonTarget(self.0.dupe()),
                            exec_resolution,
                        )?;

                        let ctx = AnalysisContext::prepare(
                            eval.heap(),
                            Some(attributes),
                            Some(self.0.configured_label()),
                            // FIXME(JakobDegen): There should probably be a way to pass plugins
                            // into anon targets
                            Some(
                                eval.heap()
                                    .alloc_typed(AnalysisPlugins::new(SmallMap::new()))
                                    .into(),
                            ),
                            registry,
                            dice.global_data().get_digest_config(),
                        );

                        let list_res = rule_impl.invoke(&mut eval, ctx)?;
                        Ok((dice, eval, ctx, list_res))
                    },
                )
                .await?;

                ctx.actions
                    .run_promises(dice, &mut eval, format!("anon_analysis$promises:{}", self))
                    .await?;
                let res_typed = ProviderCollection::try_from_value(list_res)?;
                let res = env.heap().alloc(res_typed);
                env.set("", res);

                let fulfilled_artifact_mappings = {
                    let promise_artifact_mappings =
                        rule_impl.promise_artifact_mappings(&mut eval)?;

                    self.get_fulfilled_promise_artifacts(promise_artifact_mappings, res, &mut eval)?
                };

                // Pull the ctx object back out, and steal ctx.action's state back
                let analysis_registry = ctx.take_state();
                std::mem::drop(eval);
                let num_declared_actions = analysis_registry.num_declared_actions();
                let num_declared_artifacts = analysis_registry.num_declared_artifacts();
                let (frozen_env, deferreds) = analysis_registry.finalize(&env)?(env)?;

                let res = frozen_env.get("").unwrap();
                let provider_collection = FrozenProviderCollectionValue::try_from_value(res)
                    .expect("just created this, this shouldn't happen");

                // this could look nicer if we had the entire analysis be a deferred
                let deferred = deferreds.take_result()?;
                Ok(AnalysisResult::new(
                    provider_collection,
                    deferred,
                    None,
                    fulfilled_artifact_mappings,
                    num_declared_actions,
                    num_declared_artifacts,
                ))
            }
            .map(|res| {
                let end = buck2_data::AnalysisEnd {
                    target: Some(self.0.as_proto().into()),
                    rule: self.0.rule_type().to_string(),
                    profile: None, // Not implemented for anon targets
                    declared_actions: res.as_ref().ok().map(|v| v.num_declared_actions),
                    declared_artifacts: res.as_ref().ok().map(|v| v.num_declared_artifacts),
                };
                (res, end)
            }),
        )
        .await
    }

    fn get_fulfilled_promise_artifacts<'v>(
        &self,
        promise_artifact_mappings: SmallMap<String, Value<'v>>,
        anon_target_result: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<HashMap<PromiseArtifactId, Artifact>> {
        let mut fulfilled_artifact_mappings = HashMap::new();

        for (id, func) in promise_artifact_mappings.values().enumerate() {
            let artifact = eval
                .eval_function(*func, &[anon_target_result], &[])
                .map_err(BuckStarlarkError::new)?;

            let promise_id =
                PromiseArtifactId::new(BaseDeferredKey::AnonTarget(self.0.clone()), id);

            match ValueAsArtifactLike::unpack_value(artifact).into_anyhow_result()? {
                Some(artifact) => {
                    fulfilled_artifact_mappings
                        .insert(promise_id.clone(), artifact.0.get_bound_artifact()?);
                }
                None => {
                    return Err(
                        PromiseArtifactResolveError::NotAnArtifact(artifact.to_repr()).into(),
                    );
                }
            }
        }

        Ok(fulfilled_artifact_mappings)
    }
}

/// Several attribute functions need a context, make one that is mostly useless.
pub(crate) struct AnonAttrCtx {
    pub(crate) execution_platform_resolution: ExecutionPlatformResolution,
}

impl AnonAttrCtx {
    fn new(execution_platform_resolution: &ExecutionPlatformResolution) -> Self {
        Self {
            execution_platform_resolution: execution_platform_resolution.clone(),
        }
    }

    pub(crate) fn intern_str(&self, value: &str) -> ArcStr {
        // TODO(scottcao): do intern.
        ArcStr::from(value)
    }
}

pub(crate) fn init_eval_anon_target() {
    EVAL_ANON_TARGET
        .init(|ctx, key| Box::pin(async move { AnonTargetKey::downcast(key)?.resolve(ctx).await }));
}

pub(crate) fn init_get_promised_artifact() {
    GET_PROMISED_ARTIFACT.init(|promise_artifact, ctx| {
        Box::pin(
            async move { get_artifact_from_anon_target_analysis(promise_artifact.id(), ctx).await },
        )
    });
}

pub(crate) async fn get_artifact_from_anon_target_analysis<'v>(
    promise_id: &'v PromiseArtifactId,
    ctx: &mut DiceComputations<'_>,
) -> anyhow::Result<Artifact> {
    let owner = promise_id.owner();
    let analysis_result = match owner {
        BaseDeferredKey::AnonTarget(anon_target) => {
            AnonTargetKey::downcast(anon_target.dupe())?
                .resolve(ctx)
                .await?
        }
        _ => {
            return Err(PromiseArtifactResolveError::OwnerIsNotAnonTarget(
                promise_id.clone(),
                owner.clone(),
            )
            .into());
        }
    };

    analysis_result
        .promise_artifact_map()
        .get(promise_id)
        .context(PromiseArtifactResolveError::NotFoundInAnalysis(
            promise_id.clone(),
        ))
        .cloned()
}

pub(crate) fn init_anon_target_registry_new() {
    ANON_TARGET_REGISTRY_NEW.init(|_phantom, execution_platform| {
        Box::new(AnonTargetsRegistry {
            execution_platform,
            promises: AnonPromises::default(),
            promise_artifact_registry: PromiseArtifactRegistry::new(),
        })
    });
}

impl<'v> AnonTargetsRegistry<'v> {
    pub(crate) fn downcast_mut(
        registry: &mut dyn AnonTargetsRegistryDyn<'v>,
    ) -> anyhow::Result<&'v mut AnonTargetsRegistry<'v>> {
        let registry: &mut AnonTargetsRegistry = registry
            .as_any_mut()
            .downcast_mut::<AnonTargetsRegistry>()
            .internal_error("AnonTargetsRegistryDyn is not an AnonTargetsRegistry")?;
        unsafe {
            // It is hard or impossible to express this safely with the borrow checker.
            // Has something to do with 'v being invariant.
            Ok(mem::transmute::<
                &mut AnonTargetsRegistry,
                &mut AnonTargetsRegistry,
            >(registry))
        }
    }

    pub(crate) fn anon_target_key(
        &self,
        rule: ValueTyped<'v, FrozenRuleCallable>,
        attributes: UnpackDictEntries<&'v str, Value<'v>>,
    ) -> anyhow::Result<AnonTargetKey> {
        AnonTargetKey::new(&self.execution_platform, rule, attributes)
    }

    pub(crate) fn register_one(
        &mut self,
        promise: ValueTyped<'v, StarlarkPromise<'v>>,
        key: AnonTargetKey,
    ) -> anyhow::Result<()> {
        self.promises.push_one(promise, key);

        Ok(())
    }

    pub(crate) fn register_artifact(
        &mut self,
        location: Option<FileSpan>,
        anon_target_key: AnonTargetKey,
        id: usize,
    ) -> anyhow::Result<PromiseArtifact> {
        let anon_target_key = BaseDeferredKey::AnonTarget(anon_target_key.0.dupe());
        let id = PromiseArtifactId::new(anon_target_key, id);
        self.promise_artifact_registry.register(location, id)
    }
}

impl<'v> AnonTargetsRegistryDyn<'v> for AnonTargetsRegistry<'v> {
    fn as_any_mut(&mut self) -> &mut dyn AnyLifetime<'v> {
        self
    }

    fn consumer_analysis_artifacts(&self) -> Vec<PromiseArtifact> {
        self.promise_artifact_registry.consumer_analysis_artifacts()
    }

    fn take_promises(&mut self) -> Option<Box<dyn AnonPromisesDyn<'v>>> {
        // We swap it out, so we can still collect new promises
        Some(mem::take(&mut self.promises))
            .filter(|p| !p.is_empty())
            .map(|p| Box::new(p) as Box<dyn AnonPromisesDyn>)
    }

    /*
    pub(crate) fn get_promises(&mut self) -> Option<AnonTargetsRegistry<'v>> {
        if self.entries.is_empty() {
            None
        } else {
            // We swap it out, so we can still collect new promises
            let mut new = AnonTargetsRegistry::new(self.execution_platform.dupe());
            mem::swap(&mut new, self);
            Some(new)
        }
    }
    */

    fn assert_no_promises(&self) -> anyhow::Result<()> {
        if self.promises.is_empty() {
            Ok(())
        } else {
            Err(AnonTargetsError::AssertNoPromisesFailed.into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anon_target_name() {
        assert_eq!(
            AnonTargetKey::parse_target_label("//foo:bar")
                .unwrap()
                .to_string(),
            "anon//foo:bar"
        );
        assert_eq!(
            AnonTargetKey::parse_target_label("cell//foo/bar:baz")
                .unwrap()
                .to_string(),
            "cell//foo/bar:baz"
        );
        assert!(AnonTargetKey::parse_target_label("foo").is_err());
        assert!(AnonTargetKey::parse_target_label("//foo:").is_err());
    }
}
