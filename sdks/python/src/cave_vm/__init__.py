"""Python SDK for cave-vm: a thin client over the cave daemon's socket.

A ``Sandbox`` is a project's VM, or one of its forks, found from a folder
the way ``cave`` finds it. Each call is one request to the daemon
(docs/protocol.md), which is started when it is not running. Only the
standard library is used.

    from cave_vm import Sandbox

    sb = Sandbox.open(".")
    sb.snapshot("before")
    out = sb.exec(["make", "test"])
    if not out.success:
        sb.restore("before")
    sb.write_file("notes.txt", b"tests failed")
    fork = sb.fork(from_snapshot="before").sandbox
    fork.exec(["uname", "-a"])
    fork.destroy()
"""

import base64
import hashlib
import json
import os
import socket
import struct
import subprocess
import threading
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Dict, Iterable, List, Optional, Union

__version__ = "0.1.0"
__all__ = [
    "CaveError",
    "Forked",
    "Output",
    "Restored",
    "Sandbox",
    "Snapshot",
    "WORKSPACE",
]

#: The project folder in the VM, where commands start and relative paths
#: point.
WORKSPACE = "/workspace"

#: Protocol versions this SDK speaks.
VERSIONS = [1]

#: Largest frame a peer may send.
MAX_FRAME = 1 << 20

#: Largest piece of binary data in one message, before base64.
MAX_CHUNK = 256 << 10

CONFIG_FILE = "cave.toml"
DAEMON_START_TIMEOUT = 5.0


class CaveError(Exception):
    """An error from the daemon or the SDK: what went wrong, and how to fix
    it. ``code`` is the daemon's error code, such as ``not_found``, when
    the daemon sent one. ``files`` lists the files of a ``conflict``."""

    def __init__(
        self,
        message: str,
        fix: Optional[str] = None,
        code: Optional[str] = None,
        files: Optional[List[str]] = None,
    ):
        super().__init__(message)
        self.message = message
        self.fix = fix
        self.code = code
        self.files = files or []

    def __str__(self) -> str:
        return self.message if self.fix is None else f"{self.message}\nfix: {self.fix}"


@dataclass
class Output:
    """What a finished command printed and how it ended."""

    code: Optional[int] = None
    #: The signal that ended the command, such as ``KILL``, when one did.
    signal: Optional[str] = None
    stdout: bytes = b""
    stderr: bytes = b""

    @property
    def success(self) -> bool:
        """Whether the command exited with code 0."""
        return self.code == 0


@dataclass
class Snapshot:
    name: str
    #: Bytes on disk.
    size: int


@dataclass
class Restored:
    #: The snapshot restored.
    name: str
    #: False when the memory could not be restored, so the VM booted from
    #: the snapshot's disk and its processes were not kept.
    memory_restored: bool
    reason: Optional[str] = None
    #: The automatic snapshot of the state before, which undoes the restore.
    saved: Optional[str] = None
    #: Credentials injected before the snapshot was taken, by name.
    credentials: List[str] = field(default_factory=list)


@dataclass
class Forked:
    #: The new fork.
    sandbox: "Sandbox"
    #: The snapshot it was made from, or the VM whose state was forked.
    source: str
    reason: Optional[str] = None
    credentials: List[str] = field(default_factory=list)


def _error(reply: dict) -> CaveError:
    return CaveError(
        reply.get("message", "the daemon sent an error"),
        reply.get("fix"),
        reply.get("code"),
        reply.get("files"),
    )


def _unexpected(request: str, reply: dict) -> CaveError:
    return CaveError(
        f"the daemon sent an unexpected reply to {request}: {reply.get('type')}",
        "Update cave so the SDK and the daemon match",
    )


class _Connection:
    """One operation's connection to the daemon."""

    def __init__(self, sock: socket.socket):
        self._sock = sock
        self._send_lock = threading.Lock()

    def send(self, message: dict) -> None:
        body = json.dumps(message, separators=(",", ":")).encode()
        with self._send_lock:
            try:
                self._sock.sendall(struct.pack(">I", len(body)) + body)
            except OSError as e:
                raise _daemon_gone(e) from e

    def _read(self, n: int) -> bytes:
        data = b""
        while len(data) < n:
            try:
                chunk = self._sock.recv(n - len(data))
            except OSError as e:
                raise _daemon_gone(e) from e
            if not chunk:
                raise _daemon_gone("it closed the connection")
            data += chunk
        return data

    def receive(self) -> dict:
        (length,) = struct.unpack(">I", self._read(4))
        if length == 0 or length > MAX_FRAME:
            raise _daemon_gone(f"it sent a frame of {length} bytes")
        return json.loads(self._read(length))

    def reply(self) -> dict:
        """The next message that is not progress. An ``error`` raises."""
        while True:
            message = self.receive()
            kind = message.get("type")
            if kind == "progress":
                continue
            if kind == "error":
                raise _error(message)
            return message

    def close(self) -> None:
        self._sock.close()

    def __enter__(self) -> "_Connection":
        return self

    def __exit__(self, *_) -> None:
        self.close()


def _daemon_gone(e: object) -> CaveError:
    return CaveError(
        f"lost the connection to the cave daemon: {e}",
        "Run `cave ls` to restart the daemon, then try again",
    )


class Daemon:
    """Where the daemon is, and how to start it when it is not running."""

    def __init__(self, data_dir: Union[str, Path], program: Optional[str] = None):
        self.data_dir = Path(data_dir)
        #: The ``cave-daemon`` program, or None to never start one.
        self.program = program

    @classmethod
    def locate(cls) -> "Daemon":
        """The daemon ``cave`` uses: data directory ``CAVE_HOME`` or
        ``~/Library/Application Support/cave-vm``, started when needed from
        ``CAVE_DAEMON`` or ``cave-daemon`` on the ``PATH``."""
        data_dir = os.environ.get("CAVE_HOME")
        if not data_dir:
            home = os.environ.get("HOME")
            if not home:
                raise CaveError("HOME is not set", "Set HOME to your home folder")
            data_dir = os.path.join(home, "Library/Application Support/cave-vm")
        return cls(data_dir, os.environ.get("CAVE_DAEMON") or "cave-daemon")

    @property
    def socket_path(self) -> Path:
        return self.data_dir / "daemon.sock"

    def _open(self) -> socket.socket:
        sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        try:
            sock.connect(str(self.socket_path))
            return sock
        except OSError as e:
            sock.close()
            if self.program is None:
                raise CaveError(
                    f"cannot reach the cave daemon at {self.socket_path}: {e}",
                    "Start it by running any cave command, such as `cave ls`",
                ) from e
        return self._start()

    def _start(self) -> socket.socket:
        self.data_dir.mkdir(parents=True, exist_ok=True)
        log_path = self.data_dir / "daemon.log"
        try:
            with open(log_path, "ab") as log:
                subprocess.Popen(
                    [self.program],
                    stdin=subprocess.DEVNULL,
                    stdout=log,
                    stderr=log,
                    start_new_session=True,
                )
        except OSError as e:
            raise CaveError(
                f"cannot start the cave daemon ({self.program}): {e}",
                "Install cave, or set CAVE_DAEMON to the cave-daemon program",
            ) from e
        deadline = time.monotonic() + DAEMON_START_TIMEOUT
        while True:
            sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            try:
                sock.connect(str(self.socket_path))
                return sock
            except OSError as e:
                sock.close()
                if time.monotonic() >= deadline:
                    raise CaveError(
                        f"the cave daemon did not start: {e}",
                        f"See {log_path} for its output",
                    ) from e
                time.sleep(0.02)

    def connect(self) -> _Connection:
        """A new connection, after the handshake."""
        connection = _Connection(self._open())
        try:
            connection.send(
                {"type": "hello", "versions": VERSIONS, "software": f"cave-vm-python {__version__}"}
            )
            reply = connection.receive()
            if reply.get("type") == "error":
                error = _error(reply)
                error.fix = "Update cave so the SDK and the daemon match"
                raise error
            if reply.get("type") != "hello" or reply.get("version") not in VERSIONS:
                raise _unexpected("hello", reply)
            return connection
        except BaseException:
            connection.close()
            raise

    def call(self, request: dict) -> dict:
        """One request on a new connection, and its final reply."""
        with self.connect() as connection:
            connection.send(request)
            return connection.reply()


def _find_project(start: Path, home: Optional[Path]) -> Optional[Path]:
    for folder in [start, *start.parents]:
        if home is not None and folder == home:
            return None
        if (folder / CONFIG_FILE).is_file():
            return folder
    return None


def _vm_path(path: str) -> str:
    if path.startswith("/"):
        return path
    while path.startswith("./"):
        path = path[2:]
    return f"{WORKSPACE}/{path}"


def _b64(data: bytes) -> str:
    return base64.b64encode(data).decode()


class Sandbox:
    """A project's VM, or one of its forks."""

    def __init__(self, daemon: Daemon, project: Path, fork: Optional[str] = None):
        self.daemon = daemon
        #: The project folder.
        self.project = project
        #: The fork's name, or None for the project's own VM.
        self.fork_name = fork

    def __repr__(self) -> str:
        fork = f", fork={self.fork_name!r}" if self.fork_name else ""
        return f"Sandbox({str(self.project)!r}{fork})"

    @classmethod
    def open(cls, path: Union[str, Path] = ".", daemon: Optional[Daemon] = None) -> "Sandbox":
        """The VM of the project ``path`` is in: the folder with
        ``cave.toml``, or its nearest parent with one, up to the home
        folder."""
        start = Path(path).resolve()
        home = os.environ.get("HOME")
        project = _find_project(start, Path(home).resolve() if home else None)
        if project is None:
            raise CaveError(
                f"{start} is not in a cave project",
                "Run `cave init` in the project folder first",
            )
        return cls(daemon or Daemon.locate(), project)

    def fork_named(self, name: str) -> "Sandbox":
        """The project's existing fork ``name``. Nothing is checked until
        it is used."""
        return Sandbox(self.daemon, self.project, name)

    def _vm(self) -> dict:
        vm = {"project": str(self.project)}
        if self.fork_name is not None:
            vm["fork"] = self.fork_name
        return vm

    def start(self) -> dict:
        """Starts the VM if it is not running, and returns its policy: what
        it can reach."""
        reply = self.daemon.call({"type": "start", "vm": self._vm()})
        if reply.get("type") != "policy":
            raise _unexpected("start", reply)
        reply.pop("type")
        return reply

    def exec(
        self,
        argv: Iterable[str],
        *,
        cwd: Optional[str] = None,
        env: Optional[Dict[str, str]] = None,
        stdin: Optional[bytes] = None,
        credentials: Iterable[str] = (),
    ) -> Output:
        """Runs ``argv`` in the VM, starting it first if needed, and waits
        for it to finish. ``credentials`` names values from the project's
        ``.env``, such as ``API_KEY``: each is given to the command as an
        environment variable and never returned, and output that prints it
        shows ``[API_KEY]`` instead, best effort."""
        argv = list(argv)
        if not argv or not argv[0]:
            raise CaveError("the command is empty", 'Pass a program, such as ["uname", "-a"]')
        self.start()
        request = {"type": "exec", "vm": self._vm(), "argv": argv}
        if cwd is not None:
            request["cwd"] = cwd
        if env:
            request["env"] = dict(env)
        if stdin is not None:
            request["stdin"] = True
        credentials = list(credentials)
        if credentials:
            request["credentials"] = credentials
        with self.daemon.connect() as connection:
            connection.send(request)
            if stdin is not None:
                threading.Thread(
                    target=self._send_input, args=(connection, stdin), daemon=True
                ).start()
            out, err = bytearray(), bytearray()
            while True:
                reply = connection.reply()
                kind = reply.get("type")
                if kind == "output":
                    data = base64.b64decode(reply["data"])
                    (out if reply.get("stream") == "stdout" else err).extend(data)
                elif kind == "exit":
                    return Output(reply.get("code"), reply.get("signal"), bytes(out), bytes(err))
                else:
                    raise _unexpected("exec", reply)

    @staticmethod
    def _send_input(connection: _Connection, data: bytes) -> None:
        try:
            for i in range(0, len(data), MAX_CHUNK):
                connection.send({"type": "input", "data": _b64(data[i : i + MAX_CHUNK])})
            connection.send({"type": "input_end"})
        except CaveError:
            pass

    def read_file(self, path: str) -> bytes:
        """The contents of ``path`` in the VM, starting it first if needed.
        A relative path is under ``/workspace``."""
        self.start()
        path = _vm_path(path)
        with self.daemon.connect() as connection:
            connection.send({"type": "file_read", "vm": self._vm(), "path": path})
            contents = bytearray()
            while True:
                reply = connection.reply()
                kind = reply.get("type")
                if kind == "file_info":
                    continue
                if kind == "file_data":
                    contents.extend(base64.b64decode(reply["data"]))
                elif kind == "file_end":
                    if hashlib.sha256(contents).hexdigest() != reply.get("sha256"):
                        raise CaveError(f"the copy of {path} does not match the VM's file", "Try again")
                    return bytes(contents)
                else:
                    raise _unexpected("file_read", reply)

    def write_file(self, path: str, contents: bytes) -> int:
        """Writes ``contents`` to ``path`` in the VM, replacing it and
        creating missing folders, starting the VM first if needed. Returns
        the bytes written."""
        self.start()
        with self.daemon.connect() as connection:
            connection.send(
                {"type": "file_write", "vm": self._vm(), "path": _vm_path(path), "create_dirs": True}
            )
            for i in range(0, len(contents), MAX_CHUNK):
                connection.send({"type": "file_data", "data": _b64(contents[i : i + MAX_CHUNK])})
            connection.send({"type": "file_end", "sha256": hashlib.sha256(contents).hexdigest()})
            reply = connection.reply()
            if reply.get("type") != "file_written":
                raise _unexpected("file_write", reply)
            return reply["size"]

    def snapshot(self, name: Optional[str] = None) -> Snapshot:
        """Saves a snapshot, named ``name`` or the default name."""
        request = {"type": "snapshot", "vm": self._vm()}
        if name is not None:
            request["name"] = name
        reply = self.daemon.call(request)
        if reply.get("type") != "snapshot":
            raise _unexpected("snapshot", reply)
        return Snapshot(reply["name"], reply["size"])

    def restore(self, name: Optional[str] = None) -> Restored:
        """Puts the VM back to snapshot ``name``, by default the newest
        taken by name, saving the state before it first."""
        request = {"type": "restore", "vm": self._vm()}
        if name is not None:
            request["name"] = name
        reply = self.daemon.call(request)
        if reply.get("type") != "restored":
            raise _unexpected("restore", reply)
        return Restored(
            reply.get("name", ""),
            reply["memory_restored"],
            reply.get("reason"),
            reply.get("saved"),
            reply.get("credentials", []),
        )

    def fork(self, name: Optional[str] = None, from_snapshot: Optional[str] = None) -> Forked:
        """A new fork named ``name`` (``fork-1`` and so on by default), from
        snapshot ``from_snapshot`` or from the VM's current state."""
        request = {"type": "fork", "vm": self._vm()}
        if name is not None:
            request["name"] = name
        if from_snapshot is not None:
            request["from"] = from_snapshot
        reply = self.daemon.call(request)
        if reply.get("type") != "forked":
            raise _unexpected("fork", reply)
        return Forked(
            Sandbox(self.daemon, self.project, reply["name"]),
            reply.get("from", ""),
            reply.get("reason"),
            reply.get("credentials", []),
        )

    def destroy(self) -> None:
        """Removes the VM. For a fork, that is the fork alone. For the
        project's own VM, it is its disk, snapshots and forks, as with
        ``cave rm --yes``: in a sealed project, code that is only in the VM
        is lost. ``cave.toml`` stays."""
        request = {"type": "remove", "vm": {"project": str(self.project)}}
        if self.fork_name is not None:
            request["name"] = self.fork_name
        reply = self.daemon.call(request)
        if reply.get("type") != "ok":
            raise _unexpected("remove", reply)
