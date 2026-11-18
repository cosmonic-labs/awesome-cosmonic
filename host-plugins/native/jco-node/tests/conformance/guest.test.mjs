// Jco's fixtures for builtins implemented inside the component (packages/jco/test/node/*.js),
// plus the nodejs-platform example. None of them needs a jco:node grant: what they import (a
// console or timers interface QuickJS bundles add) is refused here as Jco's default providers
// refuse it, and the fixtures never call it. Each expectation is the one Jco's test asserts.
import assert from "node:assert/strict";
import { join } from "node:path";
import { test } from "node:test";

import { EXAMPLES, component, invoke } from "./lib.mjs";

const SM = ["--backend", "starlingmonkey"];
const QJS = ["--backend", "qjs", "--backend-qjs-disable-async"];
const fixture = (name, args, options = {}) => () =>
    component(name, { entry: "source.js", wit: "source.wit", world: "test", args, ...options });

/** Call `name` on a fresh instance; JSON string results are parsed. */
async function call(build, name, args = [], options = {}) {
    const { result } = await invoke(await build(), name, args, options);
    return typeof result === "string" && /^[[{]/.test(result) ? JSON.parse(result) : result;
}

test("node:assert (assert.js)", async () => {
    assert.deepEqual(await call(fixture("node-assert", QJS), "run"), {
        scalarChecks: 19,
        deepChecks: 14,
        matcherChecks: 12,
        moduleChecks: 7,
        promiseChecks: 2,
        deprecatedChecks: 2,
        failureCode: "ERR_ASSERTION",
        failureOperator: "strictEqual",
        strictSubpath: true,
        deprecatedCode: "ERR_JCO_UNSUPPORTED_DEPRECATED_NODE_API",
    });
});

test("node:buffer (buffer.js)", async () => {
    assert.deepEqual(await call(fixture("node-buffer", QJS, { wit: "." }), "run"), {
        text: "component ✓",
        hex: "636f6d706f6e656e7420e29c93",
        base64: "Y29tcG9uZW50IOKckw==",
        byteLength: 13,
        combined: "onetwo",
        numericBytes: [239, 205, 171, 137, 255, 237, 41, 121, 219, 15, 73, 64],
        swappedBytes: [17, 0, 51, 34],
        searchIndex: 3,
        moduleIdentity: true,
        instanceIdentity: true,
        callCode: "ERR_JCO_UNSUPPORTED_DEPRECATED_NODE_API",
        constructCode: "ERR_JCO_UNSUPPORTED_DEPRECATED_NODE_API",
        slowCode: "ERR_JCO_UNSUPPORTED_DEPRECATED_NODE_API",
    });
});

test("Node error globals (errors.js)", async () => {
    const build = () => component("node-errors", { world: "component", args: SM });
    assert.deepEqual(await call(build, "run"), {
        message: "outer",
        causeMessage: "cause",
        aggregateCount: 2,
        suppressedMessage: "suppressed",
        isError: true,
        capturedStack: true,
    });
});

test("node:events (events.js)", async () => {
    assert.deepEqual(await call(fixture("node-events", SM), "run"), {
        seen: ["first", 1, "once"],
        listenerCount: 2,
        staticListenerCount: 2,
        eventNames: ["x"],
        getEventListeners: 2,
        maxOnTarget: 4,
        maxOnEmitter: 7,
        defaultApplied: 3,
        badTarget: "ERR_INVALID_ARG_TYPE",
        badRange: "ERR_OUT_OF_RANGE",
        moduleIsClass: true,
        staticOnce: "function",
        errored: "threw",
    });
});

test("Node globals (globals.js)", async () => {
    const { abortCases, wasm, ...present } = await call(fixture("node-globals", SM), "run");
    assert.equal(wasm, false);
    assert.equal(Object.keys(present).length, 28);
    for (const [name, value] of Object.entries(present)) {
        assert.equal(value, true, name);
    }
    assert.equal(Object.keys(abortCases).length, 12);
    for (const [name, value] of Object.entries(abortCases)) {
        assert.equal(value, true, `abortCases.${name}`);
    }
});

test("node:module (module.js)", async () => {
    const report = await call(fixture("node-module", SM), "run");
    assert.ok(Array.isArray(report.builtins) && report.builtins.includes("fs"));
    delete report.builtins;
    const unsupported = "ERR_JCO_UNSUPPORTED_NODE_API";
    assert.deepEqual(report, {
        moduleIsClass: true,
        isBuiltinFs: true,
        isBuiltinBareTest: false,
        isBuiltinPrefixedTest: true,
        wrapped: "(function (exports, require, module, __filename, __dirname) { const a = 1;\n});",
        sourceMapsSupport: { enabled: false, nodeModules: false, generatedCode: false },
        entry: { generatedLine: 0, generatedColumn: 9, originalSource: "a.ts", originalLine: 0, originalColumn: 9, name: "alpha" },
        origin: { name: "beta", fileName: "b.ts", lineNumber: 2, columnNumber: 1 },
        resolvesBuiltin: "node:path",
        resolveMissing: "MODULE_NOT_FOUND",
        compileCacheStatus: 0,
        globalPaths: [],
        requiring: unsupported,
        register: unsupported,
        runMain: unsupported,
        stripTypes: unsupported,
        moduleRequire: unsupported,
    });
});

test("node:querystring (querystring.js)", async () => {
    assert.deepEqual(await call(fixture("node-querystring", QJS), "run"), {
        repeated: ["one", "two words"],
        empty: "",
        flag: "",
        malformedFallback: true,
        customFirst: "one",
        customSecond: "two words",
        limitedKeys: 2,
        encoded: "value=one&value=two%20words&empty=&enabled=true&nil=",
        customEncoded: "[first]:[one];[second]:[two words]",
        escaped: "a%20b%2Bc%2F%E2%9C%93",
        unescaped: "a+b+c/✓",
        bufferBytes: [65, 32, 66],
        nullPrototype: true,
        namespaceChecks: 4,
    });
});

test("node:readline (readline.js, StarlingMonkey)", async () => {
    assert.deepEqual(await call(fixture("node-readline", SM), "run"), {
        simple: "What do you think of Node.js? Thank you for your valuable feedback: Useful!\n",
        moduleIdentity: true,
        eventIdentity: true,
        callbackAnswer: "yes",
        lines: ["A🌍", "", "next", "last", "para", "tail"],
        cleanup: true,
        editedLine: "abXc",
        cursor: { cols: 5, rows: 0 },
        history: ["abXc"],
        recalled: "abXc",
        rawReleased: true,
        completion: "hel",
        promiseCompletion: "wor",
        keys: [["a", "a", false], [null, "left", true]],
        deferred: true,
        actions: "\x1b[2;3H\x1b[1D\x1b[3B\x1b[2K\x1b[0J",
        autoCommit: "\x1b[1G",
        iterated: ["first", "second", "tail"],
        abort: ["AbortError", "ABORT_ERR", "cancelled"],
        closedError: "ERR_USE_AFTER_CLOSE",
    });
});

test("node:repl (repl.js, StarlingMonkey)", async () => {
    const { output, ...report } = await call(fixture("node-repl", SM), "run");
    assert.deepEqual(report, {
        moduleIdentity: true,
        prototype: true,
        builtinModules: true,
        validSyntax: [true, false],
        closed: true,
        events: ["exit"],
        lines: 15,
        refusal: ["ERR_JCO_UNSUPPORTED_NODE_API", 0, ""],
        deprecated: "ERR_JCO_UNSUPPORTED_DEPRECATED_NODE_API",
    });
    assert.equal(
        output,
        [
            "> 2",
            "> undefined",
            "> 42",
            "> | | | undefined",
            "> 42",
            "> { a: 1, b: 'two' }",
            "> 'from the host program'",
            "> 'from the host program'",
            "> Uncaught Error: boom",
            "> 'boom'",
            "> foo bar",
            "    ^",
            "",
            "Uncaught SyntaxError: Unexpected identifier 'bar'",
            "> HELLO THERE",
            "> undefined",
            "> 'awaited'",
            "> .break   Sometimes you get stuck, this gets you out",
            ".clear   Alias for .break",
            ".exit    Exit the REPL",
            ".help    Print this help message",
            ".load    Load JS from a file into the REPL session",
            ".save    Save all evaluated commands in this REPL session to a file",
            ".shout   Shout the rest",
            "",
            "Press Ctrl+C to abort current expression, Ctrl+D to exit the REPL",
            "> ",
        ].join("\n"),
    );
});

test("node:stream (stream.js)", async () => {
    assert.deepEqual(await call(fixture("node-stream", SM), "run"), {
        consumedText: "A€!",
        consumedJson: 24,
        consumedBuffer: "buffer",
        syncText: "sync!",
        protocolText: "protocol",
        sourceBytes: "iter!",
        arrayChunks: 2,
        transformed: "COMPONENT!",
        tapped: "1,0",
        written: "piped",
        writtenBytes: 5,
        pushed: "push",
        broadcast: ["broadcast", "broadcast"],
        shared: ["share", "share"],
        classStatics: true,
        unsupportedCode: "ERR_JCO_UNSUPPORTED_NODE_API",
    });
});

test("node:string_decoder (string-decoder.js)", async () => {
    assert.deepEqual(await call(fixture("node-string-decoder", SM), "run"), {
        decoded: "A🌍",
        encoded: "-_8",
        moduleIdentity: true,
    });
});

test("node:vm (vm.js, StarlingMonkey)", async () => {
    const build = fixture("node-vm", SM);
    const report = await call(build, "run");
    const { failures, ...rest } = report;
    assert.deepEqual(rest, {
        identity: true,
        exports: [
            "Module", "Script", "SourceTextModule", "SyntheticModule", "compileFunction", "constants", "createContext",
            "createScript", "isContext", "measureMemory", "runInContext", "runInNewContext", "runInThisContext",
        ],
        execution: [1, 2, 42, 42, 42],
        global: true,
        local: 42,
        metadata: "answer.map",
        context: false,
        constants: true,
    });
    const { syntax, input, deprecated, ...others } = failures;
    assert.deepEqual(syntax, { name: "SyntaxError" });
    assert.deepEqual(input, { name: "TypeError", code: "ERR_INVALID_ARG_TYPE" });
    assert.deepEqual(deprecated, { name: "Error", code: "ERR_JCO_UNSUPPORTED_DEPRECATED_NODE_API" });
    assert.ok(Object.keys(others).length > 0);
    for (const [name, value] of Object.entries(others)) {
        assert.deepEqual(value, { name: "Error", code: "ERR_JCO_UNSUPPORTED_NODE_API" }, name);
    }
});

test("node:path over wasi:cli/environment (path.js)", async () => {
    const build = () => component("node-path", { world: "component" });
    const wasm = await build();
    const options = { cwd: "/workspace/project" };
    assert.equal((await invoke(wasm, "lexical", [], options)).result, "a/c|C:\\a\\b|\\|true");
    assert.equal((await invoke(wasm, "from-cwd", [], options)).result, "/workspace/project/relative");
    for (const [path, pattern, posix, win32] of [
        ["src/component.ts", "**/*.{js,ts}", true, true],
        ["src/component.md", "**/*.{js,ts}", false, false],
        ["file2.js", "file{1..3}.js", true, true],
        ["file4.js", "file{1..3}.js", false, false],
        ["a/b.js", "@(a|b)/*.js", true, true],
        ["literal[1].js", "literal[[]1].js", true, true],
        ["src\\component.ts", "src\\*.{js,ts}", false, true],
    ]) {
        assert.equal((await invoke(wasm, "match", [path, pattern, false], options)).result, posix, `posix ${path} ${pattern}`);
        assert.equal((await invoke(wasm, "match", [path, pattern, true], options)).result, win32, `win32 ${path} ${pattern}`);
    }
});

test("node:url file URLs from the WASI cwd (url.js, StarlingMonkey)", async () => {
    const wasm = await component("node-url", { variant: "cwd", entry: "cwd.js", world: "cwd", args: SM });
    const options = { cwd: "/workspace/project" };
    for (const [input, expected] of [
        ["relative", { href: "file:///workspace/project/relative", path: "/workspace/project/relative" }],
        ["a/../two words#?%", { href: "file:///workspace/project/two%20words%23%3F%25", path: "/workspace/project/two words#?%" }],
        ["", { href: "file:///workspace/project", path: "/workspace/project" }],
        ["/absolute", { href: "file:///absolute", path: "/absolute" }],
    ]) {
        assert.deepEqual(JSON.parse((await invoke(wasm, "from-cwd", [input], options)).result), expected, input);
    }
});

test("example nodejs-platform: wasi:cli/run succeeds", async () => {
    const wasm = await component("nodejs-platform", {
        source: join(EXAMPLES, "nodejs-platform"),
        entry: "src/component.js",
    });
    const { result, stdout } = await invoke(wasm, "wasi:cli/run@0.2.6#run", [], {});
    assert.equal(result, null);
    assert.ok(stdout.length > 0);
});
