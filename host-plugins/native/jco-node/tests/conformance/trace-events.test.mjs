// Jco: packages/jco/test/node/trace-events.js. Jco's runner also checks that the guest shares the
// embedding Node's trace agent; a Rust host has no such agent, so that half of the runner (the
// `native` tracing object) has no counterpart, and the trace file holds the plugin's own events.
import assert from "node:assert/strict";
import { mkdtemp, readFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";

import { component, Session } from "./lib.mjs";

for (const backend of ["starlingmonkey", "quickjs"]) {
    const args = ["--backend", backend, ...(backend === "quickjs" ? ["--backend-qjs-disable-async"] : [])];
    const build = () => component("node-trace-events", { entry: "source.js", wit: "source.wit", world: "test", args, variant: backend });

    test(`node:trace_events with the interface refused (${backend})`, async () => {
        const wasm = await build();
        const session = new Session(wasm);
        assert.equal(await session.call("contract"), 29);
        assert.equal(
            await session.call("denied"),
            "ERR_JCO_TRACE_EVENTS_ADAPTER_REQUIRED,ERR_JCO_TRACE_EVENTS_ADAPTER_REQUIRED",
        );
        await session.close();
    });

    test(`node:trace_events with the interface granted (${backend})`, async () => {
        const wasm = await build();
        const cwd = await mkdtemp(join(tmpdir(), "jco-node-trace-"));
        try {
            const session = new Session(wasm, { grant: ["trace-events", "fs"], cwd });
            assert.equal(await session.call("contract"), 29);
            assert.equal(await session.call("lifecycle"), 10);
            await session.call("start-tracing");
            await session.call("stop-tracing");
            await session.call("stop-tracing");
            await session.close();
            const trace = JSON.parse(await readFile(join(cwd, "node_trace.1.log"), "utf8"));
            assert.ok(Array.isArray(trace.traceEvents));
        } finally {
            await rm(cwd, { recursive: true, force: true });
        }
    });
}

