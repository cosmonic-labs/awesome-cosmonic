// Shared pieces of the conformance suite: build a Jco fixture into a component, and run it on
// the Rust host through `jco-node-run`.
//
// The fixtures and their expectations are Jco's own (packages/jco/test/fixtures/componentize and
// packages/jco/test/node). Nothing is copied here: fixtures are componentized from a Jco checkout
// at test time, so the suite always exercises what that checkout's Node support produces.

import { spawn } from "node:child_process";
import { once } from "node:events";
import { existsSync } from "node:fs";
import { cp, mkdir, mkdtemp, rename, rm, symlink } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { createInterface } from "node:readline";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const project = resolve(here, "../..");

/** A bytecodealliance/jco checkout with `packages/jco` built (`pnpm build` in that repo). */
export const JCO_DIR = resolve(process.env.JCO_DIR ?? join(project, "../../../../jco"));
export const JCO = join(JCO_DIR, "packages/jco/dist/jco.js");
export const FIXTURES = join(JCO_DIR, "packages/jco/test/fixtures/componentize");
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

export function run(command, args, options = {}) {
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
 * @param {string[][]} [options.prepare] - commands to run in the copy first, as
 *   `[program, ...args]` with `program` relative to the copy (or `node`)
 * @param {string[]} [options.keep] - paths in the copy the component reads at run time, such as
 *   an app build's static files; kept in {@link componentFiles}
 * @param {string} [options.scratchRoot] - where to make the temporary copy, for fixtures whose
 *   imports resolve from an enclosing package's `node_modules` (default: the OS temp directory)
 */
export function component(fixture, options = {}) {
    const { args = [], entry = "component.js", wit = "wit", world, variant, source, keep = [] } = options;
    const name = variant ? `${fixture}.${variant}` : fixture;
    const output = join(CACHE, `${name}.wasm`);
    const files = componentFiles(name);
    if (existsSync(output) && (keep.length === 0 || existsSync(files))) {
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
                const scratch = await mkdtemp(join(options.scratchRoot ?? tmpdir(), `.jco-node-${name}-`));
                try {
                    const app = join(scratch, "app");
                    const from = source ?? join(FIXTURES, fixture);
                    // An example's dependencies are linked rather than copied: the bundler only
                    // reads them.
                    const modules = join(from, "node_modules");
                    await cp(from, app, { recursive: true, filter: (path) => path !== modules });
                    if (existsSync(modules)) {
                        await symlink(modules, join(app, "node_modules"), "dir");
                    }
                    // Steps the example's own build runs before componentizing, such as an app
                    // build, run in the copy.
                    // With the package's binaries on PATH, as `pnpm run` puts them.
                    const env = { ...process.env, PATH: `${join(app, "node_modules/.bin")}:${process.env.PATH}` };
                    for (const [command, ...rest] of options.prepare ?? []) {
                        const step = await run(command === "node" ? process.execPath : join(app, command), rest, { cwd: app, env });
                        if (step.status !== 0) {
                            throw new Error(`preparing ${name} failed (${command} ${rest.join(" ")}):\n${step.stdout}\n${step.stderr}`);
                        }
                    }
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
                    ], { cwd: app });
                    if (result.status !== 0) {
                        throw new Error(`componentizing ${name} failed:\n${result.stdout}\n${result.stderr}`);
                    }
                    await rename(built, output).catch(async () => {
                        await cp(built, output);
                    });
                    await rm(files, { recursive: true, force: true });
                    for (const path of keep) {
                        await cp(join(app, path), join(files, path), { recursive: true });
                    }
                } finally {
                    await rm(scratch, { recursive: true, force: true });
                }
                return output;
            })(),
        );
    }
    return building.get(output);
}

/** Where a built component's `keep` paths are, by build name (`fixture` or `fixture.variant`). */
export function componentFiles(name) {
    return join(CACHE, `${name}.files`);
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
    if (options.hostLoopback?.length) {
        args.push("--allow-host-loopback", options.hostLoopback.join(","));
    }
    for (const ca of options.tlsCa ?? []) {
        args.push("--tls-ca", ca);
    }
    if (options.tlsHandshakeTimeout) {
        args.push("--tls-handshake-timeout", String(options.tlsHandshakeTimeout));
    }
    if (options.clusterWorkerInvoke) {
        args.push("--cluster-worker-invoke", options.clusterWorkerInvoke);
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
