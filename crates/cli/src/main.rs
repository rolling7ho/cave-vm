//! The `cave` command line, specified in `docs/cli.md`.

#![deny(unsafe_code)]

mod agent;
mod client;
mod commands;
mod copy;
mod doctor;
mod error;
mod exec;
mod export;
mod gc;
mod init;
mod inspect;
mod mirror;
mod policy;
mod project;
mod remove;
mod shell;
mod skill;
mod snapshot;
mod ssh;

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
    /// Start a coding agent against this project's VM.
    Agent {
        /// The agent: claude.
        name: String,
        /// Run the agent inside the VM instead of on the Mac.
        #[arg(long)]
        in_vm: bool,
        #[arg(long)]
        fork: Option<String>,
        /// Arguments for the agent, after `--`.
        #[arg(last = true, value_name = "ARGS")]
        args: Vec<String>,
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
    /// Copy the project out of the VM, or a snapshot, into a folder.
    Export {
        /// A snapshot from `cave ls`; the running VM when left out.
        snapshot: Option<String>,
        /// The folder; ./<project>-export-<date> when left out.
        #[arg(short = 'o', value_name = "DIR")]
        out: Option<std::path::PathBuf>,
    },
    /// Show disk usage and reclaim what is safe to delete.
    Gc {
        /// Delete without asking, for scripts.
        #[arg(long)]
        yes: bool,
        /// One JSON document, for scripts.
        #[arg(long)]
        json: bool,
    },
    /// Show the policy, resources and audit log.
    Inspect {
        #[arg(long)]
        fork: Option<String>,
        /// The whole audit log, not only the last 20 entries.
        #[arg(long)]
        log: bool,
        /// One JSON document, for scripts.
        #[arg(long)]
        json: bool,
    },
    /// Run the MCP server on stdio, for agents.
    #[command(hide = true)]
    Mcp {
        /// Act on this fork unless a call names another.
        #[arg(long)]
        fork: Option<String>,
    },
    /// Carry an SSH connection to a project's VM, for editors.
    #[command(hide = true)]
    SshProxy {
        /// The project folder.
        project: std::path::PathBuf,
        #[arg(long)]
        fork: Option<String>,
    },
    /// Check this Mac, cave's signature and the image cache.
    Doctor,
    /// List the base images.
    Images {
        /// One JSON document, for scripts.
        #[arg(long)]
        json: bool,
    },
    /// Bring Mac edits into the VM (synced mode).
    Pull,
    /// Update the Mac mirror from the VM (synced mode).
    Sync,
    /// Interactive shell in the VM.
    Shell {
        #[arg(long)]
        fork: Option<String>,
    },
    /// Suspend this project's VM now.
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
        Cmd::Agent {
            name,
            in_vm,
            fork,
            args,
        } => match agent::run(&name, in_vm, fork, args) {
            Ok(code) => return ExitCode::from(code),
            Err(e) => Err(e),
        },
        Cmd::Cp { paths } => copy::run(paths),
        Cmd::Doctor => match doctor::run() {
            Ok(code) => return ExitCode::from(code),
            Err(e) => Err(e),
        },
        Cmd::Exec { fork, argv } => match exec::run(fork, argv) {
            Ok(code) => return ExitCode::from(code),
            Err(e) => Err(e),
        },
        Cmd::Export { snapshot, out } => export::run(snapshot, out),
        Cmd::Fork { name, from } => snapshot::fork(name, from),
        Cmd::Gc { yes, json } => gc::run(yes, json),
        Cmd::Images { json } => commands::images(json),
        Cmd::Inspect { fork, log, json } => inspect::run(fork, log, json),
        Cmd::Mcp { fork } => project::current_dir().and_then(|dir| {
            let tools = cave_vm_mcp::Tools::new(dir).on_fork(fork);
            cave_vm_mcp::serve(std::io::stdin().lock(), std::io::stdout().lock(), &tools)
                .map_err(Into::into)
        }),
        Cmd::Pull => mirror::pull(),
        Cmd::Restore { name, fork } => snapshot::restore(name, fork),
        Cmd::Rm { name, yes } => remove::run(name, yes),
        Cmd::Shell { fork } => match shell::run(fork) {
            Ok(code) => return ExitCode::from(code),
            Err(e) => Err(e),
        },
        Cmd::Snapshot { name, fork } => snapshot::snapshot(name, fork),
        Cmd::SshProxy { project, fork } => ssh::proxy(&project, fork),
        Cmd::Stop { fork } => commands::stop(fork),
        Cmd::Sync => mirror::sync(),
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
