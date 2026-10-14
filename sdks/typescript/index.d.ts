/** Types for the cave-vm SDK (index.js). */

export declare const VERSION: string;
/** The project folder in the VM, where commands start and relative paths point. */
export declare const WORKSPACE: "/workspace";

/** An error from the daemon or the SDK: what went wrong, and how to fix it. */
export declare class CaveError extends Error {
  /** What went wrong, without the fix. */
  readonly detail: string;
  readonly fix?: string;
  /** The daemon's error code, such as `not_found`, when it sent one. */
  readonly code?: string;
  /** For a `conflict`, the files changed on both sides. */
  readonly files: string[];
}

/** Where the daemon is, and how to start it when it is not running. */
export declare class Daemon {
  /** `program` is `cave-daemon`, or undefined to never start one. */
  constructor(dataDir: string, program?: string);
  readonly dataDir: string;
  readonly program?: string;
  readonly socketPath: string;
  /** The daemon `cave` uses, started when needed. */
  static locate(): Daemon;
}

/** What `cave run` prints: what the VM can reach. */
export interface Policy {
  name: string;
  image: string;
  storage: "sealed" | "synced";
  net: "off" | "nat" | "lan";
  ports?: { mac: number; vm: number }[];
  cpus: number;
  memory_bytes: number;
  ttl_seconds?: number;
  widened?: string[];
}

export interface ExecOptions {
  /** The working directory in the VM; `/workspace` when not set. */
  cwd?: string;
  env?: Record<string, string>;
  /** Bytes for the command's stdin. Without them, stdin is `/dev/null`. */
  stdin?: string | Uint8Array;
  /** Credentials from the project's `.env`, such as `API_KEY`. Each is given
   * to the command as an environment variable and never returned; output that
   * prints it shows `[API_KEY]` instead, best effort. */
  credentials?: string[];
}

/** What a finished command printed and how it ended. */
export interface Output {
  code?: number;
  /** The signal that ended the command, such as `KILL`, when one did. */
  signal?: string;
  stdout: Buffer;
  stderr: Buffer;
  /** Whether the command exited with code 0. */
  success: boolean;
}

export interface Snapshot {
  name: string;
  /** Bytes on disk. */
  size: number;
}

export interface Restored {
  name: string;
  /** False when the VM booted from the snapshot's disk instead. */
  memoryRestored: boolean;
  reason?: string;
  /** The automatic snapshot of the state before, which undoes the restore. */
  saved?: string;
  credentials: string[];
}

export interface Forked {
  /** The new fork. */
  sandbox: Sandbox;
  /** The snapshot it was made from, or the VM whose state was forked. */
  source: string;
  reason?: string;
  credentials: string[];
}

/** A project's VM, or one of its forks. */
export declare class Sandbox {
  /** The project folder. */
  readonly project: string;
  /** The fork's name, or undefined for the project's own VM. */
  readonly forkName?: string;
  /** The VM of the project `path` is in, found from `cave.toml` as `cave` finds it. */
  static open(path?: string, options?: { daemon?: Daemon }): Promise<Sandbox>;
  /** The project's existing fork `name`. */
  forkNamed(name: string): Sandbox;
  start(): Promise<Policy>;
  exec(argv: string[], options?: ExecOptions): Promise<Output>;
  readFile(path: string): Promise<Buffer>;
  writeFile(path: string, contents: string | Uint8Array): Promise<number>;
  snapshot(name?: string): Promise<Snapshot>;
  restore(name?: string): Promise<Restored>;
  fork(options?: { name?: string; fromSnapshot?: string }): Promise<Forked>;
  /** Removes a fork, or the project's VM as `cave rm --yes` does. */
  destroy(): Promise<void>;
}
