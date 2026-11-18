// Shared pieces of the conformance suite: build a Jco fixture into a component, and run it on
// the Rust host through `jco-node-run`.
//
// The fixtures and their expectations are Jco's own (packages/jco/test/fixtures/componentize and
// packages/jco/test/node). Nothing is copied here: fixtures are componentized from a Jco checkout
// at test time, so the suite always exercises what that checkout's Node support produces.

import { spawn } from "node:child_process";
import { once } from "node:events";
import { existsSync } from "node:fs";
import { cp, mkdir, mkdtemp, rename, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { createInterface } from "node:readline";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const project = resolve(here, "../..");

/** A bytecodealliance/jco checkout with `packages/jco` built (`pnpm build` in that repo). */
export const JCO_DIR = resolve(process.env.JCO_DIR ?? join(project, "../../../../jco"));
const JCO = join(JCO_DIR, "packages/jco/dist/jco.js");
const FIXTURES = join(JCO_DIR, "packages/jco/test/fixtures/componentize");
/** Where a Jco example lives, for the suites that run those. */
export const EXAMPLES = join(JCO_DIR, "examples/components");

/** The runner under test. */
export const RUNNER = resolve(process.env.JCO_NODE_RUN ?? join(project, "target/debug/jco-node-run"));

/** Built components are kept between runs; componentizing one takes about half a minute. */
export const CACHE = resolve(process.env.JCO_NODE_COMPONENT_CACHE ?? join(project, "target/conformance"));

export function requireToolchain() {
    for (const [path, hint] of [
        [JCO, "set JCO_DIR to a jco checkout with packages/jco built"],
        [RUNNER, "run `cargo build` first, or set JCO_NODE_RUN"],
    ]) {
        if (!existsSync(path)) {
            throw new Error(`${path} not found: ${hint}`);
        }
    }
}

function run(command, args, options = {}) {
    return new Promise((resolvePromise, reject) => {
        const child = spawn(command, args, { stdio: ["ignore", "pipe", "pipe"], ...options });
        let stdout = "";
        let stderr = "";
        child.stdout.on("data", (chunk) => (stdout += chunk));
        child.stderr.on("data", (chunk) => (stderr += chunk));
        child.on("error", reject);
        child.on("close", (status, signal) => resolvePromise({ status, signal, stdout, stderr }));
    });
}

const building = new Map();

/**
 * Componentize a Jco fixture, as `componentizeFixture` in Jco's test helpers does, and return
 * the component's path.
 *
 * @param {string} fixture - directory name under Jco's `test/fixtures/componentize`
 * @param {object} [options]
 * @param {string[]} [options.args] - extra `jco componentize` arguments
 * @param {string} [options.entry] - entry module within the fixture
 * @param {string} [options.wit] - WIT file or directory within the fixture
 * @param {string} [options.world] - WIT world to build
 * @param {string} [options.variant] - distinguishes builds of one fixture with different arguments
 * @param {string} [options.source] - build from this directory instead of a Jco fixture
 */
export function component(fixture, options = {}) {
    const { args = [], entry = "component.js", wit = "wit", world, variant, source } = options;
    const name = variant ? `${fixture}.${variant}` : fixture;
    const output = join(CACHE, `${name}.wasm`);
    if (existsSync(output)) {
        return Promise.resolve(output);
    }
    if (!building.has(output)) {
        building.set(
            output,
            (async () => {
                requireToolchain();
                await mkdir(CACHE, { recursive: true });
                // Componentizing rewrites the fixture's WIT to add the jco:node imports, so it
                // works on a copy.
                const scratch = await mkdtemp(join(tmpdir(), `jco-node-${name}-`));
                try {
                    const app = join(scratch, "app");
                    await cp(source ?? join(FIXTURES, fixture), app, { recursive: true });
                    const built = join(scratch, "component.wasm");
                    const result = await run(process.execPath, [
                        JCO,
                        "componentize",
                        join(app, entry),
                        "--bundle",
                        "-w",
                        join(app, wit),
                        ...(world ? ["-n", world] : []),
                        "-o",
                        built,
                        ...args,
                    ]);
                    if (result.status !== 0) {
                        throw new Error(`componentizing ${name} failed:\n${result.stdout}\n${result.stderr}`);
                    }
                    await rename(built, output).catch(async () => {
                        await cp(built, output);
                    });
                } finally {
                    await rm(scratch, { recursive: true, force: true });
                }
                return output;
            })(),
        );
    }
    return building.get(output);
}

function runnerArgs(wasm, options) {
    const args = [wasm];
    if (options.grant?.length) {
        args.push("--grant", [options.grant].flat().join(","));
    }
    for (const [key, value] of Object.entries(options.env ?? {})) {
        args.push("--env", `${key}=${value}`);
    }
    if (options.inheritEnv) {
        args.push("--inherit-env");
    }
    if (options.argv) {
        args.push("--argv", JSON.stringify(options.argv));
    }
    if (options.cwd) {
        args.push("--cwd", options.cwd);
    }
    if (options.nodeVersion) {
        args.push("--node-version", options.nodeVersion);
    }
    for (const dir of options.dirs ?? []) {
        args.push("--dir", dir);
    }
    if (options.allowNet) {
        args.push("--allow-net");
    }
    return args;
}

/**
 * A component instance on the Rust host. Exports are called one at a time; between calls the
 * host delivers the guest's callbacks, so a server the guest started keeps serving.
 */
export class Session {
    #child;
    #replies;
    stdout = "";
    stderr = "";

    constructor(wasm, options = {}) {
        this.#child = spawn(RUNNER, [...runnerArgs(wasm, options), "--reply-fd", "3"], {
            stdio: ["pipe", "pipe", "pipe", "pipe"],
            cwd: options.spawnCwd,
        });
        this.pid = this.#child.pid;
        this.#child.stdout.on("data", (chunk) => (this.stdout += chunk));
        this.#child.stderr.on("data", (chunk) => (this.stderr += chunk));
        this.#replies = createInterface({ input: this.#child.stdio[3] })[Symbol.asyncIterator]();
        this.closed = once(this.#child, "close").then(([status, signal]) => ({ status, signal }));
    }

    /**
     * Call an export. Resolves to its result; rejects if the guest trapped. If the guest exited
     * instead of returning, resolves to `{ exit: code }`.
     */
    async call(name, ...args) {
        this.#child.stdin.write(`${JSON.stringify({ call: name, args })}\n`);
        const { value, done } = await this.#replies.next();
        if (done) {
            const { status } = await this.closed;
            throw new Error(`the host exited with status ${status} during ${name}:\n${this.stderr}`);
        }
        const reply = JSON.parse(value);
        if ("trap" in reply) {
            throw Object.assign(new Error(reply.trap), { trap: true });
        }
        return "exit" in reply ? { exit: reply.exit } : reply.ok;
    }

    /** End the session and wait for the host to exit. */
    async close() {
        this.#child.stdin.end();
        const timer = setTimeout(() => this.#child.kill(), 10_000);
        const result = await this.closed;
        clearTimeout(timer);
        return { ...result, stdout: this.stdout, stderr: this.stderr };
    }

    kill() {
        this.#child.kill();
    }
}

/** Call one export of a fresh instance and return its result with everything the guest wrote. */
export async function invoke(wasm, name, args = [], options = {}) {
    const session = new Session(wasm, options);
    let result;
    let error;
    try {
        result = await session.call(name, ...args);
    } catch (caught) {
        error = caught;
    }
    const { status, stdout, stderr } = await session.close();
    if (error) {
        throw Object.assign(error, { status, stdout, stderr });
    }
    return { result, status, stdout, stderr };
}
