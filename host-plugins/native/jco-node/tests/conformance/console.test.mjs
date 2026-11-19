// Jco: packages/jco/test/node/console.js, "componentizes and runs default and custom consoles".
import assert from "node:assert/strict";
import { test } from "node:test";

import { component, invoke } from "./lib.mjs";

test("node:console writes to the component's stdout and stderr", async () => {
    const wasm = await component("node-console");
    const { result, stdout, stderr } = await invoke(wasm, "run", [], { grant: "console" });
    assert.equal(stderr, "guest stderr\n");
    assert.equal(stdout, "guest stdout 24\nguest: 1\ngroup\n  nested\n");
    assert.equal(result, "custom stdout\n|custom stderr\nAssertion failed: assertion 1\n");
});

test("node:console refused traps with Jco's adapter-required code", async () => {
    const wasm = await component("node-console");
    await assert.rejects(invoke(wasm, "run", []), (error) => {
        assert.ok(error.trap);
        assert.match(error.message, /ERR_JCO_CONSOLE_ADAPTER_REQUIRED/);
        return true;
    });
});
