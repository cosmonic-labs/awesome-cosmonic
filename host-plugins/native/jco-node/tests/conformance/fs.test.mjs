// Jco: packages/jco/test/node/fs.js, "componentizes sync, callback, and promise APIs through the
// opt-in Node host".
import assert from "node:assert/strict";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";

import { component, invoke } from "./lib.mjs";

const build = () => component("node-fs", { args: ["--backend", "starlingmonkey"] });

test("node:fs with the interface granted", async () => {
    const wasm = await build();
    const root = await mkdtemp(join(tmpdir(), "jco-node-fs-"));
    try {
        const { result } = await invoke(wasm, "run", [root], { grant: "fs" });
        assert.deepEqual(result, {
            syncContents: "sync",
            callbackContents: "sync",
            promiseContents: "sync + promise",
            descriptorContents: "sync",
            isFile: true,
            entries: "message.txt",
        });
    } finally {
        await rm(root, { recursive: true, force: true });
    }
});

test("node:fs with the interface refused", async () => {
    const wasm = await build();
    const root = await mkdtemp(join(tmpdir(), "jco-node-fs-"));
    try {
        // The fixture does not catch the refusal, so the guest reports it and traps, as it would
        // on Jco's default (deny) provider.
        await assert.rejects(invoke(wasm, "run", [root]), (error) => {
            assert.ok(error.trap);
            assert.match(error.stderr, /ERR_JCO_FS_ADAPTER_REQUIRED|explicitly configured filesystem host provider/);
            return true;
        });
    } finally {
        await rm(root, { recursive: true, force: true });
    }
});
