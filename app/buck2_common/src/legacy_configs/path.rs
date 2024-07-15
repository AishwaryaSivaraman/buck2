/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

pub(crate) enum ExternalConfigSource {
    // Buckconfig file in the user's home directory
    UserFile(&'static str),

    // Buckconfig folder in the user's home directory, assuming all files in this folder are buckconfig
    UserFolder(&'static str),

    // Global buckconfig file. Repo related config is not allowed
    GlobalFile(&'static str),

    // Global buckconfig folder, assuming all files in this folder are buckconfig. Repo related config is not allowed
    GlobalFolder(&'static str),
}

pub(crate) enum ProjectConfigSource {
    // Buckconfig file in the cell relative to project root, such as .buckconfig or .buckconfig.local
    CellRelativeFile(&'static str),

    // Buckconfig folder in the cell, assuming all files in this folder are buckconfig
    CellRelativeFolder(&'static str),
}

/// The default places from which buckconfigs are sourced.
///
/// Later entries take precedence over earlier ones, and project configs take precedence over
/// external configs.
pub(crate) static DEFAULT_EXTERNAL_CONFIG_SOURCES: &[ExternalConfigSource] = &[
    #[cfg(not(windows))]
    ExternalConfigSource::GlobalFolder("/etc/buckconfig.d"),
    #[cfg(not(windows))]
    ExternalConfigSource::GlobalFile("/etc/buckconfig"),
    // TODO: use %PROGRAMDATA% on Windows
    #[cfg(windows)]
    ExternalConfigSource::GlobalFolder("C:\\ProgramData\\buckconfig.d"),
    #[cfg(windows)]
    ExternalConfigSource::GlobalFile("C:\\ProgramData\\buckconfig"),
    ExternalConfigSource::UserFolder(".buckconfig.d"),
    ExternalConfigSource::UserFile(".buckconfig.local"),
];

pub(crate) static DEFAULT_PROJECT_CONFIG_SOURCES: &[ProjectConfigSource] = &[
    ProjectConfigSource::CellRelativeFolder(".buckconfig.d"),
    ProjectConfigSource::CellRelativeFile(".buckconfig"),
    ProjectConfigSource::CellRelativeFile(".buckconfig.local"),
];
