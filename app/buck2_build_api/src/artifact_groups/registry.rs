/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

use allocative::Allocative;
use dupe::Dupe;
use starlark::eval::Evaluator;
use starlark::values::FrozenValueTyped;
use starlark::values::Value;
use starlark::values::ValueTyped;

use crate::analysis::registry::AnalysisValueFetcher;
use crate::analysis::registry::AnalysisValueStorage;
use crate::deferred::types::DeferredRegistry;
use crate::interpreter::rule_defs::transitive_set::FrozenTransitiveSetDefinition;
use crate::interpreter::rule_defs::transitive_set::TransitiveSet;

#[derive(Allocative)]
pub struct ArtifactGroupRegistry;

impl ArtifactGroupRegistry {
    pub fn new() -> Self {
        Self
    }

    pub(crate) fn create_transitive_set<'v>(
        &mut self,
        definition: FrozenValueTyped<'v, FrozenTransitiveSetDefinition>,
        value: Option<Value<'v>>,
        children: Option<Value<'v>>,
        deferred: &mut DeferredRegistry,
        analysis_value_storage: &mut AnalysisValueStorage<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<ValueTyped<'v, TransitiveSet<'v>>> {
        Ok(
            analysis_value_storage.register_transitive_set(deferred.key().dupe(), move |key| {
                let set =
                    TransitiveSet::new_from_values(key.dupe(), definition, value, children, eval)
                        .map_err(|e| e.into_anyhow())?;
                Ok(eval.heap().alloc_typed(set))
            })?,
        )
    }

    pub(crate) fn ensure_bound(
        self,
        _registry: &mut DeferredRegistry,
        _analysis_value_fetcher: &AnalysisValueFetcher,
    ) -> anyhow::Result<()> {
        Ok(())
    }
}
