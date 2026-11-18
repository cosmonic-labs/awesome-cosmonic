// Jco: packages/jco/test/node/child-process.js, "componentizes and calls through the opt-in Node
// host". The children are real host processes, so `node` must be on the PATH the guest sees.
import assert from "node:assert/strict";
import { test } from "node:test";

import { component, invoke } from "./lib.mjs";

const build = () =>
    component("node-child-process", { entry: "source.js", world: "test", args: ["--backend", "starlingmonkey"] });

test("node:child_process with the interface granted", async () => {
    const wasm = await build();
    const { result } = await invoke(wasm, "run", [], { grant: "child-process", inheritEnv: true });
    assert.deepEqual(result, {
        execFile: "exec file",
        spawn: "spawn input",
        status: 0,
        shell: process.version,
    });
});

test("node:child_process refused traps with Jco's adapter-required code", async () => {
    const wasm = await build();
    await assert.rejects(invoke(wasm, "run", [], { inheritEnv: true }), (error) => {
        assert.match(error.message, /ERR_JCO_CHILD_PROCESS_ADAPTER_REQUIRED/);
        return true;
    });
});
