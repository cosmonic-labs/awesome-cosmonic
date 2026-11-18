// Jco: packages/jco/test/node/cluster.js (fixtures node-cluster and node-cluster-roundtrip).
//
// As in Node, a worker is a fresh process running the same component: the runner forks by
// running itself again (see `--cluster-worker-invoke`), and the two talk over a socket pair.
import assert from "node:assert/strict";
import { test } from "node:test";
import { setTimeout as delay } from "node:timers/promises";

import { Session, component, invoke } from "./lib.mjs";

const build = (fixture) => component(fixture, { entry: "source.js", world: "test", args: ["--backend", "starlingmonkey"] });

test("node:cluster: the supported surface, forking, and the refused parts", async () => {
    const { result } = await invoke(await build("node-cluster"), "run", [], { grant: "cluster" });
    assert.deepEqual(result, {
        roleChecks: 3,
        constantChecks: 3,
        emitterChecks: 3,
        settingsChecks: 3,
        forkChecks: 3,
        workerProcessCode: "ERR_JCO_UNSUPPORTED_NODE_API",
        setupMasterCode: "ERR_JCO_UNSUPPORTED_DEPRECATED_NODE_API",
        isMasterCode: "ERR_JCO_UNSUPPORTED_DEPRECATED_NODE_API",
        hostSettingCode: "ERR_JCO_UNSUPPORTED_NODE_API",
        nonJsonMessageCode: "ERR_JCO_UNSUPPORTED_NODE_API",
        metaEventCode: "ERR_JCO_UNSUPPORTED_NODE_API",
    });
});

test("node:cluster: a forked worker runs the component and reports back", async () => {
    const session = new Session(await build("node-cluster-roundtrip"), {
        grant: "cluster",
        clusterWorkerInvoke: "report-in",
    });
    try {
        assert.equal(await session.call("is-primary"), true);
        await session.call("start");
        // The worker is a new process that compiles the component before it can report in.
        const until = async (done, ms) => {
            const deadline = Date.now() + ms;
            let progress = await session.call("poll");
            while (!done(progress) && Date.now() < deadline) {
                await delay(100);
                progress = await session.call("poll");
            }
            return progress;
        };
        let progress = await until((p) => p.messages, 120_000);
        await session.call("shutdown");
        progress = await until((p) => p.exited, 30_000);
        const message = JSON.parse(progress.messages.split("|")[0]);
        assert.equal(message.from, "worker");
        assert.equal(message.role, "roundtrip");
        assert.equal(typeof message.id, "number");
        for (const event of ["fork", "online", "message", "disconnect", "exit"]) {
            assert.ok(progress.events.split(",").includes(event), `expected a '${event}' event in ${progress.events}`);
        }
        assert.equal(progress.exited, true);
    } finally {
        await session.close();
    }
});

test("node:cluster with the interface refused", async () => {
    await assert.rejects(invoke(await build("node-cluster"), "run", []), (error) => {
        assert.equal(error.trap, true);
        assert.match(error.message, /ERR_JCO_CLUSTER_ADAPTER_REQUIRED/);
        return true;
    });
});
