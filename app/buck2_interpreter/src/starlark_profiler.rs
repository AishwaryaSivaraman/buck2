/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

use std::cmp;
use std::time::Duration;
use std::time::Instant;

use allocative::Allocative;
use anyhow::Context;
use buck2_error::internal_error;
use buck2_error::BuckErrorContext;
use dupe::Dupe;
use starlark::environment::FrozenModule;
use starlark::eval::Evaluator;
use starlark::eval::ProfileData;
use starlark::eval::ProfileMode;
use starlark::StarlarkResultExt;

#[derive(Debug, buck2_error::Error)]
enum StarlarkProfilerError {
    #[error(
        "Retained memory profiling is available only for analysis profile \
        or bxl profile (which freezes the module)"
    )]
    RetainedMemoryNotFrozen,
}

/// When profiling Starlark file, all dependencies of that file must be
/// "instrumented" otherwise the profiler won't work.
///
/// This struct defines instrumentation level for the module.
#[derive(Debug, PartialEq, Eq, Clone, Dupe, Allocative)]
pub struct StarlarkProfilerInstrumentation {}

impl StarlarkProfilerInstrumentation {
    pub fn new() -> Self {
        Self {}
    }
}

#[derive(Debug, Allocative)]
pub struct StarlarkProfileDataAndStats {
    profile_mode: ProfileMode,
    #[allocative(skip)] // OK to skip because used only when profiling enabled.
    pub profile_data: ProfileData,
    initialized_at: Instant,
    finalized_at: Instant,
    total_retained_bytes: usize,
}

impl StarlarkProfileDataAndStats {
    pub fn elapsed(&self) -> Duration {
        self.finalized_at.duration_since(self.initialized_at)
    }

    pub fn total_retained_bytes(&self) -> usize {
        self.total_retained_bytes
    }

    pub fn merge<'a>(
        datas: impl IntoIterator<Item = &'a StarlarkProfileDataAndStats>,
    ) -> anyhow::Result<StarlarkProfileDataAndStats> {
        let datas = Vec::from_iter(datas);
        let mut iter = datas.iter().copied();
        let first = iter.next().context("empty collection of profile data")?;
        let profile_mode = first.profile_mode.dupe();
        let mut total_retained_bytes = first.total_retained_bytes;
        let mut initialized_at = first.initialized_at;
        let mut finalized_at = first.finalized_at;

        for data in iter {
            if data.profile_mode != profile_mode {
                return Err(internal_error!("profile mode are inconsistent"));
            }
            initialized_at = cmp::min(initialized_at, data.initialized_at);
            finalized_at = cmp::max(finalized_at, data.finalized_at);
            total_retained_bytes += data.total_retained_bytes;
        }

        let profile_data =
            ProfileData::merge(datas.iter().map(|data| &data.profile_data)).into_anyhow_result()?;

        Ok(StarlarkProfileDataAndStats {
            profile_mode,
            profile_data,
            initialized_at,
            finalized_at,
            total_retained_bytes,
        })
    }
}

pub struct StarlarkProfiler {
    profile_mode: ProfileMode,
    /// Evaluation will freeze the module.
    /// (And frozen module will be passed to `visit_frozen_module`).
    will_freeze: bool,

    initialized_at: Option<Instant>,
    finalized_at: Option<Instant>,
    profile_data: Option<ProfileData>,
    total_retained_bytes: Option<usize>,
}

impl StarlarkProfiler {
    pub fn new(profile_mode: ProfileMode, will_freeze: bool) -> StarlarkProfiler {
        Self {
            profile_mode,
            will_freeze,
            initialized_at: None,
            finalized_at: None,
            profile_data: None,
            total_retained_bytes: None,
        }
    }

    /// Collect all profiling data.
    pub fn finish(self) -> anyhow::Result<StarlarkProfileDataAndStats> {
        Ok(StarlarkProfileDataAndStats {
            profile_mode: self.profile_mode,
            initialized_at: self.initialized_at.internal_error("did not initialize")?,
            finalized_at: self.finalized_at.internal_error("did not finalize")?,
            total_retained_bytes: self
                .total_retained_bytes
                .internal_error("did not visit heap")?,
            profile_data: self
                .profile_data
                .internal_error("profile_data not initialized")?,
        })
    }

    /// Instrumentation level required by `bzl` files loaded by the profiled module.
    fn instrumentation(&self) -> Option<StarlarkProfilerInstrumentation> {
        Some(StarlarkProfilerInstrumentation {})
    }

    /// Prepare an Evaluator to capture output relevant to this profiler.
    fn initialize(&mut self, eval: &mut Evaluator) -> anyhow::Result<()> {
        eval.enable_profile(&self.profile_mode)?;
        self.initialized_at = Some(Instant::now());
        Ok(())
    }

    /// Post-analysis, produce the output of this profiler.
    fn evaluation_complete(&mut self, eval: &mut Evaluator) -> anyhow::Result<()> {
        self.finalized_at = Some(Instant::now());
        if !self.profile_mode.requires_frozen_module() {
            self.profile_data = Some(eval.gen_profile().into_anyhow_result()?);
        }
        Ok(())
    }

    fn visit_frozen_module(&mut self, module: Option<&FrozenModule>) -> anyhow::Result<()> {
        if self.will_freeze != module.is_some() {
            return Err(internal_error!(
                "will_freeze field was initialized incorrectly"
            ));
        }

        if self.profile_mode.requires_frozen_module() {
            let module = module.ok_or(StarlarkProfilerError::RetainedMemoryNotFrozen)?;
            let profile = module.heap_profile()?;
            self.profile_data = Some(profile);
        }

        let total_retained_bytes = module.map_or(0, |module| {
            module
                .frozen_heap()
                .allocated_summary()
                .total_allocated_bytes()
        });

        self.total_retained_bytes = Some(total_retained_bytes);

        Ok(())
    }
}

/// How individual starlark invocation (`bzl`, `BUCK` or analysis) should be interpreted.
#[derive(Clone, Dupe, Eq, PartialEq, Allocative)]
pub enum StarlarkProfileModeOrInstrumentation {
    None,
    Profile(ProfileMode),
}

impl StarlarkProfileModeOrInstrumentation {
    pub fn profile_mode(&self) -> Option<&ProfileMode> {
        match self {
            StarlarkProfileModeOrInstrumentation::Profile(profile) => Some(profile),
            StarlarkProfileModeOrInstrumentation::None => None,
        }
    }
}

enum StarlarkProfilerOrInstrumentationImpl<'p> {
    None,
    Profiler(&'p mut StarlarkProfiler),
}

/// Modules can be evaluated with profiling or with instrumentation for profiling.
/// This type enapsulates this logic.
pub struct StarlarkProfilerOrInstrumentation<'p>(StarlarkProfilerOrInstrumentationImpl<'p>);

impl<'p> StarlarkProfilerOrInstrumentation<'p> {
    pub fn new(
        profiler: &'p mut StarlarkProfiler,
        instrumentation: Option<StarlarkProfilerInstrumentation>,
    ) -> StarlarkProfilerOrInstrumentation<'p> {
        match (profiler.instrumentation(), instrumentation) {
            (None, None) => StarlarkProfilerOrInstrumentation::disabled(),
            (Some(_), Some(_)) => StarlarkProfilerOrInstrumentation::for_profiler(profiler),
            (None, Some(_)) => StarlarkProfilerOrInstrumentation::disabled(),
            (Some(_), None) => panic!("profiler, but no instrumentation"),
        }
    }

    pub fn for_profiler(profiler: &'p mut StarlarkProfiler) -> Self {
        StarlarkProfilerOrInstrumentation(StarlarkProfilerOrInstrumentationImpl::Profiler(profiler))
    }

    /// No profiling.
    pub fn disabled() -> StarlarkProfilerOrInstrumentation<'p> {
        StarlarkProfilerOrInstrumentation(StarlarkProfilerOrInstrumentationImpl::None)
    }

    pub fn initialize(&mut self, eval: &mut Evaluator) -> anyhow::Result<bool> {
        match &mut self.0 {
            StarlarkProfilerOrInstrumentationImpl::None => Ok(false),
            StarlarkProfilerOrInstrumentationImpl::Profiler(profiler) => {
                profiler.initialize(eval).map(|_| true)
            }
        }
    }

    pub fn visit_frozen_module(&mut self, module: Option<&FrozenModule>) -> anyhow::Result<()> {
        if let StarlarkProfilerOrInstrumentationImpl::Profiler(profiler) = &mut self.0 {
            profiler.visit_frozen_module(module)
        } else {
            Ok(())
        }
    }

    pub fn evaluation_complete(&mut self, eval: &mut Evaluator) -> anyhow::Result<()> {
        if let StarlarkProfilerOrInstrumentationImpl::Profiler(profiler) = &mut self.0 {
            profiler.evaluation_complete(eval)
        } else {
            Ok(())
        }
    }
}
