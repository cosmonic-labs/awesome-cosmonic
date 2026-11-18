// Jco: packages/jco/test/node/process.js, "custom process provider handles guest exit without
// exiting the host" (fixture node-process-custom). Jco's embedder provider records the exit and
// fails the call; here the embedder is jco-node-run, which reports the exit request instead of
// returning, while its caller and other instances carry on.
import assert from "node:assert/strict";
import { test } from "node:test";

import { Session, component } from "./lib.mjs";

for (const backend of ["starlingmonkey", "quickjs"]) {
    test(`node:process exit requests stay with their instance (${backend})`, async () => {
        const wasm = await component("node-process-custom", { args: ["--backend", backend], variant: backend });
        const a = new Session(wasm, { grant: "process" });
        const b = new Session(wasm, {});
        try {
            assert.equal(await b.call("denied"), "ERR_JCO_PROCESS_ADAPTER_REQUIRED");
            assert.deepEqual(await a.call("run"), { exit: 23 });
            assert.equal((await a.closed).status, 23);
            // The other instance, and this process, are unaffected.
            assert.equal(await b.call("denied"), "ERR_JCO_PROCESS_ADAPTER_REQUIRED");
            assert.equal(process.exitCode, undefined);
        } finally {
            await a.close();
            await b.close();
        }
    });
}
