// Runs each Jco example's own end-to-end test (`test/e2e.*` in the example) against the
// component served by the Rust host. The test is used as written; only its list of runtimes is
// replaced, so it starts tests/examples/serve.mjs instead of the example's `run-transpiled.js`
// (and skips its plain-Node baseline).
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { existsSync } from "node:fs";
import { cp, mkdir, readFile, rm, symlink, writeFile } from "node:fs/promises";
import { dirname, join, resolve } from "node:path";
import { test } from "node:test";
import { fileURLToPath, pathToFileURL } from "node:url";

import { component } from "../conformance/lib.mjs";
import { EXAMPLE_CONFIG } from "./config.mjs";

const here = dirname(fileURLToPath(import.meta.url));
const WORK = resolve(here, "../../target/examples");
const SERVE = pathToFileURL(join(here, "serve.mjs")).href;

for (const [name, config] of Object.entries(EXAMPLE_CONFIG)) {
    test(`example ${name}: its e2e test against the Rust host`, { timeout: 15 * 60_000 }, async () => {
        // Built up front: the e2e test gives a server only seconds to start listening.
        await component(name, config.build);
        const example = config.build.source;
        const e2e = ["test/e2e.js", "test/e2e.ts", "test/e2e.mjs"].find((file) => existsSync(join(example, file)));
        assert.ok(e2e, `${name} has no test/e2e test`);
        const work = join(WORK, name);
        await rm(work, { recursive: true, force: true });
        await mkdir(join(work, "test"), { recursive: true });
        // Bare imports in the test resolve through the example's own dependencies.
        await symlink(join(example, "node_modules"), join(work, "node_modules"), "dir");
        await cp(join(example, "package.json"), join(work, "package.json"));
        const target = join(work, e2e);
        await writeFile(target, config.retarget(await readFile(join(example, e2e), "utf8"), SERVE));

        const child = spawn(process.execPath, ["--test", "--test-reporter=spec", target], {
            cwd: work,
            env: { ...process.env, JCO_NODE_EXAMPLE: name },
            stdio: ["ignore", "pipe", "pipe"],
        });
        let output = "";
        child.stdout.on("data", (chunk) => (output += chunk));
        child.stderr.on("data", (chunk) => (output += chunk));
        const status = await new Promise((resolvePromise) => child.on("close", resolvePromise));
        assert.equal(status, 0, output);
    });
}
