/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

use std::any::Any;
use std::future::Future;
use std::ops::Deref;
use std::sync::Arc;

use allocative::Allocative;
use buck2_futures::owning_future::OwningFuture;
use derivative::Derivative;
use dupe::Dupe;
use futures::future::BoxFuture;
use futures::FutureExt;
use futures::TryFutureExt;
use parking_lot::Mutex;

use crate::api::activation_tracker::ActivationData;
use crate::api::computations::DiceComputations;
use crate::api::data::DiceData;
use crate::api::error::DiceResult;
use crate::api::key::Key;
use crate::api::projection::ProjectionKey;
use crate::api::user_data::UserComputationData;
use crate::ctx::DiceComputationsImpl;
use crate::ctx::LinearRecomputeDiceComputationsImpl;
use crate::impls::cache::DiceTaskRef;
use crate::impls::cache::SharedCache;
use crate::impls::core::state::CoreStateHandle;
use crate::impls::core::versions::VersionEpoch;
use crate::impls::dep_trackers::RecordingDepsTracker;
use crate::impls::dice::DiceModern;
use crate::impls::evaluator::AsyncEvaluator;
use crate::impls::evaluator::SyncEvaluator;
use crate::impls::events::DiceEventDispatcher;
use crate::impls::incremental::IncrementalEngine;
use crate::impls::key::CowDiceKeyHashed;
use crate::impls::key::DiceKey;
use crate::impls::key::ParentKey;
use crate::impls::opaque::OpaqueValueModern;
use crate::impls::task::dice::MaybeCancelled;
use crate::impls::task::promise::DicePromise;
use crate::impls::task::sync_dice_task;
use crate::impls::task::PreviouslyCancelledTask;
use crate::impls::transaction::ActiveTransactionGuard;
use crate::impls::transaction::TransactionUpdater;
use crate::impls::user_cycle::KeyComputingUserCycleDetectorData;
use crate::impls::user_cycle::UserCycleDetectorData;
use crate::impls::value::DiceComputedValue;
use crate::impls::value::DiceValidity;
use crate::impls::value::MaybeValidDiceValue;
use crate::result::CancellableResult;
use crate::result::Cancelled;
use crate::transaction_update::DiceTransactionUpdaterImpl;
use crate::versions::VersionNumber;
use crate::DiceError;
use crate::DiceTransactionUpdater;
use crate::HashSet;
use crate::LinearRecomputeDiceComputations;
use crate::UserCycleDetectorGuard;

/// Context that is the base for which all requests start from
#[derive(Allocative)]
pub(crate) struct BaseComputeCtx {
    // we need to give off references of `DiceComputation` so hold this for now, but really once we
    // get rid of the enum, we just hold onto the base data directly and do some ref casts
    data: DiceComputations<'static>,
    live_version_guard: ActiveTransactionGuard,
}

impl Clone for BaseComputeCtx {
    fn clone(&self) -> Self {
        match &self.data.0 {
            DiceComputationsImpl::Legacy(_) => {
                unreachable!("wrong dice")
            }
            DiceComputationsImpl::Modern(modern) => {
                BaseComputeCtx::clone_for(modern, self.live_version_guard.dupe())
            }
        }
    }
}

impl Dupe for BaseComputeCtx {}

impl BaseComputeCtx {
    pub(crate) fn new(
        per_live_version_ctx: SharedLiveTransactionCtx,
        user_data: Arc<UserComputationData>,
        dice: Arc<DiceModern>,
        live_version_guard: ActiveTransactionGuard,
    ) -> Self {
        Self {
            data: DiceComputations(DiceComputationsImpl::Modern(ModernComputeCtx::new(
                ParentKey::None,
                KeyComputingUserCycleDetectorData::Untracked,
                AsyncEvaluator {
                    per_live_version_ctx,
                    user_data,
                    dice,
                },
            ))),
            live_version_guard,
        }
    }

    fn clone_for(
        modern: &ModernComputeCtx<'_>,
        live_version_guard: ActiveTransactionGuard,
    ) -> BaseComputeCtx {
        Self {
            data: DiceComputations(DiceComputationsImpl::Modern(ModernComputeCtx::new(
                ParentKey::None,
                KeyComputingUserCycleDetectorData::Untracked,
                modern.ctx_data().async_evaluator.clone(),
            ))),
            live_version_guard,
        }
    }

    pub(crate) fn get_version(&self) -> VersionNumber {
        self.data.0.get_version()
    }

    pub(crate) fn into_updater(self) -> DiceTransactionUpdater {
        DiceTransactionUpdater(match self.data.0 {
            DiceComputationsImpl::Legacy(_) => unreachable!("modern dice"),
            DiceComputationsImpl::Modern(delegate) => {
                DiceTransactionUpdaterImpl::Modern(delegate.into_updater())
            }
        })
    }

    pub(crate) fn as_computations(&self) -> &DiceComputations<'static> {
        &self.data
    }

    pub(crate) fn as_computations_mut(&mut self) -> &mut DiceComputations<'static> {
        &mut self.data
    }
}

impl Deref for BaseComputeCtx {
    type Target = ModernComputeCtx<'static>;

    fn deref(&self) -> &Self::Target {
        match &self.data.0 {
            DiceComputationsImpl::Legacy(_) => {
                unreachable!("legacy dice instead of modern")
            }
            DiceComputationsImpl::Modern(ctx) => ctx,
        }
    }
}

impl<'d> ModernComputeCtx<'d> {
    /// Gets all the result of of the given computation key.
    /// recorded as dependencies of the current computation for which this
    /// context is for.
    pub(crate) fn compute<'a, K>(
        &'a self,
        key: &K,
    ) -> impl Future<Output = DiceResult<<K as Key>::Value>> + 'a
    where
        K: Key,
        Self: 'a,
    {
        self.compute_opaque(key)
            .map(|r| r.map(|opaque| self.opaque_into_value(opaque)))
    }

    /// Compute "opaque" value where the value is only accessible via projections.
    /// Projections allow accessing derived results from the "opaque" value,
    /// where the dependency of reading a projection is the projection value rather
    /// than the entire opaque value.
    pub(crate) fn compute_opaque<'a, K>(
        &'a self,
        key: &K,
    ) -> impl Future<Output = DiceResult<OpaqueValueModern<K>>> + 'a
    where
        K: Key,
    {
        self.ctx_data()
            .compute_opaque(key)
            .map(move |cancellable_result| {
                let cancellable = cancellable_result.map(move |(dice_key, dice_value)| {
                    OpaqueValueModern::new(dice_key, dice_value.value().dupe())
                });

                cancellable.map_err(|_| DiceError::cancelled())
            })
    }

    /// Computes all the given tasks in parallel, returning an unordered Stream
    pub(crate) fn compute_many<'a, T: 'a>(
        &'a self,
        computes: impl IntoIterator<
            Item = impl for<'x> FnOnce(&'x mut DiceComputations<'a>) -> BoxFuture<'x, T> + Send,
        >,
    ) -> Vec<impl Future<Output = T> + 'a> {
        computes
            .into_iter()
            .map(|func| OwningFuture::new(self.borrowed().into(), |ctx| func(ctx)))
            .collect()
    }

    pub(crate) fn compute2<'a, T: 'a, U: 'a>(
        &'a self,
        compute1: impl for<'x> FnOnce(&'x mut DiceComputations<'a>) -> BoxFuture<'x, T> + Send,
        compute2: impl for<'x> FnOnce(&'x mut DiceComputations<'a>) -> BoxFuture<'x, U> + Send,
    ) -> (impl Future<Output = T> + 'a, impl Future<Output = U> + 'a) {
        (
            OwningFuture::new(self.borrowed().into(), |ctx| compute1(ctx)),
            OwningFuture::new(self.borrowed().into(), |ctx| compute2(ctx)),
        )
    }

    pub(crate) fn compute3<'a, T: 'a, U: 'a, V: 'a>(
        &'a self,
        compute1: impl for<'x> FnOnce(&'x mut DiceComputations<'a>) -> BoxFuture<'x, T> + Send,
        compute2: impl for<'x> FnOnce(&'x mut DiceComputations<'a>) -> BoxFuture<'x, U> + Send,
        compute3: impl for<'x> FnOnce(&'x mut DiceComputations<'a>) -> BoxFuture<'x, V> + Send,
    ) -> (
        impl Future<Output = T> + 'a,
        impl Future<Output = U> + 'a,
        impl Future<Output = V> + 'a,
    ) {
        (
            OwningFuture::new(self.borrowed().into(), |ctx| compute1(ctx)),
            OwningFuture::new(self.borrowed().into(), |ctx| compute2(ctx)),
            OwningFuture::new(self.borrowed().into(), |ctx| compute3(ctx)),
        )
    }

    pub(crate) fn with_linear_recompute<'a, T, Fut: Future<Output = T> + 'a>(
        &'a mut self,
        func: impl FnOnce(LinearRecomputeDiceComputations<'a>) -> Fut,
    ) -> impl Future<Output = T> + 'a {
        func(LinearRecomputeDiceComputations(
            LinearRecomputeDiceComputationsImpl::Modern(LinearRecomputeModern(self.borrowed())),
        ))
    }

    pub fn opaque_into_value<'a, K: Key>(&'a self, opaque: OpaqueValueModern<K>) -> K::Value {
        self.dep_trackers()
            .lock()
            .record(opaque.derive_from_key, opaque.derive_from.validity());

        opaque
            .derive_from
            .downcast_maybe_transient::<K::Value>()
            .expect("type mismatch")
            .dupe()
    }
}

impl<'a> From<ModernComputeCtx<'a>> for DiceComputations<'a> {
    fn from(value: ModernComputeCtx<'a>) -> Self {
        DiceComputations(DiceComputationsImpl::Modern(value))
    }
}

pub(crate) struct LinearRecomputeModern<'a>(ModernComputeCtx<'a>);

impl LinearRecomputeModern<'_> {
    pub(crate) fn get(&self) -> DiceComputations<'_> {
        self.0.borrowed().into()
    }
}

/// Context given to the `compute` function of a `Key`.
#[derive(Allocative)]
pub(crate) enum ModernComputeCtx<'a> {
    Owned(Data),
    #[allocative(skip)]
    Borrowed(&'a Data),
}

#[derive(Allocative)]
pub(crate) struct Data {
    ctx_data: CoreCtx,
    dep_trackers: Mutex<RecordingDepsTracker>,
}

#[derive(Allocative)]
struct CoreCtx {
    async_evaluator: AsyncEvaluator,
    parent_key: ParentKey,
    #[allocative(skip)]
    cycles: KeyComputingUserCycleDetectorData,
    // data for the entire compute of a Key, including parallel computes
    #[allocative(skip)]
    evaluation_data: Mutex<EvaluationData>,
}

impl ModernComputeCtx<'static> {
    pub(crate) fn finalize(
        self,
    ) -> (
        (HashSet<DiceKey>, DiceValidity),
        EvaluationData,
        KeyComputingUserCycleDetectorData,
    ) {
        match self {
            ModernComputeCtx::Borrowed(..) => unreachable!(),
            ModernComputeCtx::Owned(v) => {
                let data = v.ctx_data;
                (
                    v.dep_trackers.into_inner().collect_deps(),
                    data.evaluation_data.into_inner(),
                    data.cycles,
                )
            }
        }
    }

    pub(crate) fn into_updater(self) -> TransactionUpdater {
        match self {
            ModernComputeCtx::Owned(v) => v.ctx_data.into_updater(),
            ModernComputeCtx::Borrowed(_) => unreachable!(),
        }
    }
}

impl ModernComputeCtx<'_> {
    pub(crate) fn new(
        parent_key: ParentKey,
        cycles: KeyComputingUserCycleDetectorData,
        async_evaluator: AsyncEvaluator,
    ) -> ModernComputeCtx<'static> {
        ModernComputeCtx::Owned(Data {
            dep_trackers: Mutex::new(RecordingDepsTracker::new()),
            ctx_data: CoreCtx {
                async_evaluator,
                parent_key,
                cycles,
                evaluation_data: Mutex::new(EvaluationData::none()),
            },
        })
    }

    fn borrowed<'a>(&'a self) -> ModernComputeCtx<'a> {
        ModernComputeCtx::Borrowed(self.data())
    }

    fn data(&self) -> &Data {
        match self {
            ModernComputeCtx::Owned(data) => data,
            ModernComputeCtx::Borrowed(data) => data,
        }
    }

    fn ctx_data(&self) -> &CoreCtx {
        &self.data().ctx_data
    }

    /// Compute "projection" based on deriving value
    pub(crate) fn projection<K: Key, P: ProjectionKey<DeriveFromKey = K>>(
        &self,
        derive_from: &OpaqueValueModern<K>,
        key: &P,
    ) -> DiceResult<P::Value> {
        self.ctx_data().project(
            key,
            derive_from.derive_from_key,
            derive_from.derive_from.dupe(),
            self.dep_trackers(),
        )
    }

    /// Data that is static per the entire lifetime of Dice. These data are initialized at the
    /// time that Dice is initialized via the constructor.
    pub(crate) fn global_data(&self) -> &DiceData {
        self.ctx_data().global_data()
    }

    /// Data that is static for the lifetime of the current request context. This lifetime is
    /// the lifetime of the top-level `DiceComputation` used for all requests.
    /// The data is also specific to each request context, so multiple concurrent requests can
    /// each have their own individual data.
    pub(crate) fn per_transaction_data(&self) -> &UserComputationData {
        self.ctx_data().per_transaction_data()
    }

    pub(crate) fn get_version(&self) -> VersionNumber {
        self.ctx_data().get_version()
    }

    #[allow(unused)] // used in test
    pub(super) fn dep_trackers(&self) -> &Mutex<RecordingDepsTracker> {
        match self {
            ModernComputeCtx::Owned(d) => &d.dep_trackers,
            ModernComputeCtx::Borrowed(d) => &d.dep_trackers,
        }
    }

    pub(crate) fn store_evaluation_data<T: Send + Sync + 'static>(
        &self,
        value: T,
    ) -> DiceResult<()> {
        self.ctx_data().store_evaluation_data(value)
    }

    pub(crate) fn cycle_guard<T: UserCycleDetectorGuard>(&self) -> DiceResult<Option<Arc<T>>> {
        self.ctx_data().cycle_guard()
    }
}

impl CoreCtx {
    /// Compute "opaque" value where the value is only accessible via projections.
    /// Projections allow accessing derived results from the "opaque" value,
    /// where the dependency of reading a projection is the projection value rather
    /// than the entire opaque value.
    pub(crate) fn compute_opaque<'a, K>(
        &'a self,
        key: &K,
    ) -> impl Future<Output = CancellableResult<(DiceKey, DiceComputedValue)>>
    where
        K: Key,
    {
        let dice_key = self
            .async_evaluator
            .dice
            .key_index
            .index(CowDiceKeyHashed::key_ref(key));

        self.async_evaluator
            .per_live_version_ctx
            .compute_opaque(
                dice_key,
                self.parent_key,
                &self.async_evaluator,
                self.cycles
                    .subrequest(dice_key, &self.async_evaluator.dice.key_index),
            )
            .map_ok(move |res| (dice_key, res))
    }

    /// Compute "projection" based on deriving value
    pub(crate) fn project<K>(
        &self,
        key: &K,
        base_key: DiceKey,
        base: MaybeValidDiceValue,
        dep_trackers: &Mutex<RecordingDepsTracker>,
    ) -> DiceResult<K::Value>
    where
        K: ProjectionKey,
    {
        let dice_key = self
            .async_evaluator
            .dice
            .key_index
            .index(CowDiceKeyHashed::proj_ref(base_key, key));

        let r = self
            .async_evaluator
            .per_live_version_ctx
            .compute_projection(
                dice_key,
                self.parent_key,
                self.async_evaluator.dice.state_handle.dupe(),
                SyncEvaluator::new(
                    self.async_evaluator.user_data.dupe(),
                    self.async_evaluator.dice.dupe(),
                    base,
                ),
                DiceEventDispatcher::new(
                    self.async_evaluator.user_data.tracker.dupe(),
                    self.async_evaluator.dice.dupe(),
                ),
            );

        let r = match r {
            Ok(r) => r,
            Err(_cancelled) => return Err(DiceError::cancelled()),
        };

        dep_trackers.lock().record(dice_key, r.value().validity());

        Ok(r.value()
            .downcast_maybe_transient::<K::Value>()
            .expect("Type mismatch when computing key")
            .dupe())
    }

    /// Data that is static per the entire lifetime of Dice. These data are initialized at the
    /// time that Dice is initialized via the constructor.
    pub(crate) fn global_data(&self) -> &DiceData {
        &self.async_evaluator.dice.global_data
    }

    /// Data that is static for the lifetime of the current request context. This lifetime is
    /// the lifetime of the top-level `DiceComputation` used for all requests.
    /// The data is also specific to each request context, so multiple concurrent requests can
    /// each have their own individual data.
    pub(crate) fn per_transaction_data(&self) -> &UserComputationData {
        &self.async_evaluator.user_data
    }

    pub(crate) fn get_version(&self) -> VersionNumber {
        self.async_evaluator.per_live_version_ctx.get_version()
    }

    pub(crate) fn into_updater(self) -> TransactionUpdater {
        TransactionUpdater::new(
            self.async_evaluator.dice.dupe(),
            self.async_evaluator.user_data.dupe(),
        )
    }

    pub(crate) fn store_evaluation_data<T: Send + Sync + 'static>(
        &self,
        value: T,
    ) -> DiceResult<()> {
        let mut evaluation_data = self.evaluation_data.lock();
        if evaluation_data.0.is_some() {
            return Err(DiceError::duplicate_activation_data());
        }
        evaluation_data.0 = Some(Box::new(value) as _);
        Ok(())
    }

    pub(crate) fn cycle_guard<T: UserCycleDetectorGuard>(&self) -> DiceResult<Option<Arc<T>>> {
        self.cycles.cycle_guard()
    }
}

/// Context that is shared for all current live computations of the same version.
#[derive(Allocative, Derivative, Dupe, Clone)]
#[derivative(Debug)]
pub(crate) struct SharedLiveTransactionCtx {
    version: VersionNumber,
    version_epoch: VersionEpoch,
    #[derivative(Debug = "ignore")]
    cache: SharedCache,
}

#[allow(clippy::manual_async_fn, unused)]
impl SharedLiveTransactionCtx {
    pub(crate) fn new(v: VersionNumber, version_epoch: VersionEpoch, cache: SharedCache) -> Self {
        Self {
            version: v,
            version_epoch,
            cache,
        }
    }

    /// Compute "opaque" value where the value is only accessible via projections.
    /// Projections allow accessing derived results from the "opaque" value,
    /// where the dependency of reading a projection is the projection value rather
    /// than the entire opaque value.
    pub(crate) fn compute_opaque(
        &self,
        key: DiceKey,
        parent_key: ParentKey,
        eval: &AsyncEvaluator,
        cycles: UserCycleDetectorData,
    ) -> impl Future<Output = CancellableResult<DiceComputedValue>> {
        match self.cache.get(key) {
            DiceTaskRef::Computed(result) => {
                DicePromise::ready(result).left_future()
            }
            DiceTaskRef::Occupied(mut occupied) => {
                match occupied.get().depended_on_by(parent_key) {
                    MaybeCancelled::Ok(promise) => {
                        debug!(msg = "shared state is waiting on existing task", k = ?key, v = ?self.version, v_epoch = ?self.version_epoch);

                        promise
                    },
                    MaybeCancelled::Cancelled => {
                        debug!(msg = "shared state has a cancelled task, spawning new one", k = ?key, v = ?self.version, v_epoch = ?self.version_epoch);

                        let eval = eval.dupe();
                        let events = DiceEventDispatcher::new(
                            eval.user_data.tracker.dupe(),
                            eval.dice.dupe(),
                        );

                        take_mut::take(occupied.get_mut(), |previous| {
                            IncrementalEngine::spawn_for_key(
                                key,
                                self.version_epoch,
                                eval,
                                cycles,
                                events,
                                 Some(PreviouslyCancelledTask {
                                    previous,
                                }),
                            )
                        });

                        occupied
                            .get()
                            .depended_on_by(parent_key)
                            .not_cancelled()
                            .expect("just created")
                    }
                }
                .left_future()
            }
            DiceTaskRef::Vacant(vacant) => {
                debug!(msg = "shared state is empty, spawning new task", k = ?key, v = ?self.version, v_epoch = ?self.version_epoch);

                let eval = eval.dupe();
                let events =
                    DiceEventDispatcher::new(eval.user_data.tracker.dupe(), eval.dice.dupe());

                let task = IncrementalEngine::spawn_for_key(
                    key,
                    self.version_epoch,
                    eval,
                    cycles,
                    events,
                    None,
                );

                let fut = task
                    .depended_on_by(parent_key)
                    .not_cancelled()
                    .expect("just created");

                vacant.insert(task);

                fut.left_future()
            }
            DiceTaskRef::TransactionCancelled => {
                let v = self.version;
                let v_epoch = self.version_epoch;
                async move {
                    debug!(msg = "computing shared state is cancelled", k = ?key, v = ?v, v_epoch = ?v_epoch);
                    tokio::task::yield_now().await;

                    Err(Cancelled)
                }
                    .right_future()
            },
        }
    }

    /// Compute "projection" based on deriving value
    pub(crate) fn compute_projection(
        &self,
        key: DiceKey,
        parent_key: ParentKey,
        state: CoreStateHandle,
        eval: SyncEvaluator,
        events: DiceEventDispatcher,
    ) -> CancellableResult<DiceComputedValue> {
        let promise = match self.cache.get(key) {
            DiceTaskRef::Computed(value) => DicePromise::ready(value),
            DiceTaskRef::Occupied(mut occupied) => {
                match occupied.get().depended_on_by(parent_key) {
                    MaybeCancelled::Ok(promise) => promise,
                    MaybeCancelled::Cancelled => {
                        let task = unsafe {
                            // SAFETY: task completed below by `IncrementalEngine::project_for_key`
                            sync_dice_task(key)
                        };

                        *occupied.get_mut() = task;

                        occupied
                            .get()
                            .depended_on_by(parent_key)
                            .not_cancelled()
                            .expect("just created")
                    }
                }
            }
            DiceTaskRef::Vacant(vacant) => {
                let task = unsafe {
                    // SAFETY: task completed below by `IncrementalEngine::project_for_key`
                    sync_dice_task(key)
                };

                vacant
                    .insert(task)
                    .value()
                    .depended_on_by(parent_key)
                    .not_cancelled()
                    .expect("just created")
            }
            DiceTaskRef::TransactionCancelled => {
                // for projection keys, these are cheap and synchronous computes that should never
                // be cancelled
                let task = unsafe {
                    // SAFETY: task completed below by `IncrementalEngine::project_for_key`
                    sync_dice_task(key)
                };

                task.depended_on_by(parent_key)
                    .not_cancelled()
                    .expect("just created")
            }
        };

        IncrementalEngine::project_for_key(
            state,
            promise,
            key,
            self.version,
            self.version_epoch,
            eval,
            events,
        )
    }

    pub(crate) fn get_version(&self) -> VersionNumber {
        self.version
    }
}

/// Opaque data that the key may have provided during evalution via store_evaluation_data.
pub(crate) struct EvaluationData(Option<Box<dyn Any + Send + Sync + 'static>>);

impl EvaluationData {
    pub(crate) fn none() -> Self {
        Self(None)
    }

    pub(crate) fn into_activation_data(self) -> ActivationData {
        ActivationData::Evaluated(self.0)
    }
}

#[cfg(test)]
pub(crate) mod testing {
    use crate::impls::cache::DiceTaskRef;
    use crate::impls::core::versions::VersionEpoch;
    use crate::impls::ctx::SharedLiveTransactionCtx;
    use crate::impls::key::DiceKey;
    use crate::impls::key::ParentKey;
    use crate::impls::task::promise::DiceSyncResult;
    use crate::impls::task::sync_dice_task;
    use crate::impls::value::DiceComputedValue;

    impl SharedLiveTransactionCtx {
        pub(crate) fn inject(&self, k: DiceKey, v: DiceComputedValue) {
            let task = unsafe {
                // SAFETY: completed immediately below
                sync_dice_task(k)
            };
            let _r = task
                .depended_on_by(ParentKey::None)
                .not_cancelled()
                .expect("just created")
                .sync_get_or_complete(|| DiceSyncResult::testing(v));

            match self.cache.get(k) {
                DiceTaskRef::Computed(_) => panic!("cannot inject already computed task"),
                DiceTaskRef::Occupied(o) => {
                    o.replace_entry(task);
                }
                DiceTaskRef::Vacant(v) => {
                    v.insert(task);
                }
                DiceTaskRef::TransactionCancelled => panic!("transaction cancelled"),
            }
        }

        pub(crate) fn testing_get_epoch(&self) -> VersionEpoch {
            self.version_epoch
        }
    }
}
