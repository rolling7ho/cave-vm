# 0027: Sign-in links from an agent in the VM open on the Mac, D56

Date: 2026-10-15 · Decided by: Ivan (choice), with defaults picked while building (marked) · Status: accepted

## Context

Since D53, `cave agent claude` runs Claude Code inside the VM, where you sign in once per project VM. The VM has no browser and cannot reach the Mac, so Claude Code prints the sign-in link instead of opening it. A link that long wraps across lines in Terminal, and selecting it copies the line breaks, so the sign-in page rejects it ("Invalid OAuth request"). Ivan asked for the link to open on the Mac.

## Decision

- **D56.** During `cave agent`, a sign-in link the agent in the VM tries to open is opened in the Mac's default browser (Ivan).
- **How** (*default*): `cave agent` installs a small helper at `~/.local/bin/cave-open` in the VM and starts the agent with `BROWSER` pointing at it. Claude Code opens links through `BROWSER` (checked in Claude Code 2.1.300). The helper writes the link to its terminal inside a private escape sequence, `ESC ] 7733 ; cave-open ; <link> BEL`. The `cave` process on the Mac relaying that terminal removes the sequence from the output and opens the link with `open`. Nothing new listens on the Mac, and nothing reaches it while no `cave agent` session is open.
- **Which links** (*default*): only `https://` links whose host is `claude.ai`, `claude.com` or `anthropic.com`, or a subdomain of one, with no user name or port. Anything else is ignored and stays in the output as text. Any program in the VM can write the sequence while the session is open, so the list keeps a confused or prompt-injected agent from opening arbitrary pages on the Mac.
- **Codex** (*default*): its browser sign-in returns to `localhost` on the machine with the browser, which is the Mac, not the VM, so it cannot finish there. Inside the VM, Codex signs in with `codex login --device-auth`, and the docs say so.

## Amendment: the sign-in returns to the VM

When the link opens, Claude Code signs in by sending the browser back to `http://localhost:<port>/callback`, a server it runs in the VM. On the Mac that address reaches nothing, so Safari showed "can't connect to the server" (found by Ivan in 1.0.0-rc.1). Now, when an allowed link's `redirect_uri` is `http://localhost:<port>/...` with a port of 1024 or more, `cave` listens on that port on the Mac's loopback (IPv4 and IPv6) until the browser arrives or 10 minutes pass. It passes the first request for that path to the same port inside the VM with `curl`, and sends the browser on to where the agent's server redirects it, if that is an allowed host, or shows a short page otherwise. Other paths get a 404. Nothing listens once the callback has arrived (*default*).

## Consequences

- This is the only thing a VM can make the Mac do on its own, and it lasts only while `cave agent` runs in your terminal. For a sign-in that returns to `localhost`, a port on the Mac's loopback stays open until the browser comes back, for at most 10 minutes.
- `cave shell` is unchanged: a link opened there is still printed, not opened.
- `docs/safety.md` lists it under the agent boundary.
