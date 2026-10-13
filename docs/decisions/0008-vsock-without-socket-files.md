# 0008: vsock connections without socket files (D26)

Date: 2026-10-13 · Decided by: Ivan · Status: accepted

## Context

P3.5 planned the vsock device's host side as one Unix socket file per guest port, inside the project's `.cave/`. Running the guest agent in a real guest showed the problem: macOS limits a Unix socket path to 104 bytes, so binding or connecting fails in deeply nested projects. A project at `/Users/<name>/Documents/clients/<client>/<repo>/` is already close to the limit before `.cave/vsock/1024.sock` is added. The daemon serves many VMs from one process, so it cannot work around this by changing directory.

## Decision

- **D26.**
  - The vsock device creates no socket files.
  - To reach a guest port, the VMM asks the device for a connection and gets back the host end of a socket pair.
  - The VMM hands connections to the daemon over the control socket the daemon gave it when it started the VMM, as file descriptors (`SCM_RIGHTS`).
  - Guest connections to the host go to a handler the VMM registers for that port. A port with no handler is reset.

## Consequences

- Works at any project path.
- The sandbox profile (P3.13) has no socket files to allow, so the VMM needs only `.cave/` files and the image cache.
- ROADMAP P3.5 changes from "a host-side Unix socket per port, inside `.cave/`" to this.
- P3.10 (daemon) adds the VMM control socket and descriptor passing, using `sendmsg` and `recvmsg` in the `vmm` `host` module (D24).
