// Jco: packages/jco/test/node/v8.js, "node:v8 with native and denied providers in %s" (fixture
// node-v8). The host has no V8 of its own, so `cpu` is a root-only profile rather than samples
// (see src/hosts/v8.rs); everything the fixture checks still holds.
import assert from "node:assert/strict";
import { test } from "node:test";

import { component, invoke } from "./lib.mjs";

for (const backend of ["quickjs", "starlingmonkey"]) {
    const args = ["--backend", backend, ...(backend === "quickjs" ? ["--backend-qjs-disable-async"] : [])];
    const build = () => component("node-v8", { args, variant: backend });

    test(`node:v8 with the interface granted (${backend})`, async () => {
        const report = JSON.parse((await invoke(await build(), "run", [false], { grant: "v8" })).result);
        assert.deepEqual(report, {
            module: true,
            graph: true,
            bytes: true,
            types: true,
            special: true,
            scalar: [0xffffffff, [0xffffffff, 0xfffffffe], -0.25, "abc"],
            identity: true,
            tag: true,
            heap: true,
            spaces: true,
            code: true,
            cpp: true,
            gc: true,
            cpu: true,
            unsupported: Array(3).fill("ERR_JCO_UNSUPPORTED_NODE_API"),
        });
    });

    test(`node:v8 with the interface refused (${backend})`, async () => {
        const report = JSON.parse((await invoke(await build(), "run", [true])).result);
        assert.deepEqual(report, {
            errors: Array(3).fill({ name: "Error", code: "ERR_JCO_V8_ADAPTER_REQUIRED" }),
            buildingSnapshot: false,
        });
    });
}
