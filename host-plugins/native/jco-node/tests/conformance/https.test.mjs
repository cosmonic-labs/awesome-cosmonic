// Jco: packages/jco/test/node/https.js, "terminates TLS for a guest server through the host
// node:https" (fixture node-https-server) and "performs a verified HTTPS request from a guest
// through the host node:https" (fixture node-https). Jco's runners grant jco:node/http with the
// jco:node/tls provider beside it; here that is granting both.
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import https from "node:https";
import { join } from "node:path";
import { test } from "node:test";

import { JCO_DIR, Session, component, invoke } from "./lib.mjs";

const DIRECT = ["--backend", "starlingmonkey", "--with-nodejs-http-via", "direct"];
const TLS = join(JCO_DIR, "packages/jco-std/test/wasi/0.2.x/node/24.x.x/https/helpers/tls");
const material = async () => {
    const [key, cert] = await Promise.all([readFile(join(TLS, "localhost.key"), "utf8"), readFile(join(TLS, "localhost.crt"), "utf8")]);
    return { key, cert };
};

test("node:https: the host terminates TLS for a guest server", async () => {
    const { key, cert } = await material();
    const session = new Session(await component("node-https-server", { args: DIRECT }), { grant: "http,tls" });
    try {
        const port = await session.call("start", key, cert);
        const body = await new Promise((resolve, reject) => {
            const request = https.request(
                { host: "127.0.0.1", port, path: "/items", method: "POST", ca: cert, servername: "localhost" },
                (response) => {
                    let text = "";
                    response.setEncoding("utf8");
                    response.on("data", (chunk) => (text += chunk));
                    response.on("end", () => resolve(text));
                },
            );
            request.on("error", reject);
            request.end("hello");
        });
        assert.equal(body, "POST /items: hello");
        await session.call("stop");
    } finally {
        await session.close();
    }
});

async function withPeer(cert, key, work) {
    const server = https.createServer({ key, cert }, (_request, response) => {
        response.writeHead(200, { "Content-Type": "text/plain" });
        response.end("hello from node:https");
    });
    await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
    try {
        return await work(server.address().port);
    } finally {
        await new Promise((resolve) => server.close(resolve));
    }
}

test("node:https: a verified request from the guest through the host", async () => {
    const { key, cert } = await material();
    const wasm = await component("node-https", { args: DIRECT });
    await withPeer(cert, key, async (port) => {
        const { result } = await invoke(wasm, "run", [`https://127.0.0.1:${port}/`, cert], { grant: "http,tls" });
        assert.deepEqual(result, { statusCode: 200, contentType: "text/plain", body: "hello from node:https" });
    });
});

test("node:https: without the guest's CA the certificate is refused", async () => {
    const { key, cert } = await material();
    const wasm = await component("node-https", { args: DIRECT });
    await withPeer(cert, key, async (port) => {
        await assert.rejects(invoke(wasm, "run", [`https://127.0.0.1:${port}/`, ""], { grant: "http,tls" }), (error) => {
            assert.match(error.stderr, /self-signed certificate|unable to verify the first certificate/);
            return true;
        });
    });
});

test("node:https with jco:node/tls refused", async () => {
    const { key, cert } = await material();
    const wasm = await component("node-https", { args: DIRECT });
    await withPeer(cert, key, async (port) => {
        await assert.rejects(invoke(wasm, "run", [`https://127.0.0.1:${port}/`, cert], { grant: "http" }), (error) => {
            assert.match(error.stderr, /node:tls requires an application-provided host adapter/);
            return true;
        });
    });
});
