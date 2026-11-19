// Jco: packages/jco/test/node/os.js, "componentizes and runs with the %s OS adapter".
import assert from "node:assert/strict";
import nativeOs from "node:os";
import { test } from "node:test";

import { component, invoke } from "./lib.mjs";

test("node:os with the interface granted", async () => {
    const wasm = await component("node-os");
    const { result } = await invoke(wasm, "run", [false], { grant: "os", inheritEnv: true });
    const report = JSON.parse(result);
    assert.equal(report.eol, "\n");
    assert.equal(report.devNull, "/dev/null");
    assert.equal(report.invalidArgument, 22);
    assert.equal(report.namespaceIdentity, true);
    assert.equal(report.arch, nativeOs.arch());
    assert.equal(report.platform, nativeOs.platform());
    assert.ok(report.parallelism > 0);
    assert.equal(report.username, nativeOs.userInfo().username);
    assert.equal(report.homedir, nativeOs.homedir());
    assert.equal(report.type, nativeOs.type());
    assert.equal(report.bufferedUsername, nativeOs.userInfo().username);
    assert.equal(report.loadavg.length, 3);
    assert.ok(report.loadavg.every(Number.isFinite));
    assert.ok(report.uptime > 0);
    let nativeError;
    try {
        nativeOs.getPriority(2147483647);
    } catch (error) {
        nativeError = { name: error.name, code: error.code, syscall: error.syscall, info: error.info };
    }
    assert.ok(nativeError, "the nonexistent PID should fail");
    assert.deepEqual(report.priorityError, nativeError);
});

test("node:os with the interface refused", async () => {
    const wasm = await component("node-os");
    const { result } = await invoke(wasm, "run", [true]);
    const report = JSON.parse(result);
    assert.equal(report.eol, "\n");
    assert.equal(report.devNull, "/dev/null");
    assert.equal(report.invalidArgument, 22);
    assert.deepEqual(report.errors, Array(6).fill({ name: "Error", code: "ERR_JCO_OS_ADAPTER_REQUIRED" }));
});
