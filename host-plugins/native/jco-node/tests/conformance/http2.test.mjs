// Jco: packages/jco/test/node/http2.js, "runs client and server callbacks through the direct
// component boundary" (fixture node-http2, world wit-starling, driven by its run-direct.js), which
// runs everything twice: over h2c, then over TLS with a secure context from jco:node/tls.
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import http2 from "node:http2";
import { connect } from "node:net";
import { join } from "node:path";
import { connect as connectTls } from "node:tls";
import { after, before, test } from "node:test";
import { setTimeout as delay } from "node:timers/promises";

import { JCO_DIR, Session, component } from "./lib.mjs";

const build = () =>
    component("node-http2", {
        variant: "direct",
        wit: "wit-starling",
        args: ["--backend", "starlingmonkey", "--with-nodejs-http2-via", "direct"],
    });

/** One request from a native Node client, as run-direct.js makes it. */
async function request(port, path, secure) {
    const session = http2.connect(`${secure ? "https" : "http"}://127.0.0.1:${port}`, { rejectUnauthorized: false });
    try {
        return await new Promise((resolve, reject) => {
            session.once("error", reject);
            const stream = session.request({ ":path": path });
            const chunks = [];
            let status;
            let trailers;
            stream.once("trailers", (value) => (trailers = value));
            stream.setEncoding("utf8");
            stream.once("response", (headers) => (status = headers[":status"]));
            stream.on("data", (chunk) => chunks.push(chunk));
            stream.once("error", reject);
            stream.once("end", () => {
                if (status === 200) {
                    assert.equal(trailers?.["x-component-trailer"], "complete");
                }
                resolve({ status, body: chunks.join("") });
            });
            stream.end();
        });
    } finally {
        session.destroy();
    }
}

let peer;
before(async () => {
    peer = http2.createServer();
    peer.on("stream", (stream) => {
        stream.respond({ ":status": 201, "content-type": "text/plain" });
        stream.end("native peer");
    });
    await new Promise((resolve) => peer.listen(0, "127.0.0.1", resolve));
});
after(() => new Promise((resolve) => peer.close(resolve)));

for (const secure of [false, true]) {
    test(`node:http2 client and server callbacks over ${secure ? "TLS" : "h2c"}, two isolated instances`, async () => {
        const tls = join(JCO_DIR, "packages/jco-std/test/wasi/0.2.x/node/24.x.x/https/helpers/tls");
        const [key, cert] = await Promise.all([readFile(join(tls, "localhost.key")), readFile(join(tls, "localhost.crt"))]);
        const wasm = await build();
        const options = { grant: secure ? "http2,tls" : "http2" };
        const first = new Session(wasm, options);
        const second = new Session(wasm, options);
        const start = (session) => (secure ? session.call("start-secure-server", [...key], [...cert]) : session.call("start-server"));
        try {
            assert.deepEqual(
                JSON.parse(await first.call("run-client", `http://127.0.0.1:${peer.address().port}`, "/client", "")),
                { status: 201, contentType: "text/plain", body: "native peer" },
            );
            const a = await start(first);
            const b = await start(second);
            assert.deepEqual(
                await Promise.all([request(a, "/one", secure), request(a, "/two", secure), request(b, "/other", secure)]),
                [
                    { status: 200, body: "server:/one" },
                    { status: 200, body: "server:/two" },
                    { status: 200, body: "server:/other" },
                ],
            );
            assert.equal(await first.call("count"), 2);
            assert.equal(await second.call("count"), 1);
            assert.deepEqual(await request(a, "/error", secure), { status: 500, body: "guest stream failed" });
            assert.deepEqual(await request(a, "/after-error", secure), { status: 200, body: "server:/after-error" });
            const large = await request(a, "/large", secure);
            assert.equal(large.status, 200);
            assert.equal(large.body, "s".repeat(131_072));

            // A connection that is not HTTP/2 is a session error for the guest's error listener.
            const socket = secure
                ? connectTls({ port: a, host: "127.0.0.1", rejectUnauthorized: false, ALPNProtocols: ["h2"] })
                : connect(a, "127.0.0.1");
            // The server resets the connection; that is the point.
            socket.on("error", () => {});
            try {
                socket.resume();
                socket.end("invalid HTTP/2 preface\r\n\r\n");
                let lastError = "";
                for (let attempt = 0; attempt < 200 && !lastError; attempt++) {
                    await delay(10);
                    lastError = await first.call("last-error");
                }
                assert.equal(lastError, "ERR_HTTP2_ERROR");
            } finally {
                socket.destroy();
            }

            const restarted = await first.call("restart-server");
            assert.deepEqual(await request(restarted, "/restarted", secure), { status: 200, body: "server:/restarted" });
            assert.equal(await first.call("count"), 6);
            assert.equal(await second.call("count"), 1);
        } finally {
            await first.call("stop-server").catch(() => {});
            await second.call("stop-server").catch(() => {});
            await first.close();
            await second.close();
        }
    });
}

test("node:http2 with the interface refused", async () => {
    const session = new Session(await build(), {});
    try {
        await assert.rejects(session.call("start-server"));
    } finally {
        const { stderr } = await session.close();
        assert.match(stderr, /Error/);
    }
});
