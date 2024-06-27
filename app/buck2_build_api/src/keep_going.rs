/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

use std::collections::HashMap;
use std::hash::Hash;

use dice::DiceComputations;
use dice::UserComputationData;
use futures::future::BoxFuture;
use futures::stream::FuturesOrdered;
use futures::Future;
use futures::Stream;
use futures::StreamExt;
use indexmap::IndexMap;
use smallvec::SmallVec;

pub struct KeepGoing;

impl KeepGoing {
    pub fn try_compute_join_all<'a, C, T: Send, R: 'a, E: 'a>(
        ctx: &'a mut DiceComputations<'_>,
        items: impl IntoIterator<Item = T>,
        mapper: (
            impl for<'x> FnOnce(&'x mut DiceComputations<'a>, T) -> BoxFuture<'x, Result<R, E>>
            + Send
            + Sync
            + Copy
        ),
    ) -> impl Future<Output = Result<C, E>> + 'a
    where
        C: KeepGoingCollectable<R> + 'a,
    {
        let keep_going = ctx.per_transaction_data().get_keep_going();

        let futs = ctx.compute_many(items.into_iter().map(move |v| {
            DiceComputations::declare_closure(
                move |ctx: &mut DiceComputations| -> BoxFuture<Result<R, E>> { mapper(ctx, v) },
            )
        }));

        let futs: FuturesOrdered<_> = futs.into_iter().collect();
        Self::try_join_all(keep_going, futs)
    }

    async fn try_join_all<C, R, E>(
        keep_going: bool,
        mut inputs: impl Stream<Item = Result<R, E>> + Unpin,
    ) -> Result<C, E>
    where
        C: KeepGoingCollectable<R>,
    {
        let size = inputs.size_hint().0;
        let mut res = C::with_capacity(size);
        let mut err = None;
        while let Some(x) = inputs.next().await {
            match x {
                Ok(x) => res.push(x),
                Err(e) => {
                    if keep_going {
                        err = Some(e);
                    } else {
                        return Err(e);
                    }
                }
            }
        }

        if let Some(err) = err {
            return Err(err);
        }

        Ok(res)
    }
}

pub trait KeepGoingCollectable<I> {
    fn with_capacity(cap: usize) -> Self;

    fn push(&mut self, item: I);
}

impl<K, V> KeepGoingCollectable<(K, V)> for IndexMap<K, V>
where
    K: PartialEq + Eq + Hash,
{
    fn with_capacity(cap: usize) -> Self {
        IndexMap::with_capacity(cap)
    }

    fn push(&mut self, item: (K, V)) {
        let (k, v) = item;
        IndexMap::insert(self, k, v);
    }
}

impl<K, V> KeepGoingCollectable<(K, V)> for HashMap<K, V>
where
    K: PartialEq + Eq + Hash,
{
    fn with_capacity(cap: usize) -> Self {
        HashMap::with_capacity(cap)
    }

    fn push(&mut self, item: (K, V)) {
        let (k, v) = item;
        HashMap::insert(self, k, v);
    }
}

impl<I> KeepGoingCollectable<I> for Vec<I> {
    fn with_capacity(cap: usize) -> Self {
        Vec::with_capacity(cap)
    }

    fn push(&mut self, item: I) {
        Vec::push(self, item);
    }
}

impl<I> KeepGoingCollectable<I> for SmallVec<[I; 1]> {
    fn with_capacity(cap: usize) -> Self {
        SmallVec::with_capacity(cap)
    }

    fn push(&mut self, item: I) {
        SmallVec::push(self, item);
    }
}

pub struct KeepGoingHolder(bool);

pub trait HasKeepGoing {
    fn set_keep_going(&mut self, keep_going: bool);

    fn get_keep_going(&self) -> bool;
}

impl HasKeepGoing for UserComputationData {
    fn set_keep_going(&mut self, keep_going: bool) {
        self.data.set(KeepGoingHolder(keep_going));
    }

    fn get_keep_going(&self) -> bool {
        self.data
            .get::<KeepGoingHolder>()
            .expect("KeepGoing should be set")
            .0
    }
}
