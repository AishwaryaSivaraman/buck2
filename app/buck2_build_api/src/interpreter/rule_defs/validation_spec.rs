/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

use std::fmt::Display;
use std::fmt::Formatter;

use allocative::Allocative;
use anyhow::Context;
use starlark::any::ProvidesStaticType;
use starlark::coerce::Coerce;
use starlark::environment::GlobalsBuilder;
use starlark::values::starlark_value;
use starlark::values::Freeze;
use starlark::values::NoSerialize;
use starlark::values::StarlarkValue;
use starlark::values::StringValue;
use starlark::values::Trace;
use starlark::values::ValueLifetimeless;
use starlark::values::ValueLike;
use starlark::values::ValueOf;
use starlark::values::ValueOfUncheckedGeneric;
use starlark::StarlarkResultExt;

use crate::interpreter::rule_defs::artifact::starlark_artifact_like::StarlarkArtifactLike;
use crate::interpreter::rule_defs::artifact::starlark_artifact_like::ValueAsArtifactLike;

#[derive(Debug, thiserror::Error)]
enum ValidationSpecError {
    #[error("Name of validation spec should not be empty")]
    EmptyName,
    #[error("Validation result artifact should be a build artifact, not a source one.")]
    ValidationResultIsSourceArtifact,
    #[error("Validation result artifact should be bound.")]
    ValidationResultIsNotBound,
}

/// Value describing a single identifiable validation.
/// Validation is represented by a build artifact with defined structure.
/// Content of such artifact determines if validation is successful or not.
/// A collection of such objects forms a `ValidationInfo` provider
/// which describes how a given target should be validated.
#[derive(
    Debug,
    Trace,
    NoSerialize,
    Coerce,
    ProvidesStaticType,
    Allocative,
    Freeze
)]
#[freeze(validator = validate_validation_spec, bounds = "V: ValueLike<'freeze>")]
#[repr(C)]
pub struct StarlarkValidationSpecGen<V: ValueLifetimeless> {
    /// Name used to identify validation. Should be unique per target node.
    name: ValueOfUncheckedGeneric<V, String>,
    /// Build artifact which is the result of running a validation.
    /// Should contain JSON of defined schema setting API between Buck2 and user-created validators/scripts.
    validation_result: ValueOfUncheckedGeneric<V, ValueAsArtifactLike<'static>>,
}

starlark_complex_value!(pub(crate) StarlarkValidationSpec);

impl<'v, V: ValueLike<'v>> StarlarkValidationSpecGen<V> {
    pub fn name(&self) -> &'v str {
        self.name
            .cast::<&str>()
            .unpack()
            .expect("type checked during construction or freezing")
    }

    pub fn validation_result(&self) -> &'v dyn StarlarkArtifactLike {
        self.validation_result
            .unpack()
            .expect("type checked during construction or freezing")
            .0
    }
}

impl<'v, V: ValueLike<'v>> Display for StarlarkValidationSpecGen<V>
where
    Self: ProvidesStaticType<'v>,
{
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "ValidationSpec(name={}, validation_result=", self.name)?;
        Display::fmt(&self.validation_result, f)?;
        write!(f, ")")
    }
}

fn validate_validation_spec<'v, V>(spec: &StarlarkValidationSpecGen<V>) -> anyhow::Result<()>
where
    V: ValueLike<'v>,
{
    let name = spec.name.unpack().into_anyhow_result()?;
    if name.is_empty() {
        return Err(ValidationSpecError::EmptyName.into());
    }
    let artifact = spec.validation_result.unpack().into_anyhow_result()?;
    let artifact = match artifact.0.get_bound_artifact() {
        Ok(bound_artifact) => bound_artifact,
        Err(e) => {
            return Err(e).context(ValidationSpecError::ValidationResultIsNotBound);
        }
    };
    if artifact.is_source() {
        return Err(ValidationSpecError::ValidationResultIsSourceArtifact.into());
    }
    Ok(())
}

#[starlark_value(type = "ValidationSpec")]
impl<'v, V: ValueLike<'v>> StarlarkValue<'v> for StarlarkValidationSpecGen<V> where
    Self: ProvidesStaticType<'v>
{
}

#[starlark_module]
pub fn register_validation_spec(builder: &mut GlobalsBuilder) {
    #[starlark(as_type = FrozenStarlarkValidationSpec)]
    fn ValidationSpec<'v>(
        #[starlark(require = named)] name: StringValue<'v>,
        #[starlark(require = named)] validation_result: ValueOf<'v, ValueAsArtifactLike<'v>>,
    ) -> anyhow::Result<StarlarkValidationSpec<'v>> {
        let result = StarlarkValidationSpec {
            name: name.to_value_of_unchecked().cast(),
            validation_result: validation_result.as_unchecked().cast(),
        };
        validate_validation_spec(&result)?;
        Ok(result)
    }
}
