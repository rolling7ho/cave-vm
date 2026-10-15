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
mod links;
mod memory;
mod mirror;
mod policy;
mod project;
mod remove;
mod setup;
mod shell;
mod skill;
mod snapshot;
mod ssh;
mod style;
mod update;
mod view;

use std::process::ExitCode;

use cave_vm_proto::daemon::{NetMode, PortMap};
use clap::{CommandFactory, FromArgMatches, Parser, Subcommand, ValueEnum};

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
        /// The agent: claude or codex.
        name: String,
        /// Keep the agent's permission prompts.
        #[arg(long)]
        safe: bool,
        /// Run the agent on the Mac through cave mcp, with prompts.
        #[arg(long)]
        on_mac: bool,
        /// The default since D53; accepted so older scripts still work.
        #[arg(long, hide = true, conflicts_with = "on_mac")]
        in_vm: bool,
        #[arg(long)]
        fork: Option<String>,
        /// Arguments for the agent; cave's own flags go before them.
        #[arg(
            trailing_var_arg = true,
            allow_hyphen_values = true,
            value_name = "ARGS"
        )]
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
        /// The command and its arguments; cave's own flags go before it.
        #[arg(
            trailing_var_arg = true,
            allow_hyphen_values = true,
            value_name = "CMD"
        )]
        argv: Vec<String>,
    },
    /// Copy the project out of the VM, or a snapshot, into a folder.
    Export {
        /// The folder, created after asking if it does not exist;
        /// ./<project>-export-<date> when left out.
        folder: Option<std::path::PathBuf>,
        /// Export this snapshot, from `cave ls`, instead of the VM.
        #[arg(long, value_name = "SNAPSHOT")]
        from: Option<String>,
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
    /// Send Mac edits to the VM (synced mode).
    Push,
    /// The old name of `cave push`, until the next minor release (D42).
    #[command(hide = true)]
    Pull,
    /// Update the Mac mirror from the VM (synced mode).
    Sync {
        /// Keep updating it every 2 seconds until Ctrl-C.
        #[arg(long)]
        watch: bool,
    },
    /// Interactive shell in the VM.
    Shell {
        #[arg(long)]
        fork: Option<String>,
    },
    /// Suspend this project's VM now.
    Stop {
        #[arg(long)]
        fork: Option<String>,
        /// Stop every VM on this Mac at once, saving nothing, after asking.
        #[arg(long, conflicts_with = "fork")]
        force: bool,
    },
    /// Follow what happens in the VM live: commands, file writes and
    /// connections.
    View {
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

/// `cave` with a help page that shows each command's flags, not only its
/// one-line description.
fn command() -> clap::Command {
    use clap::builder::styling::{AnsiColor, Effects, Styles};
    // clap leaves these out on a pipe, with NO_COLOR, or with TERM=dumb.
    let styles = Styles::styled()
        .header((Effects::BOLD | Effects::UNDERLINE).into())
        .usage((Effects::BOLD | Effects::UNDERLINE).into())
        .literal(Effects::BOLD.into())
        .placeholder(AnsiColor::Cyan.on_default())
        .error(AnsiColor::Red.on_default() | Effects::BOLD)
        .valid(AnsiColor::Green.on_default())
        .invalid(AnsiColor::Yellow.on_default());
    let header = *styles.get_header();
    let mut cmd = Cli::command().styles(styles);
    cmd.build();
    let list = commands_help(&cmd);
    cmd.help_template(format!(
        "{{usage-heading}} {{usage}}\n\n{header}Commands:{header:#}\n{list}\n{header}Options:{header:#}\n{{options}}"
    ))
}

/// The help page's command list: each visible command with its arguments,
/// and its description on the line below.
fn commands_help(cmd: &clap::Command) -> String {
    let bold = *cmd.get_styles().get_literal();
    let mut out = String::new();
    for sub in cmd.get_subcommands().filter(|s| !s.is_hide_set()) {
        let mut line = format!("{bold}{}{bold:#}", sub.get_name());
        for arg in sub.get_arguments().filter(|a| !a.is_hide_set()) {
            if arg.get_id() == "help" {
                continue;
            }
            line.push(' ');
            line.push_str(&arg_usage(arg));
        }
        let about = sub.get_about().map(ToString::to_string).unwrap_or_default();
        out.push_str(&format!("  {line}\n      {about}\n"));
    }
    out.push_str(&format!("  {bold}help{bold:#} [COMMAND]\n"));
    out.push_str("      Print help for cave or one command\n");
    out
}

/// One argument as the usage line writes it: `--net <off|lan>`, `[NAME]`,
/// `-p <PORT>...`, wrapped in brackets unless it is required.
fn arg_usage(arg: &clap::Arg) -> String {
    let values = arg.get_value_names().map_or_else(
        || arg.get_id().to_string().to_uppercase(),
        |names| names.join(" "),
    );
    let takes_value = arg.get_num_args().is_some_and(|n| n.takes_values());
    let possible: Vec<String> = arg
        .get_possible_values()
        .iter()
        .filter(|v| !v.is_hide_set())
        .map(|v| v.get_name().to_string())
        .collect();
    let value = if possible.is_empty() {
        format!("<{values}>")
    } else {
        format!("<{}>", possible.join("|"))
    };
    let repeats = matches!(arg.get_action(), clap::ArgAction::Append)
        && arg
            .get_num_args()
            .is_none_or(|n| n.max_values() > 1 || !arg.is_positional());
    let mut text = if arg.is_positional() {
        let dashes = if arg.is_last_set() { "-- " } else { "" };
        if arg.is_required_set() {
            format!("{dashes}<{values}>")
        } else {
            format!("{dashes}[{values}]")
        }
    } else {
        let flag = arg.get_long().map_or_else(
            || format!("-{}", arg.get_short().unwrap_or('?')),
            |l| format!("--{l}"),
        );
        if takes_value {
            format!("{flag} {value}")
        } else {
            flag
        }
    };
    if repeats {
        text.push_str("...");
    }
    if arg.is_required_set() || arg.is_positional() {
        text
    } else {
        format!("[{text}]")
    }
}

fn main() -> ExitCode {
    // clap would print `cave 0.1.2`; this also names RCs and Betas and the
    // latest stable release.
    let args: Vec<String> = std::env::args().skip(1).collect();
    if matches!(args.as_slice(), [flag] if flag == "--version" || flag == "-V") {
        println!("{}", update::version_text());
        return ExitCode::SUCCESS;
    }
    let cli = match Cli::from_arg_matches(&command().get_matches()) {
        Ok(cli) => cli,
        Err(e) => e.exit(),
    };
    // Not for commands other programs talk to over stdin and stdout.
    if !matches!(cli.command, Cmd::Mcp { .. } | Cmd::SshProxy { .. }) {
        // An outdated copy may still copy the work out (D52).
        update::offer_update(!matches!(cli.command, Cmd::Export { .. }));
    }
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
            safe,
            on_mac,
            in_vm: _,
            fork,
            args,
        } => match agent::run(&name, safe, on_mac, fork, args) {
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
        Cmd::Export { folder, from } => export::run(folder, from),
        Cmd::Fork { name, from } => snapshot::fork(name, from),
        Cmd::Gc { yes, json } => gc::run(yes, json),
        Cmd::Images { json } => commands::images(json),
        Cmd::Inspect { fork, log, json } => inspect::run(fork, log, json),
        Cmd::Mcp { fork } => project::current_dir().and_then(|dir| {
            let tools = cave_vm_mcp::Tools::new(dir).on_fork(fork);
            cave_vm_mcp::serve(std::io::stdin().lock(), std::io::stdout().lock(), &tools)
                .map_err(Into::into)
        }),
        Cmd::Pull => {
            eprintln!("cave pull is now cave push; the old name goes in a later release");
            mirror::push()
        }
        Cmd::Push => mirror::push(),
        Cmd::Restore { name, fork } => snapshot::restore(name, fork),
        Cmd::Rm { name, yes } => remove::run(name, yes),
        Cmd::Shell { fork } => match shell::run(fork) {
            Ok(code) => return ExitCode::from(code),
            Err(e) => Err(e),
        },
        Cmd::Snapshot { name, fork } => snapshot::snapshot(name, fork),
        Cmd::SshProxy { project, fork } => ssh::proxy(&project, fork),
        Cmd::Stop { force: true, .. } => commands::force_stop(),
        Cmd::Stop { fork, .. } => commands::stop(fork),
        Cmd::Sync { watch } => mirror::sync(watch),
        Cmd::Ls { all, json } => commands::ls(all, json),
        Cmd::View { fork } => view::run(fork),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{}", e.render(style::Paint::stderr()));
            ExitCode::from(e.code)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_help_page_shows_each_commands_flags() {
        let help = command().render_help().to_string();
        assert!(help.contains("  run [--net <off|lan>] [-p <PORT>...] [--fork <FORK>]\n      Boot or restore this project's VM\n"), "{help}");
        assert!(help.contains("  rm [NAME] [--yes]\n"), "{help}");
        assert!(
            help.contains("  agent <NAME> [--safe] [--on-mac] [--fork <FORK>] [ARGS]...\n"),
            "{help}"
        );
        assert!(
            !help.contains("ssh-proxy"),
            "hidden commands stay hidden: {help}"
        );
    }
}
