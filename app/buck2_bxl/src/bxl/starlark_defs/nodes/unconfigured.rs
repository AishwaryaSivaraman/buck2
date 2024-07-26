/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

use std::convert::Infallible;

use allocative::Allocative;
use buck2_interpreter::types::target_label::StarlarkTargetLabel;
use buck2_node::attrs::inspect_options::AttrInspectOptions;
use buck2_node::nodes::unconfigured::TargetNode;
use derive_more::Display;
use dupe::Dupe;
use starlark::any::ProvidesStaticType;
use starlark::environment::Methods;
use starlark::environment::MethodsBuilder;
use starlark::environment::MethodsStatic;
use starlark::starlark_module;
use starlark::starlark_simple_value;
use starlark::values::starlark_value;
use starlark::values::structs::AllocStruct;
use starlark::values::Heap;
use starlark::values::NoSerialize;
use starlark::values::StarlarkValue;
use starlark::values::StringValue;
use starlark::values::UnpackValue;
use starlark::values::Value;
use starlark::values::ValueLike;
use starlark::StarlarkDocs;

use crate::bxl::starlark_defs::file_set::StarlarkFileNode;
use crate::bxl::starlark_defs::nodes::unconfigured::attribute::StarlarkCoercedAttr;

pub(crate) mod attribute;

#[derive(Debug, Display, ProvidesStaticType, Allocative, StarlarkDocs)]
#[derive(NoSerialize)] // TODO probably should be serializable the same as how queries serialize
#[display(fmt = "{:?}", self)]
#[starlark_docs(directory = "bxl")]
pub(crate) struct StarlarkTargetNode(pub(crate) TargetNode);

starlark_simple_value!(StarlarkTargetNode);

#[starlark_value(type = "unconfigured_target_node")]
impl<'v> StarlarkValue<'v> for StarlarkTargetNode {
    fn get_methods() -> Option<&'static Methods> {
        static RES: MethodsStatic = MethodsStatic::new();
        RES.methods(target_node_value_methods)
    }
}

impl<'a> UnpackValue<'a> for StarlarkTargetNode {
    type Error = Infallible;

    fn unpack_value_impl(value: Value<'a>) -> Result<Option<Self>, Self::Error> {
        Ok(value
            .downcast_ref::<Self>()
            .map(|value| Self(value.0.dupe())))
    }
}

/// Methods for unconfigured target node.
#[starlark_module]
fn target_node_value_methods(builder: &mut MethodsBuilder) {
    /// Gets the coerced attributes from the unconfigured target node. Returns a struct.
    ///
    /// Sample usage:
    /// ```text
    /// def _impl_attributes(ctx):
    ///     target_node = ctx.uquery().eval("owner('path/to/file')")[0]
    ///     ctx.output.print(target_node.attrs.my_attr)
    /// ```
    #[starlark(attribute)]
    fn attrs<'v>(this: StarlarkTargetNode, heap: &Heap) -> anyhow::Result<Value<'v>> {
        let attrs_iter = this.0.attrs(AttrInspectOptions::All);
        let special_attrs_iter = this.0.special_attrs();
        let attrs = attrs_iter
            .map(|a| {
                (
                    a.name,
                    StarlarkCoercedAttr(a.value.clone(), this.0.label().pkg().dupe()),
                )
            })
            .chain(special_attrs_iter.map(|(name, attr)| {
                (name, StarlarkCoercedAttr(attr, this.0.label().pkg().dupe()))
            }));

        Ok(heap.alloc(AllocStruct(attrs)))
    }

    /// Gets the label from the unconfigured target node.
    ///
    /// Sample usage:
    /// ```text
    /// def _impl_label(ctx):
    ///     target_node = ctx.uquery().eval("owner('path/to/file')")[0]
    ///     ctx.output.print(target_node.label)
    /// ```
    #[starlark(attribute)]
    fn label(this: &StarlarkTargetNode) -> anyhow::Result<StarlarkTargetLabel> {
        Ok(this.0.label().dupe().into())
    }

    /// Gets the buildfile path from the unconfigured target node.
    ///
    /// Sample usage:
    /// ```text
    /// def _impl_label(ctx):
    ///     target_node = ctx.uquery().eval("owner('path/to/file')")[0]
    ///     ctx.output.print(target_node.buildfile_path)
    /// ```
    #[starlark(attribute)]
    fn buildfile_path(this: &StarlarkTargetNode) -> anyhow::Result<StarlarkFileNode> {
        Ok(StarlarkFileNode(this.0.buildfile_path().path()))
    }

    /// Gets the fully qualified name of the rule for this unconfigured target node as a
    /// string. This includes the import path as well.
    ///
    /// Sample usage:
    /// ```text
    /// def _impl_rule_type(ctx):
    ///     target_node = ctx.uquery().owner('path/to/file')[0]
    ///     ctx.output.print(target_node.rule_type)
    /// ```
    #[starlark(attribute)]
    fn rule_type<'v>(
        this: &'v StarlarkTargetNode,
        heap: &'v Heap,
    ) -> anyhow::Result<StringValue<'v>> {
        Ok(heap.alloc_str_intern(this.0.rule_type().to_string().as_str()))
    }

    /// Gets the targets' corresponding rule's kind which is one of
    ///  - normal (with no special properties)
    ///  - configured (usable in a configuration context)
    ///  - toolchain (only usable as a toolchain dep)
    ///
    /// Sample usage:
    /// ```text
    /// def _impl_rule_kind(ctx):
    ///     target_node = ctx.uquery().owner('path/to/file')[0]
    ///     ctx.output.print(target_node.rule_kind)
    /// ```
    #[starlark(attribute)]
    fn rule_kind<'v>(
        this: &'v StarlarkTargetNode,
        heap: &'v Heap,
    ) -> anyhow::Result<StringValue<'v>> {
        Ok(heap.alloc_str_intern(this.0.rule_kind().as_str()))
    }
}
