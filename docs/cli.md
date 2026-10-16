# CLI reference

This is the reference for the `cave` command. It started as the spec written before any code (ROADMAP task P1.1), and every command in it is implemented as of v0.1.0.

[docs/design.md](design.md) is the source of truth. Every command in its CLI table is here, and the flags it names (`--net off`, `--net lan`, `-p 3000`, `--in-vm`, the `cave rm` confirmation) are written as the design specifies them. Everything the design does not name was proposed in the phase 1 PR and approved by Ivan there. Questions the design left open were answered by Ivan as D13 to D22 ([decision 0004](decisions/0004-cli-and-config-open-items.md)), and are linked from the places they affect.

The quickstart stays at two commands:

```sh
brew install rolling7ho/cave/cave-vm
cd my-project && cave agent claude
```

Without Homebrew, the release's install script puts the same binaries in `~/.local/bin`, or in `$CAVE_PREFIX/bin` when that is set. It checks the download against the checksum pinned in the script and signs `cave-vmm` ad hoc:

```sh
curl -fsSL https://github.com/rolling7ho/cave-vm/releases/latest/download/install.sh | sh
```

## Conventions

### The project

The project directory is the VM's identity. Commands that act on a VM find the project by looking for `cave.toml` in the current directory, then in each parent directory up to the home directory, the way git finds a repository ([D16](../ROADMAP.md#decisions)). Outside any project, those commands fail with "not a cave project" and suggest `cave init`, except for the ones below that set the project up themselves.

### Setting up on first use

`cave run`, `cave shell`, `cave exec` and `cave agent` work in a directory that has no `cave.toml`: they run `cave init` with its defaults first and say so in one line. This is what keeps the quickstart at two commands (design principle 1: `cave run` works with no config). It refuses to do this in the home folder itself ([D15](../ROADMAP.md#decisions)).

The first time `cave` runs on a Mac it also does the first-run setup from the design:

- Before anything else, it checks the Mac as `cave doctor` does: Apple Silicon, Hypervisor.framework and macOS 26 or later. A failed check stops here with its fix. Less than 5 GB free is a warning.
- It creates the data directory.
- It signs `cave-vmm` ad hoc with the hypervisor entitlement (`codesign -s -`, on this Mac, with no Apple account, keys or certificates), and says so. It checks this every time it starts the daemon, so a reinstalled `cave-vmm` is signed again. That ad hoc signing is enough was checked in the spike [A5].
- The first VM start downloads the kernel and the base image, verified against their digests, and shows the download's percentage, updated live on one line as each percent arrives. When stderr is not a terminal, it prints a line at every tenth instead.

### Forks

A fork is a VM that lives inside its parent project at `.cave/forks/<name>`, and forks are how a project has more than one VM ([D40](../ROADMAP.md#decisions)): they are picked by name, never by number. The commands that act on one VM (`run`, `stop`, `shell`, `exec`, `snapshot`, `restore`, `inspect`, `view`, `agent` and `mcp`) take `--fork <name>` to act on that fork instead of the project's own VM, for example `cave exec --fork try-2 npm test` or `cave stop --fork try-2` ([D18](../ROADMAP.md#decisions)), and `cave rm try-2` removes one. `cave ls` lists them.

### Names

Snapshots and forks are named, never numbered. A project's snapshot and fork names share one namespace, so a name means one thing. A default snapshot name follows the design: `[ProjectFolder]-[YYYY-MM-DD]-[Distro]`, for example `my-project-2026-10-12-debian`, with `-2`, `-3` and so on appended on collisions. A name is 1 to 64 characters of letters, digits, `.`, `_` and `-`, starting with a letter or digit.

### Output

- Results go to stdout. Progress, the startup policy and errors go to stderr, so `cave exec -- cat file > out` captures only the file.
- `cave ls`, `cave inspect`, `cave images` and `cave gc` take `--json` and print one JSON document for scripts.
- Colour and progress bars appear only when stderr is a terminal, and `NO_COLOR` turns colour off. On a terminal, `error:` is red and `fix:` green, `warning:` yellow, headings and labels bold or underlined, and results coloured: an allowed connection or `exit 0` green, a denied connection red, another exit code yellow, a running VM green, and the flags that widened the policy yellow. `NO_COLOR`, `TERM=dumb` or output to a file or a pipe leaves all of it plain.

### Errors

Every error says what went wrong and how to fix it, in this shape:

```
error: not a cave project (no cave.toml here or in any parent folder)
  fix: Run `cave init` to set up this folder
```

Warnings use the same shape with `warning:` and do not change the exit code.

### Exit codes

| Code | Meaning |
| --- | --- |
| 0 | Success |
| 1 | The command failed; the message says how to fix it |
| 2 | Usage error: unknown command or flag, or a bad argument |
| 130 | Interrupted with Ctrl-C |

`cave exec`, `cave shell` and `cave agent` exit with the code of the program they ran. To keep cave's own failures distinguishable from the program's, they use 125 when cave fails before or while running it, 126 when the program exists but cannot be run, and 127 when it is not found, following the shell and Docker conventions.

### Global flags

| Flag | Meaning |
| --- | --- |
| `-h`, `--help` | Show help for `cave` or a command |
| `-V`, `--version` | Print the version, such as `v0.1.2`, `v0.1.2 RC 1` or `v0.1.3 Beta 1`, and on a second line the newest stable release cave has seen: `Latest Stable: v0.1.1`, or `not looked up yet` before the first lookup |

There are no other global flags. `cave --help` lists every command with its flags and arguments, and `cave <command> --help` explains each flag.

### Updates

When `cave` runs in a terminal, it looks up the newest stable release in the background, at most every 10 minutes ([D47](../ROADMAP.md#decisions)). When the last lookup found one newer than this copy, it asks before the command runs:

```
update: cave v0.1.3 is out; you have v0.1.2 RC 2. Update now? [Y/n]
```

Yes installs it the way this copy was installed, with Homebrew or the install script, then exits so you run the command again. No skips that version for a day. A copy built from source is never offered an update. It never asks without a terminal, nor for `cave mcp` and `cave ssh-proxy`, and `check = false` under `[updates]` in `cave.toml` turns it off ([config](config.md#updates)). Pre-releases are never offered. A copy 2 or more stable releases behind is asked every time, with a note that only the 3 newest releases get security fixes, and a no stops the command, except `cave pull`, so the work can always be copied out; without a terminal it only warns, and a copy cave cannot update only warns ([D52](../ROADMAP.md#decisions)).

### The daemon and the data directory

The CLI starts the daemon on demand, so there is no service to install. VMs survive the CLI exiting because the daemon owns them. After a reboot, nothing starts until a `cave` command needs the VM, which then restores from its last snapshot.

Shared state (kernels, base images, the daemon socket and the list of known projects) lives in `~/Library/Application Support/cave-vm/`. The `CAVE_HOME` environment variable moves it, which the tests use.

### The startup policy

The design says `cave run` prints the policy at startup and echoes widening flags back. Every command that boots a VM (`run`, `shell`, `exec`, `agent`) prints it to stderr when it boots or restores one, and prints nothing if the VM was already running, except `cave run` and `cave agent`, which print it again:

```
my-project: started in 0.4s (debian, sha256:3f2a9c1e)
  files     sealed: nothing on your Mac is reachable from the VM
  network   nat: outbound only; LAN ranges and this Mac are blocked
  ports     none
  resources 2 CPUs, 2 GB memory, 20 GB disk
  lifetime  no TTL
```

A widened item is marked with the flag that widened it:

```
  network   lan: outbound, and LAN ranges are reachable   [--net lan]
  ports     localhost:3000 -> 3000                         [-p 3000]
```

## Commands

The design's verb set, in its order, plus `cave stop` (D20) and `cave mcp`, added in phase 1.

| Command | What it does |
| --- | --- |
| [`cave init`](#cave-init) | Create `cave.toml`, `.cave/` and the agent skill files for this directory |
| [`cave run`](#cave-run) | Boot or restore this project's VM |
| [`cave stop`](#cave-stop) | Suspend this project's VM now |
| [`cave shell`](#cave-shell) | Interactive shell in the VM |
| [`cave exec <cmd>`](#cave-exec) | Run a command in the VM |
| [`cave snapshot [name]`](#cave-snapshot) | Save VM state |
| [`cave restore [name]`](#cave-restore) | Roll back to a snapshot |
| [`cave fork [name]`](#cave-fork) | Create a new VM from a snapshot, copy-on-write |
| [`cave ls`](#cave-ls) | List project VMs |
| [`cave rm [name]`](#cave-rm) | Remove a VM, fork or snapshot |
| [`cave gc`](#cave-gc) | Show and reclaim disk |
| [`cave inspect`](#cave-inspect) | Show the policy, resources and audit log |
| [`cave view`](#cave-view) | Follow what happens in the VM live |
| [`cave sync`](#cave-sync) | Update the Mac mirror (synced mode) |
| [`cave push`](#cave-push) | Copy your Mac edits into the VM |
| [`cave cp`](#cave-cp) | Copy files in and out |
| [`cave pull`](#cave-pull) | Copy the project's files out of the VM |
| [`cave agent <name>`](#cave-agent) | Start an agent inside this VM, without permission prompts |
| [`cave images`](#cave-images) | List base images |
| [`cave doctor`](#cave-doctor) | Check entitlements, macOS version, disk space and image state |
| [`cave mcp`](#cave-mcp) | Run the MCP server on stdio (plumbing for agents) |

---

### cave init

```
cave init [--sealed | --synced] [--image <name>]
```

Sets up the current directory as a project. It writes:

- `cave.toml` with the design's minimal keys (see [config.md](config.md)).
- `.cave/`, and adds `.cave/` to `.gitignore`, creating the file if needed.
- The agent skill files: the skill's text in `.cave/AGENTS.md`, and a line in `AGENTS.md` telling agents to read it (existing content is kept; the line sits between marker comments so it can be updated, and an `AGENTS.md` is created with just the line if there is none) ([D37](../ROADMAP.md#decisions)), a `.claude/skills/` entry, and, for agents that support it, a permissions config that denies host Bash. The skill's text is [`skill/cave/SKILL.md`](../skill/cave/SKILL.md) in this repository, the same text as the bootstrap skill on the website ([docs/bootstrap.md](bootstrap.md)). For Claude Code that means:
  - `.claude/skills/cave/SKILL.md`: the skill.
  - `.cave/AGENTS.md`: the skill without its frontmatter. It is written again by the next command that boots the VM if `cave rm` removed it.
  - `AGENTS.md`: `This project runs in a cave VM. Before doing anything, read .cave/AGENTS.md: it says how to work inside the VM.`, between `<!-- cave:start -->` and `<!-- cave:end -->`. A section an older cave wrote there, with the whole text, is replaced by this line.
  - `.mcp.json`: registers `cave mcp` as the `cave` server.
  - `.claude/settings.json`: adds `Bash`, `Read(./.env)`, `Edit`, `Write` and `NotebookEdit` to `permissions.deny`, so Claude Code has no shell on the Mac and reaches the VM through MCP, cannot read the credentials in `.env`, and cannot edit Mac files, whose edits would never reach the VM. Claude Code asks once to approve the `cave` server, since a project's own settings cannot approve it.
  - `.claude/settings.local.json`, only after a yes to the question below: `permissions.defaultMode` set to `dontAsk`, `mcp__cave` allowed, and `Read`, `Glob`, `Grep` and `WebFetch` denied as well as the list above, so plain `claude` never prompts and can act only through the `cave` tools; any other tool is turned away without asking. Claude Code ignores `bypassPermissions` in project and local settings, so it is not used, and a file an earlier cave wrote with it is moved to `dontAsk`. The file is personal, so it is added to `.gitignore` ([D55](../ROADMAP.md#decisions)).

  Other settings in those files are kept. A file that already has these entries is not rewritten. A file that is not valid JSON is an error that says to fix it, and is left alone.

It does not boot the VM. The files already in the folder, everything except `.cave/` and `.env`, are copied into the VM once when it first boots. `.env` stays on the Mac because it holds the credentials ([D33](../ROADMAP.md#decisions)). In sealed mode the Mac files are left untouched but are no longer updated, and cave says so ([D13](../ROADMAP.md#decisions)). The first command that boots the VM prints, in sealed mode:

```
Copied this folder's files into the VM, where the code lives now. The files on your Mac are left as they are and no longer updated
```

and in synced mode `Copied this folder's files into the VM. The VM's copy is the one to work on; cave sync updates this folder from it`. File modes and symbolic links are kept, and the files belong to the VM's `cave` user. If the copy fails, the command says so, and the next command that starts the VM tries again.

When stdin is a terminal and neither `--sealed` nor `--synced` is given, it asks the design's setup question, with sealed as the default:

```
Keep a copy of the code on your Mac? It lets you use your Mac editor and Spotlight. [y/N]
```

and next to it whether plain `claude` may skip its permission prompts, with no as the default ([D55](../ROADMAP.md#decisions)):

```
Let Claude Code skip its permission prompts in this project? Its own tools on the Mac are turned off, so it can act only inside the VM, which is its sandbox. [y/N]
```

That second question is asked whenever stdin is a terminal, with or without `--sealed` and `--synced`, also when `cave.toml` already exists, unless `.claude/settings.local.json` already skips the prompts from an earlier yes. Without a terminal it picks sealed and no, and says so for sealed. MCP servers and plugins from your own Claude Code settings run on the Mac only where your own settings allow them ([docs/safety.md](safety.md#the-agent-boundary)).

Running `cave init` again in a project is safe: it refreshes the skill files and never changes `cave.toml`.

| Flag | Default | Meaning |
| --- | --- | --- |
| `--sealed` | yes | Code lives only on the VM disk. |
| `--synced` | | Also keep a Mac mirror of the code. The mirror is the project folder itself ([D14](../ROADMAP.md#decisions)). |
| `--image <name>` | `base` | Base image, from `cave images`. |

Output:

```
Created cave.toml (sealed, debian, 2 CPUs, 2 GB)
Added .cave/ to .gitignore
Wrote .cave/AGENTS.md, a line pointing to it in AGENTS.md, and .claude/skills/cave
Set up Claude Code to use cave mcp, with host Bash, Mac edits and reading .env denied (.mcp.json, .claude/settings.json)
Added an Include line to ~/.ssh/config for cave's SSH hosts
Edit in the VM: connect VS Code Remote-SSH or Zed to my-project.cave
Next: cave shell, or cave agent claude
```

Errors:

| Error | Fix |
| --- | --- |
| `this folder is inside the project at <path>` | Run cave from that project, or move this folder out of it |
| `cave cannot set up a project in your home folder itself` | Run `cave init` in a project folder instead. |
| `unknown image "<name>"` | Run `cave images` to see the image names |

### cave run

```
cave run [--net off|lan] [-p <port>]... [--fork <name>]
```

Boots this project's VM, or restores it if it is suspended, and returns once the guest agent answers. It prints the startup policy.

If the VM is already running and `cave run` is given no flags in a terminal, it offers to start another VM for the project ([D40](../ROADMAP.md#decisions)):

```
my-project is already running. Start another VM, a fork of it? [y/N]
```

Yes runs `cave fork`, so the new VM is a fork of the running one's current state, named `fork-1`, `fork-2` and so on. Enter cancels and changes nothing. With flags, or without a terminal, it says the VM is already running and prints the policy again, so `cave run --fork <name>` or `cave inspect` is the way to look at what a running VM can reach.

`cave shell`, `cave exec` and `cave agent` boot the VM themselves, so `cave run` is needed only to pass flags or to start a VM ahead of time.

| Flag | Default | Meaning |
| --- | --- | --- |
| `--net off` | | No network device at all (design) |
| `--net lan` | | Allow private and link-local ranges as well (design). Echoed back at startup |
| `-p <port>` | none | Forward `localhost:<port>` on the Mac to the same port in the VM (design: `-p 3000`). `-p <mac>:<vm>` maps different ports. Repeatable. Forwards listen on localhost only |
| `--fork <name>` | | Run a fork instead ([D18](../ROADMAP.md#decisions)) |

Without `--net`, the mode comes from `cave.toml`, which is `nat` by default. Flags apply to this boot only; there is no way to make `--net lan` or a port forward permanent in `cave.toml`, so a cloned repository cannot widen access on its own ([config.md](config.md#network)).

On a running VM, `-p` adds a forward without a restart, because forwarding goes through the guest agent. `--net` takes effect only when the VM boots, so on a running VM a different mode is an error that says to `cave stop` first.

Errors:

| Error | Fix |
| --- | --- |
| `my-project is running with network "nat"; --net lan needs a restart` | `cave stop`, then `cave run --net lan` |
| `port 3000 on this Mac is already in use` | Pick another Mac port: `cave run -p 3001:3000` |
| `cannot clone the base image into <folder>: the folder must be on an APFS volume, the same one as your home folder` [A9] | Move the project to a folder on your Mac's main disk |
| `the VM did not boot: <reason>` | See `vmm.log` in the VM's folder for the VMM's output |

Warning, from the design: `memory is getting tight: N VMs are running and M GB is free`. It is printed after a VM boots, by `cave run` and by every command that boots one, when macOS reports memory pressure or when less memory is available than the VM's `memory` cap. There is no cap on the number of VMs, so the VM boots anyway; `cave stop` frees the memory of one you are not using.

### cave stop

Added by [D20](../ROADMAP.md#decisions). The design has running, suspended and stopped states and auto-suspend after an idle time, but no verb to suspend a VM now. Without one, changing `--net` or freeing memory means waiting for the idle timer.

```
cave stop [--fork <name>]
cave stop --force
```

Suspends the VM to a snapshot, freeing its memory, and prints `Suspended <name> (1.2s)`. The next command that needs the VM restores it with processes intact, unless it changes `--net` between `off` and another mode, which boots from the disk ([suspensions](snapshots.md#suspensions)). If the VM is not running, it says so and exits 0. If the VM cannot be saved, it keeps running and the error says why.

`cave stop --force` is the emergency stop ([D43](../ROADMAP.md#decisions)). It stops every running VM on this Mac, in every project, at once and saves nothing: each VMM process gets SIGTERM, and SIGKILL if it is still running 5 seconds later. It names them and asks first:

```
Stop 2 VMs now without saving: try-a (fork of my-project), my-project?
Processes running in them are lost; their disks keep what was written. [y/N]
```

Then it prints `Stopped 2 VMs without saving: try-a, my-project. The next start boots each from its disk`. Without a terminal it refuses, since it cannot ask. With no VM running it says so and exits 0.

### cave shell

```
cave shell [--fork <name>]
```

Opens an interactive shell in the VM through the guest agent, with a pseudo-terminal that follows window resizes. It boots the VM first if needed. The shell starts in the project directory inside the VM ([guest layout](#guest-layout)). Exiting the shell does not stop the VM.

To leave, type `exit` or press Ctrl-D; `cave shell` says so when it starts. When a program in the VM holds the keyboard, `~.` at the start of a line leaves at once, as in ssh, and `~~` types one `~`. The escape is only in `cave shell`, not in `cave agent`, which runs the agent inside the VM.

Exit code: the shell's.

Errors:

| Error | Fix |
| --- | --- |
| `cave shell needs a terminal` | Use `cave exec <cmd>` from scripts |
| `lost the connection to the guest agent: <reason>` | Run `cave stop`, then `cave run`; the guest's console is in `vmm.log` in the VM's folder |

### cave exec

```
cave exec [--fork <name>] <cmd> [args...]
```

Runs one command in the VM and streams its stdout and stderr back, keeping them separate. Stdin is passed through. It boots the VM first if needed. The command runs in the project directory inside the VM, without a shell, so `cave exec sh -c 'cd sub && make'` is the way to use shell syntax. There is no pseudo-terminal and no `-t` flag; use `cave shell` for interactive programs.

Everything from the command on belongs to the command, so cave's own flags go before it: in `cave exec --fork try-2 npm test --fork x`, the second `--fork x` goes to `npm`. A `--` before the command still works, as in `cave exec -- uname -a` ([D35](../ROADMAP.md#decisions)).

Exit code: the command's, or 125, 126 or 127 as described under [exit codes](#exit-codes).

Errors:

| Error | Fix |
| --- | --- |
| `missing the command to run` | `cave exec uname -a` |
| `cave: <cmd>: command not found` on stderr, exit 127 (126 when it cannot be run) | Install it in the VM, for example `cave exec sudo apt-get install -y <pkg>` |

### cave snapshot

```
cave snapshot [name] [--fork <name>]
```

Saves the VM's disk state, memory state, VM config and metadata. With no name it uses the default name. On a stopped VM it saves disk state only, and restoring it boots normally.

Output: `Saved snapshot my-project-2026-10-12-debian (1.2s)`.

Errors:

| Error | Fix |
| --- | --- |
| `a snapshot or fork named "<name>" already exists` | Pick another name, or remove it with `cave rm <name>` |

### cave restore

```
cave restore [name] [--fork <name>]
```

Rolls the VM back to a snapshot, by default the most recent one. Processes in the snapshot resume where they were. Open shells and running `cave exec` calls are disconnected first, and port forwards are re-applied.

Before rolling back, it saves the current state as an automatic snapshot, so a restore can itself be undone. Automatic snapshots are deleted by the retention rules in `cave.toml` ([`[snapshots]`](config.md#snapshots)), once the restore is done, so restoring an old automatic snapshot does not delete it. With `auto = false` nothing is saved first, and the output has no second line. With no name it restores the newest snapshot, passing over the ones restores save, so after an agent run it goes back to the snapshot taken before the run, and a second `cave restore` does not just undo the first.

Output:

```
Restored before (0.9s)
The state before was saved as my-project-2026-10-12-debian-2; `cave restore my-project-2026-10-12-debian-2` goes back to it
```

If the memory state cannot be loaded, for example after a `cave` upgrade that cannot read it, restore falls back to the disk state and a normal boot, and says so (design):

```
warning: could not load the memory state of my-project-2026-10-12-debian (format 1 is not supported)
  restored the disk state and booted normally; processes from the snapshot were not kept
```

If injected credentials were recorded in the snapshot's metadata, it names them:

```
warning: snapshot before was taken after API_KEY was injected
  the VM's memory may contain it
```

Errors:

| Error | Fix |
| --- | --- |
| `no snapshot named <name>` | Run `cave ls` to see this project's snapshots |
| `this project has no snapshots yet` | Take one with `cave snapshot` |

### cave fork

```
cave fork [name] [--from <snapshot>]
```

Creates a new VM from a snapshot, copy-on-write, stored at `.cave/forks/<name>` (design). The design left two things open, answered by [D18](../ROADMAP.md#decisions):

- `name` is the new fork's name. Without it, forks are named `fork-1`, `fork-2` and so on.
- The source is a fresh snapshot of the VM's current state. `--from <snapshot>` forks an existing snapshot instead.

The fork starts running. Use it with `--fork <name>` on other commands. Forking a snapshot that recorded injected credentials shows the design's warning:

```
warning: snapshot my-project-2026-10-12-debian was taken after API_KEY was injected
  the fork's memory may contain it
```

Forking the VM's current state names the fresh snapshot it was made from as `the snapshot of my-project`.

Output: `Forked fork-1 from my-project-2026-10-12-debian (0.3s). Use it with: cave shell --fork fork-1`.

Errors: the same name errors as `cave snapshot`, and `no snapshot named <name>` for `--from`.

### cave ls

```
cave ls [--all] [--json]
```

Inside a project, lists this project's VM, its forks and its snapshots. With `--all`, or outside a project, lists every project VM on this Mac that the daemon knows about. The daemon keeps a list of project folders it has run, pruned when a folder is gone, because the design has no registry ([D17](../ROADMAP.md#decisions)).

```
NAME                            KIND      STATE      SIZE    CREATED
my-project                      vm        running    3.1 GB  2026-10-12 09:14
fork-1                          fork      suspended  0.2 GB  2026-10-12 11:02
my-project-2026-10-12-debian    snapshot  -          1.4 GB  2026-10-12 10:58
my-project-2026-10-12-debian-2  snapshot  auto       0.6 GB  2026-10-12 11:01
```

States are the design's: running, suspended, stopped. Sizes count blocks shared through APFS clones once, in the first row that uses them [A9].

### cave rm

```
cave rm [name] [--yes]
```

With no name, removes this project's VM: `.cave/` with its disk, snapshots and forks. `cave.toml`, `AGENTS.md` and the rest of the folder stay. With a name, removes that fork or snapshot.

Because `.cave/` can hold the only copy of the work in sealed mode, it asks first (design):

```
Remove the VM for my-project? Its disk, snapshots and forks are deleted (4.8 GB).
This project is sealed, so code that is only in the VM is lost. [y/N]
```

With a name it asks `Remove <name>? [y/N]`. `--yes` skips the question, for scripts. Without a terminal and without `--yes`, it refuses.

Exit code: 1 when the answer is no.

### cave gc

```
cave gc [--yes] [--json]
```

Shows disk usage and what can be reclaimed, then asks before deleting anything. It only offers things that are safe to delete: base images no project uses, automatic snapshots beyond the retention in `cave.toml`, and leftovers from crashed VMM processes. Named snapshots and VMs are never offered; remove those with `cave rm`.

```
Images        2.1 GB   debian (in use), ubuntu (unused, 1.9 GB)
my-project    4.8 GB   disk 3.1 GB, snapshots 1.5 GB, forks 0.2 GB
Reclaimable   2.3 GB   unused images 1.9 GB, old automatic snapshots 0.4 GB
Delete 2.3 GB? [y/N]
```

Shared APFS clone blocks are reported once, not double-counted [A9]. Sizes come from where each file's blocks sit on the volume, and each block counts in the first row that uses it: a project's disk counts only what its VM wrote over the base image, and a snapshot only what changed since the disk it was cloned from. A project's suspension counts with its snapshots. Each reclaimable entry is what deleting it frees on top of the entries before it, so they add up to the total.

Leftovers from crashed VMM processes are snapshot, fork and suspension folders with no manifest, listed as `unfinished snapshots` once they are 10 minutes old. Nothing of a VM that is starting, stopping or being snapshotted is offered. With nothing to delete it prints `Nothing to reclaim`. Without a terminal it needs `--yes`. `--json` prints the daemon's `usage` reply ([protocol](protocol.md#reply-objects)), after deleting when `--yes` is given.

### cave inspect

```
cave inspect [--fork <name>] [--log] [--json]
```

Shows the policy (the same lines as the startup policy), resources, and the most recent 20 audit-log entries: exec calls, file writes and network connections. `--log` prints the whole log. Credentials appear by name only. The log is in the VM's folder (`.cave/`, or `.cave/forks/<name>/` for a fork), one JSON object per line, and is removed with the VM. The newest entries are in `audit-latest.log`. When it passes 10 MB it is renamed `audit-<YYYY-MM-DD>-<HHMMSS>.log`, for the time it was renamed, and a new `audit-latest.log` starts ([D36](../ROADMAP.md#decisions)). `--log` and the entry count cover every file. A VM's `audit.log` from cave 0.1.0 becomes its `audit-latest.log` on the next write.

```
my-project: running since 09:14 (debian, sha256:3f2a9c1e)
  files     sealed: nothing on your Mac is reachable from the VM
  network   nat: outbound only; LAN ranges and this Mac are blocked
  ports     localhost:3000 -> 3000
  resources 2 CPUs, 2 GB memory, 3.1 GB of 20 GB disk used
  lifetime  no TTL

Audit log (last 20 of 412, cave inspect --log for all)
  11:04:12  exec     npm test                          exit 0
  11:04:40  write    src/app.ts                        1.2 KB
  11:05:01  connect  registry.npmjs.org:443            allowed
  11:05:03  connect  192.168.1.10:445                  denied (LAN)
  11:06:20  exec     ./deploy.sh   credentials: API_KEY   exit 0
```

### cave view

```
cave view [--fork <name>]
```

Added by [D39](../ROADMAP.md#decisions). Follows the VM's audit log as it is written, so you can watch what an agent does: each command with its exit code, each file written through cave, and each connection, allowed or denied. It starts with the last 10 entries, then prints new ones as they happen, in the same rows as `cave inspect`, until Ctrl-C. It reads the log from the VM's folder, so it works whether or not the VM is running, and follows it when an old part is moved aside ([D36](../ROADMAP.md#decisions)).

```
Following what happens in my-project: commands, file writes and connections. Ctrl-C stops
  11:04:12  exec     npm test                          exit 0
  11:04:40  write    /workspace/src/app.ts             1.2 KB
  11:05:03  connect  192.168.1.10:445                  denied (LAN)
```

Connections show up within about a second, since the daemon collects them from the VM every second.

### cave sync

```
cave sync [--watch]
```

Synced mode only. Updates the Mac mirror from the VM now; the daemon also does it after agent runs and on snapshot (design). If the VM is not running, the sync waits until it is, and the command says so: it blocks and prints "waiting for the VM" rather than returning.

The VM is authoritative, so a file changed only in the VM is written to the mirror, and one deleted in the VM is deleted there. A file you changed on the Mac is never written over: it waits for `cave push`, and one changed on both sides is left alone and listed. The same folders as `cave push` are excluded, as are `.git`, `.cave` and the top-level `.env`. The hashes both sides agreed on are kept in `.cave/sync.json`. Writes go through a temporary file, so an editor never sees half a file.

```
Wrote 2 files to your Mac: src/app.ts, src/util.ts
1 file you changed on your Mac waits for `cave push`: README.md
```

`--watch` is the live mode ([D41](../ROADMAP.md#decisions)): it says `Updating the Mac mirror from the VM every 2 seconds. Ctrl-C stops`, syncs every 2 seconds until Ctrl-C, and prints each pass's writes and deletions with the time. Files waiting for `cave push` or left alone are listed again only when that list changes, and `The Mac mirror is up to date` is said once, at the start. It is one-way, from the VM to the Mac, with the same rules as one `cave sync`; edits on the Mac still reach the VM only through `cave push`. Each pass compares every file, so on a large project it keeps the VM a little busy.

```
12:04:05  Wrote 1 file to your Mac: src/app.ts
12:04:09  Deleted 1 file on your Mac to match the VM: src/old.ts
```

The mirror backs up source only: it lacks dependencies, build output, `.git` and running processes, so snapshots remain the safety net ([docs/safety.md](safety.md#the-mac-mirror-backs-up-source-only)). When nothing changed in the VM it prints `The Mac mirror is up to date`. After `cave snapshot` and at the end of `cave agent`, the sync runs on its own; a failed one is reported and does not change the command's result.

Errors:

| Error | Fix |
| --- | --- |
| `this project is sealed, so it has no Mac mirror` | Set `mode = "synced"` under `[storage]` in cave.toml to keep a copy on your Mac |
| `forks have no Mac mirror` | Sync the project's own VM, without --fork |

### cave push

```
cave push [--force]
```

Copies the files you changed on the Mac into the VM, following the design's rules exactly. It was called `cave pull` before [D42](../ROADMAP.md#decisions); since 1.0.0 that name is [`cave pull`](#cave-pull), which copies the other way ([D58](../ROADMAP.md#decisions)). Neither has anything to do with `git push` or `git pull`. The daemon's request is still `pull`:

- It compares the SHA-256 of each file in the Mac mirror against the VM's.
- A file that changed only on the Mac is applied. One that changed only in the VM is left alone.
- A file that changed on both sides since the last sync stops the push before anything is applied, and is listed.
- A snapshot is taken automatically before anything is applied.
- Dependency and build folders (`node_modules`, `target`, and paths in `.gitignore`) are excluded.
- `--force` applies the Mac's version of the files changed on both sides instead of stopping, after the same snapshot.

In a sealed project ([D58](../ROADMAP.md#decisions)), the Mac files are compared against what the first boot copied in, recorded in `.cave/sync.json`, and from then on against what the last push left. A file deleted on the Mac is not deleted in the VM, since the Mac's files are not kept up to date there. A sealed project set up before 1.0.0 has no such record, so `cave push` stops with `cave has no record of the files it first copied into this VM, so it cannot tell your edits from the VM's` and says to run `cave push --force`, which copies every file that differs.

```
Saved snapshot my-project-2026-10-12-debian-3 before pushing
Applied 3 files from your Mac: src/app.ts, src/util.ts, README.md
```

On a conflict, exit code 1:

```
error: 2 files changed both on your Mac and in the VM since the last sync, so nothing was pushed
  src/app.ts
  package.json
  fix: keep one version of each file (copy it with cave cp, or undo the edit on your Mac) and run cave push again, or run cave push --force to overwrite the VM's version
```

When nothing changed on the Mac, no snapshot is taken and it prints `Nothing to push: the VM has every edit from your Mac`. The VM must be running.

In a sealed project the message ends `since the last push`. Errors: `forks have no Mac copy to push` for a fork.

### cave cp

```
cave cp <src>... <dest>
```

Copies files between the Mac and the VM. A path starting with `:` is in the VM, relative to the project directory there unless it is absolute; anything else is on the Mac, as with `scp`. Directories are copied recursively. Exactly one side must be in the VM.

```sh
cave cp ./fixtures :test/fixtures    # Mac to VM
cave cp :dist/app.tar.gz .           # VM to Mac
```

Errors:

| Error | Fix |
| --- | --- |
| `one side of cave cp must be in the VM` | Prefix the VM path with `:`, for example `:src/app.ts` |
| `no such file in the VM: <path>` | Check the path with `cave exec -- ls <dir>` |

### cave pull

```
cave pull [folder] [--from <snapshot>]
```

Decided by [D19](../ROADMAP.md#decisions), with the arguments from [D38](../ROADMAP.md#decisions). It was called `cave export` before 1.0.0, and that name still works, hidden from help ([D58](../ROADMAP.md#decisions)). It has nothing to do with `git pull`. The design lists it with `cave cp` as the way to "move files in and out" and as an escape hatch for code in sealed mode. `cave pull` copies the project directory out of the VM, or out of a snapshot's disk, into a folder on the Mac. `--from <snapshot>` copies that snapshot instead of the VM. Without a folder, it is `./<ProjectFolder>-export-<YYYY-MM-DD>`. A folder you name that does not exist is created after asking `<folder> does not exist. Create it? [Y/n]`, where Enter means yes; without a terminal it is created, and cave says so. It copies everything, including `.git`, and leaves out nothing, so it is a full copy of the work. Copying from a snapshot that recorded injected credentials shows the same warning as `cave fork`, ending in `the copied files may contain it`.

Output: `Copied 1,204 files (38 MB) to ./my-project-export-2026-10-12`.

The copy goes through `tar` in the VM, as `cave cp` copies folders. Without a snapshot it boots or resumes the VM if needed. A snapshot is read through a temporary fork named `export-<number>`, started from it and removed when the copy ends. A folder that already has files in it is refused rather than written over, and a failed copy removes the folder it created.

### cave agent

```
cave agent <name> [--safe] [--on-mac] [--fork <name>] [<agent args>...]
```

Starts a coding agent against this project's VM (design, [decision 0026](decisions/0026-agent-runs-in-the-vm-in-full-auto.md)):

1. Sets up the project and boots the VM if needed, and prints the startup policy.
2. Takes a snapshot, because the design calls for one before each agent run. It is an automatic snapshot, so it counts towards `keep` in `cave.toml` ([D51](../ROADMAP.md#decisions)).
3. Prints `Permissions skipped. The VM is the sandbox.` and starts the agent inside the VM with its permission prompts skipped, since its tools reach only the VM. On first use it installs the agent inside the VM with the agent's own installer, and you sign in inside the VM. The agent needs the VM's network to reach its model API.
4. When the agent exits, in synced mode it updates the Mac mirror as `cave sync` does and prints what changed. It then prints how to undo the run:

```
Snapshot before this run: my-project-2026-10-12-debian-4
Undo everything the agent did with: cave restore my-project-2026-10-12-debian-4
```

`--safe` keeps the agent's permission prompts.

`--on-mac` runs the agent on the Mac instead, connected to this VM through the MCP server, with host Bash denied where the agent supports it. The model API key stays on the Mac. Its prompts are never skipped there, and the agent's own flags that would skip them are refused. `--in-vm`, the way to run inside the VM before D53, is still accepted and changes nothing.

Arguments after the agent's name go to the agent unchanged, once cave's own flags (`--safe`, `--on-mac`, `--fork`) are read: `cave agent claude --safe --resume` passes `--resume`. A `--` before them still works ([D35](../ROADMAP.md#decisions)).

Denying Bash is a guardrail with limits: Claude Code keeps its other tools on the Mac, and an agent started some other way keeps its shell. [docs/safety.md](safety.md#the-agent-boundary) lists them.

Agents in v1: `claude` (Claude Code) and `codex` (Codex, [D44](../ROADMAP.md#decisions), [D54](../ROADMAP.md#decisions)).

With `--on-mac`, `cave` starts `claude` in the project folder with `--mcp-config`, naming `cave mcp` as the `cave` server (`cave mcp --fork <name>` with `--fork`), and `--disallowedTools=Bash,Read(./.env),Edit,Write,NotebookEdit`, which takes the Bash tool and its file-editing tools away from it and stops its Read and Grep tools from opening `.env`. Without the second rule, Claude Code on the Mac could read the credentials straight from `.env`. All of this was checked with Claude Code 2.1 on a Mac. In `--fork` runs there is no snapshot first, since forks cannot be snapshotted yet, and `cave` says so.

Inside the VM, `cave` installs Claude Code with `curl -fsSL https://claude.ai/install.sh | bash` if it is not there, then runs it with `--dangerously-skip-permissions` (left out with `--safe`) on a pseudo-terminal like `cave shell`, so it needs a terminal. It also passes `--setting-sources user` and `--strict-mcp-config`, because the project's `.claude/settings.json` and `.mcp.json` were copied into the VM with the folder and are written for Claude Code on the Mac: their deny rules would take away its shell in the VM, and `cave mcp` does not exist there. Your own settings in the VM still apply. An appended system prompt tells it that it is already inside the VM, so the project's skill and `AGENTS.md` line, which say to use `cave`, do not apply. Codex in the VM gets the same note as `developer_instructions`.

Signing in inside the VM: the VM has no browser, so `cave agent` installs a helper at `~/.local/bin/cave-open` and starts the agent with `BROWSER` set to it. When Claude Code opens its sign-in link, the helper passes it to `cave` on the Mac through the terminal, and `cave` opens it in your default browser. Only `https://` links on `claude.ai`, `claude.com` or `anthropic.com` are opened ([D56](../ROADMAP.md#decisions), [decision 0027](decisions/0027-sign-in-links-open-on-the-mac.md)). After the session, `cave` says how many it opened. When the sign-in sends the browser back to `http://localhost:<port>`, where Claude Code waits inside the VM, `cave` listens on that port on the Mac until the browser arrives, for up to 10 minutes, and passes the request into the VM, so the sign-in finishes by itself. If the sign-in page shows a code instead, paste it back into Claude Code. If you copy a printed link by hand instead, make the window wide enough for it to fit on one line, because a wrapped link copies with line breaks and the sign-in page rejects it. Codex's browser sign-in returns to `localhost` on the Mac, not the VM, so inside the VM use `codex login --device-auth`.

For Codex with `--on-mac`, `cave` starts `codex` in the project folder with `-c` settings naming `cave mcp` as the `cave` MCP server (with `--fork <name>` for a fork), approving its tools in advance, since they act only on the VM, and giving it the text of `.cave/AGENTS.md` as `developer_instructions`, since without a shell it cannot open that file, and with `--disable shell_tool --disable unified_exec`, which takes away its shell. Without a shell, Codex has no tool that reads a file on the Mac, so `.env` is out of its reach too. This was checked with Codex 0.162 on a Mac: it ran commands in the VM through `cave` and reported no tool that could read `.env`. Inside the VM, `cave` installs Codex with `sudo npm install -g @openai/codex` if it is not there, and runs it with `--dangerously-bypass-approvals-and-sandbox` (left out with `--safe`).

Exit code: the agent's.

Errors:

| Error | Fix |
| --- | --- |
| `unknown agent "<name>"` | Supported agents: claude, codex |
| `claude is not installed on this Mac` | Install Claude Code, or run it inside the VM with `cave agent claude` |
| `codex is not installed on this Mac` | Install Codex (npm install -g @openai/codex), or run it inside the VM with `cave agent codex` |
| `--dangerously-skip-permissions is refused with --on-mac: the agent's own tools reach this Mac` | Run `cave agent claude` without `--on-mac`, which skips the prompts inside the VM |
| `Claude Code cannot reach its model API: this VM has no network` | Run `cave stop`, set `mode = "nat"` under `[network]` in `cave.toml` if it says `"off"`, then run it again; or use `--on-mac` |

### cave images

```
cave images [--json]
```

Lists the base images: name, digest, whether it is downloaded, and which projects use it. `base` is an alias for the recommended default, Debian; `debian` and `ubuntu` name each image.

```
NAME     DIGEST           DOWNLOADED  USED BY
debian   sha256:3f2a9c1e  yes         3 projects   (base)
ubuntu   sha256:8b17d04a  no          -
```

If the update detector has seen a newer image, it says so here and in `cave doctor`. It never updates an image by itself; a newer cave is offered when cave runs in a terminal, and installed if you say yes ([config](config.md#updates), D47).

### cave doctor

```
cave doctor
```

Re-runs the first-run checks (design). Each line passes or fails, and each failure has its fix. It changes nothing and downloads nothing.

The checks, in order:

- The chip is Apple Silicon, and Hypervisor.framework is available (it is not inside a VM).
- macOS is 26 or later.
- `cave-vmm`, next to `cave-daemon`, is signed with the hypervisor entitlement.
- At least 5 GB is free on the volume of the current folder.
- The daemon answers. It then hashes every image and kernel in its cache again and checks each against its digest. A damaged file is reported with its path, for you to delete; the next `cave run` downloads it again.
- If the update detector has seen a newer version of a downloaded image, a line says so. Doctor itself looks nothing up; see [config.md](config.md#updates).

```
ok    Apple Silicon (M3 Pro)
ok    Hypervisor.framework is available
ok    macOS 26.1
fail  cave is not signed with the hypervisor entitlement
      fix: Reinstall cave (brew reinstall cave-vm); cave signs cave-vmm when it next starts its daemon
ok    48 GB free on the volume with your projects
ok    the cave daemon is running
ok    debian image sha256:3f2a9c1e verified
ok    kernel for debian sha256:9b1c44d0 verified
```

Exit code: 1 if any check fails.

### cave mcp

The design gives agents an MCP server that resolves the VM from the working directory, and `cave init` registers it in agent configs. Something has to start it, so this spec gives it a command:

```
cave mcp [--fork <name>]
```

Runs the MCP server on stdin and stdout for the project in the current directory. `--fork <name>` makes the tools act on that fork when a call names none. People do not run it by hand; `cave agent` and the configs `cave init` writes do. It is left out of `cave --help`'s main list.

The server is written by hand ([decision 0013](decisions/0013-mcp-server-by-hand.md)): JSON-RPC 2.0, one message per line, answering `initialize`, `ping`, `tools/list` and `tools/call` one request at a time. It finds the project at each call, the way `cave` does, so it works once `cave init` has run even if it started earlier. Its tools:

| Tool | Arguments | Does |
| --- | --- | --- |
| `exec` | `command`, optional `cwd`, `fork` and `credentials` (a list of names) | Runs `bash -c <command>` in the VM, starting it if needed, and returns the exit code, stdout and stderr. Each stream keeps its first and last 50 KB when longer. Each credential named is given to the command as an environment variable (see [Credentials](#credentials)) |
| `read_file` | `path`, optional `fork` | Returns a UTF-8 text file up to 1 MiB; relative paths are under `/workspace` |
| `write_file` | `path`, `content`, optional `fork` | Writes a text file, creating missing folders |
| `snapshot` | `name`, optional `fork` | As `cave snapshot <name>` |
| `restore` | `name`, optional `fork` | As `cave restore <name>`, including the automatic snapshot that undoes it |
| `fork` | optional `from` | As `cave fork [--from <snapshot>]`; returns the fork's name to pass as `fork` |
| `destroy` | `fork` | Removes a fork. The project's own VM cannot be removed through MCP, since in a sealed project it can hold the only copy of the code; that is `cave rm` |

A tool that fails returns the error and its fix as text, marked `isError`, so the agent can act on it.

---

## Remote editing

sshd runs in the VM and is reached over vsock, so an editor that expects SSH, such as VS Code Remote-SSH or Zed, edits the code where it lives without the user touching SSH (design, "Remote editing"). `cave init`, and the setup on first use, prepare it:

- An SSH key in the data directory, `ssh/id_ed25519`, made once with `ssh-keygen`.
- A host entry for the project, `ssh/hosts/<folder>.cave.conf`, named after the project folder in lowercase, with `-2`, `-3` and so on when another project has the name. It connects as the `cave` user through `cave ssh-proxy <project>`. It does not check the VM's host key, since the connection never leaves the Mac.
- An `Include` line at the top of `~/.ssh/config` for those entries, added once ([D34](../ROADMAP.md#decisions)). The rest of the file is kept.

`cave ssh-proxy` is the entry's `ProxyCommand`. People do not run it by hand, and it is left out of `cave --help`. It starts the VM if needed, adds the public key to the `cave` user's `authorized_keys` the first time, waits up to 30 seconds for sshd after a first boot, then carries the connection. Forks have no host entry yet.

## Credentials

A command run through the MCP server's `exec` tool, or the SDK's `Command::credential`, can name credentials, such as `API_KEY` (design, "Credentials"). The daemon reads each value from `.env` in the project folder and gives it to that command only, as an environment variable. The agent never gets the value:

- Output that prints it has the exact value replaced with the name in brackets, so `API_KEY: snednao_#m34nr` comes back as `API_KEY: [API_KEY]`, even when the value is split across two writes [D9]. Every credential the VM's memory may hold is redacted this way, from the output of later commands that do not name it and from files read out of the VM (the MCP server's `read_file`, `cave cp`), using the value `.env` has now. This is best effort: a command can still leak the value through the network, a file changed since, or `cave sync` and `cave pull`, which copy files as they are.
- The audit log records the name, never the value.
- A snapshot taken afterwards records the names, since the VM's memory may still hold the values, and `cave restore`, `cave fork` and `cave pull` of it warn.

`.env` is never copied into the VM ([D33](../ROADMAP.md#decisions)), so the agent cannot read it there. `.env` has one `NAME=value` per line, optionally after `export `. Lines starting with `#` are comments. A value in single quotes is taken as written, one in double quotes may use `\n`, `\"` and `\\`, and an unquoted value ends at ` #`. A name that is not in `.env` is an error, and the command does not run:

```
error: no credential named API_KEY in /Users/ivan/my-project/.env
  fix: Add a line `API_KEY=<value>` to /Users/ivan/my-project/.env
```

## Guest layout

The design does not say where the code lives inside the VM or which user runs commands.

- The project directory inside the VM is `/workspace`. `cave exec`, `cave shell` and the MCP tools start there, and relative `:` paths in `cave cp` are relative to it.
- Commands run as a user named `cave` with passwordless `sudo` and membership in the `docker` group, not as root.
- The guest's hostname is the project folder's name.

## Flows

### First run (the quickstart)

```sh
brew install rolling7ho/cave/cave-vm
cd my-project && cave agent claude
```

1. `cave agent` finds no `cave.toml`, so it runs `cave init`, which asks whether to keep a copy of the code on the Mac. Enter keeps the default, sealed.
2. First-run setup fetches the kernel and the Debian image with a progress bar, signs `cave` ad hoc [A5], and runs the doctor checks.
3. The VM boots, the files in `my-project` are copied into `/workspace` ([D13](../ROADMAP.md#decisions)), and the startup policy is printed.
4. A snapshot is taken, Claude Code is installed in the VM on first use, and it starts there with its permission prompts skipped. You sign in inside the VM the first time.

### Undo a bad agent run

```sh
cave agent claude                     # the agent breaks something
cave restore                          # back to the snapshot taken before the run
```

### Try two approaches at once

```sh
cave snapshot before-refactor
cave fork try-a --from before-refactor
cave fork try-b --from before-refactor
cave agent claude --fork try-a        # in one terminal
cave agent claude --fork try-b        # in another
cave exec --fork try-a -- npm test
cave rm try-b                         # keep the one that worked
```

### Edit on the Mac in synced mode

```sh
cave init --synced
cave agent claude                     # the daemon updates the Mac mirror after the run
# edit files in your Mac editor
cave push                             # snapshot, then apply your edits to the VM
```

### See what the VM did and clean up

```sh
cave inspect                          # policy, resources, recent audit log
cave ls                               # VM, forks and snapshots
cave gc                               # what can be reclaimed
cave rm                               # remove the VM after confirming
```
