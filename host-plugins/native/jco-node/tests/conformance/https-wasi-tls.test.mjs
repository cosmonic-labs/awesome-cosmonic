// Jco: packages/jco/test/node/https-wasi-tls.ts, "node:https over wasi:sockets + wasi:tls
// (starlingmonkey)" (fixture node-https-wasi-tls, built by its build.ts and driven by its run.ts).
// The guest's node:https is built over wasi:sockets, and upgrades its connections through
// jco:node/tls's start-tls (the TLS capability), which this host serves with its wasi:tls
// provider.
//
// Jco's runner configures its Node TLS provider with the fixture's CA (policy "trusted") or with
// none (policy "public"), and a 1.5 s handshake timeout; here those are the runner's --tls-ca and
// --tls-handshake-timeout. Policy "denied" grants no TLS capability.
//
// Jco's runner also counts the provider's handshakes and TCP sockets, and the TLS host's live
// resources before and after the call (`_resourceCounts`, an internal of its JS provider). Those
// assertions are replaced by what they stand for, observed from outside: the peer server's TLS
// handshakes (`secureConnections`) and TCP connections, and the host process ending cleanly.
// The public-network case (example.com) runs only with JCO_NODE_PUBLIC_NETWORK=1.
//
// Under wash-runtime a guest's 127.0.0.1 is its own virtual network; this machine is
// host.wasmcloud.internal, reachable on the ports the host grants (`--allow-host-loopback`). So the
// guest addresses the test's servers by that name, where Jco's addresses 127.0.0.1.
import assert from "node:assert/strict";
import { existsSync } from "node:fs";
import { cp, mkdir, mkdtemp, readFile, rename, rm, writeFile } from "node:fs/promises";
import { createServer as createHttpServer } from "node:http";
import { createServer as createTcpServer } from "node:net";
import { once } from "node:events";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { describe, test } from "node:test";
import { createServer as createTlsServer } from "node:tls";
import { pathToFileURL } from "node:url";

import { CACHE, FIXTURES, JCO, JCO_DIR, invoke, requireToolchain, run } from "./lib.mjs";

const fixture = join(FIXTURES, "node-https-wasi-tls");
const dist = join(JCO_DIR, "packages/jco/dist");
// Jco's build.ts points at its own jco-std build; this uses the jco-std the jco CLI resolves.
const std = join(JCO_DIR, "packages/jco/node_modules/@bytecodealliance/jco-std/dist/wasi/0.2.x/node/24.x.x");
const CA = join(fixture, "certs/ca.crt");

let building;
/** As the fixture's build.ts, for StarlingMonkey, up to the component (no transpile). */
function build() {
    const output = join(CACHE, "node-https-wasi-tls.wasm");
    if (existsSync(output)) {
        return Promise.resolve(output);
    }
    building ??= (async () => {
        requireToolchain();
        const load = (path) => import(pathToFileURL(join(dist, path)));
        const { bundleComponentSource } = await load("bundle.js");
        const { componentize } = await load("cmd/componentize.js");
        const { nodeBuiltinPlugin, nodeGlobals } = await load("node-builtins/index.js");
        const { injectNodeWitImports } = await load("node-wit.js");
        const root = await mkdtemp(join(tmpdir(), ".jco-node-https-wasi-tls-"));
        try {
            await cp(join(fixture, "wit"), join(root, "wit"), { recursive: true });
            const requirements = [];
            const source = await bundleComponentSource(join(fixture, "component.js"), {
                inject: nodeGlobals({ webGlobals: false }),
                plugins: [
                    nodeBuiltinPlugin(
                        { imports: [], exports: [] },
                        {
                            nodejsHttpVia: "wasi-sockets",
                            wasiSocketsVersion: "0.2.10",
                            httpsCoreModule: join(std, "https/core.js"),
                            httpCoreModule: join(std, "http/core.js"),
                            httpWasiSocketsImplementationModule: join(std, "http/impl/wasi-sockets/index.js"),
                            onWitRequirement: (requirement) => requirements.push(requirement),
                        },
                    ),
                ],
            });
            await injectNodeWitImports(join(root, "wit"), "component", requirements);
            await writeFile(join(root, "bundle.js"), source);
            await componentize(join(root, "bundle.js"), {
                wit: join(root, "wit"),
                worldName: "component",
                backend: "starlingmonkey",
                backendQjsDisableAsync: false,
                disable: ["http"],
                out: join(root, "component.wasm"),
            });
            await mkdir(CACHE, { recursive: true });
            await rename(join(root, "component.wasm"), output);
        } finally {
            await rm(root, { recursive: true, force: true });
        }
        return output;
    })();
    return building;
}

/**
 * The URL of a server listening on this machine's loopback, as the guest reaches it: its own
 * 127.0.0.1 is its virtual network, and wash-runtime names this machine host.wasmcloud.internal.
 */
const local = (scheme, port) => `${scheme}://${HOST}:${port}/`;
const HOST = "host.wasmcloud.internal";

/** Call the guest's `run` as the fixture's run.ts does, under a trust policy. */
async function request(url, policy = "trusted", body = "", servername = "") {
    const { result, status } = await invoke(await build(), "run", [url, body === "large" ? "a".repeat(150_000) : "", servername], {
        grant: policy === "denied" ? [] : "tls",
        allowNet: true,
        hostLoopback: new URL(url).hostname === HOST ? [Number(new URL(url).port)] : [],
        tlsCa: policy === "trusted" ? [CA] : [],
        tlsHandshakeTimeout: 1500,
    });
    // The host ran the call to completion and shut down cleanly.
    assert.equal(status, 0);
    return result;
}

describe("node:https over wasi:sockets + wasi:tls (starlingmonkey)", () => {
    test("imports TLS through jco:node/tls and wasi:tls, and HTTP through wasi:sockets", async () => {
        const { status, stdout, stderr } = await run(process.execPath, [JCO, "wit", await build()]);
        assert.equal(status, 0, stderr);
        assert.match(stdout, /import jco:node\/tls@0\.1\.0/);
        assert.match(stdout, /import wasi:tls\/types@0\.2\.0-draft/);
        assert.match(stdout, /import wasi:sockets\/tcp@/);
        assert.match(stdout, /import wasi:io\/streams@0\.2\.12/);
        assert.doesNotMatch(stdout, /wasi:io\/streams@0\.2\.6/);
        assert.doesNotMatch(stdout, /jco:tls-streams/);
        assert.doesNotMatch(stdout, /import jco:node\/http@/);
        assert.doesNotMatch(stdout, /import wasi:http\/outgoing-handler@/);
    });

    test("public network: verified GET https://example.com/", { skip: process.env.JCO_NODE_PUBLIC_NETWORK !== "1" && "needs public DNS and outbound TCP/443 (set JCO_NODE_PUBLIC_NETWORK=1)" }, async () => {
        const report = await request("https://example.com/", "public");
        assert.equal(report.error, "");
        assert.ok(report.status >= 200 && report.status < 600);
    });

    const cert = readFile(join(fixture, "certs/localhost.crt"));
    const key = readFile(join(fixture, "certs/localhost.key"));
    for (const [name, policy, servername, body, success, status] of [
        ["verified custom host CA, SNI, ALPN, and fragmented response", "trusted", "localhost", "", true, 200],
        ["HTTP 503 is a verified HTTPS response", "trusted", "localhost", "", true, 503],
        ["fragmented large request writes", "trusted", "localhost", "large", true, 200],
        ["untrusted certificate", "public", "localhost", "", false, 200],
        ["hostname mismatch", "trusted", "wrong.example", "", false, 200],
        ["missing TLS capability without plaintext fallback", "denied", "localhost", "", false, 200],
    ]) {
        test(name, async () => {
            const peers = new Set();
            let connections = 0;
            let secureConnections = 0;
            let negotiated;
            let received = 0;
            const responseBody = status === 503 ? "temporarily unavailable" : "hello verified TLS";
            // The peer, as in Jco's test: it answers byte-fragmented once the request is in.
            const server = createTlsServer({ cert: await cert, key: await key, ALPNProtocols: ["http/1.1"] }, (socket) => {
                secureConnections++;
                negotiated = { servername: socket.servername, alpn: socket.alpnProtocol };
                let bytes = Buffer.alloc(0);
                let responded = false;
                socket.on("data", (chunk) => {
                    bytes = Buffer.concat([bytes, chunk]);
                    const end = bytes.indexOf("\r\n\r\n");
                    if (end < 0) {
                        return;
                    }
                    received = bytes.length - end - 4;
                    if (responded || (body === "large" && received < 150_000)) {
                        return;
                    }
                    responded = true;
                    const response = Buffer.from(
                        `HTTP/1.1 ${status} ${status === 503 ? "Service Unavailable" : "OK"}\r\nContent-Length: ${Buffer.byteLength(responseBody)}\r\nConnection: close\r\n\r\n${responseBody}`,
                    );
                    let offset = 0;
                    function fragment() {
                        if (socket.destroyed) {
                            return;
                        }
                        if (offset === response.length) {
                            socket.end();
                            return;
                        }
                        const next = Math.min(offset + 3, response.length);
                        socket.write(response.subarray(offset, next));
                        offset = next;
                        setImmediate(fragment);
                    }
                    fragment();
                });
            });
            server.on("connection", (peer) => {
                connections++;
                peers.add(peer);
                peer.on("close", () => peers.delete(peer));
            });
            server.on("tlsClientError", () => {});
            server.listen(0, "127.0.0.1");
            await once(server, "listening");
            try {
                const report = await request(local("https", server.address().port), policy, body, servername);
                if (success) {
                    assert.deepEqual(report, { status, body: responseBody, error: "" });
                    assert.deepEqual(negotiated, { servername: "localhost", alpn: "http/1.1" });
                    // One TLS handshake (Jco: `handshakes` is 1).
                    assert.equal(secureConnections, 1);
                    if (body) {
                        assert.equal(received, 150_000);
                    }
                } else {
                    assert.equal(report.status, 0);
                    assert.match(report.error, policy === "denied" ? /jco:node\/tls.*TLS capability/ : /TLS handshake failed/);
                    if (policy === "public") {
                        assert.match(report.error, /certificate/i);
                    }
                    if (servername === "wrong.example") {
                        assert.match(report.error, /Hostname\/IP does not match|not in the cert/);
                    }
                    assert.equal(secureConnections, 0);
                    // Denied, the guest never even connects (Jco: `connections` is 0).
                    if (policy === "denied") {
                        assert.equal(connections, 0);
                    }
                }
            } finally {
                for (const peer of peers) {
                    peer.destroy();
                }
                await new Promise((resolve) => server.close(() => resolve()));
            }
        });
    }

    test("plain HTTP keeps using TCP without a TLS handshake", async () => {
        const server = createHttpServer((_request, response) => response.end("plain HTTP"));
        server.listen(0, "127.0.0.1");
        await once(server, "listening");
        try {
            // Denied: plain HTTP needs no TLS capability.
            const report = await request(local("http", server.address().port), "denied");
            assert.deepEqual(report, { status: 200, body: "plain HTTP", error: "" });
        } finally {
            await new Promise((resolve) => server.close(() => resolve()));
        }
    });

    test("connection refusal releases socket resources", async () => {
        const server = createTcpServer();
        server.listen(0, "127.0.0.1");
        await once(server, "listening");
        const port = server.address().port;
        await new Promise((resolve) => server.close(() => resolve()));
        const report = await request(local("https", port));
        assert.equal(report.status, 0);
        assert.match(report.error, /ECONNREFUSED/);
    });

    for (const kind of ["reset", "stalled handshake"]) {
        test(`cleans up after ${kind}`, async () => {
            const peers = new Set();
            const server = createTcpServer((socket) => {
                peers.add(socket);
                socket.on("close", () => peers.delete(socket));
                if (kind === "reset") {
                    socket.destroy();
                }
            });
            server.listen(0, "127.0.0.1");
            await once(server, "listening");
            try {
                const report = await request(local("https", server.address().port), "trusted", "", "localhost");
                assert.equal(report.status, 0);
                assert.match(report.error, /TLS handshake failed/);
            } finally {
                for (const peer of peers) {
                    peer.destroy();
                }
                await new Promise((resolve) => server.close(() => resolve()));
            }
        });
    }
});
