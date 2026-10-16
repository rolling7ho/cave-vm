# Security Policy

cave-vm runs untrusted code (coding agents) inside virtual machines on your Mac. A flaw in cave-vm can let that code reach your files, your network or your credentials, so we take security reports seriously.

## Reporting a vulnerability

Please do not open a public issue, pull request or discussion for a security problem.

Report it privately through GitHub: open the **Security** tab of this repository and choose **Report a vulnerability**. Include as much of the following as you can:

- What the problem is and which component it affects (for example the VMM, a device model, the guest agent, the daemon or the CLI).
- The cave-vm version or commit, your macOS version, and the guest image (Debian or Ubuntu).
- Steps to reproduce, or a proof of concept.
- What an attacker gains: for example code execution on the Mac, reading host files, or leaking a credential.

If you cannot use GitHub's reporting, open a public issue that says only that you have a security report and want a private way to share it. Do not put details in it.

## What to expect

- We aim to acknowledge your report within 7 days.
- We aim to tell you whether we consider it a vulnerability, and what happens next, within 30 days.
- We will keep you updated while we work on a fix, and credit you in the advisory if you want that.
- We ask that you give us a reasonable chance to fix the problem before you disclose it publicly. We will coordinate a date with you.

cave-vm is a small open-source project, so these are targets, not guarantees.

## Supported versions

The three newest stable releases, and `main`, get security fixes. Release candidates, betas and older releases get none. cave asks to update when a newer release is out, and a copy two or more releases behind does not run in a terminal until it is updated, except `cave pull`, so work can always be copied out (D52). Update with `brew upgrade cave-vm` or by running the install script again.

## What counts as a vulnerability

The things cave-vm promises to protect, and so the reports we most want:

- **Guest to host escape:** code in a VM gets code execution on the Mac, or reads or writes host files outside what the VM's policy allows. Bugs in the VMM's device models (virtio block, net, vsock, balloon, serial) are the most likely source.
- **Policy bypass:** a VM reaches something its policy denies, such as the local network when private ranges are denied, or a Mac file outside what the VM's policy reaches.
- **Credential leaks:** a credential is exposed to the agent, written to a log, or stored somewhere it should not be. Credentials are meant to be injected into a command and never returned to the agent, and logs record names, not values.
- **Daemon and socket access:** another user or process can talk to the daemon's socket, or a VM can reach the daemon beyond its intended interface.
- **Snapshot and image integrity:** a tampered snapshot, image, kernel or update response is accepted without detection.
- **Install and update flow:** the setup skill, the install script, the update detector or cave's self-update (`Update now?`) can be made to run attacker-controlled commands, fetch attacker-controlled content or install an attacker's build.
- **Memory safety bugs** in any `unsafe` code, in particular the Hypervisor.framework wrapper and guest-memory handling.

## What is not a vulnerability

- Anything an agent does **inside** a VM within the VM's policy, such as deleting files in the VM, installing packages, or using the network when the policy allows it. The VM exists so that this is contained.
- Secrets that a user chooses to place inside a VM. A snapshot of a VM that has held a credential can contain it, and cave-vm warns when you fork, restore or pull one.
- Access that the user opts into with a flag, such as `--net lan`.
- Attacks that need an attacker who already has code execution as your macOS user on the host, or physical access to the Mac.
- Vulnerabilities in Apple's Hypervisor.framework or macOS itself. Please report those to Apple.
- Vulnerabilities in the Linux kernel or distribution packages that are not specific to how cave-vm configures or ships them. Report those upstream.

If you are not sure whether something qualifies, report it anyway.

## Safe harbor

We will not take legal action against people who research and report vulnerabilities in good faith under this policy: you test only on your own machines, you do not access or damage anyone else's data, and you give us time to fix the problem before disclosing it.
