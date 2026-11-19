// Jco: packages/jco/test/node/express.js, "serves an unmodified Express application over a
// socket" (fixture node-express, driven by its run.js). Jco's runner maps jco:node/http and tty to
// Node and fs, zlib and console to their denials; here that is granting http and tty.
//
// Jco also compares every response with native Express; the expectations below are the ones its
// test asserts directly.
import assert from "node:assert/strict";
import http from "node:http";
import { join } from "node:path";
import { test } from "node:test";

import { JCO_DIR, Session, component } from "./lib.mjs";

const build = () =>
    component("node-express", {
        // Express resolves from the jco package's dependencies, as in Jco's own test.
        scratchRoot: join(JCO_DIR, "packages/jco"),
        args: [
            "--backend",
            "starlingmonkey",
            "--with-nodejs-http-via",
            "direct",
            "--bundle-config",
            "rolldown.config.mjs",
        ],
    });

/** One request through node:http, as Jco's run.js makes it (fetch adds cache headers of its own). */
function fetchResult(url, { method = "GET", headers = {}, body } = {}) {
    return new Promise((resolve, reject) => {
        const request = http.request(url, { method, headers }, (response) => {
            let text = "";
            response.setEncoding("utf8");
            response.on("data", (chunk) => (text += chunk));
            response.on("end", () =>
                resolve({
                    status: response.statusCode,
                    contentType: response.headers["content-type"] ?? null,
                    etag: response.headers.etag ?? null,
                    body: text,
                }),
            );
        });
        request.on("error", reject);
        request.end(body);
    });
}

test("Express serves an unmodified application", async () => {
    const session = new Session(await build(), { grant: "http,tty" });
    try {
        const port = await session.call("start");
        const base = `http://127.0.0.1:${port}`;
        const root = await fetchResult(`${base}/`);
        const [params, posted, missing, malformed, failure, cached] = await Promise.all([
            fetchResult(`${base}/items/42?page=3`),
            fetchResult(`${base}/echo`, {
                method: "POST",
                headers: { "Content-Type": "application/json" },
                body: JSON.stringify({ hello: "world" }),
            }),
            fetchResult(`${base}/nope`),
            fetchResult(`${base}/echo`, { method: "POST", headers: { "Content-Type": "application/json" }, body: "{" }),
            fetchResult(`${base}/error`),
            fetchResult(`${base}/`, { headers: { "If-None-Match": root.etag } }),
        ]);
        assert.equal(root.status, 200);
        assert.equal(root.body, "Hello World!");
        assert.match(root.contentType, /^text\/html/);
        assert.match(root.etag, /^W\//);
        assert.equal(params.status, 200);
        assert.deepEqual(JSON.parse(params.body), { id: "42", page: "3" });
        assert.equal(posted.status, 201);
        assert.deepEqual(JSON.parse(posted.body), { echoed: { hello: "world" } });
        assert.equal(malformed.status, 400);
        assert.equal(failure.status, 500);
        assert.deepEqual(JSON.parse(failure.body), { error: "application failure" });
        assert.equal(cached.status, 304);
        assert.equal(cached.body, "");
        assert.equal(missing.status, 404);
        assert.ok(missing.body.includes("Cannot GET /nope"));
        await session.call("stop");
    } finally {
        await session.close();
    }
});
