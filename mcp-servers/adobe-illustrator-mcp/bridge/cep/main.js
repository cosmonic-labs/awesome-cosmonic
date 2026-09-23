// Illustrator MCP Bridge — CEP panel logic.
//
// Polls the illustrator-mcp workload on Cosmonic Desktop every ~2 seconds,
// executes claimed commands in Illustrator's ExtendScript engine (via the
// commands.jsx library loaded by the manifest's ScriptPath), and posts
// results back.
//
// Uses the raw __adobe_cep__ API rather than CSInterface.js so the panel is
// fully self-contained. The URL carries the ingress hostname
// (illustrator-mcp.localhost resolves to loopback), so no Host-header
// gymnastics are needed here, unlike the ExtendScript Socket pump.

/* global window, document, XMLHttpRequest, localStorage */
"use strict";

var POLL_MS = 2000;

// Newest instance wins server-side, so a reloaded panel supersedes the old.
var CLIENT_ID = Date.now();

var statusEl = document.getElementById("status");
var serverEl = document.getElementById("server");
var autorunEl = document.getElementById("autorun");
var logEl = document.getElementById("log");
var checking = false;

try {
  var savedServer = localStorage.getItem("ilst-mcp-server");
  if (savedServer) { serverEl.value = savedServer; }
} catch (e) {}

serverEl.addEventListener("change", function () {
  try { localStorage.setItem("ilst-mcp-server", serverEl.value); } catch (e) {}
  log("server set to " + serverEl.value);
});

document.getElementById("checknow").addEventListener("click", function () {
  poll();
});

function log(message) {
  var stamp = new Date().toLocaleTimeString();
  logEl.textContent = stamp + "  " + message + "\n" + logEl.textContent;
  if (logEl.textContent.length > 20000) {
    logEl.textContent = logEl.textContent.slice(0, 15000);
  }
}

function setStatus(text, cls) {
  statusEl.textContent = text;
  statusEl.className = cls || "";
}

function baseUrl() {
  return serverEl.value.replace(/\/+$/, "");
}

function request(method, path, body, done) {
  var xhr = new XMLHttpRequest();
  xhr.open(method, baseUrl() + path, true);
  xhr.timeout = 5000;
  if (body !== null && body !== undefined) {
    // text/plain keeps this a CORS "simple request" (no preflight); the
    // server parses the body as JSON regardless.
    xhr.setRequestHeader("Content-Type", "text/plain");
  }
  xhr.onload = function () {
    done(xhr.status >= 200 && xhr.status < 300 ? xhr.responseText : null);
  };
  xhr.onerror = function () { done(null); };
  xhr.ontimeout = function () { done(null); };
  xhr.send(body === undefined ? null : body);
}

function evalInIllustrator(commandData, done) {
  var encoded = encodeURIComponent(JSON.stringify(commandData));
  var script = "ilstMcpExecuteWire(" + JSON.stringify(encoded) + ")";
  try {
    window.__adobe_cep__.evalScript(script, function (result) { done(result); });
  } catch (e) {
    done(JSON.stringify({ status: "error", message: "evalScript failed: " + e }));
  }
}

function postResult(id, resultString, attempt) {
  request("POST", "/bridge/result?id=" + id, resultString, function (posted) {
    if (posted === null) {
      if (attempt < 3) {
        log("post attempt " + attempt + " failed; retrying");
        postResult(id, resultString, attempt + 1);
      } else {
        log("WARNING: failed to post result after 3 attempts");
      }
    }
  });
}

function poll() {
  if (checking || !autorunEl.checked) { return; }
  checking = true;
  request("GET", "/bridge/command?v=2&client=" + CLIENT_ID, undefined, function (text) {
    if (text === null) {
      setStatus("Server unreachable — is the illustrator-mcp workload running?", "bad");
      checking = false;
      return;
    }
    var data = null;
    try { data = JSON.parse(text); } catch (e) {}
    if (!data || !data.command) {
      if (data && data.note) { setStatus(data.note, "bad"); }
      else { setStatus("Connected (" + baseUrl() + ")", "ok"); }
      checking = false;
      return;
    }
    setStatus("Running: " + data.command, "ok");
    log("executing " + data.command + " (id " + data.id + ")");
    evalInIllustrator(data, function (resultString) {
      postResult(data.id, resultString, 1);
      log("result posted for " + data.command);
      setStatus("Connected (" + baseUrl() + ")", "ok");
      checking = false;
    });
  });
}

setInterval(poll, POLL_MS);
log("Illustrator MCP Bridge started (client " + CLIENT_ID + ")");
poll();
