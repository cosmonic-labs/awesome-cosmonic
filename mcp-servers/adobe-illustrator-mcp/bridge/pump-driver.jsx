// pump-driver.jsx — zero-install bridge pump for Illustrator.
//
// Served (concatenated after commands.jsx) at /bridge/pump.jsx. Run it from
// File > Scripts > Other Script… — it polls the illustrator-mcp workload,
// executes queued commands against the Illustrator DOM, posts results back,
// and exits after ~2 minutes (or ~20s of quiet after having done work).
//
// Illustrator's ExtendScript has no timer (no app.scheduleTask like After
// Effects), so while a pump run is active the UI is busy. For a persistent,
// non-blocking bridge use the CEP panel installed by ./install-bridge.sh.

// Cosmonic Desktop's ingress listens on one port and routes to a workload by
// the HTTP Host header, so the address to dial and the name to ask for are
// separate settings. Must match the deployed workload's hostInterface config.
var BRIDGE_SERVER = "127.0.0.1";
var BRIDGE_PORT = 8200;
var BRIDGE_HOST_HEADER = "illustrator-mcp.localhost";

// How long one pump run lasts, and how long to linger after the last command
// before exiting early.
var PUMP_RUN_MS = 120000;
var PUMP_IDLE_EXIT_MS = 20000;
var PUMP_POLL_MS = 700;

// Identifies this pump instance; the server serves commands only to the
// highest client id it has seen, so a rerun supersedes a stuck predecessor.
var PUMP_CLIENT_ID = new Date().getTime();

function pumpUtf8Len(str) {
    return unescape(encodeURIComponent(str)).length;
}

// Split an HTTP response into its body. ExtendScript's Socket.read() strips
// carriage returns, so the response arrives LF-only and the canonical
// "\r\n\r\n" separator may never be present.
function pumpExtractBody(response) {
    var separators = ["\r\n\r\n", "\n\n", "\r\r"];
    for (var i = 0; i < separators.length; i++) {
        var at = response.indexOf(separators[i]);
        if (at >= 0) { return response.substring(at + separators[i].length); }
    }
    var brace = response.indexOf("{");
    var close = response.lastIndexOf("}");
    if (brace >= 0 && close > brace) { return response.substring(brace, close + 1); }
    return null;
}

// Minimal HTTP/1.0 client over an ExtendScript Socket. Returns the response
// body as a string, or null if the request failed.
function pumpHttp(method, path, body) {
    var conn = new Socket();
    conn.timeout = 5;
    if (!conn.open(BRIDGE_SERVER + ":" + BRIDGE_PORT, "UTF-8")) { return null; }
    try {
        var req = method + " " + path + " HTTP/1.0\r\n" +
                  "Host: " + BRIDGE_HOST_HEADER + "\r\n" +
                  "Connection: close\r\n";
        if (body !== null && body !== undefined) {
            req += "Content-Type: application/json\r\n" +
                   "Content-Length: " + pumpUtf8Len(body) + "\r\n";
        }
        req += "\r\n";
        if (body !== null && body !== undefined) { req += body; }
        conn.write(req);
        var response = "";
        while (conn.connected && !conn.eof) {
            var chunk = conn.read(65536);
            if (chunk === null || chunk === "") { break; }
            response += chunk;
        }
        conn.close();
        return pumpExtractBody(response);
    } catch (e) {
        try { conn.close(); } catch (ignored) {}
        return null;
    }
}

function pumpRun() {
    // Illustrator 2023+ removed ExtendScript's Socket class; the pump cannot
    // speak HTTP there. The shuttle (/bridge/shuttle.sh) and the CEP panel
    // are the working vehicles on modern versions.
    if (typeof Socket === "undefined") {
        alert("Illustrator MCP pump: this Illustrator has no ExtendScript Socket " +
              "class (removed in 2023+), so the in-app pump cannot reach the server.\n\n" +
              "Use the zero-install shuttle instead:\n" +
              "  curl -H 'Host: " + BRIDGE_HOST_HEADER + "' http://" + BRIDGE_SERVER + ":" +
              BRIDGE_PORT + "/bridge/shuttle.sh -o shuttle.sh && bash shuttle.sh\n\n" +
              "or install the CEP panel with ./install-bridge.sh and restart Illustrator.");
        return;
    }
    var palette = null;
    var statusLine = null;
    var logLine = null;
    try {
        palette = new Window("palette", "Illustrator MCP pump");
        palette.orientation = "column";
        palette.alignChildren = ["fill", "top"];
        statusLine = palette.add("statictext", undefined, "Connecting to " + BRIDGE_HOST_HEADER + "…");
        logLine = palette.add("statictext", undefined, "0 commands executed");
        palette.show();
    } catch (uiError) { palette = null; }

    function report(status, log) {
        try {
            if (statusLine && status) { statusLine.text = status; }
            if (logLine && log) { logLine.text = log; }
            if (palette) { palette.update(); }
        } catch (e) {}
    }

    var started = new Date().getTime();
    var lastActivity = started;
    var served = 0;
    var failedPosts = 0;

    while (true) {
        var now = new Date().getTime();
        if (now - started > PUMP_RUN_MS) { break; }
        if (served > 0 && now - lastActivity > PUMP_IDLE_EXIT_MS) { break; }

        var responseText = pumpHttp(
            "GET", "/bridge/command?v=2&client=" + PUMP_CLIENT_ID, null);
        if (responseText === null) {
            report("Server unreachable — is the illustrator-mcp workload running?", null);
        } else {
            var commandData = null;
            try { commandData = JSON.parse(responseText); } catch (e) {}
            if (commandData && commandData.command) {
                report("Running: " + commandData.command, null);
                var resultString = executeCommand(commandData.command, commandData.args || {});
                var posted = null;
                for (var attempt = 0; attempt < 3 && posted === null; attempt++) {
                    posted = pumpHttp("POST", "/bridge/result?id=" + commandData.id, resultString);
                }
                if (posted === null) { failedPosts++; }
                served++;
                lastActivity = new Date().getTime();
                report("Done: " + commandData.command, served + " commands executed");
                try { app.redraw(); } catch (redrawError) {}
                continue; // check for the next command immediately
            }
            report("Connected (" + BRIDGE_HOST_HEADER + "), waiting for commands…", null);
        }
        $.sleep(PUMP_POLL_MS);
    }

    if (palette) { try { palette.close(); } catch (e) {} }
    if (served > 0) {
        // A quiet exit is fine; only surface trouble.
        if (failedPosts > 0) {
            alert("Illustrator MCP pump: executed " + served + " command(s), but " +
                  failedPosts + " result(s) could not be posted back.");
        }
    }
}

pumpRun();
