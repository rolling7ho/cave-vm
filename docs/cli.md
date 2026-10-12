# CLI reference

This is the spec for the `cave` command, written before any code (ROADMAP task P1.1). Nothing here is implemented yet.

[docs/design.md](design.md) is the source of truth. Every command in its CLI table is here, and the flags it names (`--net off`, `--net lan`, `-p 3000`, `--in-vm`, the `cave rm` confirmation) are written as the design specifies them. Anything the design does not name is marked **Proposed** and waits for Ivan to accept or cut it. Questions the design leaves open are listed under "Needs Ivan's decision" in [ROADMAP.md](../ROADMAP.md) as D13 to D22, and are linked from the places they affect.

The quickstart stays at two commands:

```sh
brew install cave-vm
cd my-project && cave agent claude
```

## Conventions

### The project

The project directory is the VM's identity. Commands that act on a VM find the project by looking for `cave.toml` in the current directory, then in each parent directory up to the home directory, the way git finds a repository. **Proposed, D16.** Outside any project, those commands fail with "not a cave project" and suggest `cave init`, except for the ones below that set the project up themselves.

### Setting up on first use

`cave run`, `cave shell`, `cave exec` and `cave agent` work in a directory that has no `cave.toml`: they run `cave init` with its defaults first and say so in one line. This is what keeps the quickstart at two commands (design principle 1: `cave run` works with no config). **Proposed, D15.**

The first time `cave` runs on a Mac it also does the first-run setup from the design: it checks the host, fetches the kernel and the base image, signs itself ad hoc with the hypervisor entitlement, creates the data directory and runs the `cave doctor` checks. Ad hoc signing without a paid Apple account is an assumption checked in the spike [A5].

### Forks

A fork is a VM that lives inside its parent project at `.cave/forks/<name>`. Every command that acts on a VM takes `--fork <name>` to act on that fork instead of the project's own VM, for example `cave exec --fork try-2 -- npm test`. **Proposed, D18.**

### Names

Snapshots and forks are named, never numbered. A project's snapshot and fork names share one namespace, so a name means one thing. **Proposed.** A default snapshot name follows the design: `[ProjectFolder]-[YYYY-MM-DD]-[Distro]`, for example `my-project-2026-10-12-debian`, with `-2`, `-3` and so on appended on collisions. A name is 1 to 64 characters of letters, digits, `.`, `_` and `-`, starting with a letter or digit. **Proposed.**

### Output

- Results go to stdout. Progress, the startup policy and errors go to stderr, so `cave exec -- cat file > out` captures only the file. **Proposed.**
- `cave ls`, `cave inspect`, `cave images` and `cave gc` take `--json` and print one JSON document for scripts. **Proposed.**
- Colour and progress bars appear only when stderr is a terminal, and `NO_COLOR` turns colour off. **Proposed.**

### Errors

Every error says what went wrong and how to fix it, in this shape (**Proposed** format):

```
error: the project folder is not on an APFS volume, so cave cannot clone the base image
  fix: move the project to a folder on an APFS volume, such as your home folder
```

Warnings use the same shape with `warning:` and do not change the exit code.

### Exit codes

| Code | Meaning |
| --- | --- |
| 0 | Success |
| 1 | The command failed; the message says how to fix it |
| 2 | Usage error: unknown command or flag, or a bad argument |
| 130 | Interrupted with Ctrl-C |

`cave exec`, `cave shell` and `cave agent` exit with the code of the program they ran. To keep cave's own failures distinguishable from the program's, they use 125 when cave fails before or while running it, 126 when the program exists but cannot be run, and 127 when it is not found, following the shell and Docker conventions. **Proposed.**

### Global flags

| Flag | Meaning |
| --- | --- |
| `-h`, `--help` | Show help for `cave` or a command |
| `-V`, `--version` | Print the version, for example `cave 0.1.0` |

There are no other global flags. **Proposed.**

### The daemon and the data directory

The CLI starts the daemon on demand, so there is no service to install. VMs survive the CLI exiting because the daemon owns them. After a reboot, nothing starts until a `cave` command needs the VM, which then restores from its last snapshot. **Proposed.**

Shared state (kernels, base images, the daemon socket and the list of known projects) lives in `~/Library/Application Support/cave-vm/`. The `CAVE_HOME` environment variable moves it, which the tests use. **Proposed.**

### The startup policy

The design says `cave run` prints the policy at startup and echoes widening flags back. Every command that boots a VM (`run`, `shell`, `exec`, `agent`) prints it to stderr when it boots or restores one, and prints nothing if the VM was already running. **Proposed** format:

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

The design's verb set, in its order, plus `cave stop` and `cave mcp`, which are proposed.

| Command | What it does |
| --- | --- |
| [`cave init`](#cave-init) | Create `cave.toml`, `.cave/` and the agent skill files for this directory |
| [`cave run`](#cave-run) | Boot or restore this project's VM |
| [`cave stop`](#cave-stop) | **Proposed.** Suspend this project's VM now |
| [`cave shell`](#cave-shell) | Interactive shell in the VM |
| [`cave exec -- <cmd>`](#cave-exec) | Run a command in the VM |
| [`cave snapshot [name]`](#cave-snapshot) | Save VM state |
| [`cave restore [name]`](#cave-restore) | Roll back to a snapshot |
| [`cave fork [name]`](#cave-fork) | Create a new VM from a snapshot, copy-on-write |
| [`cave ls`](#cave-ls) | List project VMs |
| [`cave rm [name]`](#cave-rm) | Remove a VM, fork or snapshot |
| [`cave gc`](#cave-gc) | Show and reclaim disk |
| [`cave inspect`](#cave-inspect) | Show the policy, resources and audit log |
| [`cave sync`](#cave-sync) | Update the Mac mirror (synced mode) |
| [`cave pull`](#cave-pull) | Bring Mac edits in (synced mode) |
| [`cave cp`](#cave-cp) | Copy files in and out |
| [`cave export`](#cave-export) | Copy the project's files out of the VM |
| [`cave agent <name>`](#cave-agent) | Start an agent on the Mac against this VM through MCP |
| [`cave images`](#cave-images) | List base images |
| [`cave doctor`](#cave-doctor) | Check entitlements, macOS version, disk space and image state |
| [`cave mcp`](#cave-mcp) | **Proposed.** Run the MCP server on stdio (plumbing for agents) |

---

### cave init

```
cave init [--sealed | --synced] [--image <name>]
```

Sets up the current directory as a project. It writes:

- `cave.toml` with the design's minimal keys (see [config.md](config.md)).
- `.cave/`, and adds `.cave/` to `.gitignore`, creating the file if needed.
- The agent skill files: a cave section in `AGENTS.md` (existing content is kept; the section sits between marker comments so it can be updated), a `.claude/skills/` entry, and, for agents that support it, a permissions config that denies host Bash.

It does not boot the VM. The files already in the folder are copied into the VM when it first boots ([D13](../ROADMAP.md#needs-ivans-decision)).

When stdin is a terminal and neither `--sealed` nor `--synced` is given, it asks the design's setup question, with sealed as the default:

```
Keep a copy of the code on your Mac? It lets you use your Mac editor and Spotlight. [y/N]
```

Without a terminal it picks sealed and says so. **Proposed.**

Running `cave init` again in a project is safe: it refreshes the skill files and never changes `cave.toml`. **Proposed.**

| Flag | Default | Meaning |
| --- | --- | --- |
| `--sealed` | yes | Code lives only on the VM disk. **Proposed** flag |
| `--synced` | | Also keep a Mac mirror of the code. The mirror is the project folder itself ([D14](../ROADMAP.md#needs-ivans-decision)). **Proposed** flag |
| `--image <name>` | `base` | Base image, from `cave images`. **Proposed** flag |

Output:

```
Created cave.toml (sealed, debian, 2 CPUs, 2 GB)
Added .cave/ to .gitignore
Wrote the cave section of AGENTS.md and .claude/skills/cave
Next: cave shell, or cave agent claude
```

Errors:

| Error | Fix |
| --- | --- |
| `this folder is inside the project at <path>` | Run cave from that project, or move this folder out of it |
| `the project folder is not on an APFS volume, so cave cannot clone the base image` [A9] | Move the project to a folder on an APFS volume, such as your home folder |
| `cave cannot set up a project in your home folder itself` | Run `cave init` in a project folder instead. **Proposed** |
| `unknown image "<name>"` | Run `cave images` to see the image names |

### cave run

```
cave run [--net off|lan] [-p <port>]... [--fork <name>]
```

Boots this project's VM, or restores it if it is suspended, and returns once the guest agent answers. It prints the startup policy. If the VM is already running it says so and prints the policy again, so `cave run` is also the way to look at what a running VM can reach. **Proposed.**

`cave shell`, `cave exec` and `cave agent` boot the VM themselves, so `cave run` is needed only to pass flags or to start a VM ahead of time.

| Flag | Default | Meaning |
| --- | --- | --- |
| `--net off` | | No network device at all (design) |
| `--net lan` | | Allow private and link-local ranges as well (design). Echoed back at startup |
| `-p <port>` | none | Forward `localhost:<port>` on the Mac to the same port in the VM (design: `-p 3000`). `-p <mac>:<vm>` maps different ports. Repeatable. **Proposed:** the `<mac>:<vm>` form, and binding to localhost only |
| `--fork <name>` | | Run a fork instead. **Proposed, D18** |

Without `--net`, the mode comes from `cave.toml`, which is `nat` by default. Flags apply to this boot only; there is no way to make `--net lan` or a port forward permanent in `cave.toml`, so a cloned repository cannot widen access on its own ([config.md](config.md#network)). **Proposed.**

On a running VM, `-p` adds a forward without a restart, because forwarding goes through the guest agent. Changing `--net` needs the VM to stop first. **Proposed.**

Errors:

| Error | Fix |
| --- | --- |
| `the VM is running with network "nat"; --net lan needs a restart` | `cave stop`, then `cave run --net lan` |
| `port 3000 on this Mac is already in use` | Pick another Mac port: `cave run -p 3001:3000` |
| `the VM did not start: <reason>` | Run `cave doctor`; the reason names the failing piece |
| `not enough free memory to start a 2 GB VM` | Stop another VM with `cave stop`, or lower `memory` in cave.toml |

Warning, from the design: `memory is getting tight: N VMs are running and M GB is free`.

### cave stop

**Proposed command, D20.** The design has running, suspended and stopped states and auto-suspend after an idle time, but no verb to suspend a VM now. Without one, changing `--net` or freeing memory means waiting for the idle timer.

```
cave stop [--fork <name>]
```

Suspends the VM to a snapshot, freeing its memory. The next command that needs the VM restores it with processes intact. If the VM is not running, it says so and exits 0.

### cave shell

```
cave shell [--fork <name>]
```

Opens an interactive shell in the VM through the guest agent, with a pseudo-terminal that follows window resizes. It boots the VM first if needed. The shell starts in the project directory inside the VM ([guest layout](#guest-layout)). Exiting the shell does not stop the VM.

Exit code: the shell's.

Errors:

| Error | Fix |
| --- | --- |
| `cave shell needs a terminal` | Use `cave exec -- <cmd>` from scripts |
| `the guest agent is not answering` | `cave stop`, then try again; if it keeps happening, run `cave doctor` |

### cave exec

```
cave exec [--fork <name>] -- <cmd> [args...]
```

Runs one command in the VM and streams its stdout and stderr back, keeping them separate. Stdin is passed through. It boots the VM first if needed. The command runs in the project directory inside the VM, without a shell, so `cave exec -- sh -c 'cd sub && make'` is the way to use shell syntax. There is no pseudo-terminal; use `cave shell` for interactive programs. **Proposed:** no `-t` flag.

The `--` is required, so cave's flags and the command's flags never mix. **Proposed.**

Exit code: the command's, or 125, 126 or 127 as described under [exit codes](#exit-codes).

Errors:

| Error | Fix |
| --- | --- |
| `missing command after --` | `cave exec -- uname -a` |
| `command not found in the VM: <cmd>` (exit 127) | Install it in the VM, for example `cave exec -- sudo apt-get install -y <pkg>` |

### cave snapshot

```
cave snapshot [name] [--fork <name>]
```

Saves the VM's disk state, memory state, VM config and metadata. With no name it uses the default name. On a stopped VM it saves disk state only, and restoring it boots normally. **Proposed.**

Output: `Saved snapshot my-project-2026-10-12-debian (1.2s)`.

Errors:

| Error | Fix |
| --- | --- |
| `a snapshot or fork named "<name>" already exists` | Pick another name, or remove it with `cave rm <name>` |
| `not enough disk space for a snapshot (needs about N GB)` | Run `cave gc` to see what can be reclaimed |

### cave restore

```
cave restore [name] [--fork <name>]
```

Rolls the VM back to a snapshot, by default the most recent one. Processes in the snapshot resume where they were. Open shells and running `cave exec` calls are disconnected first, and port forwards are re-applied. **Proposed.**

Before rolling back, it saves the current state as an automatic snapshot, so a restore can itself be undone. **Proposed.** Automatic snapshots are deleted by the retention rules in `cave.toml`.

If the memory state cannot be loaded, for example after a `cave` upgrade that cannot read it, restore falls back to the disk state and a normal boot, and says so (design):

```
warning: could not load the memory state of my-project-2026-10-12-debian (format 1 is not supported)
  restored the disk state and booted normally; processes from the snapshot were not kept
```

If injected credentials were recorded in the snapshot's metadata, it names them. **Proposed.**

Errors:

| Error | Fix |
| --- | --- |
| `no snapshot named "<name>"` | Run `cave ls` to see this project's snapshots |
| `this project has no snapshots yet` | Take one with `cave snapshot` |

### cave fork

```
cave fork [name] [--from <snapshot>]
```

Creates a new VM from a snapshot, copy-on-write, stored at `.cave/forks/<name>` (design). The design leaves two things open, so both are **Proposed, D18**:

- `name` is the new fork's name. Without it, forks are named `fork-1`, `fork-2` and so on.
- The source is a fresh snapshot of the VM's current state. `--from <snapshot>` forks an existing snapshot instead.

The fork starts running. Use it with `--fork <name>` on other commands. Forking a snapshot that recorded injected credentials shows the design's warning:

```
warning: snapshot my-project-2026-10-12-debian was taken after API_KEY was injected
  the fork's memory may contain it
```

Output: `Forked fork-1 from my-project-2026-10-12-debian (0.3s). Use it with: cave shell --fork fork-1`.

Errors: the same name errors as `cave snapshot`, and `no snapshot named "<name>"` for `--from`.

### cave ls

```
cave ls [--all] [--json]
```

Inside a project, lists this project's VM, its forks and its snapshots. With `--all`, or outside a project, lists every project VM on this Mac that the daemon knows about. **Proposed, D17:** the daemon keeps a list of project folders it has run, pruned when a folder is gone, because the design has no registry.

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

With no name, removes this project's VM: `.cave/` with its disk, snapshots and forks. `cave.toml`, `AGENTS.md` and the rest of the folder stay. With a name, removes that fork or snapshot. **Proposed:** the name form.

Because `.cave/` can hold the only copy of the work in sealed mode, it asks first (design):

```
Remove the VM for my-project? Its disk, 3 snapshots and 1 fork are deleted (4.8 GB).
This project is sealed, so code that is only in the VM is lost. [y/N]
```

`--yes` skips the question, for scripts. Without a terminal and without `--yes`, it refuses. **Proposed.**

Exit code: 1 when the answer is no.

### cave gc

```
cave gc [--yes] [--json]
```

Shows disk usage and what can be reclaimed, then asks before deleting anything. It only offers things that are safe to delete: base images no project uses, automatic snapshots beyond the retention in `cave.toml`, and leftovers from crashed VMM processes. Named snapshots and VMs are never offered; remove those with `cave rm`. **Proposed.**

```
Images        2.1 GB   debian (in use), ubuntu (unused, 1.9 GB)
my-project    4.8 GB   disk 3.1 GB, snapshots 1.5 GB, forks 0.2 GB
Reclaimable   2.3 GB   unused images 1.9 GB, old automatic snapshots 0.4 GB
Delete 2.3 GB? [y/N]
```

Shared APFS clone blocks are reported once, not double-counted [A9].

### cave inspect

```
cave inspect [--fork <name>] [--log] [--json]
```

Shows the policy (the same lines as the startup policy), resources, and the most recent 20 audit-log entries: exec calls, file writes and network connections. `--log` prints the whole log. **Proposed:** the 20-entry default and `--log`. Credentials appear by name only.

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

### cave sync

```
cave sync
```

Synced mode only. Updates the Mac mirror from the VM now; the daemon also does it after agent runs and on snapshot (design). If the VM is not running, the sync waits until it is, and the command says so. **Proposed:** the command blocks and says "waiting for the VM" rather than returning.

Errors:

| Error | Fix |
| --- | --- |
| `this project is sealed, so it has no Mac mirror` | Set `mode = "synced"` under `[storage]` in cave.toml to keep a copy on your Mac |

### cave pull

```
cave pull
```

Synced mode only. Brings Mac edits into the VM, following the design's rules exactly:

- It compares the SHA-256 of each file in the Mac mirror against the VM's.
- A file that changed only on the Mac is applied. One that changed only in the VM is left alone.
- A file that changed on both sides since the last sync stops the pull before anything is applied, and is listed.
- A snapshot is taken automatically before anything is applied.
- Dependency and build folders (`node_modules`, `target`, and paths in `.gitignore`) are excluded.

```
Saved snapshot my-project-2026-10-12-debian-3 before pulling
Applied 3 files from your Mac: src/app.ts, src/util.ts, README.md
```

On a conflict, exit code 1:

```
error: 2 files changed both on your Mac and in the VM since the last sync, so nothing was pulled
  src/app.ts
  package.json
  fix: keep one version of each file (copy it with cave cp, or undo the edit on your Mac), then run cave pull again
```

Errors: the same sealed-mode error as `cave sync`.

### cave cp

```
cave cp <src>... <dest>
```

Copies files between the Mac and the VM. A path starting with `:` is in the VM, relative to the project directory there unless it is absolute; anything else is on the Mac. **Proposed** syntax, borrowed from `scp`. Directories are copied recursively. Exactly one side must be in the VM.

```sh
cave cp ./fixtures :test/fixtures    # Mac to VM
cave cp :dist/app.tar.gz .           # VM to Mac
```

Errors:

| Error | Fix |
| --- | --- |
| `one side of cave cp must be in the VM` | Prefix the VM path with `:`, for example `:src/app.ts` |
| `no such file in the VM: <path>` | Check the path with `cave exec -- ls <dir>` |

### cave export

```
cave export [snapshot] [-o <dir>]
```

**Proposed, D19.** The design lists `cave export` with `cave cp` as the way to "move files in and out" and as an escape hatch for code in sealed mode. This spec reads it as: copy the project directory out of the VM, or out of a snapshot's disk, into a folder on the Mac. Without `-o`, the folder is `./<ProjectFolder>-export-<YYYY-MM-DD>`. It copies everything, including `.git`, and leaves out nothing, so it is a full copy of the work. Exporting from a snapshot that recorded injected credentials shows the same warning as `cave fork`.

Output: `Exported 1,204 files (38 MB) to ./my-project-export-2026-10-12`.

### cave agent

```
cave agent <name> [--in-vm] [--fork <name>] [-- <agent args>...]
```

Starts a coding agent against this project's VM (design, [decision 0002](decisions/0002-cave-agent-runs-on-the-mac.md)):

1. Sets up the project and boots the VM if needed, and prints the startup policy.
2. Takes a snapshot, because the design calls for one before each agent run.
3. Starts the agent on the Mac, connected to this VM through the MCP server, with host Bash denied where the agent supports it. The model API key stays on the Mac.
4. When the agent exits, it prints how to undo the run:

```
Snapshot before this run: my-project-2026-10-12-debian-4
Undo everything the agent did with: cave restore my-project-2026-10-12-debian-4
```

`--in-vm` runs the agent inside the VM instead, which needs the VM's network to reach the model API. On first use it installs the agent inside the VM with the agent's own installer, and you sign in inside the VM. **Proposed:** the install step.

Arguments after `--` go to the agent unchanged. **Proposed.**

Agents in v1: `claude` (Claude Code). The README also lists Codex; it is added once its MCP and permission settings are checked. **Proposed:** `claude` only at first. How `cave` passes the MCP server and the Bash denial to Claude Code depends on Claude Code's flags and settings, which are assumptions to verify in phase 5.

Exit code: the agent's.

Errors:

| Error | Fix |
| --- | --- |
| `unknown agent "<name>"` | Supported agents: claude |
| `claude is not installed on this Mac` | Install Claude Code, or run it inside the VM with `cave agent claude --in-vm` |

### cave images

```
cave images [--json]
```

Lists the base images: name, digest, whether it is downloaded, and which projects use it. `base` is an alias for the recommended default, Debian; `debian` and `ubuntu` name each image. **Proposed** names.

```
NAME     DIGEST           DOWNLOADED  USED BY
debian   sha256:3f2a9c1e  yes         3 projects   (base)
ubuntu   sha256:8b17d04a  no          -
```

If the update detector has seen a newer image, it says so here and in `cave doctor`. It never updates anything by itself (design).

### cave doctor

```
cave doctor
```

Re-runs the first-run checks (design): Apple Silicon, macOS version, the hypervisor entitlement and signature, disk space, the kernel and image state, and the daemon. Each line passes or fails, and each failure has its fix. It changes nothing. **Proposed:** report-only.

```
ok    Apple Silicon (M3 Pro)
ok    macOS 26.1
fail  cave is not signed with the hypervisor entitlement
      fix: brew reinstall cave-vm, which re-signs it on first run
ok    48 GB free on the volume with your projects
ok    debian image sha256:3f2a9c1e verified
```

Exit code: 1 if any check fails.

### cave mcp

**Proposed command.** The design gives agents an MCP server that resolves the VM from the working directory, and `cave init` registers it in agent configs. Something has to start it, so this spec gives it a command:

```
cave mcp
```

Runs the MCP server on stdin and stdout for the project in the current directory. People do not run it by hand; `cave agent` and the configs `cave init` writes do. It is left out of `cave --help`'s main list. **Proposed.**

---

## Guest layout

**Proposed.** The design does not say where the code lives inside the VM or which user runs commands.

- The project directory inside the VM is `/workspace`. `cave exec`, `cave shell` and the MCP tools start there, and relative `:` paths in `cave cp` are relative to it.
- Commands run as a user named `cave` with passwordless `sudo` and membership in the `docker` group, not as root.
- The guest's hostname is the project folder's name.

## Flows

### First run (the quickstart)

```sh
brew install cave-vm
cd my-project && cave agent claude
```

1. `cave agent` finds no `cave.toml`, so it runs `cave init`, which asks whether to keep a copy of the code on the Mac. Enter keeps the default, sealed.
2. First-run setup fetches the kernel and the Debian image with a progress bar, signs `cave` ad hoc [A5], and runs the doctor checks.
3. The VM boots, the files in `my-project` are copied into `/workspace` ([D13](../ROADMAP.md#needs-ivans-decision)), and the startup policy is printed.
4. A snapshot is taken, and Claude Code starts on the Mac with the VM as its only place to run commands.

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
cave pull                             # snapshot, then apply your edits to the VM
```

### See what the VM did and clean up

```sh
cave inspect                          # policy, resources, recent audit log
cave ls                               # VM, forks and snapshots
cave gc                               # what can be reclaimed
cave rm                               # remove the VM after confirming
```
