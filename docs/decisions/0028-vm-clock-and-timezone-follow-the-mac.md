# 0028: The VM's clock and timezone follow the Mac, D57

Date: 2026-10-16 · Decided by: Ivan (choice), with defaults picked while building (marked) · Status: accepted

## Context

The VM has no clock of its own. The guest agent set the guest's time from the Mac's at every boot, restore and resume, but nothing corrected it afterwards, and the guest stayed on UTC. A VM left running while the Mac slept fell behind by the time it slept, and `date`, logs and timestamps in the VM did not match the Mac. Ivan asked for the VM's clock to match the user's, with the timezone as well.

## Decision

- **D57.** The VM's time and timezone follow the Mac's (Ivan).
- **When** (*default*): at every boot, restore and resume, as before, and also while the VM runs: the VMM compares the Mac's clock with its monotonic clock, which stops while the Mac sleeps, every 5 seconds, and sends the time again when they moved apart by a second or more, when the Mac's timezone changed, and every 10 minutes to correct drift.
- **Timezone** (*default*): the VMM reads the Mac's zone from the `/etc/localtime` link and sends its name and the Mac's compiled zone file with the time. The agent links `/etc/localtime` to the guest's own `/usr/share/zoneinfo/<name>` when the guest has it, and otherwise writes the Mac's file there, and writes the name to `/etc/timezone`. A name that is not a plain zone name, or a file that is not a zone file, is refused. A timezone that cannot be set leaves the old one and never stops a boot.

## Consequences

- The protocol's `restored` message gains an optional `timezone`. An older agent ignores it and keeps the guest on UTC until its image is rebuilt with the new agent.
- Programs in the VM that cache the timezone at start, rather than reading `/etc/localtime` again, keep the old one until restarted.
