// Registry steps for a built WIT package at ghcr.io/cosmonic-labs/<namespace>/<package>.
//
//   check  <package> <wasm> [--anonymous]   is this version published, and is it these bytes?
//   push   <package> <wasm>                 push the package, print its digest
//   verify <package> <wasm> <digest>        pull it back and check what it declares
//
// `check` fails if the version is published with other contents. Anonymous
// mode is read-only and passes when the package is not visible.
import { createHash } from "node:crypto";
import { readFileSync, realpathSync } from "node:fs";
import { tmpdir } from "node:os";
import { pathToFileURL } from "node:url";
import { join } from "node:path";
import { declaration, declaredPackage, packageInfo, notice, run, runMain, setOutput, warning } from "./lib.mjs";

const REGISTRY = "ghcr.io";
const ACCEPT = [
  "application/vnd.oci.image.manifest.v1+json",
  "application/vnd.oci.image.index.v1+json",
  "application/vnd.docker.distribution.manifest.v2+json",
  "application/vnd.docker.distribution.manifest.list.v2+json",
].join(", ");

const sha256 = (buf) => `sha256:${createHash("sha256").update(buf).digest("hex")}`;
const versionOf = (decl) => decl.slice(decl.indexOf("@") + 1, -1);

// Looks the version up in the registry. Returns { status, body, digest }.
export async function probe(name, version, { anonymous }) {
  const headers = {};
  const user = process.env.GITHUB_ACTOR;
  const pass = process.env.REGISTRY_TOKEN ?? process.env.GITHUB_TOKEN;
  if (!anonymous && user && pass) headers.Authorization = `Basic ${Buffer.from(`${user}:${pass}`).toString("base64")}`;

  const tokenRes = await fetch(`https://${REGISTRY}/token?service=${REGISTRY}&scope=repository:${name}:pull`, { headers });
  const token = tokenRes.ok ? (await tokenRes.json()).token : undefined;
  if (!token && !anonymous) throw new Error(`${REGISTRY} issued no token for ${name}`);

  const res = await fetch(`https://${REGISTRY}/v2/${name}/manifests/${version}`, {
    headers: { Accept: ACCEPT, ...(token && { Authorization: `Bearer ${token}` }) },
  });
  const raw = Buffer.from(await res.arrayBuffer());
  let body;
  try {
    body = JSON.parse(raw.toString());
  } catch {}
  return { status: res.status, body, digest: sha256(raw) };
}

async function check(pkg, wasm, anonymous) {
  const built = await declaredPackage(wasm);
  const version = versionOf(built);
  if (built !== declaration(pkg, version)) throw new Error(`artifact declares '${built}', expected ${packageInfo(pkg).id}`);
  const name = packageInfo(pkg).repository;
  const ref = `${REGISTRY}/${name}:${version}`;
  const { status, body, digest } = await probe(name, version, { anonymous });

  if (status === 200) {
    // wash pushes the package as the manifest's only layer.
    if (body?.layers?.[0]?.digest === sha256(readFileSync(wasm))) {
      notice(`${ref} already holds this build`);
      setOutput("action", "attest");
      setOutput("digest", digest);
      return;
    }
    throw new Error(`${ref} is already published with different contents; bump the package version to release these changes`);
  }
  if (status === 404) {
    notice(`${ref} is not published yet`);
    setOutput("action", "push");
  } else if (anonymous && [401, 403].includes(status)) {
    notice(`${ref} is not visible anonymously (HTTP ${status}); skipping the version check`);
    setOutput("action", "push");
  } else if (anonymous) {
    warning(`probing ${ref} returned HTTP ${status}; skipping the version check`);
    setOutput("action", "push");
  } else {
    throw new Error(`probing ${ref} returned HTTP ${status}`);
  }
}

async function push(pkg, wasm) {
  const built = await declaredPackage(wasm);
  const version = versionOf(built);
  if (built !== declaration(pkg, version)) throw new Error(`artifact declares '${built}', expected ${packageInfo(pkg).id}`);
  const ref = `${REGISTRY}/${packageInfo(pkg).repository}:${version}`;
  const out = await run("wash", ["-o", "json", "oci", "push", ref, wasm]);
  console.log(out);
  const digest = JSON.parse(out).data?.digest;
  if (!digest?.startsWith("sha256:")) throw new Error("wash oci push did not report a manifest digest");
  setOutput("digest", digest);
}

async function verify(pkg, wasm, digest) {
  const built = await declaredPackage(wasm);
  const version = versionOf(built);
  if (built !== declaration(pkg, version)) throw new Error(`artifact declares '${built}', expected ${packageInfo(pkg).id}`);
  const ref = `${REGISTRY}/${packageInfo(pkg).repository}@${digest}`;
  const pulled = join(process.env.RUNNER_TEMP ?? tmpdir(), `pulled-${packageInfo(pkg).namespace}-${packageInfo(pkg).name}.wasm`);
  await run("wash", ["oci", "pull", ref, pulled]);
  const decl = await declaredPackage(pulled);
  if (decl !== declaration(pkg, version)) throw new Error(`${ref} declares '${decl}'`);
  console.log(`${ref} declares ${decl}`);
}

async function main([cmd, pkg, wasm, ...rest]) {
  if (!pkg || !wasm) throw new Error("usage: registry.mjs <check|push|verify> <package> <wasm> [digest|--anonymous]");
  if (cmd === "check") return check(pkg, wasm, rest.includes("--anonymous"));
  if (cmd === "push") return push(pkg, wasm);
  if (cmd === "verify") return verify(pkg, wasm, rest[0]);
  throw new Error(`unknown command '${cmd}'`);
}

if (process.argv[1] && import.meta.url === pathToFileURL(realpathSync(process.argv[1])).href) runMain(main);
