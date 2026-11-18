// How each Jco example is built and served here: its own build steps and `componentize`
// arguments, the capabilities its `run-transpiled` script maps (a denial provider there is an
// ungranted interface here), and where its e2e test lists the runtimes it starts.
import assert from "node:assert/strict";
import { join } from "node:path";

import { EXAMPLES, componentFiles } from "../conformance/lib.mjs";

const RUNTIME = "jco-node host plugin";

/** Replace an e2e test's `for (const [runtime, script] of [...])` list with `serve`. */
function runtimeList(source, serve) {
    const list = /for \(const \[runtime, script\] of \[[\s\S]*?\]\)\s*\{/;
    assert.match(source, list, "the e2e test no longer lists its runtimes as this harness expects");
    return source.replace(list, `for (const [runtime, script] of [[${JSON.stringify(RUNTIME)}, ${JSON.stringify(serve)}]]) {`);
}

/** Replace an e2e test's `const targets ... = [...];` with one target, `serve`. */
function targetList(source, serve) {
    const list = /const targets(?::[^=]+)? = \[[\s\S]*?\n\];/;
    assert.match(source, list, "the e2e test no longer lists its targets as this harness expects");
    return source.replace(list, `const targets = [{ runtime: ${JSON.stringify(RUNTIME)}, script: ${JSON.stringify(serve)} }];`);
}

export const EXAMPLE_CONFIG = {
    "node-mcp-server": {
        build: {
            source: join(EXAMPLES, "node-mcp-server"),
            entry: "src/server.js",
            wit: "wit",
            args: ["--with-nodejs-http-via", "direct"],
        },
        // run-transpiled.js maps node:http to the Node provider and node:process to its denial.
        session: { grant: ["http"] },
        startArgs: (port) => [port],
        listening: (port) => `MCP server listening at http://127.0.0.1:${port}/mcp`,
        retarget: runtimeList,
    },
    "node-grpc-server": {
        build: {
            source: join(EXAMPLES, "node-grpc-server"),
            entry: "src/server.js",
            wit: "wit",
            args: ["--with-nodejs-http2-via", "direct"],
        },
        // run-transpiled.js maps node:http2 to the direct Node provider and jco:node/tls (with
        // wasi:tls) to its Node providers.
        session: { grant: ["http2", "tls"] },
        startArgs: (port) => [port],
        listening: (port) => `gRPC server listening at http://127.0.0.1:${port}`,
        retarget: runtimeList,
    },
    "node-sveltekit-server": {
        build: {
            source: join(EXAMPLES, "node-sveltekit-server"),
            // `pnpm build`: the app build, then scripts/build.mjs's componentize command.
            prepare: [["node_modules/.bin/vite", "build"]],
            // The adapter serves `build/` from the working directory at run time.
            keep: ["build"],
            entry: "src/component.ts",
            wit: "wit",
            world: "component",
            args: [
                "--bundle-config",
                "scripts/bundle-config.mjs",
                "--backend",
                "starlingmonkey",
                "--with-nodejs-http-via",
                "direct",
                "--disable",
                "http",
                "fetch-event",
            ],
        },
        // src/run-transpiled.ts maps fs, http and process to the Node providers and tty to a
        // non-terminal stand-in (this host's tty answers the same for piped streams), and fills in
        // the adapter's environment defaults.
        session: {
            grant: ["fs", "http", "process", "tty"],
            // run-transpiled.ts runs from the example directory, where `build/` is.
            cwd: componentFiles("node-sveltekit-server"),
            inheritEnv: true,
            env: {
                ADDRESS_HEADER: "",
                BODY_SIZE_LIMIT: "512K",
                HOST_HEADER: "",
                PORT_HEADER: "",
                PROTOCOL_HEADER: "",
                XFF_DEPTH: "1",
            },
        },
        startArgs: (port) => [port],
        listening: (port) => `SvelteKit TODO server (StarlingMonkey component) listening at http://127.0.0.1:${port}`,
        retarget: targetList,
    },
};
