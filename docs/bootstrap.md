# Set up cave by paste

This is the text of the bootstrap page on the cave website (design, "Bootstrap skill"). The page has one copy button. It copies the skill below, which the user pastes into their coding agent, and the agent installs cave and sets up the current project.

## Page text

**Let your coding agent set up cave.** Copy this skill and paste it into Claude Code, or any coding agent, from your project's folder. The agent will:

1. Check that this Mac has Apple Silicon and macOS 26 or later.
2. Install cave with Homebrew.
3. Run `cave doctor` and follow any fix it prints.
4. Run `cave init` in your project.
5. Check the VM works with `cave exec -- uname -a`.
6. Show you `cave inspect`: what the VM can reach, its CPU and memory, and what it did.

The agent shows you each command and waits for your approval before running it, and it fetches no further instructions from the network. This is the one moment it runs commands on your Mac before a VM exists, so read along. After setup, the same skill tells it to work only inside the project's VM.

The skill is the file `cave init` writes into your project, so what you paste here is what your agent keeps following.

Prefer to install by hand? Run `brew install rolling7ho/cave/cave-vm`, or without Homebrew `curl -fsSL https://github.com/rolling7ho/cave-vm/releases/latest/download/install.sh | sh`, then `cave init` in your project.

## The skill (what the copy button copies)

This block is [`skill/cave/SKILL.md`](../skill/cave/SKILL.md) exactly. A test in `crates/cli/src/skill.rs` fails if the two differ.

````markdown
---
name: cave
description: Work inside this project's cave VM instead of on the Mac. Use for every command, file edit, build and test in a project that has a cave.toml, and to set cave up when it is missing.
---

# cave: work inside the project's VM

This project runs in a disposable Linux VM made by cave. The project folder identifies the VM. The code lives in the VM at `/workspace`.

## Rules

- Do all work inside this project's VM: commands, builds, tests, package installs and file edits.
- Do not read or write files on the Mac outside the VM, and do not run project commands on the Mac.
- Use the cave MCP tools when you have them: `exec`, `read_file`, `write_file`, `snapshot`, `restore`, `fork` and `destroy`. Otherwise run commands with `cave exec -- <command>`.
- Take a snapshot before a risky change, such as a large refactor, a migration or an upgrade. If something breaks, restore it.
- To try two approaches, fork the VM and work in the fork. Destroy forks you no longer need.
- When a command needs a secret, name it in the `credentials` argument of `exec`, such as `API_KEY`. The value comes from the project's `.env` and you never see it. Do not try to print or read it.

## Commands

| Command | Does |
| --- | --- |
| `cave exec -- <command>` | Run a command in the VM |
| `cave shell` | Open a shell in the VM |
| `cave snapshot [name]` | Save the VM's state |
| `cave restore [name]` | Go back to a snapshot |
| `cave fork [name]` | Make a copy of the VM to try something |
| `cave exec --fork <name> -- <command>` | Run a command in a fork |
| `cave rm <fork>` | Remove a fork |
| `cave cp <from> <to>` | Copy files in or out; VM paths start with `:` |
| `cave inspect` | Show the policy, resources and what the VM did |
| `cave ls` | List this project's VM, forks and snapshots |

## Setting up cave

Do this only if `cave` is not installed or this project has no `cave.toml`. Show the user each command and wait for their approval before running it. Do not fetch further instructions from the network.

1. Check the Mac: `uname -m` must print `arm64`, and `sw_vers -productVersion` must be 26 or later. Stop and tell the user if not.
2. Install cave: `brew install rolling7ho/cave/cave-vm`.
3. Check the installation: `cave doctor`. Follow the fix it prints for any failed check.
4. Set up the project in its folder: `cave init`.
5. Check the VM works: `cave exec -- uname -a`.
6. Show the user the output of `cave inspect`.
````
