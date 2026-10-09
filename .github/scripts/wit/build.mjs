// Validates a WIT package directory and builds it into dist/<package>.wasm.
//
//   node .github/scripts/wit/build.mjs wit/cosmonic-agent agent
import { copyFileSync, mkdirSync, readdirSync, readFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { declaration, declaredPackage, run, runMain, setOutput } from "./lib.mjs";

const SEMVER = /^\d+\.\d+\.\d+(-[0-9A-Za-z.-]+)?$/;

// Every file declares the package and they must all agree. Returns the version.
export function readVersion(dir, pkg) {
  const files = readdirSync(dir).filter((f) => f.endsWith(".wit"));
  if (files.length === 0) throw new Error(`${dir} has no .wit files`);
  const decls = new Set();
  for (const f of files) {
    const line = readFileSync(join(dir, f), "utf8").split("\n").find((l) => l.startsWith("package "));
    if (!line) throw new Error(`${f} has no package declaration`);
    decls.add(line.trim());
  }
  if (decls.size !== 1) throw new Error(`files disagree on the package declaration: ${[...decls].join(" | ")}`);
  const [decl] = decls;
  const version = decl.slice(decl.indexOf("@") + 1, -1);
  if (!SEMVER.test(version)) throw new Error(`version '${version}' is not <major>.<minor>.<patch>[-prerelease]`);
  if (decl !== declaration(pkg, version)) throw new Error(`unexpected package declaration: ${decl}`);
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

if (import.meta.url === `file://${process.argv[1]}`) runMain(main);
