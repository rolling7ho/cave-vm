"""Tests against a fake daemon on a Unix socket. No network, no VM."""

import base64
import hashlib
import json
import os
import socket
import struct
import tempfile
import threading
import unittest
from pathlib import Path

from cave_vm import CaveError, Daemon, Sandbox


def b64(data: bytes) -> str:
    return base64.b64encode(data).decode()


class FakeDaemon:
    """Answers hello, records each request, and answers it with
    ``handler(request, conn)``, which returns the replies to send."""

    def __init__(self, data_dir: Path, handler):
        self.handler = handler
        self.requests = []
        # Short, so the socket path fits in sun_path.
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.bind(str(data_dir / "daemon.sock"))
        self.sock.listen()
        threading.Thread(target=self._serve, daemon=True).start()

    def _serve(self):
        while True:
            try:
                conn, _ = self.sock.accept()
            except OSError:
                return
            threading.Thread(target=self._handle, args=(conn,), daemon=True).start()

    @staticmethod
    def read(conn):
        head = b""
        while len(head) < 4:
            chunk = conn.recv(4 - len(head))
            if not chunk:
                return None
            head += chunk
        (n,) = struct.unpack(">I", head)
        body = b""
        while len(body) < n:
            body += conn.recv(n - len(body))
        return json.loads(body)

    @staticmethod
    def write(conn, message):
        body = json.dumps(message).encode()
        conn.sendall(struct.pack(">I", len(body)) + body)

    def _handle(self, conn):
        with conn:
            hello = self.read(conn)
            assert hello["type"] == "hello" and 1 in hello["versions"]
            self.write(conn, {"type": "hello", "version": 1, "software": "fake"})
            request = self.read(conn)
            if request is None:
                return
            self.requests.append(request)
            for reply in self.handler(request, conn):
                self.write(conn, reply)

    def close(self):
        self.sock.close()


POLICY = {"type": "policy", "name": "proj", "image": "base", "storage": "sealed", "net": "nat", "cpus": 2, "memory_bytes": 1 << 31}


class SandboxTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(dir="/tmp", prefix="cvp")
        root = Path(self.tmp.name).resolve()
        self.data = root / "d"
        self.data.mkdir()
        self.project = root / "proj"
        (self.project / "src").mkdir(parents=True)
        (self.project / "cave.toml").write_text("")
        self.files = {}

    def tearDown(self):
        self.fake.close()
        self.tmp.cleanup()

    def serve(self, handler):
        self.fake = FakeDaemon(self.data, handler)
        return Sandbox.open(self.project / "src", daemon=Daemon(self.data))

    def requests(self, kind):
        return [r for r in self.fake.requests if r["type"] == kind]

    def test_the_project_is_found_from_a_subfolder(self):
        sb = self.serve(lambda r, c: [])
        self.assertEqual(sb.project, self.project)
        with self.assertRaises(CaveError) as e:
            Sandbox.open(self.data, daemon=Daemon(self.data))
        self.assertIn("cave init", e.exception.fix)

    def test_exec_starts_the_vm_streams_stdin_and_collects_output(self):
        def handler(request, conn):
            if request["type"] == "start":
                return [POLICY]
            stdin = b""
            while True:
                message = FakeDaemon.read(conn)
                if message["type"] == "input_end":
                    break
                stdin += base64.b64decode(message["data"])
            return [
                {"type": "progress", "message": "working"},
                {"type": "output", "stream": "stdout", "data": b64(stdin.upper())},
                {"type": "output", "stream": "stderr", "data": b64(b"warn\n")},
                {"type": "exit", "code": 3},
            ]

        sb = self.serve(handler)
        out = sb.exec(["tr", "a-z", "A-Z"], cwd="/tmp", env={"A": "1"}, stdin=b"hi\n" * 200000, credentials=["API_KEY"])
        self.assertEqual(out.stdout, b"HI\n" * 200000)
        self.assertEqual(out.stderr, b"warn\n")
        self.assertEqual(out.code, 3)
        self.assertFalse(out.success)
        (request,) = self.requests("exec")
        self.assertEqual(request["vm"], {"project": str(self.project)})
        self.assertEqual(request["argv"], ["tr", "a-z", "A-Z"])
        self.assertEqual(request["cwd"], "/tmp")
        self.assertEqual(request["env"], {"A": "1"})
        self.assertTrue(request["stdin"])
        self.assertEqual(request["credentials"], ["API_KEY"])
        self.assertEqual(len(self.requests("start")), 1)

    def test_errors_carry_the_message_fix_and_code(self):
        def handler(request, conn):
            return [{"type": "error", "code": "conflict", "message": "2 files changed", "fix": "keep one", "files": ["a", "b"]}]

        sb = self.serve(handler)
        with self.assertRaises(CaveError) as e:
            sb.snapshot("x")
        self.assertEqual(e.exception.code, "conflict")
        self.assertEqual(e.exception.files, ["a", "b"])
        self.assertEqual(str(e.exception), "2 files changed\nfix: keep one")
        with self.assertRaises(CaveError) as e:
            sb.exec([])
        self.assertIn("empty", e.exception.message)

    def test_files_go_both_ways_in_chunks_with_their_digest(self):
        data = os.urandom(600 << 10)

        def handler(request, conn):
            kind = request["type"]
            if kind == "start":
                return [POLICY]
            if kind == "file_write":
                got = b""
                while True:
                    message = FakeDaemon.read(conn)
                    if message["type"] == "file_end":
                        assert message["sha256"] == hashlib.sha256(got).hexdigest()
                        break
                    assert len(base64.b64decode(message["data"])) <= 256 << 10
                    got += base64.b64decode(message["data"])
                self.files[request["path"]] = got
                return [{"type": "file_written", "size": len(got)}]
            got = self.files[request["path"]]
            return [
                {"type": "file_info", "size": len(got), "mode": 0o644},
                {"type": "file_data", "data": b64(got[:1000])},
                {"type": "file_data", "data": b64(got[1000:])},
                {"type": "file_end", "sha256": hashlib.sha256(got).hexdigest()},
            ]

        sb = self.serve(handler)
        self.assertEqual(sb.write_file("./notes/a.bin", data), len(data))
        self.assertEqual(self.requests("file_write")[0]["path"], "/workspace/notes/a.bin")
        self.assertTrue(self.requests("file_write")[0]["create_dirs"])
        self.assertEqual(sb.read_file("notes/a.bin"), data)
        self.files["/etc/x"] = b"x"
        self.assertEqual(sb.read_file("/etc/x"), b"x")

    def test_snapshot_restore_fork_and_destroy(self):
        def handler(request, conn):
            kind = request["type"]
            if kind == "snapshot":
                return [{"type": "snapshot", "name": request["name"], "size": 10}]
            if kind == "restore":
                return [{"type": "restored", "name": "before", "memory_restored": False, "reason": "disk only", "saved": "auto-1", "credentials": ["API_KEY"]}]
            if kind == "fork":
                return [{"type": "forked", "name": "fork-1", "from": request.get("from", "proj")}]
            return [{"type": "ok"}]

        sb = self.serve(handler)
        self.assertEqual(sb.snapshot("before").name, "before")
        restored = sb.restore("before")
        self.assertFalse(restored.memory_restored)
        self.assertEqual((restored.reason, restored.saved, restored.credentials), ("disk only", "auto-1", ["API_KEY"]))
        forked = sb.fork(from_snapshot="before")
        self.assertEqual(forked.source, "before")
        self.assertEqual(forked.sandbox.fork_name, "fork-1")
        forked.sandbox.destroy()
        (remove,) = self.requests("remove")
        self.assertEqual(remove, {"type": "remove", "vm": {"project": str(self.project), "fork": "fork-1"}})
        self.assertEqual(sb.fork_named("try")._vm(), {"project": str(self.project), "fork": "try"})

    def test_no_daemon_and_none_to_start_says_how_to_start_one(self):
        self.fake = FakeDaemon(self.data, lambda r, c: [])
        self.fake.close()
        os.unlink(self.data / "daemon.sock")
        sb = Sandbox.open(self.project, daemon=Daemon(self.data))
        with self.assertRaises(CaveError) as e:
            sb.snapshot()
        self.assertIn("cave ls", e.exception.fix)


if __name__ == "__main__":
    unittest.main()
