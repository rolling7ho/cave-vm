"""Drives a real VM through the SDK: run from a cave project folder.
The e2e test `python_sdk_drives_a_real_vm` runs it."""

from cave_vm import Sandbox

sb = Sandbox.open(".")
out = sb.exec(["uname", "-s"])
assert out.success and out.stdout == b"Linux\n", out
assert sb.exec(["cat"], stdin=b"piped").stdout == b"piped"
assert sb.write_file("py/hello.txt", b"from python\n") == 12
assert sb.read_file("py/hello.txt") == b"from python\n"
sb.snapshot("py-before")
sb.exec(["rm", "py/hello.txt"])
fork = sb.fork(from_snapshot="py-before").sandbox
assert fork.read_file("py/hello.txt") == b"from python\n"
fork.destroy()
print("python sdk ok")
