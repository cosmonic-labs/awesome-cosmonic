// Jco: packages/jco/test/node/http.js, "serves a request through guest -> WIT callback resource
// -> host node:http" (fixture node-http-server and its run.js) and "componentizes and performs a
// local request via direct" (fixture node-http and its peer.js).
import assert from "node:assert/strict";
import http from "node:http";
import { test } from "node:test";

import { component, invoke, Session } from "./lib.mjs";

const DIRECT = ["--backend", "starlingmonkey", "--with-nodejs-http-via", "direct"];

function request(port, path = "/items", body = "hello") {
    return new Promise((resolve, reject) => {
        const outgoing = http.request(`http://127.0.0.1:${port}${path}`, { method: "POST" }, (response) => {
            response.setEncoding("utf8");
            const chunks = [];
            response.on("data", (chunk) => chunks.push(chunk));
            response.once("end", () => resolve(chunks.join("")));
        });
        outgoing.once("error", reject);
        outgoing.end(body);
    });
}

test("node:http serves requests through the guest's callback listener", async () => {
    const wasm = await component("node-http-server", { args: DIRECT });
    const first = new Session(wasm, { grant: "http" });
    const second = new Session(wasm, { grant: "http" });
    try {
        const firstPort = await first.call("start");
        const secondPort = await second.call("start");
        const body = await request(firstPort);
        assert.equal(body, "POST /items: hello");
        assert.deepEqual(
            await Promise.all([
                request(firstPort, "/first", "one"),
                request(firstPort, "/second", "two"),
                request(secondPort, "/other", "three"),
            ]),
            ["POST /first: one", "POST /second: two", "POST /other: three"],
        );
        assert.equal(await first.call("count"), 3);
        assert.equal(await second.call("count"), 1);
        await first.call("stop");
        assert.equal(await first.call("jco:node/http-callbacks@0.1.0#take-request-listener", 1), null);
        const restartedPort = await first.call("start");
        assert.equal(await request(restartedPort), body);
        assert.equal(await first.call("count"), 4);
        assert.equal(await second.call("count"), 1);
    } finally {
        await first.call("stop").catch(() => {});
        await second.call("stop").catch(() => {});
        await first.close();
        await second.close();
    }
});

test("node:http performs a client request through the host", async () => {
    const wasm = await component("node-http", { args: DIRECT, variant: "direct" });
    const server = http.createServer((_request, response) => {
        response.setHeader("Content-Type", "text/plain");
        response.end("hello from node:http");
    });
    await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
    try {
        const { result } = await invoke(wasm, "run", [`http://127.0.0.1:${server.address().port}/`], { grant: "http" });
        assert.deepEqual(result, { statusCode: 200, contentType: "text/plain", body: "hello from node:http" });
    } finally {
        server.close();
    }
});

test("node:http refused fails the request with Jco's adapter-required code", async () => {
    const wasm = await component("node-http", { args: DIRECT, variant: "direct" });
    await assert.rejects(invoke(wasm, "run", ["http://127.0.0.1:9/"]), (error) => {
        assert.match(`${error.message}\n${error.stderr}`, /ERR_JCO_HTTP_ADAPTER_REQUIRED/);
        return true;
    });
});
