// Jco: packages/jco/test/node/vfs.js (fixture node-vfs), StarlingMonkey, the `direct` mode (the
// guest's node:fs goes through jco:node/fs) and the `wasi-filesystem` mode (through a WASI
// preopen). Jco runs the real-storage cases on StarlingMonkey only.
import assert from "node:assert/strict";
import { mkdir, mkdtemp, readFile, rm, symlink } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import { setTimeout as delay } from "node:timers/promises";

import { Session, component } from "./lib.mjs";

const syncExpected = {
    sync: "hello world",
    bytes: "world",
    descriptorSize: 11,
    realpath: "/dir/sub/file",
    symlink: true,
    readlink: "sub/file",
    directoryEntry: "file",
    listing: ["hardlink", "link", "moved", "sub"],
    truncated: "Hello",
    temporary: true,
    missing: "ENOENT",
    removed: true,
};
const expected = { ...syncExpected, callback: "hello world", promise: "hello world" };

async function report(session, start, ...args) {
    await session.call(start, ...args);
    for (let attempt = 0; attempt < 100; attempt++) {
        const value = await session.call("take-report");
        if (value) {
            return JSON.parse(value);
        }
        await delay(10);
    }
    throw new Error("VFS report did not complete");
}

/** A host directory with `storage/` and the alias `storage-alias -> storage`. */
async function storage(work) {
    const root = await mkdtemp(join(tmpdir(), "jco-node-vfs-"));
    try {
        await mkdir(join(root, "storage"));
        await symlink("storage", join(root, "storage-alias"), "dir");
        await work(root);
        assert.equal(await readFile(join(root, "storage", "placement.txt"), "utf8"), "vfs contents");
    } finally {
        await rm(root, { recursive: true, force: true });
    }
}

for (const via of ["direct", "wasi-filesystem"]) {
    const build = () =>
        component("node-vfs", { variant: via, args: ["--backend", "starlingmonkey", "--with-nodejs-vfs-via", via] });
    // Direct mode: the guest names host paths and jco:node/fs serves them. WASI mode: the guest
    // names paths under a preopen.
    const options = (root) => (via === "direct" ? { grant: "fs" } : { dirs: [`${root}::/data`] });
    const guestRoot = (root) => (via === "direct" ? join(root, "storage-alias") : "/data/storage");

    test(`node:fs VFS via ${via}: the synchronous memory API, and refusal without a grant`, async () => {
        const session = new Session(await build(), {});
        try {
            assert.deepEqual(JSON.parse(await session.call("sync-memory")), syncExpected);
            if (via === "direct") {
                assert.equal(await session.call("denied"), "ERR_JCO_FS_ADAPTER_REQUIRED");
            }
        } finally {
            await session.close();
        }
    });

    test(`node:fs VFS via ${via}: the memory filesystem's callbacks and promises`, async () => {
        const session = new Session(await build(), {});
        try {
            assert.deepEqual(await report(session, "start-memory"), {
                ...expected,
                namespace: true,
                provider: true,
                isolated: true,
                readOnlyError: "EROFS",
            });
        } finally {
            await session.close();
        }
    });

    test(`node:fs VFS via ${via}: synchronous real storage`, async () => {
        await storage(async (root) => {
            const session = new Session(await build(), options(root));
            try {
                assert.deepEqual(JSON.parse(await session.call("sync-filesystem", guestRoot(root))), syncExpected);
            } finally {
                await session.close();
            }
        });
    });

    test(`node:fs VFS via ${via}: real storage through sync, callbacks, promises, descriptors and links`, async () => {
        await storage(async (root) => {
            const session = new Session(await build(), options(root));
            try {
                assert.deepEqual(await report(session, "start-filesystem", guestRoot(root)), expected);
                await assert.rejects(readFile(join(root, "storage", "dir", "sub", "file"), "utf8"), { code: "ENOENT" });
            } finally {
                await session.close();
            }
        });
    });
}
