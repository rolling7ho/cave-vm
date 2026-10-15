# Protocol

This is the wire protocol for the guest agent and the daemon API (ROADMAP task P1.3). It is the source for the `proto` crate, which implements all of it.

The design fixes the shape: length-prefixed JSON messages, versioned from day one, with streamed output and a pseudo-terminal for `cave shell`, and the same message format on the daemon's Unix socket so the SDKs stay thin. The framing width, the maximum frame size, the version handshake and everything else below were proposed in the phase 1 PR and approved by Ivan there.

## Where it runs

| Link | Transport | Client | Server |
| --- | --- | --- | --- |
| Guest agent | vsock, guest port 1024. | VMM, on the daemon's behalf | `agent` in the guest |
| Daemon API | Unix socket `daemon.sock` in the data directory, mode `0600` (design: user-only permissions) | CLI, SDKs, MCP server | `daemon` |
| VMM control | The VMM process's standard input, one end of a Unix socket pair the daemon made when it started the VMM | `daemon` | the VMM |

## Framing

Each frame is a 4-byte unsigned big-endian length followed by that many bytes of UTF-8 JSON, one object per frame.

- The length counts only the JSON bytes. A length of 0 is an error.
- The maximum frame is 1 MiB. A peer that receives a larger length sends an `error` with code `frame_too_large` and closes the connection, without reading the body.
- Binary data travels as base64 in a `data` field. Senders split data into chunks of at most 256 KiB before encoding, which keeps every frame well under the limit.

Why these numbers: 4 bytes is the usual width and leaves room; 1 MiB bounds the memory a peer must buffer before it can parse anything; base64 costs a third more bytes but keeps every frame plain JSON, which keeps the Python and TypeScript SDKs simple. If file-copy speed matters more later, a raw binary frame type can be added in a new protocol version.

## Connections and the handshake

**One operation per connection.** A client opens a connection, does the handshake, sends one request, and the connection carries that operation's messages until it ends. Concurrent operations use separate connections. This avoids multiplexing and flow control in the protocol, because vsock and Unix sockets already apply backpressure per connection. The one long-lived connection is the agent's control connection.

The client's first frame is `hello`, and the server answers with `hello` or an `error`:

```json
{"type": "hello", "versions": [1], "software": "cave 0.1.0"}
{"type": "hello", "version": 1, "software": "cave-agent 0.1.0"}
```

- The client lists every protocol version it speaks. The server picks the highest one it also speaks and returns it as `version`.
- With no version in common, the server sends `{"type": "error", "code": "unsupported_version", "message": "...", "supported": [2, 3]}` and closes. The CLI turns that into an error with a fix, such as updating the base image or `cave`.
- Within a version, changes are additive only: new optional fields and new message types. Receivers ignore fields they do not know. A request of a type the server does not know gets an `error` with code `unknown_type`. Anything else needs a new version number.
- The daemon keeps speaking older agent versions, so a snapshot or image made before a `cave` upgrade still works (design: snapshots survive upgrades).

## Message envelope

Every message is a JSON object with a `type` string. Field names are `snake_case`. Optional fields are left out rather than set to `null`.

## Errors

```json
{"type": "error", "code": "not_found", "message": "no such file in the VM: src/app.ts", "fix": "Check the path with cave exec -- ls src"}
```

`code` is a stable `snake_case` identifier for programs. `message` and the optional `fix` are for people, and follow the CLI's rule that errors say how to fix the problem. An `error` ends the operation, and the sender closes the connection after it.

Codes in version 1: `bad_frame`, `frame_too_large`, `unsupported_version`, `unknown_type`, `bad_request`, `not_found`, `permission_denied`, `already_exists`, `not_running`, `busy`, `io`, `internal`, `conflict`. An `error` with `conflict` (from `pull`) also has `files`, the paths changed on both sides.

## Guest agent messages

### exec

Runs a command in the guest. The agent runs `argv` directly, without a shell.

```json
{"type": "exec", "argv": ["npm", "test"], "cwd": "/workspace", "env": {"CI": "1"}, "stdin": true}
```

| Field | Meaning |
| --- | --- |
| `argv` | Program and arguments. Required, at least one element |
| `cwd` | Working directory. Default `/workspace` |
| `env` | Extra environment variables, added to the guest user's default environment |
| `stdin` | Whether the client will send `input`. If `false`, the program's stdin is `/dev/null` |

Then, in any order until the program exits:

```json
{"type": "output", "stream": "stdout", "data": "aGVsbG8K"}
{"type": "output", "stream": "stderr", "data": "..."}
```

From the client:

```json
{"type": "input", "data": "..."}
{"type": "input_end"}
{"type": "signal", "signal": "INT"}
```

`signal` accepts `INT`, `TERM`, `KILL` and `HUP`. Finally, from the agent:

```json
{"type": "exit", "code": 0}
{"type": "exit", "signal": "KILL"}
```

`exit` has either `code` or `signal`, and is the last message of the operation. If the program cannot be started, the agent sends `exit` with code 127 when it is not found and 126 when it cannot be run, plus a line on stderr saying why, to match shell behaviour.

Credentials never appear in this message on the daemon API. There the client names them, and the daemon resolves the values and adds them to `env` only on the agent link (see [the daemon's exec](#exec-and-shell)).

### pty_open, pty_data, pty_resize

The pseudo-terminal for `cave shell`.

```json
{"type": "pty_open", "argv": ["/bin/bash", "-l"], "cwd": "/workspace", "env": {}, "term": "xterm-256color", "cols": 120, "rows": 40}
```

`argv` defaults to the guest user's login shell. After it, both sides send raw terminal bytes:

```json
{"type": "pty_data", "data": "..."}
```

The client sends `{"type": "pty_resize", "cols": 100, "rows": 30}` when the window changes, and may send `signal`. The operation ends with `exit`, as for `exec`.

### file_read

```json
{"type": "file_read", "path": "/workspace/src/app.ts"}
```

The agent answers with the file's metadata, then its contents in chunks, then the end with a checksum:

```json
{"type": "file_info", "size": 1234, "mode": 420}
{"type": "file_data", "data": "..."}
{"type": "file_end", "sha256": "9f86d081..."}
```

`mode` is the Unix permission bits as a number. Directories are not read with this message; `cave cp` copies a directory by running `tar` through `exec`, which the base image includes.

### file_write

```json
{"type": "file_write", "path": "/workspace/src/app.ts", "mode": 420, "create_dirs": true}
```

The client then sends `file_data` chunks and `file_end` with the SHA-256 of what it sent. The agent writes to a temporary file in the same directory, checks the hash, renames it into place, and answers:

```json
{"type": "file_written", "size": 1234}
```

A hash mismatch is an `error` with code `io`, and the original file is untouched.

### port_forward

```json
{"type": "port_forward", "port": 3000}
```

The agent connects to `127.0.0.1:<port>` in the guest and answers `{"type": "port_forward_ok"}`. From then on the connection carries raw TCP bytes in both directions, with no more framing, until either side closes. The host opens one vsock connection per accepted connection on the forwarded Mac port. Only TCP is forwarded in version 1.

### Control connection

The daemon, through the VMM, keeps one connection to the agent open while the VM runs and sends it `control` after the handshake. It carries:

| Message | Direction | Meaning |
| --- | --- | --- |
| `{"type": "control"}` | host to agent | Opens the control connection |
| `{"type": "ping"}` / `{"type": "pong"}` | either way | Liveness |
| `{"type": "snapshot_prepare"}` | host to agent | A snapshot is about to be taken. The agent flushes file systems (`sync`) and answers `snapshot_ready` |
| `{"type": "snapshot_ready"}` | agent to host | Safe to pause the VM |
| `{"type": "restored", "time_unix_ns": 1791800569000000000}` | host to agent | Sent after a boot and after a restore, since the VM has no clock of its own. The agent sets the guest clock to the host's time and answers `ok` |
| `{"type": "ok"}` | agent to host | Acknowledgement |

After a restore the vsock transport is reset, so every connection from before the snapshot is gone. The host opens a new control connection and sends `restored` before anything else (ROADMAP P4.3).

## Daemon API messages

The daemon speaks the same framing, handshake, errors and stream messages. Requests name a VM by the project folder's absolute path and an optional fork name. The absolute path appears only on the wire, never inside `.cave/`, so moving a project still works.

```json
{"project": "/Users/ivan/Desktop/Projects/my-project", "fork": "try-a"}
```

This object is written `vm` below.

### Long-running operations

`start` sends `progress` while it downloads an image, and `sync` sends one while it waits for the VM, before the result:

```json
{"type": "progress", "message": "downloading the debian image: 20% of 0.5 GB", "done": 104857600, "total": 524288000}
```

`done` and `total` are in bytes or steps, and both are optional.

### Requests

| Request | Fields | Reply |
| --- | --- | --- |
| `init` | reserved | `cave init` writes its files itself, and the daemon answers `init` with `bad_request` |
| `start` | `vm`, `net` (optional: `"off"`, `"nat"` or `"lan"`, from `--net`; the mode in `cave.toml` when left out), `ports` (list of `{mac, vm}`) | `policy`. On a running VM, `ports` are added, and a `net` other than the one it booted with is refused with `bad_request` |
| `stop` | `vm` | `ok` once the VM is suspended, its memory saved |
| `stop_all` | none | `stopped_all`. Every running VM on the Mac stops at once without being saved, for `cave stop --force` ([D43](../ROADMAP.md#decisions)): each VMM gets SIGTERM, and SIGKILL 5 seconds later if it is still running |
| `exec` | `vm`, the agent's `exec` fields, `credentials` (list of names) | the agent's stream, ending in `exit` |
| `shell` | `vm`, the agent's `pty_open` fields | the agent's pty stream, ending in `exit` |
| `ssh` | `vm` | `ok` once the agent has connected to sshd, port 22 in the guest, with `port_forward`. From then on the connection carries raw bytes, for remote editing. A `not_found` error means sshd is not listening yet |
| `file_read` / `file_write` | `vm`, the agent's fields | as on the agent link |
| `snapshot` | `vm`, `name` (optional; the default name otherwise) | `snapshot` with the name and size. A running VM keeps running and its memory is saved; a stopped VM's disk is saved. For a synced project, the Mac mirror is then synced |
| `restore` | `vm`, `name` (optional; the newest snapshot otherwise, preferring named ones) | `restored` once the VM runs from the snapshot. The state before is saved first as an automatic snapshot, unless `[snapshots] auto` is `false`. `memory_restored` is `false` when it fell back to a disk boot |
| `fork` | `vm`, `name` (optional; `fork-1`, `fork-2` and so on otherwise), `from` (optional snapshot; the VM's current state otherwise) | `forked` once the fork is running. It is then addressed as a `vm` with `fork` set |
| `list` | `project` (optional; all known projects if absent) | `list` with VMs, forks and snapshots |
| `remove` | `vm`, `name` (optional: the snapshot or fork to delete; without it the VM, its forks and its `.cave/` state are removed) | `ok`, or `not_found` for an unknown name |
| `inspect` | `vm`, `log_limit` | `policy`, `resources` and `audit` entries |
| `sync` | `vm` (a synced project's own VM) | `synced`. If the VM is not running, a `progress` with `waiting for the VM` comes first and the reply waits until it runs. A sealed project or a fork is refused at once with `bad_request` |
| `pull` | `vm` (a synced project's own, running VM) | `pulled`, or an `error` with code `conflict` and a `files` list, in which case nothing was applied |

`snapshot`, `restore` and `fork` act on a project's own VM only; with `fork` set in `vm` they are refused with `bad_request`.
| `gc` | `apply` (boolean) | `usage`, and what was reclaimed when `apply` is true |
| `images` | none | `images` |
| `doctor` | none | `checks`, each with `name`, `ok` and `fix` |

Because a request and its reply can share a type (`snapshot`), the `proto` crate decodes each direction with its own message set. The daemon's stream messages are the agent's, relayed: the client sends `input`, `input_end`, `signal`, `pty_data`, `pty_resize`, `file_data` and `file_end`, and the daemon sends `output`, `exit`, `pty_data`, `file_info`, `file_data`, `file_end` and `file_written`.

### Reply objects

The reply objects mirror the `--json` output of the matching CLI command. Sizes are in bytes and times are Unix seconds unless a name says otherwise. Optional fields are left out.

| Reply | Fields |
| --- | --- |
| `policy` | `name`, `image` (the pinned value), `storage`, `net`, `ports`, `cpus`, `memory_bytes`, optional `ttl_seconds`, and `widened`: the flags that widened the defaults, echoed back |
| `snapshot` | `name`, `size` |
| `stopped_all` | `vms`: the VMs `stop_all` stopped |
| `restored` | `name`, `memory_restored`, `reason` (why the memory was not restored, when it was not), `saved` (the automatic snapshot taken first, which undoes the restore) and `credentials` (the names the snapshot recorded, left out when none) |
| `forked` | `name`, `from` (the snapshot, or the VM whose state was forked), `reason` (why the fork booted from its disk rather than the snapshot's memory, when it did) and `credentials` (the names the snapshot recorded, left out when none) |
| `synced` | `written` (files copied from the VM to the Mac), `deleted` (files deleted on the Mac to match the VM), `conflicts` (files changed on both sides, left alone) and `pending` (files changed only on the Mac, waiting for `pull`), each a list of paths relative to the project and left out when empty |
| `pulled` | `snapshot` (taken before anything was applied; left out when there was nothing to apply), `applied` (files copied from the Mac into the VM) and `deleted` (files deleted in the VM to match the Mac) |
| `list` | `entries`, each with `project`, `name`, `kind` (`vm`, `fork`, `snapshot`), `state` for VMs and forks (`running`, `suspended`, `stopped`), `auto` for automatic snapshots, `size` and `created_unix` |
| `inspect` | `policy`, `resources` (`cpus`, `memory_bytes`, `disk_used`, `disk_size`), `audit` (the latest entries), `audit_total`, `state` (`running`, `suspended` or `stopped`) and, while it runs, `started_unix`. For a VM that is not running, `policy` is what `start` with no flags would give, read from `cave.toml` without fetching anything. An audit entry has `time_unix_ms` and an `event`: `exec` with `argv`, credential names and `exit_code`; `write` with `path` and `size`; or `connect` with `host`, `port`, `allowed` and an optional `reason` such as `LAN` |
| `usage` | `images` (`name`, `digest`, `size`, `in_use`), `projects` (`project`, `disk`, `snapshots`, `forks`), `reclaimable` (`what`, `size`), and `reclaimed` when `apply` was set |
| `images` | `images`, each with `name`, `digest`, `downloaded`, `used_by` (projects pinned to it), `base` when `base` stands for it, and `update` when a newer digest was seen |
| `checks` | `checks`, each with `name`, `ok` and an optional `fix` |

### exec and shell

`exec` on the daemon takes credential names, never values:

```json
{"type": "exec", "vm": {"project": "/Users/ivan/p"}, "argv": ["./deploy.sh"], "credentials": ["API_KEY"]}
```

The daemon resolves each name from the project's `.env` (see [the CLI reference](cli.md#credentials)), adds the values to `env` on the agent link only, and records the names, not the values, in the audit log and in the metadata of later snapshots (design). While streaming `output` back, it replaces the exact value of each injected credential with the credential's name in brackets, so `API_KEY_1: snednao_#m34nr` is streamed as `API_KEY_1: [API_KEY_1]`, including values split across two chunks [D9]. Redaction is best effort, and a command can still leak a value through files or the network; the docs say so. A name missing from `.env` is a `not_found` error that names the file, and the command does not run. A snapshot records the names injected since the VM's memory began: since it booted, or those recorded in the snapshot it was restored, resumed or forked from. A snapshot without memory records none.

## VMM control

The daemon starts one VMM process per VM and talks to it over a Unix socket pair, whose VMM end is the process's standard input. There is no socket file, so this works at any project path (D26). When the link closes, the VMM stops its VM and exits, so a VMM never outlives the daemon that started it. Its standard output and error go to `vmm.log` in the VM's folder in the data directory.

The link uses the same framing, handshake and errors. Unlike the other links it carries more than one operation: the daemon sends one request at a time and reads its reply.

| Request | Fields | Reply |
| --- | --- | --- |
| `boot` | `config`: `kernel` and `disk` (absolute paths), `memory_bytes`, `cpus`, `net` (`"off"`, `"nat"`, `"lan"`), and optionally `restore`, the absolute path of a snapshot folder to restore instead of booting. A restore needs `disk` to be the snapshot's disk (a clone of its `disk.img`) and `cpus` and `memory_bytes` to match the snapshot | `booted` once the guest agent answers `ping` and has set the guest clock (`restored`), or an `error` |
| `agent_connect` | none | `agent_connection`, with the host end of a new connection to the guest agent attached as a file descriptor (`SCM_RIGHTS`) |
| `ping` | none | `pong` |
| `snapshot` | `dir`: the absolute path of a folder, created if missing | `ok` once the VM is saved into `dir` (`state`, `memory` and `disk.img`, `docs/snapshots.md`) and running again, or an `error`. The VMM first asks the agent to flush the guest's file systems (`snapshot_prepare`), then pauses every vCPU, saves, and resumes |
| `stop` | none | `ok`, after which the VMM stops the VM and exits |
| `take_connects` | none | `connects`: `entries`, the guest's connection attempts since the last `take_connects` as audit entries (`connect` events, oldest first), and `dropped`, left out when zero, the attempts not kept because 10,000 were already waiting. The daemon asks every second while the VM runs, and before `stop`, and appends the entries to the VM's audit log |

The daemon then speaks the agent protocol, from the handshake on, over each connection it gets from `agent_connect`.

## Versioning this document

This is protocol version 1. A change that is not additive gets a new version number, a new section in this file, and a line in `CHANGELOG.md`.
