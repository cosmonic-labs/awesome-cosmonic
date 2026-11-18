// Serves a Jco example component on the Rust host, standing in for the example's own
// `run-transpiled.js`: the same start call and the same "listening" line, so the example's
// end-to-end test can drive it unchanged. The example is named by JCO_NODE_EXAMPLE.
import { component, Session } from "../conformance/lib.mjs";
import { EXAMPLE_CONFIG } from "./config.mjs";

const name = process.env.JCO_NODE_EXAMPLE;
const config = EXAMPLE_CONFIG[name];
if (!config) {
    throw new Error(`unknown example ${name}`);
}

const wasm = await component(name, config.build);
const session = new Session(wasm, config.session);
session.closed.then(({ status, signal }) => {
    process.stderr.write(session.stderr);
    process.exit(status ?? (signal ? 1 : 0));
});

const port = await session.call("start", ...config.startArgs(Number(process.env.PORT ?? 0)));
process.stdout.write(`${config.listening(port)}\n`);

async function stop() {
    await session.call("stop").catch(() => {});
    await session.close();
}

process.once("SIGTERM", stop);
process.once("SIGINT", stop);
