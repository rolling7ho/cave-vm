# cave.toml reference

`cave.toml` is a project's small config file, committed to git (ROADMAP task P1.2). Together with the pinned base-image digest it rebuilds the environment on any Mac. The daemon reads it (`cave-vm-daemon`'s `config` module).

The keys in the design's example are fixed: `[vm] image cpus memory`, `[storage] mode` and `[network] mode`. Every other key was proposed under [decision D10](decisions/0003-roadmap-open-items.md), which says the key names for idle auto-suspend, automatic snapshots and deletion, TTL and the update detector are proposed and approved here.

## The file `cave init` writes

`cave init` writes the design's minimal file and nothing else, so the common case stays short. Every other key has a default and can be added when needed.

```toml
[vm]
image   = "base"        # pinned to a digest on first run
cpus    = 2
memory  = "2G"

[storage]
mode    = "sealed"      # or "synced"

[network]
mode    = "nat"         # off | nat
```

## Every key

```toml
[vm]
image        = "base"      # fixed key
cpus         = 2           # fixed key
memory       = "2G"        # fixed key
idle_suspend = "30m"
ttl          = "off"

[storage]
mode         = "sealed"    # fixed key
disk         = "20G"

[network]
mode         = "nat"       # fixed key

[snapshots]
auto         = true
keep         = 10

[updates]
check        = true
```

### [vm]

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `image` | string | `"base"` | Base image name from `cave images`. `base` and `debian` are Debian 13 (trixie), the recommended default, and `ubuntu` is Ubuntu 26.04 LTS (design: the user chooses Debian or Ubuntu). On first run cave pins it by writing the digest into this key, as `base@sha256:...`, and uses that digest from then on ([D22](../ROADMAP.md#decisions)) |
| `cpus` | integer | `2` | Virtual CPUs, at least 1. A change applies after `cave stop`: the next start boots from the disk instead of resuming |
| `memory` | size | `"2G"` | Guest memory. The configurable guest address size bounds the maximum [A4]. A change applies after `cave stop`, as for `cpus` |
| `idle_suspend` | duration or `"off"` | `"30m"` | Suspend the VM after this long idle, as `cave stop` would. Idle means no `cave` or SDK or MCP connection to the VM and no traffic on a forwarded port: the time counts from when the last command, shell, file copy or forwarded connection closed, or from `cave run`. An open forwarded connection counts as traffic even when quiet. The daemon checks every 10 seconds |
| `ttl` | duration or `"off"` | `"off"` | Lifetime for disposable VMs, counted from the VM's first boot; a suspend and resume or a restore does not reset it. When it passes, the VM is stopped and its state removed, as `cave rm --yes` would. The daemon checks every 10 seconds, and a VM whose TTL ran out while the daemon was not running is removed when it is next started, which then boots a new one. `cave run` prints when that will happen. Decided by [D21](../ROADMAP.md#decisions) |

The design says `cave.toml` "plus a pinned base-image digest rebuilds the environment anywhere", and `.cave/` is gitignored, so the pin lives in `cave.toml`: on first run cave rewrites `image = "base"` as `image = "base@sha256:..."`, and the pin is committed with the file. This is the one time cave edits `cave.toml` ([D22](../ROADMAP.md#decisions)). Names and digests come from the `images.json` manifest on the project's GitHub releases, which also gives each image's kernel ([D28](decisions/0010-image-manifest.md)).

### [storage]

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `mode` | `"sealed"` or `"synced"` | `"sealed"` | Fixed by the design. Sealed keeps code only on the VM disk; synced also keeps a Mac mirror |
| `disk` | size | `"20G"` | The design's 20 GB sparse disk, growable. Raising it grows the disk on the next boot, also after `cave stop`, when the next start boots instead of resuming; lowering it has no effect, since disks do not shrink |

Changing `mode` from sealed to synced creates the mirror on the next `cave sync`. Changing from synced to sealed stops syncing and leaves the Mac files where they are.

### [network]

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `mode` | `"off"` or `"nat"` | `"nat"` | Fixed by the design. `nat` is outbound only, with LAN ranges and the Mac denied; `off` has no network device |

`lan` is not a valid value here, and port forwards cannot be set here either. The design's example lists only `off | nat`, and widening access "takes an explicit flag that is echoed back". Keeping widening out of the committed file also means a cloned repository cannot widen access on its own. Use `cave run --net lan` and `cave run -p 3000`.

### [snapshots]

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `auto` | boolean | `true` | Take the automatic snapshot that saves the state before each `cave restore` (see [cli.md](cli.md#cave-restore)). The automatic snapshots before each `cave agent` run and each `cave push` (design) are always taken, even when this is `false` |
| `keep` | integer | `10` | How many automatic snapshots to keep. Automatic snapshots are the ones before each `cave agent` run, each `cave push` and each `cave restore` ([D51](../ROADMAP.md#decisions)). When there are more, the oldest are deleted after each new one before an agent run or a push, after each `cave restore`, and by `cave gc`. The one just taken and one being restored are never deleted, even with `keep = 0`. Snapshots named by the user are never deleted automatically |

### [updates]

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `check` | boolean | `true` | Let the update detector look up whether a newer base image exists, and let cave look up a newer cave and offer to install it ([D47](../ROADMAP.md#decisions)). The lookups carry no identifying data, and nothing is updated without asking. `false` means cave makes no network request of its own for this project |

When a VM starts in a project with `check = true`, the daemon reads the image manifest (the same `images.json` the first run reads) in the background, at most once a day, and records the newest version of each image in `updates.json` in its data folder. A failed lookup is tried again a day later. `cave doctor` then says when a downloaded image has a newer version, and `cave images` marks it.

When cave runs in a terminal, it also fetches the latest release's `images.json` in the background, at most every 10 minutes, to learn the newest stable cave. If that is newer than the running cave, cave asks `Update now? [Y/n]` before the command ([D47](../ROADMAP.md#decisions)). Yes installs it the way cave was installed: `brew update` and `brew upgrade rolling7ho/cave/cave-vm` for Homebrew, or the release's `install.sh` into the same folder; then cave stops so you run the command again on the new version. No skips it for a day. A copy 2 or more stable releases behind is asked every time, with a note that only the 3 newest releases get security fixes, and a no stops the command, except `cave pull`, so the work can always be copied out; without a terminal it only warns, and a copy cave cannot update only warns ([D52](../ROADMAP.md#decisions)). Scripts and pipes are never asked, and a copy run from a build folder or installed with `cargo install` is never offered an update. With VMs running, the daemon keeps its version until they stop.

cave never moves a project to a newer image by itself. To move one, remove the `@sha256:...` part of `image` in its `cave.toml` and run `cave rm`, which deletes the VM and its disk; the next `cave run` builds a new VM from the newest image and pins it.

## Value formats


- **Sizes** are a number and a unit: `K`, `M`, `G` or `T`, powers of 1024, for example `"512M"` or `"2G"`. A bare integer is an error, to avoid guessing at units.
- **Durations** are a number and a unit: `s`, `m`, `h` or `d`, for example `"30m"` or `"8h"`. `"off"` turns the setting off.

## Errors

A bad file stops every command that reads it, with the line and the fix:

```
error: cave.toml line 4: memory = 2048 needs a unit
  fix: write memory = "2G" (units: K, M, G, T)
```

```
error: cave.toml line 10: network mode "lan" cannot be set in cave.toml
  fix: pass --net lan to cave run when you need LAN access
```

Unknown keys are an error rather than ignored, so a typo does not silently keep a default.
