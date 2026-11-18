// Jco: packages/jco/test/node/dgram.js, "module shape, 35 guest contract checks and catchable
// denial" and "UDP component passthrough" (fixture node-dgram and its run.js), for both backends.
import assert from "node:assert/strict";
import dgram from "node:dgram";
import { once } from "node:events";
import { test } from "node:test";
import { setTimeout as delay } from "node:timers/promises";

import { component, Session } from "./lib.mjs";

async function until(session, condition) {
    for (let attempt = 0; attempt < 500; attempt++) {
        const state = JSON.parse(await session.call("status"));
        assert.deepEqual(state.errors, []);
        if (condition(state)) {
            return state;
        }
        await delay(10);
    }
    throw Error("UDP component timed out: " + (await session.call("status")));
}

for (const backend of ["starlingmonkey", "quickjs"]) {
    const build = () => component("node-dgram", { args: ["--backend", backend], variant: backend });

    test(`node:dgram shape, contract checks and catchable denial (${backend})`, async () => {
        const session = new Session(await build());
        try {
            assert.equal(JSON.parse(await session.call("shape")).identity, true);
            assert.equal(await session.call("contract"), 35);
            assert.match(await session.call("denied"), /^ERR_JCO_DGRAM_ADAPTER_REQUIRED:/);
        } finally {
            await session.close();
        }
    });

    // Jco skips this one on QuickJS too: that backend does not yet run the work an exported
    // resource method queues when the host invokes it ("TODO(quickjs)" in Jco's test).
    test(`node:dgram UDP passthrough (${backend})`, { skip: backend === "quickjs" && "skipped in Jco: QuickJS host-invoked resource methods" }, async () => {
        const wasm = await build();
        const first = new Session(wasm, { grant: "dgram" });
        const second = new Session(wasm, { grant: "dgram" });
        const native = dgram.createSocket("udp4");
        const echo = dgram.createSocket("udp4");
        const native6 = dgram.createSocket("udp6");
        try {
            assert.deepEqual(JSON.parse(await first.call("shape")), {
                keys: ["Socket", "_createSocketHandle", "createSocket"],
                named: ["Socket", "_createSocketHandle", "createSocket", "default"],
                identity: true,
                socket: true,
                ref: true,
                type: "udp4",
            });
            const firstPort = await first.call("start", false);
            const secondPort = await second.call("start", false);
            const payloads = [Buffer.from("hello"), Buffer.from([0, 255, 42]), Buffer.alloc(0)];
            for (const [port, payload] of [
                [firstPort, payloads[0]],
                [firstPort, payloads[1]],
                [secondPort, payloads[2]],
            ]) {
                const response = once(native, "message");
                native.send(payload, port, "127.0.0.1");
                const [message, remote] = await response;
                assert.deepEqual(message, Buffer.concat([Buffer.from("echo:"), payload]));
                assert.equal(remote.port, port);
            }
            const state = await until(first, (state) => state.messages.length === 2 && state.sent.length === 2);
            assert.ok(
                state.messages.every(
                    (message) =>
                        message.buffer && message.remote.family === "IPv4" && message.remote.size === message.bytes.length,
                ),
            );
            assert.equal(JSON.parse(await second.call("status")).messages.length, 1);
            echo.on("message", (message, remote) => echo.send(message, remote.port, remote.address));
            echo.bind(0, "127.0.0.1");
            await once(echo, "listening");
            await first.call("client", echo.address().port);
            const client = await until(first, (state) => state.messages.length === 7 && state.sent.length === 7);
            assert.deepEqual(
                client.messages.slice(2).map((message) => message.bytes),
                [Array.from(Buffer.from("hello")), [97, 0, 255], [], Array.from(Buffer.from("lic")), Array.from(Buffer.from("lia"))],
            );
            assert.equal(client.connected, 1);
            assert.equal(client.listening, 2);
            assert.deepEqual(client.sent.slice(2), [5, 3, 0, 3, 3]);
            const ipv6Port = await first.call("start", true);
            const ipv6Response = once(native6, "message");
            native6.send("ipv6", ipv6Port, "::1");
            const [ipv6Message, ipv6Remote] = await ipv6Response;
            assert.equal(ipv6Message.toString(), "echo:ipv6");
            assert.equal(ipv6Remote.family, "IPv6");
            await until(first, (state) => state.messages.length === 8 && state.sent.length === 8);
            await first.call("finish", echo.address().port);
            await until(first, (state) => state.sent.length === 9 && state.closed === 1);
            await first.call("stop");
            await second.call("stop");
            assert.equal(JSON.parse(await first.call("status")).closed, 4);
            assert.equal(await first.call("jco:node/dgram-callbacks@0.1.0#take-socket-listener", 1), null);
        } finally {
            await first.call("stop").catch(() => {});
            await second.call("stop").catch(() => {});
            await first.close();
            await second.close();
            native.close();
            native6.close();
            echo.close();
        }
    });
}
