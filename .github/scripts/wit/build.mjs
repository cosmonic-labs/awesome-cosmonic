// Validates a WIT package directory and builds it into dist/<package>.wasm.
//
//   node .github/scripts/wit/build.mjs wit/cosmonic-agent agent
import { copyFileSync, mkdirSync, readdirSync, readFileSync, realpathSync } from "node:fs";
import { tmpdir } from "node:os";
import { pathToFileURL } from "node:url";
import { join, resolve } from "node:path";
import { declaration, declaredPackage, run, runMain, setOutput } from "./lib.mjs";

const SEMVER = /^\d+\.\d+\.\d+(-[0-9A-Za-z.-]+)?$/;

const DECL = /^package\s+cosmonic:([a-z0-9-]+)@(\S+?)\s*;/;

// Every file declares the package and they must all agree. Returns the version.
export function readVersion(dir, pkg) {
  const files = readdirSync(dir).filter((f) => f.endsWith(".wit"));
  if (files.length === 0) throw new Error(`${dir} has no .wit files`);
  const found = new Set();
  for (const f of files) {
    const m = readFileSync(join(dir, f), "utf8").split(/\r?\n/).map((l) => DECL.exec(l)).find(Boolean);
    if (!m) throw new Error(`${f} has no cosmonic package declaration`);
    found.add(`${m[1]}@${m[2]}`);
  }
  if (found.size !== 1) throw new Error(`files disagree on the package declaration: ${[...found].join(" | ")}`);
  const [name, version] = [...found][0].split("@");
  if (!SEMVER.test(version)) throw new Error(`version '${version}' is not <major>.<minor>.<patch>[-prerelease]`);
  if (name !== pkg) throw new Error(`${dir} declares cosmonic:${name}, expected cosmonic:${pkg}`);
  return version;
}

async function main([dir, pkg]) {
  if (!dir || !pkg) throw new Error("usage: build.mjs <wit-dir> <package>");
  const version = readVersion(dir, pkg);
  setOutput("version", version);

  await run("wasm-tools", ["component", "wit", dir]);

  // wash builds the package in ./wit, so stage the one being built.
  const stage = join(process.env.RUNNER_TEMP ?? tmpdir(), `wit-${pkg}`);
  mkdirSync(join(stage, "wit"), { recursive: true });
  for (const f of readdirSync(dir).filter((f) => f.endsWith(".wit"))) {
    copyFileSync(join(dir, f), join(stage, "wit", f));
  }
  mkdirSync("dist", { recursive: true });
  const out = resolve("dist", `${pkg}.wasm`);
  await run("wash", ["wit", "build", "--output-file", out], { cwd: stage });

  await run("wasm-tools", ["validate", "--features", "all", out]);
  const built = await declaredPackage(out);
  if (built !== declaration(pkg, version)) throw new Error(`built package declares '${built}'`);
  console.log(`built ${out} (${built})`);
}

if (process.argv[1] && import.meta.url === pathToFileURL(realpathSync(process.argv[1])).href) runMain(main);
