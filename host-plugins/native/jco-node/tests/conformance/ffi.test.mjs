// Jco: packages/jco/test/node/ffi.js, "componentizes and calls native code through the opt-in Node
// host" (fixture node-ffi). Jco skips it unless its own Node runs with --experimental-ffi; here the
// native calls are the host's own, so it always runs.
import assert from "node:assert/strict";
import { test } from "node:test";

import { component, invoke } from "./lib.mjs";

const UNSUPPORTED = "ERR_JCO_UNSUPPORTED_NODE_API";
const build = () => component("node-ffi", { entry: "source.js", world: "test", args: ["--backend", "starlingmonkey"] });

test("node:ffi with the interface granted", async () => {
    const { result } = await invoke(await build(), "run", [], { grant: "ffi" });
    assert.deepEqual(JSON.parse(result), {
        abs: 7,
        allocated: true,
        readBack: 123456,
        text: "hello ffi",
        nativeLength: "9",
        bytes: [104, 101, 108, 108, 111],
        symbolIsBigInt: true,
        eventLoop: true,
        suffix: process.platform === "darwin" ? "dylib" : process.platform === "win32" ? "dll" : "so",
        rawPointer: UNSUPPORTED,
        liveView: UNSUPPORTED,
        callback: UNSUPPORTED,
        bufferArgument: UNSUPPORTED,
        afterClose: "ERR_FFI_LIBRARY_CLOSED",
    });
});

test("node:ffi with the interface refused", async () => {
    // The guest reads `ffi.suffix` first, a function with no error case: refused, it traps with
    // the code Jco's deny stub throws.
    await assert.rejects(invoke(await build(), "run", []), (error) => {
        assert.equal(error.trap, true);
        assert.match(error.message, /ERR_JCO_FFI_ADAPTER_REQUIRED/);
        return true;
    });
});
