// Jco: packages/jco/test/node/process.js, "process component runs with native and
// default-denied providers (%s)".
//
// Jco's provider hands the guest the embedding Node process, so its test compares the guest's
// view with that process. Here the guest gets a process of its own: the pid and ids are the
// host's, while argv, version and the working directory are what the embedder configured, so
// those are compared with what this test passed in.
import assert from "node:assert/strict";
import { test } from "node:test";

import { component, Session } from "./lib.mjs";

const ARGV = ["node", "/app/guest.js", "--flag"];
const VERSION = "v24.20.0";

for (const backend of ["starlingmonkey", "quickjs"]) {
    const build = () => component("node-process", { args: ["--backend", backend], variant: backend });

    test(`node:process with the interface granted (${backend})`, async () => {
        const wasm = await build();
        const session = new Session(wasm, {
            grant: "process",
            inheritEnv: true,
            argv: ARGV,
            nodeVersion: VERSION,
            cwd: process.cwd(),
        });
        const guest = JSON.parse(await session.call("run", "node"));
        if (backend === "starlingmonkey") {
            // QuickJS rejects Promise-returning exports for synchronous WIT functions.
            guest.tick = JSON.parse(await session.call("tick"));
        }
        await session.close();

        assert.equal(guest.failure, undefined);
        assert.equal(guest.identity, true);
        if (backend === "starlingmonkey") {
            assert.deepEqual(guest.tick, ["sync", "tick2"]);
        }
        assert.equal(guest.pid, session.pid);
        assert.equal(guest.ppid, process.pid);
        assert.equal(guest.arch, process.arch);
        assert.equal(guest.platform, process.platform);
        assert.equal(guest.version, VERSION);
        assert.deepEqual(guest.argv, ARGV);
        assert.equal(guest.cwd, process.cwd());
        assert.ok(guest.flagsSize > 0);
        assert.deepEqual(guest.environment, { value: "guest", enumerable: true, descriptor: "guest", deleted: true });
        // The guest's environment is its own: setting a variable there never reaches a host.
        assert.equal(process.env.JCO_PROCESS_E2E, undefined);
        assert.deepEqual(guest.events, ["once"]);
        assert.ok(guest.cpu.user >= 0);
        assert.ok(guest.memory.rss > 0);
        assert.ok(guest.rss > 0);
        assert.ok(guest.resources.maxRSS > 0);
        assert.ok(guest.time[0] >= 0);
        assert.ok(BigInt(guest.bigint) > 0n);
        assert.equal(guest.signalZero, true);
        assert.equal(guest.exitCode, 7);
        assert.deepEqual(guest.ids, {
            uid: process.getuid(),
            euid: process.geteuid(),
            gid: process.getgid(),
            egid: process.getegid(),
            groups: process.getgroups(),
        });
        assert.equal(guest.flag, true);
        assert.equal(guest.reportPid, session.pid);
        assert.equal(guest.missingDirectory.name, "Error");
        assert.equal(guest.missingDirectory.code, "ENOENT");
        assert.equal(guest.missingDirectory.syscall, "chdir");
        assert.equal(guest.unknownSignal.code, "ERR_UNKNOWN_SIGNAL");
        assert.equal(guest.deprecated.code, "ERR_JCO_UNSUPPORTED_DEPRECATED_NODE_API");
        assert.equal(guest.unsupported.code, "ERR_JCO_UNSUPPORTED_NODE_API");
    });

    test(`process.exit ends the guest with its status (${backend})`, async () => {
        const wasm = await build();
        const session = new Session(wasm, { grant: "process" });
        assert.deepEqual(await session.call("terminate", 23), { exit: 23 });
        const { status, stderr } = await session.closed.then((result) => ({ ...result, stderr: session.stderr }));
        assert.equal(stderr, "");
        assert.equal(status, 23);
    });

    test(`node:process with the interface refused (${backend})`, async () => {
        const wasm = await build();
        const session = new Session(wasm);
        const guest = JSON.parse(await session.call("run", "denied"));
        await session.close();
        assert.equal(guest.errors.length, 7);
        for (const error of guest.errors) {
            assert.equal(error.name, "Error");
            assert.equal(error.code, "ERR_JCO_PROCESS_ADAPTER_REQUIRED");
        }
    });
}
