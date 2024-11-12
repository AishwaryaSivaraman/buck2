/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

use std::fmt::Debug;

use allocative::Allocative;
use buck2_build_api_derive::internal_provider;
use buck2_core::configuration::data::ConfigurationData;
use starlark::any::ProvidesStaticType;
use starlark::coerce::Coerce;
use starlark::environment::GlobalsBuilder;
use starlark::values::Freeze;
use starlark::values::Heap;
use starlark::values::StringValue;
use starlark::values::Trace;
use starlark::values::ValueLifetimeless;
use starlark::values::ValueLike;
use starlark::values::ValueOf;
use starlark::values::ValueOfUnchecked;
use starlark::values::ValueOfUncheckedGeneric;

use crate::interpreter::rule_defs::provider::builtin::configuration_info::ConfigurationInfo;
use crate::interpreter::rule_defs::provider::builtin::configuration_info::FrozenConfigurationInfo;

#[internal_provider(platform_info_creator)]
#[derive(Clone, Debug, Trace, Coerce, Freeze, ProvidesStaticType, Allocative)]
#[repr(C)]
pub struct PlatformInfoGen<V: ValueLifetimeless> {
    label: ValueOfUncheckedGeneric<V, String>,
    configuration: ValueOfUncheckedGeneric<V, FrozenConfigurationInfo>,
}

impl<'v, V: ValueLike<'v>> PlatformInfoGen<V> {
    pub fn to_configuration(&self) -> anyhow::Result<ConfigurationData> {
        Ok(ConfigurationData::from_platform(
            self.label
                .to_value()
                .get()
                .unpack_str()
                .expect("type checked during construction")
                .to_owned(),
            ConfigurationInfo::from_value(self.configuration.get().to_value())
                .expect("type checked during construction")
                .to_configuration_data()?,
        )?)
    }
}

impl<'v> PlatformInfo<'v> {
    pub fn from_configuration(
        cfg: &ConfigurationData,
        heap: &'v Heap,
    ) -> anyhow::Result<PlatformInfo<'v>> {
        let label = heap.alloc_str(cfg.label()?);
        let configuration = heap.alloc(ConfigurationInfo::from_configuration_data(
            cfg.data()?,
            heap,
        ));
        Ok(PlatformInfoGen {
            label: label.to_value_of_unchecked().cast(),
            configuration: ValueOfUnchecked::<FrozenConfigurationInfo>::new(configuration),
        })
    }
}

#[starlark_module]
fn platform_info_creator(globals: &mut GlobalsBuilder) {
    #[starlark(as_type = FrozenPlatformInfo)]
    fn PlatformInfo<'v>(
        #[starlark(require = named)] label: StringValue<'v>,
        #[starlark(require = named)] configuration: ValueOf<'v, &'v ConfigurationInfo<'v>>,
    ) -> anyhow::Result<PlatformInfo<'v>> {
        Ok(PlatformInfo {
            label: label.to_value_of_unchecked().cast(),
            configuration: ValueOfUnchecked::<FrozenConfigurationInfo>::new(configuration.value),
        })
    }
}

#[cfg(test)]
mod tests {
    use buck2_node::attrs::internal::internal_attrs_platform_info_provider_id;

    use crate::interpreter::rule_defs::provider::builtin::platform_info::PlatformInfoCallable;

    #[test]
    fn test_platform_info_provider_id_in_internal_attrs_correct() {
        assert_eq!(
            internal_attrs_platform_info_provider_id(),
            PlatformInfoCallable::provider_id()
        );
    }
}
