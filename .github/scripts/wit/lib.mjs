// Shared helpers for the wit workflow scripts.
import { execFile } from "node:child_process";
import { appendFileSync } from "node:fs";
import { promisify } from "node:util";

const exec = promisify(execFile);

// Runs a command and returns stdout. Failures carry the command's own output.
export async function run(cmd, args, opts = {}) {
  try {
    const { stdout } = await exec(cmd, args, { maxBuffer: 64 << 20, ...opts });
    return stdout;
  } catch (err) {
    throw new Error(`${cmd} ${args.join(" ")} failed:\n${err.stdout ?? ""}${err.stderr ?? ""}`);
  }
}

// Sets a step output when running in Actions, and echoes it either way.
export function setOutput(name, value) {
  console.log(`${name}=${value}`);
  if (process.env.GITHUB_OUTPUT) appendFileSync(process.env.GITHUB_OUTPUT, `${name}=${value}\n`);
}

export const notice = (msg) => console.log(`::notice::${msg}`);
export const warning = (msg) => console.log(`::warning::${msg}`);

// Entry-point wrapper: reports a failure as an Actions error annotation.
export function runMain(main) {
  main(process.argv.slice(2)).catch((err) => {
    console.error(`::error::${String(err.message ?? err).split("\n")[0]}`);
    console.error(err.message ?? err);
    process.exit(1);
  });
}

// The package line the registry-side checks compare against.
export const declaration = (pkg, version) => `package cosmonic:${pkg}@${version};`;

// Reads the `package` line out of a built package.
export async function declaredPackage(wasm) {
  const wit = await run("wasm-tools", ["component", "wit", wasm]);
  const line = wit.split("\n").find((l) => l.startsWith("package "));
  if (!line) throw new Error(`${wasm} has no package declaration`);
  return line;
}
