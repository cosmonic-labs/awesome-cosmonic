// Jco: packages/jco/test/node/wasi.js, "componentizes and runs against the deny and Node
// providers (%s)". The native values are what `node:wasi` answers in this process.
import assert from "node:assert/strict";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import { WASI as NodeWASI } from "node:wasi";

import { component, invoke } from "./lib.mjs";

function failure(fn) {
    try {
        fn();
        return null;
    } catch (error) {
        if (typeof error !== "object" || error === null) {
            return { thrown: typeof error };
        }
        return {
            name: error.name,
            code: error.code,
            message: error.message,
            errno: error.errno,
            syscall: error.syscall,
            keys: Object.keys(error),
            typeError: error instanceof TypeError,
            rangeError: error instanceof RangeError,
        };
    }
}

function native(sandbox) {
    return {
        badVersion: failure(() => new NodeWASI({ version: "preview2" })),
        badStdin: failure(() => new NodeWASI({ version: "preview1", stdin: -1 })),
        missingPreopen: failure(() => new NodeWASI({ version: "preview1", preopens: { "/x": `${sandbox}/missing` } })),
        filePreopen: failure(() => new NodeWASI({ version: "preview1", preopens: { "/x": `${sandbox}/file` } })),
        badFd: failure(() => new NodeWASI({ version: "preview1", stdin: 12345 })),
        lateReturnOnExit: failure(
            () => new NodeWASI({ version: "preview1", returnOnExit: 1, preopens: { "/x": `${sandbox}/missing` } }),
        ),
        notStarted: failure(() => new NodeWASI({ version: "preview1" }).wasiImport.fd_write(1, 0, 0, 0)),
        einval: new NodeWASI({ version: "preview1" }).wasiImport.fd_write(1),
        startShape: failure(() => new NodeWASI({ version: "preview1" }).start({})),
        start: failure(() => new NodeWASI({ version: "preview1" }).start({ exports: {} })),
    };
}

for (const backend of ["starlingmonkey", "quickjs"]) {
    const build = () => component("node-wasi", { args: ["--backend", backend], variant: backend });

    test(`node:wasi with the interface refused (${backend})`, async () => {
        const wasm = await build();
        const sandbox = mkdtempSync(join(tmpdir(), "jco-node-wasi-"));
        try {
            // Jco compares both sides after JSON, which drops the fields Node leaves undefined.
            const expected = JSON.parse(JSON.stringify(native(sandbox)));
            const { result } = await invoke(wasm, "run", ["denied", sandbox]);
            const guest = JSON.parse(result);
            assert.equal(guest.identity, true);
            assert.deepEqual(guest.keys, ["WASI"]);
            assert.equal(guest.webAssembly, "undefined");
            assert.deepEqual(guest.badVersion, expected.badVersion);
            assert.deepEqual(guest.badStdin, expected.badStdin);
            assert.equal(guest.denied.name, "Error");
            assert.equal(guest.denied.code, "ERR_JCO_WASI_ADAPTER_REQUIRED");
            assert.deepEqual(guest.denied.keys, ["code"]);
            assert.match(
                guest.denied.message,
                /^node:wasi requires an application-provided host adapter\. Mapping one only makes construction behave as Node's does: running a module through node:wasi is not supported in a WebAssembly component/,
            );
        } finally {
            rmSync(sandbox, { recursive: true, force: true });
        }
    });

    test(`node:wasi with the interface granted (${backend})`, async () => {
        const wasm = await build();
        const sandbox = mkdtempSync(join(tmpdir(), "jco-node-wasi-"));
        writeFileSync(join(sandbox, "file"), "");
        try {
            // Jco compares both sides after JSON, which drops the fields Node leaves undefined.
            const expected = JSON.parse(JSON.stringify(native(sandbox)));
            const { result } = await invoke(wasm, "run", ["node", sandbox], { grant: "wasi" });
            const guest = JSON.parse(result);
            assert.deepEqual(guest.importObject, ["wasi_snapshot_preview1"]);
            assert.deepEqual(guest.unstable, ["wasi_unstable"]);
            assert.equal(guest.sameTable, true);
            assert.equal(guest.syscalls, 46);
            assert.deepEqual(guest.procExit, ["bound wasiReturnOnProcExit", 1]);
            assert.deepEqual(guest.fdWrite, ["bound fd_write", 4]);
            assert.deepEqual(guest.exitCode, { thrown: "symbol" });
            for (const key of [
                "notStarted",
                "einval",
                "startShape",
                "missingPreopen",
                "filePreopen",
                "badFd",
                "lateReturnOnExit",
            ]) {
                assert.deepEqual(guest[key], expected[key], key);
            }
            assert.equal(guest.missingPreopen.code, "UVWASI_ENOENT");
            assert.equal(guest.badFd.code, "UVWASI_EBADF");
            for (const key of ["start", "initialize"]) {
                assert.equal(guest[key].code, "ERR_JCO_UNSUPPORTED_NODE_API", key);
                assert.match(guest[key].message, /cannot instantiate a nested WebAssembly module/);
                assert.match(guest[key].message, /wac or wasm-tools compose/);
            }
        } finally {
            rmSync(sandbox, { recursive: true, force: true });
        }
    });
}
