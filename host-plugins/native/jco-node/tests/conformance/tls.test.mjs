// Jco: packages/jco/test/node/tls.js, "TLS documentation echo examples execute in StarlingMonkey"
// (fixture node-tls, driven by its run.js). The guest runs a node:tls echo server and connects
// to it with a client certificate, inspects the connection, and serves and fetches over
// node:https. Jco's runner gives it the Node TLS provider and the HTTP host with TLS; here that
// is granting tls and http.
//
// Jco builds this fixture by hand rather than with `jco componentize --bundle`: the source is
// bundled with the TLS and HTTP callback exports, and the jco:node imports are injected into a
// world of its own. This does the same, with Jco's built modules.
import assert from "node:assert/strict";
import { existsSync } from "node:fs";
import { mkdir, mkdtemp, readFile, rename, rm, writeFile } from "node:fs/promises";
import https from "node:https";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import { pathToFileURL } from "node:url";
import { setTimeout as delay } from "node:timers/promises";

import { CACHE, FIXTURES, JCO, JCO_DIR, Session, requireToolchain, run } from "./lib.mjs";

const dist = join(JCO_DIR, "packages/jco/dist");
const certificate = join(JCO_DIR, "packages/jco-std/test/wasi/0.2.x/node/24.x.x/https/helpers/tls/localhost");
const WORLD = `package test:tls;
world component { import wasi:io/streams@0.2.12; import wasi:io/poll@0.2.12; import wasi:io/error@0.2.12; import wasi:tls/types@0.2.0-draft; export start: func(key: string, cert: string); export status: func() -> string; export denied: func() -> string; export start-https: func(key: string, cert: string) -> u32; export stop-https: func(); export fetch-https: func(port: u32, cert: string) -> string; }
`;

let building;
function build() {
    const output = join(CACHE, "node-tls.wasm");
    if (existsSync(output)) {
        return Promise.resolve(output);
    }
    building ??= (async () => {
        requireToolchain();
        const load = (path) => import(pathToFileURL(join(dist, path)));
        const { nodeBuiltinPlugin } = await load("node-builtins/index.js");
        const { bundleNodeGuestExportsWrapper } = await load("cmd/componentize.js");
        const { TLS_WIT_REQUIREMENT, HTTP_WIT_REQUIREMENT, injectNodeWitImports } = await load("node-wit.js");
        const dir = await mkdtemp(join(tmpdir(), ".jco-node-node-tls-"));
        try {
            // As Jco's test, with its default (installed jco-std) implementations of the modules.
            const source = await bundleNodeGuestExportsWrapper(
                join(FIXTURES, "node-tls/source.js"),
                [...TLS_WIT_REQUIREMENT.guestExports, ...HTTP_WIT_REQUIREMENT.guestExports],
                { external: [/^jco:node\//], plugins: [nodeBuiltinPlugin({ imports: [], exports: [] }, {})] },
            );
            await writeFile(join(dir, "source.js"), source);
            await writeFile(join(dir, "world.wit"), WORLD);
            await injectNodeWitImports(dir, undefined, [TLS_WIT_REQUIREMENT, HTTP_WIT_REQUIREMENT]);
            const built = join(dir, "component.wasm");
            const result = await run(process.execPath, [
                JCO,
                "componentize",
                join(dir, "source.js"),
                "-w",
                dir,
                "-o",
                built,
                "--backend",
                "starlingmonkey",
            ]);
            if (result.status !== 0) {
                throw new Error(`componentizing node-tls failed:\n${result.stdout}\n${result.stderr}`);
            }
            await mkdir(CACHE, { recursive: true });
            await rename(built, output);
        } finally {
            await rm(dir, { recursive: true, force: true });
        }
        return output;
    })();
    return building;
}

const material = () => Promise.all([readFile(`${certificate}.key`, "utf8"), readFile(`${certificate}.crt`, "utf8")]);

test("node:tls echo, inspection and node:https, as in Jco's documentation example", async () => {
    const [key, cert] = await material();
    const session = new Session(await build(), { grant: "tls,http" });
    try {
        await session.call("start", key, cert);
        let report;
        for (const deadline = Date.now() + 15_000; !(report = await session.call("status")); ) {
            assert.ok(Date.now() < deadline, `the TLS fixture timed out:\n${session.stderr}`);
            await delay(10);
        }
        report = JSON.parse(report);
        assert.equal(report.error, undefined, report.stack);
        assert.equal(report.echo, "welcome!\nhello from a component\n");
        assert.deepEqual(
            {
                authorized: report.inspection.authorized,
                alpn: report.inspection.alpn,
                encrypted: report.inspection.encrypted,
                cipher: report.inspection.cipher,
                certificate: report.inspection.certificate,
                issuerCycle: report.inspection.issuerCycle,
                verified: report.inspection.verified,
                mismatch: report.inspection.mismatch,
                keyingMaterial: report.inspection.keyingMaterial,
                maxFragment: report.inspection.maxFragment,
                localPort: report.inspection.localPort,
                remotePort: report.inspection.remotePort,
            },
            {
                authorized: true,
                alpn: "echo",
                encrypted: true,
                cipher: true,
                certificate: true,
                issuerCycle: true,
                verified: true,
                mismatch: "ERR_TLS_CERT_ALTNAME_INVALID",
                keyingMaterial: 32,
                maxFragment: true,
                localPort: true,
                remotePort: true,
            },
        );
        assert.equal(report.identities, true);
        assert.equal(report.ticketKeys, 48);

        // The component serves HTTPS to a native client ...
        const port = await session.call("start-https", key, cert);
        const served = await new Promise((resolve, reject) => {
            https
                .get({ hostname: "127.0.0.1", port, servername: "localhost", ca: [cert], agent: false }, (response) => {
                    let body = "";
                    response.setEncoding("utf8");
                    response.on("data", (chunk) => (body += chunk));
                    response.on("end", () => resolve(body));
                    response.on("error", reject);
                })
                .on("error", reject);
        });
        await session.call("stop-https");
        // ... and fetches from a native server.
        const peer = https.createServer({ key, cert }, (_request, response) => response.end("native HTTPS"));
        await new Promise((resolve) => peer.listen(0, "127.0.0.1", resolve));
        try {
            const fetched = await session.call("fetch-https", peer.address().port, cert);
            assert.deepEqual({ client: fetched, server: served }, { client: "native HTTPS", server: "component HTTPS" });
        } finally {
            await new Promise((resolve) => peer.close(resolve));
        }
    } finally {
        await session.close();
    }
});

// Jco's runner refuses `wasi:tls/types` and passes the real provider; refusing jco:node/tls is
// the denial path the fixture's `denied` export reports.
test("node:tls is refused without the tls grant", async () => {
    const session = new Session(await build(), { grant: "http" });
    try {
        assert.equal(await session.call("denied"), "ERR_JCO_TLS_ADAPTER_REQUIRED");
    } finally {
        await session.close();
    }
});
