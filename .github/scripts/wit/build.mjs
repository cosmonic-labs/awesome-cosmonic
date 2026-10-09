// Validates a WIT package directory and builds it into dist/<package>.wasm.
//
//   node .github/scripts/wit/build.mjs wit/cosmonic-agent agent
//
// Imported packages are pinned by wkg.lock; fetching must preserve the lock.
import { copyFileSync, existsSync, mkdirSync, mkdtempSync, readdirSync, readFileSync, realpathSync } from "node:fs";
import { tmpdir } from "node:os";
import { pathToFileURL } from "node:url";
import { join, resolve } from "node:path";
import { declaration, declaredPackage, packageInfo, run, runMain, setOutput } from "./lib.mjs";

const SEMVER = /^\d+\.\d+\.\d+(-[0-9A-Za-z.-]+)?$/;

const DECL = /^package\s+([a-z0-9-]+:[a-z0-9-]+)@(\S+?)\s*;/;

// Every file declares the package and they must all agree. Returns the version.
export function readVersion(dir, pkg) {
  const files = readdirSync(dir).filter((f) => f.endsWith(".wit"));
  if (files.length === 0) throw new Error(`${dir} has no .wit files`);
  const found = new Set();
  for (const f of files) {
    const m = readFileSync(join(dir, f), "utf8").split(/\r?\n/).map((l) => DECL.exec(l)).find(Boolean);
    if (!m) throw new Error(`${f} has no package declaration`);
    found.add(`${m[1]}@${m[2]}`);
  }
  if (found.size !== 1) throw new Error(`files disagree on the package declaration: ${[...found].join(" | ")}`);
  const [name, version] = [...found][0].split("@");
  if (!SEMVER.test(version)) throw new Error(`version '${version}' is not <major>.<minor>.<patch>[-prerelease]`);
  if (name !== packageInfo(pkg).id) throw new Error(`${dir} declares ${name}, expected ${packageInfo(pkg).id}`);
  return version;
}

async function main([dir, pkg]) {
  if (!dir || !pkg) throw new Error("usage: build.mjs <wit-dir> <package>");
  const info = packageInfo(pkg);
  const version = readVersion(dir, pkg);
  setOutput("version", version);

  // wash builds the package in ./wit, so stage the one being built.
  const stage = mkdtempSync(join(process.env.RUNNER_TEMP ?? tmpdir(), `wit-${info.namespace}-${info.name}-`));
  mkdirSync(join(stage, "wit"), { recursive: true });
  for (const f of readdirSync(dir).filter((f) => f.endsWith(".wit"))) {
    copyFileSync(join(dir, f), join(stage, "wit", f));
  }

  // A package with dependencies commits a wkg.lock; fetching must reproduce it.
  const lock = join(dir, "wkg.lock");
  if (existsSync(lock)) {
    copyFileSync(lock, join(stage, "wkg.lock"));
    await run("wash", ["wit", "fetch"], {
      cwd: stage,
      env: { ...process.env, WKG_CONFIG_FILE: resolve("wit/wkg-registries.toml") },
    });
    if (readFileSync(lock, "utf8") !== readFileSync(join(stage, "wkg.lock"), "utf8")) {
      throw new Error(`wash wit fetch changed ${lock}; commit the updated lock deliberately`);
    }
  }

  await run("wasm-tools", ["component", "wit", join(stage, "wit")]);
  mkdirSync("dist", { recursive: true });
  const out = resolve("dist", `${info.name}.wasm`);
  await run("wash", ["wit", "build", "--output-file", out], { cwd: stage });

  await run("wasm-tools", ["validate", "--features", "all", out]);
  const built = await declaredPackage(out);
  if (built !== declaration(pkg, version)) throw new Error(`built package declares '${built}'`);
  console.log(`built ${out} (${built})`);
}

if (process.argv[1] && import.meta.url === pathToFileURL(realpathSync(process.argv[1])).href) runMain(main);
