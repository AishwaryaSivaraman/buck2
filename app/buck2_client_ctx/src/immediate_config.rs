/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

use std::path::Path;
use std::sync::OnceLock;
use std::time::SystemTime;

use anyhow::Context as _;
use buck2_common::init::DaemonStartupConfig;
use buck2_common::invocation_roots::find_invocation_roots;
use buck2_common::legacy_configs::cells::BuckConfigBasedCells;
use buck2_core::buck2_env;
use buck2_core::cells::CellAliasResolver;
use buck2_core::cells::CellResolver;
use buck2_core::fs::fs_util;
use buck2_core::fs::paths::abs_norm_path::AbsNormPath;
use buck2_core::fs::paths::abs_norm_path::AbsNormPathBuf;
use buck2_core::fs::paths::abs_path::AbsPath;
use buck2_core::fs::project::ProjectRoot;
use buck2_core::fs::project_rel_path::ProjectRelativePath;
use buck2_core::fs::working_dir::WorkingDir;
use prost::Message;

/// Limited view of the root config. This does not follow includes.
struct ImmediateConfig {
    cell_resolver: CellResolver,
    cwd_cell_alias_resolver: CellAliasResolver,
    daemon_startup_config: DaemonStartupConfig,
}

impl ImmediateConfig {
    /// Performs a parse of the root `.buckconfig` for the cell _only_ without following includes
    /// and without parsing any configs for any referenced cells. This means this function might return
    /// an empty mapping if the root `.buckconfig` does not contain the cell definitions.
    fn parse(
        project_fs: &ProjectRoot,
        cwd: &ProjectRelativePath,
    ) -> anyhow::Result<ImmediateConfig> {
        // This function is non-reentrant, and blocking for a bit should be ok
        let cells = futures::executor::block_on(BuckConfigBasedCells::parse_no_follow_includes(
            project_fs,
        ))?;

        let cell_resolver = cells.cell_resolver;
        let cwd_cell_alias_resolver = futures::executor::block_on(
            BuckConfigBasedCells::get_cell_alias_resolver_for_cwd_fast(
                &cell_resolver,
                project_fs,
                cwd,
            ),
        )?;

        let root_config = cells
            .configs_by_name
            .get(cell_resolver.root_cell())
            .context("No config for root cell")?;

        Ok(ImmediateConfig {
            cell_resolver,
            cwd_cell_alias_resolver,
            daemon_startup_config: DaemonStartupConfig::new(root_config)
                .context("Error loading daemon startup config")?,
        })
    }
}

/// Lazy-computed immediate config data. This is produced by reading the root buckconfig (but not
/// processing any includes).
struct ImmediateConfigContextData {
    cell_resolver: CellResolver,
    cwd_cell_alias_resolver: CellAliasResolver,
    daemon_startup_config: DaemonStartupConfig,
    project_filesystem: ProjectRoot,
}

pub struct ImmediateConfigContext<'a> {
    // Deliberately use `OnceLock` rather than `Lazy` because `Lazy` forces
    // us to have a shared reference to the underlying `anyhow::Error` which
    // we cannot use to correct chain the errors. Using `OnceLock` means
    // we don't get the result by a shared reference but instead as local
    // value which can be returned.
    data: OnceLock<ImmediateConfigContextData>,
    cwd: &'a WorkingDir,
    trace: Vec<AbsNormPathBuf>,
}

impl<'a> ImmediateConfigContext<'a> {
    pub fn new(cwd: &'a WorkingDir) -> Self {
        Self {
            data: OnceLock::new(),
            cwd,
            trace: Vec::new(),
        }
    }

    pub(crate) fn push_trace(&mut self, path: &AbsNormPath) {
        self.trace.push(path.to_buf());
    }

    pub(crate) fn trace(&self) -> &[AbsNormPathBuf] {
        &self.trace
    }

    pub fn daemon_startup_config(&self) -> anyhow::Result<&DaemonStartupConfig> {
        Ok(&self.data()?.daemon_startup_config)
    }

    pub(crate) fn canonicalize(&self, path: &Path) -> anyhow::Result<AbsNormPathBuf> {
        fs_util::canonicalize(self.cwd.path().as_abs_path().join(path))
    }

    /// Resolves a cell path (i.e., contains `//`) into an absolute path. The cell path must have
    /// been split into two components: `cell_alias` and `cell_path`. For example, if the cell path
    /// is `cell//path/to/file`, then:
    ///   - `cell_alias` would be `cell`
    ///   - `cell_relative_path` would be `path/to/file`
    pub(crate) fn resolve_cell_path(
        &self,
        cell_alias: &str,
        cell_relative_path: &str,
    ) -> anyhow::Result<AbsNormPathBuf> {
        let data = self.data()?;

        let cell = data.cwd_cell_alias_resolver.resolve(cell_alias)?;
        let cell = data.cell_resolver.get(cell)?;
        let path = cell.path().join_normalized(cell_relative_path)?;
        Ok(data.project_filesystem.resolve(&path))
    }

    fn data(&self) -> anyhow::Result<&ImmediateConfigContextData> {
        self.data
            .get_or_try_init(|| {
                let roots = find_invocation_roots(self.cwd.path())?;
                let paranoid_info_path = roots.paranoid_info_path()?;

                // See comment in `ImmediateConfig` about why we use `OnceLock` rather than `Lazy`
                let project_filesystem = roots.project_root;
                let cfg = ImmediateConfig::parse(
                    &project_filesystem,
                    project_filesystem.relativize(self.cwd.path())?.as_ref(),
                )?;

                // It'd be nice to deal with this a little differently by having this be a separate
                // type.
                let mut daemon_startup_config = cfg.daemon_startup_config;

                match is_paranoid_enabled(&paranoid_info_path) {
                    Ok(paranoid) => {
                        daemon_startup_config.paranoid = paranoid;
                    }
                    Err(e) => {
                        tracing::warn!(
                            "Failed to determine whether paranoid is enabled in `{}`: {:#}",
                            paranoid_info_path,
                            e
                        );
                    }
                };

                anyhow::Ok(ImmediateConfigContextData {
                    cell_resolver: cfg.cell_resolver,
                    cwd_cell_alias_resolver: cfg.cwd_cell_alias_resolver,
                    daemon_startup_config,
                    project_filesystem,
                })
            })
            .context("Error creating cell resolver")
    }
}

fn is_paranoid_enabled(path: &AbsPath) -> anyhow::Result<bool> {
    if let Some(p) = buck2_env!("BUCK_PARANOID", type=bool)? {
        return Ok(p);
    }

    let bytes = match fs_util::read_if_exists(path)? {
        Some(b) => b,
        None => return Ok(false),
    };

    let info = buck2_cli_proto::ParanoidInfo::decode(bytes.as_slice()).context("Invalid data ")?;

    let now = SystemTime::now();
    let expires_at = SystemTime::try_from(info.expires_at.context("Missing expires_at")?)
        .context("Invalid expires_at")?;
    Ok(now < expires_at)
}
