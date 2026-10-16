// Tests against a fake daemon on a Unix socket. No network, no VM.
import assert from "node:assert/strict";
import { createHash, randomBytes } from "node:crypto";
import { mkdirSync, mkdtempSync, realpathSync, rmSync, unlinkSync, writeFileSync } from "node:fs";
import { createServer } from "node:net";
import { join } from "node:path";
import { afterEach, beforeEach, test } from "node:test";

import { CaveError, Daemon, Sandbox } from "../index.js";

const b64 = (data) => Buffer.from(data).toString("base64");
const sha256 = (data) => createHash("sha256").update(data).digest("hex");
const POLICY = { type: "policy", name: "proj", image: "base", storage: "sealed", net: "nat", cpus: 2, memory_bytes: 2 ** 31 };

/** Frames in and out of one fake-daemon connection. */
class Peer {
  constructor(socket) {
    this.socket = socket;
    this.buffer = Buffer.alloc(0);
    this.waiting = [];
    this.frames = [];
    socket.on("data", (chunk) => {
      this.buffer = Buffer.concat([this.buffer, chunk]);
      while (this.buffer.length >= 4 && this.buffer.length >= 4 + this.buffer.readUInt32BE(0)) {
        const n = this.buffer.readUInt32BE(0);
        const message = JSON.parse(this.buffer.subarray(4, 4 + n).toString());
        this.buffer = this.buffer.subarray(4 + n);
        const next = this.waiting.shift();
        if (next) next(message);
        else this.frames.push(message);
      }
    });
  }
  read() {
    if (this.frames.length > 0) return Promise.resolve(this.frames.shift());
    return new Promise((done) => this.waiting.push(done));
  }
  write(message) {
    const body = Buffer.from(JSON.stringify(message));
    const head = Buffer.alloc(4);
    head.writeUInt32BE(body.length);
    this.socket.write(Buffer.concat([head, body]));
  }
}

let root, data, project, server, requests;

/** Serves hello, records each request, and sends `handler`'s replies. */
function serve(handler) {
  requests = [];
  server = createServer(async (socket) => {
    const peer = new Peer(socket);
    const hello = await peer.read();
    assert.equal(hello.type, "hello");
    assert.ok(hello.versions.includes(1));
    peer.write({ type: "hello", version: 1, software: "fake" });
    const request = await peer.read();
    requests.push(request);
    for (const reply of await handler(request, peer)) peer.write(reply);
    socket.end();
  });
  return new Promise((done) => server.listen(join(data, "daemon.sock"), done));
}

const kind = (type) => requests.filter((r) => r.type === type);

beforeEach(() => {
  // Short, so the socket path fits in sun_path.
  root = realpathSync(mkdtempSync("/tmp/cvt"));
  data = join(root, "d");
  project = join(root, "proj");
  mkdirSync(data);
  mkdirSync(join(project, "src"), { recursive: true });
  writeFileSync(join(project, "cave.toml"), "");
});

afterEach(async () => {
  if (server?.listening) await new Promise((done) => server.close(done));
  rmSync(root, { recursive: true, force: true });
});

const open = () => Sandbox.open(join(project, "src"), { daemon: new Daemon(data) });

test("the project is found from a subfolder", async () => {
  await serve(() => []);
  assert.equal((await open()).project, project);
  await assert.rejects(Sandbox.open(data, { daemon: new Daemon(data) }), (e) => e instanceof CaveError && e.fix.includes("cave init"));
});

test("exec starts the VM, streams stdin and collects output", async () => {
  await serve(async (request, peer) => {
    if (request.type === "start") return [POLICY];
    const parts = [];
    for (;;) {
      const message = await peer.read();
      if (message.type === "input_end") break;
      parts.push(Buffer.from(message.data, "base64"));
    }
    const stdin = Buffer.concat(parts);
    return [
      { type: "progress", message: "working" },
      { type: "output", stream: "stdout", data: b64(stdin.toString().toUpperCase()) },
      { type: "output", stream: "stderr", data: b64("warn\n") },
      { type: "exit", code: 3 },
    ];
  });
  const sb = await open();
  const out = await sb.exec(["tr", "a-z", "A-Z"], { cwd: "/tmp", env: { A: "1" }, stdin: "hi\n".repeat(200000), credentials: ["API_KEY"] });
  assert.equal(out.stdout.toString(), "HI\n".repeat(200000));
  assert.equal(out.stderr.toString(), "warn\n");
  assert.equal(out.code, 3);
  assert.equal(out.success, false);
  const [request] = kind("exec");
  assert.deepEqual(request, {
    type: "exec",
    vm: { project },
    argv: ["tr", "a-z", "A-Z"],
    cwd: "/tmp",
    env: { A: "1" },
    stdin: true,
    credentials: ["API_KEY"],
  });
  assert.equal(kind("start").length, 1);
});

test("errors carry the message, fix, code and files", async () => {
  await serve(() => [{ type: "error", code: "conflict", message: "2 files changed", fix: "keep one", files: ["a", "b"] }]);
  const sb = await open();
  await assert.rejects(sb.snapshot("x"), (e) => {
    assert.ok(e instanceof CaveError);
    assert.equal(e.code, "conflict");
    assert.deepEqual(e.files, ["a", "b"]);
    assert.equal(e.message, "2 files changed\nfix: keep one");
    return true;
  });
  await assert.rejects(sb.exec([]), /the command is empty/);
});

test("files go both ways in chunks with their digest", async () => {
  const files = new Map();
  const contents = randomBytes(600 << 10);
  await serve(async (request, peer) => {
    if (request.type === "start") return [POLICY];
    if (request.type === "file_write") {
      const parts = [];
      for (;;) {
        const message = await peer.read();
        if (message.type === "file_end") {
          assert.equal(message.sha256, sha256(Buffer.concat(parts)));
          break;
        }
        const chunk = Buffer.from(message.data, "base64");
        assert.ok(chunk.length <= 256 << 10);
        parts.push(chunk);
      }
      files.set(request.path, Buffer.concat(parts));
      return [{ type: "file_written", size: files.get(request.path).length }];
    }
    const got = files.get(request.path);
    return [
      { type: "file_info", size: got.length, mode: 0o644 },
      { type: "file_data", data: b64(got.subarray(0, 1000)) },
      { type: "file_data", data: b64(got.subarray(1000)) },
      { type: "file_end", sha256: sha256(got) },
    ];
  });
  const sb = await open();
  assert.equal(await sb.writeFile("./notes/a.bin", contents), contents.length);
  assert.equal(kind("file_write")[0].path, "/workspace/notes/a.bin");
  assert.equal(kind("file_write")[0].create_dirs, true);
  assert.deepEqual(await sb.readFile("notes/a.bin"), contents);
  files.set("/etc/x", Buffer.from("x"));
  assert.equal((await sb.readFile("/etc/x")).toString(), "x");
});

test("snapshot, restore, fork and destroy", async () => {
  await serve((request) => {
    switch (request.type) {
      case "snapshot":
        return [{ type: "snapshot", name: request.name, size: 10 }];
      case "restore":
        return [{ type: "restored", name: "before", memory_restored: false, reason: "disk only", saved: "auto-1", credentials: ["API_KEY"] }];
      case "fork":
        return [{ type: "forked", name: "fork-1", from: request.from ?? "proj" }];
      default:
        return [{ type: "ok" }];
    }
  });
  const sb = await open();
  assert.equal((await sb.snapshot("before")).name, "before");
  assert.deepEqual(await sb.restore("before"), {
    name: "before",
    memoryRestored: false,
    reason: "disk only",
    saved: "auto-1",
    credentials: ["API_KEY"],
  });
  const forked = await sb.fork({ fromSnapshot: "before" });
  assert.equal(forked.source, "before");
  assert.equal(forked.sandbox.forkName, "fork-1");
  await forked.sandbox.destroy();
  assert.deepEqual(kind("remove"), [{ type: "remove", vm: { project, fork: "fork-1" } }]);
  assert.deepEqual(sb.forkNamed("try").vm(), { project, fork: "try" });
});

test("with no daemon and none to start, the error says how to start one", async () => {
  const sb = await open();
  try {
    unlinkSync(join(data, "daemon.sock"));
  } catch {
    // Never created.
  }
  await assert.rejects(sb.snapshot(), (e) => e instanceof CaveError && e.fix.includes("cave ls"));
});
