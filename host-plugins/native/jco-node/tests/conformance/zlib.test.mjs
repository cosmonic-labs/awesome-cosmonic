// Jco: packages/jco/test/node/zlib.js, "node:zlib runs with Node and deny providers in %s".
import assert from "node:assert/strict";
import { test } from "node:test";

import { component, Session } from "./lib.mjs";

for (const backend of ["quickjs", "starlingmonkey"]) {
    const args = ["--backend", backend, ...(backend === "quickjs" ? ["--backend-qjs-disable-async"] : [])];
    const build = () => component("node-zlib", { args, variant: backend });

    test(`node:zlib with the interface granted (${backend})`, async () => {
        const wasm = await build();
        const session = new Session(wasm, { grant: "zlib" });
        const report = JSON.parse(await session.call("run", false));
        if (backend === "starlingmonkey") {
            report.callback = await session.call("run-async");
        }
        await session.close();
        assert.deepEqual(report, {
            identity: true,
            roundTrips: Array(5).fill(true),
            ...(backend === "starlingmonkey" ? { callback: true } : {}),
            incremental: true,
            streamText: "firstsecond",
            streamType: true,
            bytesWritten: 11,
            checksum: 0xcbf43926,
            info: true,
            corrupt: { name: "Error", code: "Z_DATA_ERROR" },
        });
    });

    test(`node:zlib with the interface refused (${backend})`, async () => {
        const wasm = await build();
        const session = new Session(wasm);
        const report = JSON.parse(await session.call("run", true));
        await session.close();
        assert.deepEqual(report, {
            errors: Array(3).fill({ name: "Error", code: "ERR_JCO_ZLIB_ADAPTER_REQUIRED" }),
            constant: 4,
        });
    });
}
