# 0002: `cave agent` runs the agent on the Mac

Date: 2026-10-11 · Decided by: Ivan · Status: accepted

## Context

The design said both that `cave agent claude` "starts an agent already running against a fresh VM" and that the default is the agent on the Mac using the VM through MCP, while "`cave agent <name>` runs the agent inside the VM instead". It did not say which one the command does.

## Decision

`cave agent <name>` starts the agent on the Mac, connected to the project's VM through the MCP server, with host Bash denied where the agent supports it. `cave agent <name> --in-vm` runs the agent inside the VM instead.

## Consequences

- The model API key stays on the Mac by default and never enters the VM.
- `--in-vm` needs the VM's network to reach the model API.
- The README quickstart (`cave agent claude`) gets the Mac-side behavior.
