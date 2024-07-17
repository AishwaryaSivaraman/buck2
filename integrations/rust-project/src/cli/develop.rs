/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

use std::io::BufWriter;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;

use rustc_hash::FxHashMap;
use rustc_hash::FxHashSet;
use serde::Deserialize;
use serde::Serialize;
use tracing::info;
use tracing::instrument;
use tracing::warn;

use super::Input;
use crate::buck;
use crate::buck::relative_to;
use crate::buck::select_mode;
use crate::buck::to_json_project;
use crate::json_project::JsonProject;
use crate::json_project::Sysroot;
use crate::path::canonicalize;
use crate::sysroot::resolve_buckconfig_sysroot;
use crate::sysroot::resolve_rustup_sysroot;
use crate::sysroot::SysrootConfig;
use crate::target::Target;
use crate::Command;

#[derive(Debug)]
pub(crate) struct Develop {
    pub(crate) sysroot: SysrootConfig,
    pub(crate) relative_paths: bool,
    pub(crate) buck: buck::Buck,
    pub(crate) check_cycles: bool,
    pub(crate) invoked_by_ra: bool,
}

pub(crate) struct OutputCfg {
    out: Output,
    pretty: bool,
}

#[derive(Debug)]
pub(crate) enum Output {
    Path(PathBuf),
    Stdout,
}

impl Develop {
    pub(crate) fn from_command(command: Command) -> (Develop, Input, OutputCfg) {
        if let crate::Command::Develop {
            files,
            targets,
            out,
            stdout,
            prefer_rustup_managed_toolchain,
            sysroot,
            pretty,
            relative_paths,
            mode,
            check_cycles,
            log_scuba_to_stdout: _,
        } = command
        {
            let out = if stdout {
                Output::Stdout
            } else {
                Output::Path(out)
            };

            let sysroot = if prefer_rustup_managed_toolchain {
                SysrootConfig::Rustup
            } else if let Some(sysroot) = sysroot {
                SysrootConfig::Sysroot(sysroot)
            } else {
                SysrootConfig::BuckConfig
            };

            let mode = select_mode(mode.as_deref());
            let buck = buck::Buck::new(mode);

            let develop = Develop {
                sysroot,
                relative_paths,
                buck,
                check_cycles,
                invoked_by_ra: false,
            };
            let out = OutputCfg { out, pretty };

            let input = if !targets.is_empty() {
                let targets = targets.into_iter().map(Target::new).collect();
                Input::Targets(targets)
            } else {
                Input::Files(files)
            };

            return (develop, input, out);
        }

        if let crate::Command::DevelopJson { args } = command {
            let out = Output::Stdout;
            let sysroot = SysrootConfig::BuckConfig;
            let mode = select_mode(None);

            let buck = buck::Buck::new(mode);

            let develop = Develop {
                sysroot,
                relative_paths: false,
                buck,
                check_cycles: false,
                invoked_by_ra: true,
            };
            let out = OutputCfg { out, pretty: false };

            let input = match args {
                crate::JsonArguments::Path(path) => Input::Files(vec![path]),
                crate::JsonArguments::Label(target) => Input::Targets(vec![Target::new(target)]),
            };

            return (develop, input, out);
        }

        unreachable!("No other subcommand is supported.")
    }
}

const DEFAULT_EXTRA_TARGETS: usize = 50;

#[derive(Serialize, Deserialize)]
struct OutputData {
    buildfile: PathBuf,
    project: JsonProject,
}

impl Develop {
    #[instrument(name = "develop", skip_all, fields(develop_input = ?input))]
    pub(crate) fn run(self, input: Input, cfg: OutputCfg) -> Result<(), anyhow::Error> {
        let input = match input {
            Input::Targets(targets) => Input::Targets(targets),
            Input::Files(files) => {
                let canonical_files = files
                    .into_iter()
                    .map(|p| match canonicalize(&p) {
                        Ok(path) => path,
                        Err(_) => p,
                    })
                    .collect::<Vec<_>>();

                Input::Files(canonical_files)
            }
        };
        let mut writer: BufWriter<Box<dyn Write>> = match cfg.out {
            Output::Path(ref p) => {
                let out = std::fs::File::create(p)?;
                BufWriter::new(Box::new(out))
            }
            Output::Stdout => BufWriter::new(Box::new(std::io::stdout())),
        };

        let targets = self.related_targets(input.clone())?;
        if targets.is_empty() {
            let err = anyhow::anyhow!("No owning target found")
                .context(format!("Could not find owning target for {:?}", input));
            return Err(err);
        }

        if self.invoked_by_ra {
            for (buildfile, targets) in targets {
                let project = self.run_inner(targets)?;
                let output = OutputData { buildfile, project };
                serde_json::to_writer(&mut writer, &output)?;
                writeln!(writer)?;
                info!("wrote rust-project.json to stdout");
            }
        } else {
            let mut targets = targets.into_values().flatten().collect::<Vec<_>>();
            targets.sort();
            targets.dedup();

            let project = self.run_inner(targets)?;
            if cfg.pretty {
                serde_json::to_writer_pretty(&mut writer, &project)?;
            } else {
                serde_json::to_writer(&mut writer, &project)?;
            }
            writeln!(writer)?;
            match &cfg.out {
                Output::Path(p) => info!(file = ?p, "wrote rust-project.json"),
                Output::Stdout => info!("wrote rust-project.json to stdout"),
            }
        }

        Ok(())
    }

    pub(crate) fn run_inner(&self, targets: Vec<Target>) -> Result<JsonProject, anyhow::Error> {
        let start = std::time::Instant::now();
        let Develop {
            sysroot,
            relative_paths,
            buck,
            check_cycles,
            ..
        } = self;

        let project_root = buck.resolve_project_root()?;

        info!("building generated code");
        let expanded_and_resolved = buck.expand_and_resolve(&targets)?;

        info!("fetching sysroot");
        let aliased_libraries =
            buck.query_aliased_libraries(&expanded_and_resolved.expanded_targets)?;

        info!("fetching sysroot");
        let sysroot = match &sysroot {
            SysrootConfig::Sysroot(path) => {
                let mut sysroot_path = canonicalize(expand_tilde(path)?)?;
                if *relative_paths {
                    sysroot_path = relative_to(&sysroot_path, &project_root);
                }

                Sysroot {
                    sysroot: sysroot_path,
                    sysroot_src: None,
                }
            }
            SysrootConfig::BuckConfig => {
                resolve_buckconfig_sysroot(&project_root, *relative_paths)?
            }
            SysrootConfig::Rustup => resolve_rustup_sysroot()?,
        };
        info!("converting buck info to rust-project.json");
        let rust_project = to_json_project(
            sysroot,
            expanded_and_resolved,
            aliased_libraries,
            *relative_paths,
            *check_cycles,
        )?;

        let duration = start.elapsed();
        info!(
            duration_ms = duration.as_millis(),
            "finished generating rust-project"
        );

        Ok(rust_project)
    }

    /// For every Rust file, return the relevant buck targets that should be used to configure rust-analyzer.
    pub(crate) fn related_targets(
        &self,
        input: Input,
    ) -> Result<FxHashMap<PathBuf, Vec<Target>>, anyhow::Error> {
        // We want to load additional targets from the enclosing buildfile, to help users
        // who have a bunch of small targets in their buildfile. However, we want to set a limit
        // so we don't try to load everything in very large generated buildfiles.
        let max_extra_targets: usize = match std::env::var("RUST_PROJECT_EXTRA_TARGETS") {
            Ok(s) => s.parse::<usize>().unwrap_or(DEFAULT_EXTRA_TARGETS),
            Err(_) => DEFAULT_EXTRA_TARGETS,
        };

        // We always want the targets that directly own these Rust files.
        let mut targets = self.buck.query_owners(input, max_extra_targets)?;
        for targets in targets.values_mut() {
            *targets = dedupe_targets(targets);
        }

        Ok(targets)
    }
}

fn expand_tilde(path: &Path) -> Result<PathBuf, anyhow::Error> {
    if path.starts_with("~") {
        let path = path.strip_prefix("~")?;
        let home = std::env::var("HOME")?;
        let home = PathBuf::from(home);
        Ok(home.join(path))
    } else {
        Ok(path.to_path_buf())
    }
}

/// Remove duplicate targets, but preserve order.
///
/// This function will also remove `foo` if `foo-unittest` is present.
fn dedupe_targets(targets: &[Target]) -> Vec<Target> {
    let mut seen = FxHashSet::default();
    let mut unique_targets_acc = vec![];
    let unique_targets = targets.iter().collect::<FxHashSet<_>>();

    let targets: Vec<Target> = targets
        .iter()
        .filter(|t| !unique_targets.contains(&Target::new(format!("{}-unittest", t))))
        .cloned()
        .collect();

    for target in targets {
        if !seen.contains(&target) {
            seen.insert(target.clone());
            unique_targets_acc.push(target);
        }
    }

    unique_targets_acc
}

#[test]
fn test_dedupe_unittest() {
    let targets = vec![
        Target::new("foo-unittest".to_owned()),
        Target::new("bar".to_owned()),
        Target::new("foo".to_owned()),
        Target::new("baz-unittest".to_owned()),
    ];

    let expected = vec![
        Target::new("foo-unittest".to_owned()),
        Target::new("bar".to_owned()),
        Target::new("baz-unittest".to_owned()),
    ];

    assert_eq!(dedupe_targets(&targets), expected);
}
