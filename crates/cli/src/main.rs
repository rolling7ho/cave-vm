//! The `cave` command line, specified in `docs/cli.md`.

#![deny(unsafe_code)]

mod client;
mod commands;
mod copy;
mod error;
mod exec;
mod init;
mod policy;
mod project;
mod remove;
mod shell;
mod snapshot;

use std::process::ExitCode;

use cave_vm_proto::daemon::{NetMode, PortMap};
use clap::{Parser, Subcommand, ValueEnum};

/// Disposable Linux VMs for coding agents, one per project.
#[derive(Parser)]
#[command(name = "cave", version)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Clone, Copy, ValueEnum)]
enum Net {
    /// No network device at all.
    Off,
    /// Also allow private and link-local ranges.
    Lan,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create cave.toml and .cave/ for this directory.
    Init {
        /// Code lives only on the VM disk (the default).
        #[arg(long, conflicts_with = "synced")]
        sealed: bool,
        /// Also keep a Mac mirror of the code.
        #[arg(long)]
        synced: bool,
        /// Base image, from `cave images`.
        #[arg(long, default_value = "base")]
        image: String,
    },
    /// Boot or restore this project's VM.
    Run {
        /// Network mode for this boot.
        #[arg(long, value_enum)]
        net: Option<Net>,
        /// Forward localhost:<port> on the Mac to the VM; <mac>:<vm> maps
        /// different ports.
        #[arg(short = 'p', value_name = "PORT", value_parser = commands::parse_port)]
        ports: Vec<PortMap>,
        /// Run a fork instead.
        #[arg(long)]
        fork: Option<String>,
    },
    /// Remove the project's VM, or a fork or snapshot.
    Rm {
        /// A fork or snapshot; the project's VM when left out.
        name: Option<String>,
        /// Do not ask first, for scripts.
        #[arg(long)]
        yes: bool,
    },
    /// Create a new VM from a snapshot, copy-on-write.
    Fork {
        /// The new fork's name; fork-1, fork-2 and so on when left out.
        name: Option<String>,
        /// Fork this snapshot instead of the VM's current state.
        #[arg(long, value_name = "SNAPSHOT")]
        from: Option<String>,
    },
    /// Roll the VM back to a snapshot, by default the newest.
    Restore {
        /// A snapshot from `cave ls`.
        name: Option<String>,
        #[arg(long)]
        fork: Option<String>,
    },
    /// Save the VM's state.
    Snapshot {
        /// The snapshot's name; [ProjectFolder]-[YYYY-MM-DD]-[Distro] when
        /// left out.
        name: Option<String>,
        #[arg(long)]
        fork: Option<String>,
    },
    /// Copy files in and out; a path starting with `:` is in the VM.
    Cp {
        #[arg(value_name = "PATH", required = true)]
        paths: Vec<String>,
    },
    /// Run one command in the VM.
    Exec {
        #[arg(long)]
        fork: Option<String>,
        /// The command and its arguments, after `--`.
        #[arg(last = true, value_name = "CMD")]
        argv: Vec<String>,
    },
    /// List the base images.
    Images {
        /// One JSON document, for scripts.
        #[arg(long)]
        json: bool,
    },
    /// Interactive shell in the VM.
    Shell {
        #[arg(long)]
        fork: Option<String>,
    },
    /// Stop this project's VM now.
    Stop {
        #[arg(long)]
        fork: Option<String>,
    },
    /// List project VMs.
    Ls {
        /// Every project on this Mac.
        #[arg(long)]
        all: bool,
        /// One JSON document, for scripts.
        #[arg(long)]
        json: bool,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Cmd::Init {
            sealed,
            synced,
            image,
        } => init::run(sealed, synced, &image),
        Cmd::Run { net, ports, fork } => {
            let net = net.map(|n| match n {
                Net::Off => NetMode::Off,
                Net::Lan => NetMode::Lan,
            });
            commands::run(net, ports, fork)
        }
        Cmd::Cp { paths } => copy::run(paths),
        Cmd::Exec { fork, argv } => match exec::run(fork, argv) {
            Ok(code) => return ExitCode::from(code),
            Err(e) => Err(e),
        },
        Cmd::Fork { name, from } => snapshot::fork(name, from),
        Cmd::Images { json } => commands::images(json),
        Cmd::Restore { name, fork } => snapshot::restore(name, fork),
        Cmd::Rm { name, yes } => remove::run(name, yes),
        Cmd::Shell { fork } => match shell::run(fork) {
            Ok(code) => return ExitCode::from(code),
            Err(e) => Err(e),
        },
        Cmd::Snapshot { name, fork } => snapshot::snapshot(name, fork),
        Cmd::Stop { fork } => commands::stop(fork),
        Cmd::Ls { all, json } => commands::ls(all, json),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e}");
            ExitCode::from(e.code)
        }
    }
}
