/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

use buck2_node::attrs::attr_type::source::SourceAttrType;
use buck2_node::attrs::coerced_attr::CoercedAttr;
use buck2_node::attrs::coercion_context::AttrCoercionContext;
use buck2_node::attrs::configurable::AttrIsConfigurable;
use gazebo::prelude::*;
use starlark::typing::Ty;
use starlark::values::string::STRING_TYPE;
use starlark::values::Value;

use crate::attrs::coerce::attr_type::ty_maybe_select::TyMaybeSelect;
use crate::attrs::coerce::error::CoercionError;
use crate::attrs::coerce::AttrTypeCoerce;

#[derive(Debug, buck2_error::Error)]
#[buck2(input)]
enum SourceLabelCoercionError {
    #[error(
        "Couldn't coerce `{0}` as a source.\n  Error when treated as a target: {1:#}\n  Error when treated as a path: {2:#}"
    )]
    CoercionFailed(String, anyhow::Error, anyhow::Error),
}

/// Try cleaning up irrelevant details users often type
fn cleanup_path(value: &str) -> &str {
    let value = value.trim_start_match("./");
    let value = value.trim_end_match("/");
    if value == "." { "" } else { value }
}

impl AttrTypeCoerce for SourceAttrType {
    fn coerce_item(
        &self,
        _configurable: AttrIsConfigurable,
        ctx: &dyn AttrCoercionContext,
        value: Value,
    ) -> anyhow::Result<CoercedAttr> {
        let source_label = value
            .unpack_str()
            .ok_or_else(|| anyhow::anyhow!(CoercionError::type_error(STRING_TYPE, value)))?;
        // FIXME(JakobDegen): We should not be recovering from an `Err` here. Two reasons:
        // 1. This codepath is at least one of the reasons that running buck with `RUST_BACKTRACE=1`
        //    is slow, since producing an anyhow error is quite expensive.
        // 2. For source attrs, we should have simpler rules for whether a string is interpreted as
        //    a label or as a path than whether or not this errors. This can error for all kinds of
        //    reasons
        match ctx.coerce_providers_label(source_label) {
            Ok(label) => Ok(CoercedAttr::SourceLabel(label)),
            Err(label_err) => {
                match ctx.coerce_path(cleanup_path(source_label), self.allow_directory) {
                    Ok(path) => Ok(CoercedAttr::SourceFile(path)),
                    Err(path_err) => Err(SourceLabelCoercionError::CoercionFailed(
                        value.to_str(),
                        label_err,
                        path_err,
                    )
                    .into()),
                }
            }
        }
    }

    fn starlark_type(&self) -> TyMaybeSelect {
        TyMaybeSelect::Basic(Ty::string())
    }
}
