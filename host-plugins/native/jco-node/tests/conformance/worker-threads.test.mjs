// Jco: packages/jco/test/node/worker-threads.js (fixture node-worker-threads), both backends.
//
// Jco's second test starts real workers: their bodies are JavaScript that its Node provider runs
// in a host `Worker`. This host has no JavaScript engine outside the component, so creating a
// worker is refused as unsupported (see src/hosts/worker_threads.rs); that test is kept as a TODO.
import assert from "node:assert/strict";
import { describe, test } from "node:test";
import { setTimeout as delay } from "node:timers/promises";

import { Session, component, invoke } from "./lib.mjs";

for (const backend of ["starlingmonkey", "quickjs"]) {
    describe(backend, () => {
        const build = () => component("node-worker-threads", { variant: backend, args: ["--backend", backend] });

        test("module, environment, marking, unsupported surface and catchable denial", async () => {
            const wasm = await build();
            const { result } = await invoke(wasm, "contract");
            assert.deepEqual(JSON.parse(result), { module: true, environment: true, marking: true, unsupported: true });
            const denied = await invoke(wasm, "denied");
            assert.equal(denied.result, "ERR_JCO_WORKER_THREADS_ADAPTER_REQUIRED");
        });

        test("granted, creating a worker is refused as unsupported", async () => {
            const { result } = await invoke(await build(), "denied", [], { grant: "worker-threads" });
            assert.equal(result, "ERR_JCO_UNSUPPORTED_NODE_API");
        });

        test(
            "real host workers exchange structured messages with isolated component instances",
            { todo: "worker bodies are JavaScript, and this host has no engine outside the component" },
            async () => {
                const session = new Session(await build(), { grant: "worker-threads" });
                try {
                    await session.call("start", 7);
                    let state;
                    for (let attempt = 0; attempt < 300; attempt++) {
                        state = JSON.parse(await session.call("status"));
                        if (state.exit !== null) {
                            break;
                        }
                        await delay(10);
                    }
                    assert.deepEqual(state.errors, []);
                    assert.deepEqual(state.events, ["online", "message", "exit"]);
                    assert.equal(state.exit, 0);
                } finally {
                    await session.close();
                }
            },
        );
    });
}
