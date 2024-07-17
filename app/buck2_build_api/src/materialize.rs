/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

use std::sync::Arc;

use anyhow::Context;
use buck2_artifact::artifact::artifact_type::BaseArtifactKind;
use buck2_artifact::artifact::build_artifact::BuildArtifact;
use buck2_cli_proto::build_request::Materializations;
use dashmap::DashMap;
use dice::DiceComputations;
use dupe::Dupe;
use futures::FutureExt;

use crate::actions::artifact::materializer::ArtifactMaterializer;
use crate::artifact_groups::calculation::ArtifactGroupCalculation;
use crate::artifact_groups::ArtifactGroup;
use crate::artifact_groups::ArtifactGroupValues;

pub async fn materialize_artifact_group(
    ctx: &mut DiceComputations<'_>,
    artifact_group: &ArtifactGroup,
    materialization_context: &MaterializationContext,
) -> anyhow::Result<ArtifactGroupValues> {
    let values = ctx.ensure_artifact_group(artifact_group).await?;

    if let MaterializationContext::Materialize { map, force } = materialization_context {
        let mut artifacts_to_materialize = Vec::new();
        for (artifact, _value) in values.iter() {
            if let BaseArtifactKind::Build(artifact) = artifact.as_parts().0 {
                if map.insert(artifact.dupe(), ()).is_some() {
                    // We've already requested this artifact, no use requesting it again.
                    continue;
                }
                artifacts_to_materialize.push(artifact);
            }
        }

        ctx.try_compute_join(artifacts_to_materialize, |ctx, artifact| {
            async move {
                ctx.try_materialize_requested_artifact(artifact, *force)
                    .await
            }
            .boxed()
        })
        .await
        .context("Failed to materialize artifacts")?;
    }

    Ok(values)
}

#[derive(Clone, Dupe)]
pub enum MaterializationContext {
    Skip,
    Materialize {
        /// This map contains all the artifacts that we enqueued for materialization. This ensures
        /// we don't enqueue the same thing more than once.
        map: Arc<DashMap<BuildArtifact, ()>>,
        /// Whether we should force the materialization of requested artifacts, or defer to the
        /// config.
        force: bool,
    },
}

impl MaterializationContext {
    /// Create a new MaterializationContext that will force all materializations.
    pub fn force_materializations() -> Self {
        Self::Materialize {
            map: Arc::new(DashMap::new()),
            force: true,
        }
    }
}

pub trait ConvertMaterializationContext {
    fn from(self) -> MaterializationContext;

    fn with_existing_map(self, map: &Arc<DashMap<BuildArtifact, ()>>) -> MaterializationContext;
}

impl ConvertMaterializationContext for Materializations {
    fn from(self) -> MaterializationContext {
        match self {
            Materializations::Skip => MaterializationContext::Skip,
            Materializations::Default => MaterializationContext::Materialize {
                map: Arc::new(DashMap::new()),
                force: false,
            },
            Materializations::Materialize => MaterializationContext::Materialize {
                map: Arc::new(DashMap::new()),
                force: true,
            },
        }
    }

    fn with_existing_map(self, map: &Arc<DashMap<BuildArtifact, ()>>) -> MaterializationContext {
        match self {
            Materializations::Skip => MaterializationContext::Skip,
            Materializations::Default => MaterializationContext::Materialize {
                map: map.dupe(),
                force: false,
            },
            Materializations::Materialize => MaterializationContext::Materialize {
                map: map.dupe(),
                force: true,
            },
        }
    }
}
