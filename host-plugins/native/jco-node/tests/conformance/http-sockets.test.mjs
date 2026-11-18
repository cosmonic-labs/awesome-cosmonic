// Jco: packages/jco/test/node/http.js, "serves a request over wasi:sockets" (fixture
// node-http-sockets-server). node:http built over wasi:sockets imports no jco:node interface; it
// runs here on the host's WASI sockets. `start` never returns: the guest owns its accept loop.
import assert from "node:assert/strict";
import { test } from "node:test";
import { setTimeout as delay } from "node:timers/promises";

import { Session, component } from "./lib.mjs";

// The component imports Jco's wasi:sockets@0.2.10, whose `network` declares a `noop` method the
// host's WASI (wasmtime) lacks. The plugin supplies it for components that import jco:node, but
// this one imports none, so the plugin is never bound to it: that is the host's WASI to provide.
test("node:http over wasi:sockets serves a request", { todo: "the host's WASI lacks Jco's network.noop" }, async () => {
    const wasm = await component("node-http-sockets-server", {
        args: ["--backend", "starlingmonkey", "--with-nodejs-http-via", "wasi-sockets"],
    });
    const session = new Session(wasm, { allowNet: true });
    session.call("start").catch(() => {});
    try {
        let port;
        for (const deadline = Date.now() + 120_000; !port && Date.now() < deadline; ) {
            port = session.stderr.match(/listening on (\d+)/)?.[1];
            if (!port) {
                await delay(50);
            }
        }
        assert.ok(port, `the guest never reported its port:\n${session.stderr}`);
        const response = await fetch(`http://127.0.0.1:${port}/items`, { method: "POST", body: "hello" });
        assert.equal(await response.text(), "POST /items: hello");
    } finally {
        session.kill();
        await session.closed;
    }
});
