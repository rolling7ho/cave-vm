// TypeScript and JavaScript SDK for cave-vm: a thin client over the cave
// daemon's socket (docs/protocol.md). Node.js built-ins only; the types are
// in index.d.ts.
//
//   import { Sandbox } from "cave-vm";
//   const sb = await Sandbox.open(".");
//   await sb.snapshot("before");
//   const out = await sb.exec(["make", "test"]);
//   if (!out.success) await sb.restore("before");

import { spawn } from "node:child_process";
import { createHash } from "node:crypto";
import { existsSync, mkdirSync, openSync, realpathSync, statSync } from "node:fs";
import { connect as netConnect } from "node:net";
import { dirname, join, resolve } from "node:path";

export const VERSION = "1.0.0-rc.2";

/** The project folder in the VM, where commands start and relative paths point. */
export const WORKSPACE = "/workspace";

const VERSIONS = [1];
const MAX_FRAME = 1 << 20;
const MAX_CHUNK = 256 << 10;
const CONFIG_FILE = "cave.toml";
const DAEMON_START_TIMEOUT_MS = 5000;

/** An error from the daemon or the SDK: what went wrong, and how to fix it. */
export class CaveError extends Error {
  constructor(message, { fix, code, files } = {}) {
    super(fix === undefined ? message : `${message}\nfix: ${fix}`);
    this.name = "CaveError";
    this.detail = message;
    this.fix = fix;
    this.code = code;
    this.files = files ?? [];
  }
}

function fromReply(reply) {
  return new CaveError(reply.message ?? "the daemon sent an error", {
    fix: reply.fix,
    code: reply.code,
    files: reply.files,
  });
}

function unexpected(request, reply) {
  return new CaveError(`the daemon sent an unexpected reply to ${request}: ${reply.type}`, {
    fix: "Update cave so the SDK and the daemon match",
  });
}

function daemonGone(e) {
  return new CaveError(`lost the connection to the cave daemon: ${e}`, {
    fix: "Run `cave ls` to restart the daemon, then try again",
  });
}

const sha256 = (data) => createHash("sha256").update(data).digest("hex");
const b64 = (data) => Buffer.from(data).toString("base64");
const sleep = (ms) => new Promise((done) => setTimeout(done, ms));

/** One operation's connection to the daemon. */
class Connection {
  constructor(socket) {
    this.socket = socket;
    this.buffer = Buffer.alloc(0);
    this.frames = [];
    this.waiting = [];
    this.failure = null;
    socket.on("data", (chunk) => {
      this.buffer = Buffer.concat([this.buffer, chunk]);
      while (this.buffer.length >= 4) {
        const length = this.buffer.readUInt32BE(0);
        if (length === 0 || length > MAX_FRAME) {
          this.fail(daemonGone(`it sent a frame of ${length} bytes`));
          return;
        }
        if (this.buffer.length < 4 + length) break;
        const body = this.buffer.subarray(4, 4 + length).toString("utf8");
        this.buffer = this.buffer.subarray(4 + length);
        this.push(JSON.parse(body));
      }
    });
    socket.on("error", (e) => this.fail(daemonGone(e.message)));
    socket.on("close", () => this.fail(daemonGone("it closed the connection")));
  }

  push(message) {
    const next = this.waiting.shift();
    if (next) next.resolve(message);
    else this.frames.push(message);
  }

  fail(error) {
    if (this.failure) return;
    this.failure = error;
    for (const next of this.waiting.splice(0)) next.reject(error);
  }

  send(message) {
    const body = Buffer.from(JSON.stringify(message), "utf8");
    const head = Buffer.alloc(4);
    head.writeUInt32BE(body.length);
    return new Promise((done, failed) => {
      this.socket.write(Buffer.concat([head, body]), (e) => (e ? failed(daemonGone(e.message)) : done()));
    });
  }

  receive() {
    if (this.frames.length > 0) return Promise.resolve(this.frames.shift());
    if (this.failure) return Promise.reject(this.failure);
    return new Promise((resolve, reject) => this.waiting.push({ resolve, reject }));
  }

  /** The next message that is not progress. An `error` rejects. */
  async reply() {
    for (;;) {
      const message = await this.receive();
      if (message.type === "progress") continue;
      if (message.type === "error") throw fromReply(message);
      return message;
    }
  }

  close() {
    this.socket.destroy();
  }
}

function openSocket(path) {
  return new Promise((done, failed) => {
    const socket = netConnect(path);
    socket.once("connect", () => {
      socket.removeAllListeners("error");
      done(socket);
    });
    socket.once("error", failed);
  });
}

/** Where the daemon is, and how to start it when it is not running. */
export class Daemon {
  /**
   * @param dataDir the daemon's data directory
   * @param program the `cave-daemon` program, or undefined to never start one
   */
  constructor(dataDir, program) {
    this.dataDir = dataDir;
    this.program = program;
  }

  /** The daemon `cave` uses: data directory `CAVE_HOME` or
   * `~/Library/Application Support/cave-vm`, started when needed from
   * `CAVE_DAEMON` or `cave-daemon` on the `PATH`. */
  static locate() {
    let dataDir = process.env.CAVE_HOME;
    if (!dataDir) {
      if (!process.env.HOME) throw new CaveError("HOME is not set", { fix: "Set HOME to your home folder" });
      dataDir = join(process.env.HOME, "Library/Application Support/cave-vm");
    }
    return new Daemon(dataDir, process.env.CAVE_DAEMON || "cave-daemon");
  }

  get socketPath() {
    return join(this.dataDir, "daemon.sock");
  }

  async open() {
    try {
      return await openSocket(this.socketPath);
    } catch (e) {
      if (this.program === undefined) {
        throw new CaveError(`cannot reach the cave daemon at ${this.socketPath}: ${e.message}`, {
          fix: "Start it by running any cave command, such as `cave ls`",
        });
      }
    }
    return this.start();
  }

  async start() {
    mkdirSync(this.dataDir, { recursive: true });
    const log = join(this.dataDir, "daemon.log");
    const fd = openSync(log, "a");
    await new Promise((done, failed) => {
      const child = spawn(this.program, [], { stdio: ["ignore", fd, fd], detached: true });
      child.once("spawn", done);
      child.once("error", (e) =>
        failed(
          new CaveError(`cannot start the cave daemon (${this.program}): ${e.message}`, {
            fix: "Install cave, or set CAVE_DAEMON to the cave-daemon program",
          }),
        ),
      );
      child.unref();
    });
    const deadline = Date.now() + DAEMON_START_TIMEOUT_MS;
    for (;;) {
      try {
        return await openSocket(this.socketPath);
      } catch (e) {
        if (Date.now() >= deadline) {
          throw new CaveError(`the cave daemon did not start: ${e.message}`, { fix: `See ${log} for its output` });
        }
        await sleep(20);
      }
    }
  }

  /** A new connection, after the handshake. */
  async connect() {
    const connection = new Connection(await this.open());
    try {
      await connection.send({ type: "hello", versions: VERSIONS, software: `cave-vm-typescript ${VERSION}` });
      const reply = await connection.receive();
      if (reply.type === "error") {
        throw new CaveError(reply.message, { code: reply.code, fix: "Update cave so the SDK and the daemon match" });
      }
      if (reply.type !== "hello" || !VERSIONS.includes(reply.version)) throw unexpected("hello", reply);
      return connection;
    } catch (e) {
      connection.close();
      throw e;
    }
  }

  /** One request on a new connection, and its final reply. */
  async call(request) {
    const connection = await this.connect();
    try {
      await connection.send(request);
      return await connection.reply();
    } finally {
      connection.close();
    }
  }
}

function findProject(start, home) {
  let folder = start;
  for (;;) {
    if (home !== undefined && folder === home) return undefined;
    try {
      if (statSync(join(folder, CONFIG_FILE)).isFile()) return folder;
    } catch {
      // Not here; try the parent.
    }
    const parent = dirname(folder);
    if (parent === folder) return undefined;
    folder = parent;
  }
}

function vmPath(path) {
  if (path.startsWith("/")) return path;
  while (path.startsWith("./")) path = path.slice(2);
  return `${WORKSPACE}/${path}`;
}

/** A project's VM, or one of its forks. */
export class Sandbox {
  constructor(daemon, project, fork) {
    this.daemon = daemon;
    /** The project folder. */
    this.project = project;
    /** The fork's name, or undefined for the project's own VM. */
    this.forkName = fork;
  }

  /** The VM of the project `path` is in: the folder with `cave.toml`, or
   * its nearest parent with one, up to the home folder. */
  static async open(path = ".", { daemon } = {}) {
    const start = realpathSync(resolve(path));
    const home = process.env.HOME && existsSync(process.env.HOME) ? realpathSync(process.env.HOME) : undefined;
    const project = findProject(start, home);
    if (project === undefined) {
      throw new CaveError(`${start} is not in a cave project`, { fix: "Run `cave init` in the project folder first" });
    }
    return new Sandbox(daemon ?? Daemon.locate(), project);
  }

  /** The project's existing fork `name`. Nothing is checked until it is used. */
  forkNamed(name) {
    return new Sandbox(this.daemon, this.project, name);
  }

  vm() {
    return this.forkName === undefined ? { project: this.project } : { project: this.project, fork: this.forkName };
  }

  /** Starts the VM if it is not running, and returns its policy. */
  async start() {
    const { type, ...policy } = await this.daemon.call({ type: "start", vm: this.vm() });
    if (type !== "policy") throw unexpected("start", { type });
    return policy;
  }

  /** Runs `argv` in the VM, starting it first if needed, and waits for it
   * to finish. */
  async exec(argv, { cwd, env, stdin, credentials } = {}) {
    if (argv.length === 0 || !argv[0]) {
      throw new CaveError("the command is empty", { fix: 'Pass a program, such as ["uname", "-a"]' });
    }
    await this.start();
    const request = { type: "exec", vm: this.vm(), argv: [...argv] };
    if (cwd !== undefined) request.cwd = cwd;
    if (env && Object.keys(env).length > 0) request.env = { ...env };
    if (stdin !== undefined) request.stdin = true;
    if (credentials && credentials.length > 0) request.credentials = [...credentials];
    const connection = await this.daemon.connect();
    try {
      await connection.send(request);
      if (stdin !== undefined) {
        const data = typeof stdin === "string" ? Buffer.from(stdin, "utf8") : Buffer.from(stdin);
        // Sent while output is read, so neither side waits on a full socket.
        (async () => {
          for (let i = 0; i < data.length; i += MAX_CHUNK) {
            await connection.send({ type: "input", data: b64(data.subarray(i, i + MAX_CHUNK)) });
          }
          await connection.send({ type: "input_end" });
        })().catch(() => {});
      }
      const stdout = [];
      const stderr = [];
      for (;;) {
        const reply = await connection.reply();
        if (reply.type === "output") {
          (reply.stream === "stdout" ? stdout : stderr).push(Buffer.from(reply.data, "base64"));
        } else if (reply.type === "exit") {
          return {
            code: reply.code,
            signal: reply.signal,
            stdout: Buffer.concat(stdout),
            stderr: Buffer.concat(stderr),
            success: reply.code === 0,
          };
        } else {
          throw unexpected("exec", reply);
        }
      }
    } finally {
      connection.close();
    }
  }

  /** The contents of `path` in the VM. A relative path is under `/workspace`. */
  async readFile(path) {
    await this.start();
    const full = vmPath(path);
    const connection = await this.daemon.connect();
    try {
      await connection.send({ type: "file_read", vm: this.vm(), path: full });
      const parts = [];
      for (;;) {
        const reply = await connection.reply();
        if (reply.type === "file_info") continue;
        if (reply.type === "file_data") {
          parts.push(Buffer.from(reply.data, "base64"));
        } else if (reply.type === "file_end") {
          const contents = Buffer.concat(parts);
          if (sha256(contents) !== reply.sha256) {
            throw new CaveError(`the copy of ${full} does not match the VM's file`, { fix: "Try again" });
          }
          return contents;
        } else {
          throw unexpected("file_read", reply);
        }
      }
    } finally {
      connection.close();
    }
  }

  /** Writes `contents` to `path` in the VM, replacing it and creating
   * missing folders. Returns the bytes written. */
  async writeFile(path, contents) {
    await this.start();
    const data = typeof contents === "string" ? Buffer.from(contents, "utf8") : Buffer.from(contents);
    const connection = await this.daemon.connect();
    try {
      await connection.send({ type: "file_write", vm: this.vm(), path: vmPath(path), create_dirs: true });
      for (let i = 0; i < data.length; i += MAX_CHUNK) {
        await connection.send({ type: "file_data", data: b64(data.subarray(i, i + MAX_CHUNK)) });
      }
      await connection.send({ type: "file_end", sha256: sha256(data) });
      const reply = await connection.reply();
      if (reply.type !== "file_written") throw unexpected("file_write", reply);
      return reply.size;
    } finally {
      connection.close();
    }
  }

  /** Saves a snapshot, named `name` or the default name. */
  async snapshot(name) {
    const request = { type: "snapshot", vm: this.vm() };
    if (name !== undefined) request.name = name;
    const reply = await this.daemon.call(request);
    if (reply.type !== "snapshot") throw unexpected("snapshot", reply);
    return { name: reply.name, size: reply.size };
  }

  /** Puts the VM back to snapshot `name`, by default the newest taken by
   * name, saving the state before it first. */
  async restore(name) {
    const request = { type: "restore", vm: this.vm() };
    if (name !== undefined) request.name = name;
    const reply = await this.daemon.call(request);
    if (reply.type !== "restored") throw unexpected("restore", reply);
    return {
      name: reply.name ?? "",
      memoryRestored: reply.memory_restored,
      reason: reply.reason,
      saved: reply.saved,
      credentials: reply.credentials ?? [],
    };
  }

  /** A new fork named `name` (`fork-1` and so on by default), from snapshot
   * `fromSnapshot` or from the VM's current state. */
  async fork({ name, fromSnapshot } = {}) {
    const request = { type: "fork", vm: this.vm() };
    if (name !== undefined) request.name = name;
    if (fromSnapshot !== undefined) request.from = fromSnapshot;
    const reply = await this.daemon.call(request);
    if (reply.type !== "forked") throw unexpected("fork", reply);
    return {
      sandbox: new Sandbox(this.daemon, this.project, reply.name),
      source: reply.from ?? "",
      reason: reply.reason,
      credentials: reply.credentials ?? [],
    };
  }

  /** Removes the VM: a fork alone, or the project's VM with its disk,
   * snapshots and forks, as `cave rm --yes` does. `cave.toml` stays. */
  async destroy() {
    const request = { type: "remove", vm: { project: this.project } };
    if (this.forkName !== undefined) request.name = this.forkName;
    const reply = await this.daemon.call(request);
    if (reply.type !== "ok") throw unexpected("remove", reply);
  }
}
