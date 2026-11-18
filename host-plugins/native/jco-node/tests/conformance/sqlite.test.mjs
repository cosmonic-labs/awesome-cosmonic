// Jco: packages/jco/test/node/sqlite.js, "runs SQLite resources and denial in %s" (fixture
// node-sqlite), for both backends; the async backup half runs on StarlingMonkey only, as in Jco.
import assert from "node:assert/strict";
import { mkdtemp, rm } from "node:fs/promises";
import { DatabaseSync } from "node:sqlite";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";

import { component, invoke } from "./lib.mjs";

for (const backend of ["starlingmonkey", "quickjs"]) {
    const build = () =>
        component("node-sqlite", { entry: "source.js", wit: "source.wit", world: "test", args: ["--backend", backend], variant: backend });

    const common = (report) => {
        assert.equal(report.identity, true);
        assert.deepEqual(report.exports, ["DatabaseSync", "Session", "StatementSync", "backup", "constants"]);
        assert.equal(report.constant, 2);
    };

    test(`node:sqlite with the interface refused (${backend})`, async () => {
        const { result } = await invoke(await build(), "run");
        const report = JSON.parse(result);
        common(report);
        assert.equal(report.denied, "ERR_JCO_SQLITE_ADAPTER_REQUIRED");
    });

    test(`node:sqlite with the interface granted (${backend})`, async () => {
        const wasm = await build();
        const report = JSON.parse((await invoke(wasm, "run", [], { grant: "sqlite" })).result);
        common(report);
        const expected = {
            open: true,
            session: true,
            statement: true,
            insert: { changes: 1, lastInsertRowid: 1 },
            nullPrototype: true,
            columns: ["id", "value", "blob"],
            iterated: [1],
            afterReturn: 2,
            arrayBigInt: "1",
            missing: true,
            transaction: true,
            changeset: true,
            copied: 2,
            patchset: true,
            tagState: [1, 2, true],
            cleared: 0,
            limit: true,
            restored: 2,
            callback: "ERR_JCO_SQLITE_CALLBACK_UNSUPPORTED",
            closed: true,
            closedStatement: "ERR_INVALID_STATE",
        };
        for (const [key, value] of Object.entries(expected)) {
            assert.deepEqual(report[key], value, key);
        }
        assert.deepEqual(report.rows, [
            { id: 1, value: "hello", blob: [0, 255] },
            { id: 2, value: "world", blob: null },
        ]);
        assert.deepEqual(report.sqlError, { name: "Error", code: "ERR_SQLITE_ERROR", errcode: 1555, errstr: "constraint failed" });
        if (backend === "quickjs") {
            return;
        }
        const root = await mkdtemp(join(tmpdir(), "jco-node-sqlite-"));
        try {
            const destination = join(root, "backup.db");
            const result = JSON.parse((await invoke(wasm, "backup-test", [destination], { grant: "sqlite" })).result);
            assert.ok(result.pages > 0);
            const db = new DatabaseSync(destination);
            try {
                assert.equal(db.prepare("SELECT value FROM backup_data").get().value, 42);
            } finally {
                db.close();
            }
        } finally {
            await rm(root, { recursive: true, force: true });
        }
    });
}
