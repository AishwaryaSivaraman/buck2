/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

mod buck;
mod cli;
mod diagnostics;
mod json_project;
mod path;
mod progress;
mod scuba;
mod server;
mod sysroot;
mod target;

use std::io;
use std::io::IsTerminal as _;
use std::path::PathBuf;

use clap::ArgAction;
use clap::Parser;
use clap::Subcommand;
use progress::ProgressLayer;
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::Layer;

use crate::cli::ProjectKind;
use crate::json_project::Crate;
use crate::json_project::Dep;

#[derive(Parser, Debug, PartialEq)]
struct Opt {
    #[clap(subcommand)]
    command: Option<Command>,
    /// Print the current version.
    #[arg(short = 'V', long)]
    version: bool,
}

#[derive(Subcommand, Debug, PartialEq)]
enum Command {
    /// Create a new Rust project
    New {
        /// Name of the project being created.
        name: String,
        /// Kinds of Rust projects that can be created
        #[clap(long, value_enum, default_value = "binary")]
        kind: ProjectKind,

        /// Path to create new crate at. The new directory will be created as a
        /// subdirectory.
        path: Option<PathBuf>,
    },
    /// Convert buck's build to a format that rust-analyzer can consume.
    Develop {
        /// Buck targets to include in rust-project.json.
        #[clap(required = true, conflicts_with = "files", num_args=1..)]
        targets: Vec<String>,

        /// Path of the file being developed.
        ///
        /// Used to discover the owning set of targets.
        #[clap(required = true, last = true, num_args=1..)]
        files: Vec<PathBuf>,

        /// Where to write the generated `rust-project.json`.
        ///
        /// If not provided, rust-project will write in the current working directory.
        #[clap(short = 'o', long, value_hint = clap::ValueHint::DirPath, default_value = "rust-project.json")]
        out: PathBuf,

        /// Writes the generated `rust-project.json` to stdout.
        #[clap(long = "stdout", conflicts_with = "out")]
        stdout: bool,

        /// Log in a JSON format.
        #[clap(long, default_value = "false")]
        log_json: bool,

        /// Use a `rustup`-managed sysroot instead of a `.buckconfig`-managed sysroot.
        ///
        /// This option requires the presence of `rustc` in the `$PATH`, as rust-project
        /// will run `rustc --print sysroot` and ignore any other `sysroot` configuration.
        #[clap(long, conflicts_with = "sysroot")]
        prefer_rustup_managed_toolchain: bool,

        /// The directory containing the Rust source code, including std.
        /// Default value is determined based on platform.
        #[clap(short = 's', long)]
        sysroot: Option<PathBuf>,

        /// Pretty-print generated `rust-project.json` file.
        #[clap(short, long)]
        pretty: bool,

        /// Check that there are no cycles in the generated crate graph.
        #[clap(long)]
        check_cycles: bool,

        /// Use paths relative to the project root in `rust-project.json`.
        #[clap(long, hide = true)]
        relative_paths: bool,

        /// Optional argument specifying build mode.
        #[clap(short = 'm', long)]
        mode: Option<String>,

        /// Write Scuba sample to stdout instead.
        #[clap(long, hide = true)]
        log_scuba_to_stdout: bool,
    },
    /// Build the saved file's owning target. This is meant to be used by IDEs to provide diagnostics on save.
    Check {
        /// Optional argument specifying build mode.
        #[clap(short = 'm', long)]
        mode: Option<String>,
        #[clap(short = 'c', long, default_value = "true", action = ArgAction::Set)]
        use_clippy: bool,
        /// The file saved by the user. `rust-project` will infer the owning target(s) of the saved file and build them.
        saved_file: PathBuf,
        /// Write Scuba sample to stdout instead.
        #[clap(long, hide = true)]
        log_scuba_to_stdout: bool,
    },
    /// Start an LSP server whose functionality is similar to [Command::Develop].
    #[clap(hide = true)]
    LspServer,
}

fn main() -> Result<(), anyhow::Error> {
    #[cfg(fbcode_build)]
    {
        // SAFETY: This is as safe as using fbinit::main but with slightly less conditional compilation.
        unsafe { fbinit::perform_init() };
    }

    let opt = Opt::parse();

    let filter = EnvFilter::builder()
        .with_default_directive(LevelFilter::INFO.into())
        .from_env()?;

    if opt.version {
        println!("{}", build_info());
        return Ok(());
    }

    let Some(command) = opt.command else {
        eprintln!("Expected a subcommand, see --help for more information.");
        return Ok(());
    };

    let fmt = tracing_subscriber::fmt::layer()
        .with_ansi(io::stderr().is_terminal())
        .with_writer(io::stderr);

    match command {
        c @ Command::Develop {
            log_json,
            log_scuba_to_stdout,
            ..
        } => {
            if log_json {
                let subscriber = tracing_subscriber::registry()
                    .with(fmt.json().with_filter(filter))
                    .with(scuba::ScubaLayer::new(log_scuba_to_stdout));
                tracing::subscriber::set_global_default(subscriber)?;
            } else {
                let subscriber = tracing_subscriber::registry()
                    .with(fmt.with_filter(filter))
                    .with(scuba::ScubaLayer::new(log_scuba_to_stdout));
                tracing::subscriber::set_global_default(subscriber)?;
            };

            let (develop, input, out) = cli::Develop::from_command(c);
            match develop.run_as_cli(input, out) {
                Ok(_) => Ok(()),
                Err(e) => {
                    tracing::error!(
                        error = <anyhow::Error as AsRef<
                            dyn std::error::Error + Send + Sync + 'static,
                        >>::as_ref(&e),
                        source = e.source()
                    );
                    Ok(())
                }
            }
        }
        Command::LspServer => {
            let state = server::State::new()?;
            let sender = state.server.sender.clone();

            let progress = ProgressLayer::new(sender);

            let subscriber = tracing_subscriber::registry()
                .with(fmt.with_filter(filter))
                .with(progress);
            tracing::subscriber::set_global_default(subscriber)?;

            state.run()
        }
        Command::New { name, kind, path } => {
            let subscriber = tracing_subscriber::registry().with(fmt.with_filter(filter));
            tracing::subscriber::set_global_default(subscriber)?;

            cli::New { name, kind, path }.run()
        }
        Command::Check {
            mode,
            use_clippy,
            saved_file,
            log_scuba_to_stdout,
        } => {
            let subscriber = tracing_subscriber::registry()
                .with(fmt.with_filter(filter))
                .with(scuba::ScubaLayer::new(log_scuba_to_stdout));
            tracing::subscriber::set_global_default(subscriber)?;
            cli::Check::new(mode, use_clippy, saved_file).run()
        }
    }
}

#[cfg(not(unix))]
fn build_info() -> String {
    "No build info available.".to_owned()
}

#[cfg(unix)]
fn build_info() -> String {
    match fb_build_info_from_elf() {
        Ok(s) => s,
        Err(_) => "No build info available.".to_owned(),
    }
}

#[cfg(unix)]
fn fb_build_info_from_elf() -> Result<String, anyhow::Error> {
    let bin_path = std::env::current_exe()?;
    let bin_bytes = std::fs::read(&bin_path)?;

    let elf_file = elf::ElfBytes::<elf::endian::AnyEndian>::minimal_parse(&bin_bytes)?;
    let elf_section = elf_file
        .section_header_by_name("fb_build_info")?
        .ok_or(anyhow::anyhow!("no header"))?;

    let (section_bytes, _) = elf_file.section_data(&elf_section)?;
    let section_cstr = std::ffi::CStr::from_bytes_with_nul(section_bytes)?;

    let build_info: serde_json::Value = serde_json::from_str(&section_cstr.to_str()?)?;
    let revision = build_info["revision"].as_str().unwrap_or("(unknown)");
    let build_time = build_info["time"].as_str().unwrap_or("(unknown)");

    Ok(format!("revision: {revision}, build time: {build_time}"))
}

#[test]
fn test_parse_use_clippy() {
    assert!(matches!(
        Opt::try_parse_from([
            "rust-project",
            "check",
            "--use-clippy=true",
            "fbcode/foo.rs",
        ]),
        Ok(Opt {
            command: Some(Command::Check {
                use_clippy: true,
                ..
            }),
            ..
        })
    ));

    assert!(matches!(
        Opt::try_parse_from([
            "rust-project",
            "check",
            "--use-clippy=false",
            "fbcode/foo.rs",
        ]),
        Ok(Opt {
            command: Some(Command::Check {
                use_clippy: false,
                ..
            }),
            ..
        })
    ));
}
