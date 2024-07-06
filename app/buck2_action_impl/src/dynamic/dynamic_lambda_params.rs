/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

use allocative::Allocative;
use buck2_build_api::interpreter::rule_defs::plugins::AnalysisPlugins;
use buck2_build_api::interpreter::rule_defs::plugins::FrozenAnalysisPlugins;
use buck2_error::BuckErrorContext;
use gazebo::prelude::OptionExt;
use starlark::any::ProvidesStaticType;
use starlark::values::structs::StructRef;
use starlark::values::typing::FrozenStarlarkCallable;
use starlark::values::typing::StarlarkCallable;
use starlark::values::Freeze;
use starlark::values::Freezer;
use starlark::values::FrozenValue;
use starlark::values::FrozenValueOfUnchecked;
use starlark::values::FrozenValueTyped;
use starlark::values::Trace;
use starlark::values::Value;
use starlark::values::ValueOfUnchecked;
use starlark::values::ValueTypedComplex;

#[derive(Allocative, Trace, Debug, ProvidesStaticType)]
pub(crate) struct DynamicLambdaParams<'v> {
    pub(crate) attributes: Option<ValueOfUnchecked<'v, StructRef<'static>>>,
    pub(crate) plugins: Option<ValueTypedComplex<'v, AnalysisPlugins<'v>>>,
    pub(crate) lambda: StarlarkCallable<'v>,
    pub(crate) arg: Option<Value<'v>>,
}

#[derive(Allocative, Debug, ProvidesStaticType)]
pub struct FrozenDynamicLambdaParams {
    pub(crate) attributes: Option<FrozenValueOfUnchecked<'static, StructRef<'static>>>,
    pub(crate) plugins: Option<FrozenValueTyped<'static, FrozenAnalysisPlugins>>,
    pub lambda: FrozenStarlarkCallable,
    pub arg: Option<FrozenValue>,
}

impl FrozenDynamicLambdaParams {
    pub(crate) fn attributes<'v>(
        &'v self,
    ) -> anyhow::Result<Option<ValueOfUnchecked<'v, StructRef<'static>>>> {
        let Some(attributes) = self.attributes else {
            return Ok(None);
        };
        Ok(Some(attributes.to_value().cast()))
    }

    pub(crate) fn plugins<'v>(
        &'v self,
    ) -> anyhow::Result<Option<ValueTypedComplex<'v, AnalysisPlugins<'v>>>> {
        let Some(plugins) = self.plugins else {
            return Ok(None);
        };
        Ok(Some(
            ValueTypedComplex::new(plugins.to_value())
                .internal_error("plugins must be AnalysisPlugins")?,
        ))
    }

    pub fn lambda<'v>(&'v self) -> Value<'v> {
        self.lambda.0.to_value()
    }

    pub fn arg<'v>(&'v self) -> Option<Value<'v>> {
        self.arg.map(|v| v.to_value())
    }
}

impl<'v> Freeze for DynamicLambdaParams<'v> {
    type Frozen = FrozenDynamicLambdaParams;

    fn freeze(self, freezer: &Freezer) -> anyhow::Result<Self::Frozen> {
        Ok(FrozenDynamicLambdaParams {
            attributes: self
                .attributes
                .try_map(|a| anyhow::Ok(a.freeze(freezer)?.cast()))?,
            plugins: self.plugins.freeze(freezer)?,
            lambda: self.lambda.freeze(freezer)?,
            arg: self.arg.freeze(freezer)?,
        })
    }
}
