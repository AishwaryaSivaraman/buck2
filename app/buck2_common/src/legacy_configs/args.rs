/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

use std::path::Path;

use anyhow::Context;
use buck2_cli_proto::config_override::ConfigType;
use buck2_cli_proto::ConfigOverride;
use buck2_core::cells::cell_root_path::CellRootPathBuf;
use buck2_core::fs::paths::abs_path::AbsPath;
use buck2_core::fs::project::ProjectRoot;
use buck2_core::fs::project_rel_path::ProjectRelativePath;
use buck2_core::fs::project_rel_path::ProjectRelativePathBuf;

use crate::legacy_configs::configs::parse_config_section_and_key;
use crate::legacy_configs::configs::ConfigArgumentParseError;
use crate::legacy_configs::configs::ConfigSectionAndKey;
use crate::legacy_configs::configs::LegacyBuckConfig;
use crate::legacy_configs::file_ops::ConfigParserFileOps;
use crate::legacy_configs::file_ops::ConfigPath;
use crate::legacy_configs::parser::LegacyConfigParser;

/// Representation of a processed config arg, namely after file path resolution has been performed.
#[derive(Debug, Clone, PartialEq, Eq, allocative::Allocative)]
pub(crate) enum ResolvedLegacyConfigArg {
    /// A single config key-value pair (in `a.b=c` format).
    Flag(ResolvedConfigFlag),
    /// A file containing additional config values (in `.buckconfig` format).
    File(ResolvedConfigFile),
}

#[derive(Clone, Debug, PartialEq, Eq, allocative::Allocative)]
pub(crate) enum ResolvedConfigFile {
    /// If the config file is project relative, the path of the file
    Project(ProjectRelativePathBuf),
    /// If the config file is external, we pre-parse it to be able to insert the results into dice
    Global(LegacyConfigParser),
}

#[derive(Clone, Debug, PartialEq, Eq, allocative::Allocative)]
pub(crate) struct ResolvedConfigFlag {
    pub(crate) section: String,
    pub(crate) key: String,
    // None value means this config is unset.
    pub(crate) value: Option<String>,
    // If this arg only applies to one cell, the root of that cell.
    pub(crate) cell: Option<CellRootPathBuf>,
}

fn resolve_config_flag_arg(
    cell: Option<CellRootPathBuf>,
    raw_arg: &str,
) -> anyhow::Result<ResolvedConfigFlag> {
    let (raw_section_and_key, raw_value) = raw_arg
        .split_once('=')
        .ok_or_else(|| ConfigArgumentParseError::NoEqualsSeparator(raw_arg.to_owned()))?;
    let ConfigSectionAndKey { section, key } =
        parse_config_section_and_key(raw_section_and_key, Some(raw_arg))?;

    let value = match raw_value {
        "" => None, // An empty string unsets this config.
        v => Some(v.to_owned()),
    };

    Ok(ResolvedConfigFlag {
        cell,
        section,
        key,
        value,
    })
}

async fn resolve_config_file_arg(
    cell: Option<CellRootPathBuf>,
    arg: &str,
    project_filesystem: &ProjectRoot,
    cwd: &ProjectRelativePath,
    file_ops: &mut dyn ConfigParserFileOps,
) -> anyhow::Result<ResolvedConfigFile> {
    if let Some(cell_path) = cell {
        let proj_path = cell_path.as_project_relative_path().join_normalized(arg)?;
        return Ok(ResolvedConfigFile::Project(proj_path));
    }

    let path = Path::new(arg);
    let path = if path.is_absolute() {
        AbsPath::new(path)?.to_owned()
    } else {
        let cwd = project_filesystem.resolve(cwd);
        cwd.into_abs_path_buf().join(path)
    };

    Ok(ResolvedConfigFile::Global(
        LegacyBuckConfig::start_parse_for_external_files(
            &[ConfigPath::Global(path)],
            file_ops,
            // Note that when reading immediate configs that don't follow includes, we don't apply
            // config args either
            true, // follow includes
        )
        .await?,
    ))
}

pub(crate) async fn resolve_config_args(
    args: &[ConfigOverride],
    project_fs: &ProjectRoot,
    cwd: &ProjectRelativePath,
    file_ops: &mut dyn ConfigParserFileOps,
) -> anyhow::Result<Vec<ResolvedLegacyConfigArg>> {
    let mut resolved_args = Vec::new();

    for u in args {
        let config_type = ConfigType::from_i32(u.config_type).with_context(|| {
            format!(
                "Unknown ConfigType enum value `{}` when trying to deserialize",
                u.config_type
            )
        })?;
        let resolved = match config_type {
            ConfigType::Value => {
                let cell = u.get_cell()?.map(|p| p.to_buf());
                let resolved_flag = resolve_config_flag_arg(cell, &u.config_override)?;
                ResolvedLegacyConfigArg::Flag(resolved_flag)
            }
            ConfigType::File => {
                let cell = u.get_cell()?.map(|p| p.to_buf());
                let resolved_path =
                    resolve_config_file_arg(cell, &u.config_override, project_fs, cwd, file_ops)
                        .await?;
                ResolvedLegacyConfigArg::File(resolved_path)
            }
        };
        resolved_args.push(resolved);
    }

    Ok(resolved_args)
}

#[cfg(test)]
mod tests {
    use super::resolve_config_flag_arg;

    #[test]
    fn test_argument_pair() -> anyhow::Result<()> {
        // Valid Formats

        let normal_pair = resolve_config_flag_arg(None, "apple.key=value")?;

        assert_eq!("apple", normal_pair.section);
        assert_eq!("key", normal_pair.key);
        assert_eq!(Some("value".to_owned()), normal_pair.value);

        let unset_pair = resolve_config_flag_arg(None, "apple.key=")?;

        assert_eq!("apple", unset_pair.section);
        assert_eq!("key", unset_pair.key);
        assert_eq!(None, unset_pair.value);

        // Whitespace

        let section_leading_whitespace = resolve_config_flag_arg(None, "  apple.key=value")?;
        assert_eq!("apple", section_leading_whitespace.section);
        assert_eq!("key", section_leading_whitespace.key);
        assert_eq!(Some("value".to_owned()), section_leading_whitespace.value);

        let pair_with_whitespace_in_key = resolve_config_flag_arg(None, "apple. key=value");
        assert!(pair_with_whitespace_in_key.is_err());

        let pair_with_whitespace_in_value =
            resolve_config_flag_arg(None, "apple.key= value with whitespace  ")?;
        assert_eq!("apple", pair_with_whitespace_in_value.section);
        assert_eq!("key", pair_with_whitespace_in_value.key);
        assert_eq!(
            Some(" value with whitespace  ".to_owned()),
            pair_with_whitespace_in_value.value
        );

        // Invalid Formats

        let pair_without_section = resolve_config_flag_arg(None, "key=value");
        assert!(pair_without_section.is_err());

        let pair_without_equals = resolve_config_flag_arg(None, "apple.keyvalue");
        assert!(pair_without_equals.is_err());

        let pair_without_section_or_equals = resolve_config_flag_arg(None, "applekeyvalue");
        assert!(pair_without_section_or_equals.is_err());

        Ok(())
    }
}
