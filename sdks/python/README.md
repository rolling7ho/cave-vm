# cave-vm for Python

A thin client for [cave-vm](../../README.md) over the cave daemon's socket. It uses only the Python standard library and needs Python 3.9 or later. `cave` must be installed, because the SDK starts `cave-daemon` when it is not running.

```python
from cave_vm import Sandbox

sb = Sandbox.open(".")            # the project this folder is in
sb.snapshot("before")
out = sb.exec(["make", "test"])
if not out.success:
    sb.restore("before")
sb.write_file("notes.txt", b"tests failed")
fork = sb.fork(from_snapshot="before").sandbox
print(fork.exec(["uname", "-a"]).stdout.decode())
fork.destroy()
```

| Call | What it does |
| --- | --- |
| `Sandbox.open(path=".")` | The VM of the project `path` is in, found from `cave.toml` as `cave` finds it |
| `start()` | Boots the VM if needed and returns its policy |
| `exec(argv, cwd=, env=, stdin=, credentials=)` | Runs a command and returns an `Output` with `code`, `signal`, `stdout`, `stderr` and `success` |
| `read_file(path)` / `write_file(path, contents)` | Files in the VM; relative paths are under `/workspace` |
| `snapshot(name=None)` / `restore(name=None)` | Save the VM's state, and roll back to it |
| `fork(name=None, from_snapshot=None)` | A new copy-on-write fork; `.sandbox` is the fork |
| `fork_named(name)` | An existing fork |
| `destroy()` | Removes a fork, never a snapshot, or the project's VM as `cave rm --yes` does |

A credential named in `credentials`, such as `API_KEY`, is read from the project's `.env` by the daemon and given to that one command. Its value is never returned, and output that prints it shows `[API_KEY]` instead. [docs/safety.md](../../docs/safety.md) covers what that does not protect.

Errors raise `CaveError`, with `message`, `fix` and the daemon's `code`.

## Install

The SDK is not on PyPI yet. Install it from the release tag:

```sh
pip install "cave-vm @ git+https://github.com/rolling7ho/cave-vm@v1.0.0-rc.3#subdirectory=sdks/python"
```

## Tests

```sh
cd sdks/python && PYTHONPATH=src python3 -m unittest discover -s tests
```

The tests run against a fake daemon. `examples/smoke.py` drives a real VM, and the e2e test `python_sdk_drives_a_real_vm` runs it.
