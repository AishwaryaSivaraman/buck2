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
use starlark::any::ProvidesStaticType;
use starlark::coerce::Coerce;
use starlark::environment::GlobalsBuilder;
use starlark::eval::Evaluator;
use starlark::values::list::AllocList;
use starlark::values::Freeze;
use starlark::values::Trace;
use starlark::values::Value;
use starlark::values::ValueLifetimeless;
use starlark::values::ValueLike;
use starlark::values::ValueOf;
use starlark::values::ValueOfUnchecked;
use starlark::values::ValueOfUncheckedGeneric;
use starlark::values::ValueTyped;
use starlark::values::ValueTypedComplex;

use crate::interpreter::rule_defs::cmd_args::FrozenStarlarkCmdArgs;
use crate::interpreter::rule_defs::cmd_args::StarlarkCmdArgs;
use crate::interpreter::rule_defs::provider::builtin::worker_info::FrozenWorkerInfo;
use crate::interpreter::rule_defs::provider::builtin::worker_info::WorkerInfo;

/// Provider that signals that a rule can run using a worker
#[internal_provider(worker_run_info_creator)]
#[derive(Clone, Debug, Coerce, Trace, Freeze, ProvidesStaticType, Allocative)]
#[repr(C)]
pub struct WorkerRunInfoGen<V: ValueLifetimeless> {
    // Configuration needed to spawn a new worker
    worker: ValueOfUncheckedGeneric<V, FrozenWorkerInfo>,

    // Command to execute without spawning a worker, when the build environment or configuration does not support workers
    exe: ValueOfUncheckedGeneric<V, FrozenStarlarkCmdArgs>,
}

#[starlark_module]
fn worker_run_info_creator(globals: &mut GlobalsBuilder) {
    #[starlark(as_type = FrozenWorkerRunInfo)]
    fn WorkerRunInfo<'v>(
        #[starlark(require = named)] worker: ValueOf<'v, &'v WorkerInfo<'v>>,
        #[starlark(require = named, default = AllocList::EMPTY)] exe: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<WorkerRunInfo<'v>> {
        let heap = eval.heap();
        let valid_exe = StarlarkCmdArgs::try_from_value(exe)?;
        Ok(WorkerRunInfo {
            worker: ValueOfUnchecked::new(worker.to_value()),
            exe: ValueOfUnchecked::new(heap.alloc(valid_exe)),
        })
    }
}

impl<'v, V: ValueLike<'v>> WorkerRunInfoGen<V> {
    pub fn worker(&self) -> ValueTypedComplex<'v, WorkerInfo<'v>> {
        ValueTypedComplex::new(self.worker.get().to_value()).expect("validated at construction")
    }

    pub fn exe(&self) -> ValueTyped<'v, StarlarkCmdArgs<'v>> {
        ValueTyped::new_err(self.exe.get().to_value()).expect("validated at construction")
    }
}
