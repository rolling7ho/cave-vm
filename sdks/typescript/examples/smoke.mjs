// Drives a real VM through the SDK: run from a cave project folder.
// The e2e test `typescript_sdk_drives_a_real_vm` runs it.
import assert from "node:assert/strict";
import { Sandbox } from "../index.js";

const sb = await Sandbox.open(".");
const out = await sb.exec(["uname", "-s"]);
assert.ok(out.success);
assert.equal(out.stdout.toString(), "Linux\n");
assert.equal((await sb.exec(["cat"], { stdin: "piped" })).stdout.toString(), "piped");
assert.equal(await sb.writeFile("ts/hello.txt", "from typescript\n"), 16);
assert.equal((await sb.readFile("ts/hello.txt")).toString(), "from typescript\n");
await sb.snapshot("ts-before");
await sb.exec(["rm", "ts/hello.txt"]);
const fork = (await sb.fork({ fromSnapshot: "ts-before" })).sandbox;
assert.equal((await fork.readFile("ts/hello.txt")).toString(), "from typescript\n");
await fork.destroy();
console.log("typescript sdk ok");
