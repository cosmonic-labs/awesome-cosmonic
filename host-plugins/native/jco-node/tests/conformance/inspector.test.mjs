// Jco: packages/jco/test/node/inspector.js, "drives the real inspector through the opt-in Node
// host" (fixture node-inspector and its run.js), StarlingMonkey only as in Jco.
//
// Jco's test evaluates `6 * 7` through a real inspector session. This host runs the guest's
// JavaScript inside WebAssembly with no inspector protocol backend, so connecting a session is
// refused as unavailable (see src/hosts/inspector.rs) and Jco's expectations cannot be met: the
// test is kept, as a TODO, so the gap stays visible.
import assert from "node:assert/strict";
import { test } from "node:test";

import { Session, component } from "./lib.mjs";

const build = () => component("node-inspector", { entry: "source.js", world: "test", args: ["--backend", "starlingmonkey"] });

async function report(options) {
    const session = new Session(await build(), options);
    try {
        const results = JSON.parse(await session.call("run"));
        await new Promise((resolve) => setTimeout(resolve, 150));
        return { results, notified: JSON.parse(await session.call("observed")) };
    } catch (error) {
        const { stderr } = await session.close();
        throw Object.assign(error, { stderr });
    } finally {
        await session.close();
    }
}

test("node:inspector with the interface granted", { todo: "no inspector protocol backend in this host" }, async () => {
    const { results, notified } = await report({ grant: "inspector" });
    assert.equal(results.evalValue, 42);
    assert.equal(results.badMethod, "ERR_INVALID_ARG_TYPE");
    assert.equal(results.postBeforeConnect, "ERR_INSPECTOR_NOT_CONNECTED");
    assert.equal(results.doubleConnect, "ERR_INSPECTOR_ALREADY_CONNECTED");
    assert.equal(results.badCommand, "ERR_INSPECTOR_COMMAND");
    assert.equal(notified, true);
});

// Granted or not, connecting the fixture's session fails (unavailable, or denied), and the fixture
// does not catch it: the guest reports the uncaught error from `connect` and traps.
for (const [label, grant] of [
    ["granted: connecting a session is refused as unavailable", "inspector"],
    ["refused", undefined],
]) {
    test(`node:inspector ${label}`, async () => {
        await assert.rejects(report(grant ? { grant } : {}), (error) => {
            assert.equal(error.trap, true);
            assert.match(error.stderr, /_connect@/);
            return true;
        });
    });
}
