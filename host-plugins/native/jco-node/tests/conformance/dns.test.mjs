// Jco: packages/jco/test/node/dns.js, "componentizes and resolves example.com through the opt-in
// Node host" (fixture node-dns and its run.js). Like Jco's test, this performs a real lookup.
import assert from "node:assert/strict";
import { test } from "node:test";

import { component, invoke } from "./lib.mjs";

test("node:dns with the interface granted", async () => {
    const wasm = await component("node-dns", { args: ["--backend", "starlingmonkey"] });
    const { result } = await invoke(wasm, "run", [], { grant: "dns" });
    assert.ok(result.serverCount >= 0);
    assert.equal(result.namespaceIdentity, true);
    assert.equal(result.promisesIdentity, true);
    assert.equal(result.resultOrder, "ipv4first");
    assert.equal(result.cancelCode, "ERR_JCO_UNSUPPORTED_NODE_API");
    assert.ok(result.externalAddressCount >= 1);
    assert.equal(result.externalAddressesAreIpv4, true);
});

test("node:dns with the interface refused", async () => {
    const wasm = await component("node-dns", { args: ["--backend", "starlingmonkey"] });
    await assert.rejects(invoke(wasm, "run", []), (error) => {
        // The fixture does not catch the denial, so the guest reports it as an uncaught error,
        // which prints the message (not the code) before trapping.
        assert.equal(error.trap, true);
        assert.match(error.stderr, /node:dns requires an application-provided host adapter/);
        return true;
    });
});
