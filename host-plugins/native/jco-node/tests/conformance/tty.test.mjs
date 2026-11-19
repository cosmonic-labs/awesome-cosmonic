// Jco: packages/jco/test/node/tty.js, the deny and Node-provider cases of "componentizes and runs
// against the deny, scripted, and Node terminal providers (%s)". The runner's standard streams
// are pipes here, as in Jco's plain Node run, so the native values come from a Node process
// started the same way.
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { test } from "node:test";

import { component, invoke } from "./lib.mjs";

const NATIVE = `
const tty = require("node:tty");
const failure = (fn) => {
    try { fn(); return null; } catch (error) {
        return { name: error.name, code: error.code, message: error.message, errno: error.errno,
            syscall: error.syscall, info: error.info ?? null, rangeError: error instanceof RangeError };
    }
};
process.stdout.write(JSON.stringify({
    isatty: [0, 1, 2, 4096].map((fd) => tty.isatty(fd)),
    notATerminal: failure(() => new tty.WriteStream(4096)),
    init: failure(() => new tty.WriteStream(1)),
    invalidFd: failure(() => new tty.WriteStream(-1)),
}));
`;

function native() {
    return JSON.parse(execFileSync(process.execPath, ["-e", NATIVE], { stdio: ["pipe", "pipe", "pipe"] }));
}

for (const backend of ["starlingmonkey", "quickjs"]) {
    const build = () => component("node-tty", { args: ["--backend", backend], variant: backend });

    test(`node:tty with the interface refused (${backend})`, async () => {
        const wasm = await build();
        const { result } = await invoke(wasm, "run", ["denied"]);
        const guest = JSON.parse(result);
        assert.equal(guest.identity, true);
        assert.deepEqual(guest.outOfRange, [false, false, false, false, false]);
        assert.equal(guest.invalidFd.code, "ERR_INVALID_FD");
        assert.equal(guest.invalidFd.rangeError, true);
        assert.deepEqual(guest.denied, Array(4).fill("ERR_JCO_TTY_ADAPTER_REQUIRED"));
    });

    test(`node:tty with the interface granted, without a terminal (${backend})`, async () => {
        const wasm = await build();
        const expected = native();
        const { result } = await invoke(wasm, "run", ["node"], { grant: "tty" });
        const guest = JSON.parse(result);
        assert.deepEqual(guest.isatty, expected.isatty);
        assert.equal(guest.isatty[1], false);
        assert.deepEqual(guest.invalidFd, expected.invalidFd);
        assert.deepEqual(guest.notATerminal, expected.notATerminal);
        assert.equal(guest.notATerminal.code, "ERR_TTY_INIT_FAILED");
        assert.deepEqual(guest.init, expected.init);
    });
}
