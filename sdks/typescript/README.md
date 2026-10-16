# cave-vm for TypeScript

A thin client for [cave-vm](../../README.md) over the cave daemon's socket. It is plain JavaScript with hand-written types in `index.d.ts`, uses only Node's built-in modules and needs Node 20 or later. `cave` must be installed, because the SDK starts `cave-daemon` when it is not running.

```ts
import { Sandbox } from "cave-vm";

const sb = await Sandbox.open(".");  // the project this folder is in
await sb.snapshot("before");
const out = await sb.exec(["make", "test"]);
if (!out.success) await sb.restore("before");
await sb.writeFile("notes.txt", "tests failed");
const fork = (await sb.fork({ fromSnapshot: "before" })).sandbox;
console.log((await fork.exec(["uname", "-a"])).stdout.toString());
await fork.destroy();
```

| Call | What it does |
| --- | --- |
| `Sandbox.open(path = ".")` | The VM of the project `path` is in, found from `cave.toml` as `cave` finds it |
| `start()` | Boots the VM if needed and returns its policy |
| `exec(argv, { cwd, env, stdin, credentials })` | Runs a command and returns `{ code, signal, stdout, stderr, success }`, with the output as Buffers |
| `readFile(path)` / `writeFile(path, contents)` | Files in the VM; relative paths are under `/workspace` |
| `snapshot(name?)` / `restore(name?)` | Save the VM's state, and roll back to it |
| `fork({ name, fromSnapshot })` | A new copy-on-write fork; `.sandbox` is the fork |
| `forkNamed(name)` | An existing fork |
| `destroy()` | Removes a fork, or the project's VM as `cave rm --yes` does |

A credential named in `credentials`, such as `API_KEY`, is read from the project's `.env` by the daemon and given to that one command. Its value is never returned, and output that prints it shows `[API_KEY]` instead. [docs/safety.md](../../docs/safety.md) covers what that does not protect.

Errors reject with `CaveError`, with `detail`, `fix` and the daemon's `code`.

## Install

The SDK is not on npm yet. Install it from a checkout of the release tag:

```sh
git clone --depth 1 --branch v1.0.0-rc.3 https://github.com/rolling7ho/cave-vm
npm install ./cave-vm/sdks/typescript
```

## Tests

```sh
cd sdks/typescript && npm test
```

The tests run against a fake daemon. `examples/smoke.mjs` drives a real VM, and the e2e test `typescript_sdk_drives_a_real_vm` runs it.
