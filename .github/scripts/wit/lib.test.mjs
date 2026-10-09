import assert from "node:assert/strict";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { declaration, packageInfo } from "./lib.mjs";
import { readVersion } from "./build.mjs";

const writeWit = (dir, name, id) => writeFileSync(join(dir, name), `package ${id};\ninterface types {}\n`);

test("package identity controls declaration and registry namespace", () => {
  assert.equal(packageInfo("agent").id, "cosmonic:agent");
  assert.equal(packageInfo("cosmonic:kafka").repository, "cosmonic-labs/cosmonic/kafka");
  assert.equal(packageInfo("wasmcloud:notifications").repository, "cosmonic-labs/wasmcloud/notifications");
  assert.equal(declaration("wasmcloud:notifications", "0.3.0"), "package wasmcloud:notifications@0.3.0;");
  for (const id of ["../notify", "cosmonic:notify/extra", "WasmCloud:notify", "cosmonic:bad--name"]) {
    assert.throws(() => packageInfo(id), /invalid package id/);
  }
});

test("build validation rejects namespace mismatches and mixed declarations", () => {
  const dir = mkdtempSync(join(tmpdir(), "wit-build-test-"));
  try {
    writeWit(dir, "types.wit", "wasmcloud:notifications@0.3.0");
    writeWit(dir, "world.wit", "wasmcloud:notifications@0.3.0");
    assert.equal(readVersion(dir, "wasmcloud:notifications"), "0.3.0");
    assert.throws(() => readVersion(dir, "cosmonic:notifications"), /expected cosmonic:notifications/);
    writeWit(dir, "world.wit", "wasmcloud:notifications@0.4.0");
    assert.throws(() => readVersion(dir, "wasmcloud:notifications"), /files disagree/);
    writeWit(dir, "types.wit", "cosmonic:agent@0.3.0");
    writeWit(dir, "world.wit", "cosmonic:agent@0.3.0");
    assert.equal(readVersion(dir, "agent"), "0.3.0");
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("registry override is package-specific", () => {
  const config = readFileSync(new URL("../../../wit/wkg-registries.toml", import.meta.url), "utf8");
  assert.match(config, /\[package_registry_overrides\."wasmcloud:notifications"\]/);
  assert.doesNotMatch(config, /\[namespace_registries\.wasmcloud(?:\]|\.)/);
});
