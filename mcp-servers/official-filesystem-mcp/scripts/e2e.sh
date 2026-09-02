#!/usr/bin/env bash
# End-to-end tests for official-filesystem-mcp. Framework checks (protocol,
# spec enforcement, discovery route, skills over MCP, robustness, Host guard)
# come from the shared harness in ../../scripts/mcp_e2e_lib.sh; tool cases
# live below.
#
# This server dials no upstream (allowedHosts: []), so there is no HTTP
# fixture to impersonate. The hermetic fixture is a directory tree built
# under $E2E_TMP and handed to wasmtime as preopens — exactly what a Desktop
# volumeMount becomes inside the component:
#
#   primary  :PORT        --dir fix::/data --dir fix2::/data2, FS_ALLOWED_DIRS=/data,/data2,/notes
#                         (/notes is deliberately NOT mounted), FS_MAX_RESULTS=50
#   read-only :FIXTURE_PORT  --dir fix::/data, FS_READ_ONLY=true
#   guard    :GUARD_PORT  --dir fix::/data but NO FS_ALLOWED_DIRS (missing-config path)
#
# An OUTSIDE directory next to the fixtures holds a secret that must never
# be reachable — through `..`, an absolute path, or the symlinks the fixture
# plants (escape.txt / escapedir point at it).
#
# Usage: scripts/e2e.sh [--no-build]        E2E_LIVE=1 adds a Desktop smoke.
set -u
cd "$(dirname "$0")/.."

PORT=${PORT:-9808}
GUARD_PORT=${GUARD_PORT:-9809}
FIXTURE_PORT=${FIXTURE_PORT:-9810}
RO_PORT=${RO_PORT:-$FIXTURE_PORT}
WASM=${WASM:-target/wasm32-wasip2/release/official_filesystem_mcp.wasm}
SKILL_NAME=official-filesystem-mcp

# shellcheck source=../../scripts/mcp_e2e_lib.sh
source "$(dirname "$0")/../../scripts/mcp_e2e_lib.sh"

RO_BASE="http://127.0.0.1:${RO_PORT}/"
GUARD_BASE="http://127.0.0.1:${GUARD_PORT}/"

# tree_text <body> — the directory_tree JSON text, decoded out of the SSE
# envelope so assertions can use plain (unescaped) quotes.
tree_text() {
  printf '%s' "$1" | python3 -c '
import json, sys
raw = sys.stdin.read()
for line in raw.splitlines():
    if line.startswith("data:"):
        print(json.loads(line[5:].strip())["result"]["content"][0]["text"]); break
'
}

# assert_rejected <name> <body> — a malformed argument is refused either as a
# tool-level error (isError:true, how rmcp reports a params deserialization
# failure) or as JSON-RPC -32602; both are clean refusals.
assert_rejected() {
  case "$2" in
    *'"isError":true'* | *'"error"'*) pass "$1" ;;
    *) fail "$1" "expected a refusal, got: $2" ;;
  esac
}

# ---------------------------------------------------------------------------
# Fixture tree
# ---------------------------------------------------------------------------
FIXDIR="$E2E_TMP/fix"
FIXDIR2="$E2E_TMP/fix2"
OUTSIDE="$E2E_TMP/outside"
rm -rf "$FIXDIR" "$FIXDIR2" "$OUTSIDE"
mkdir -p "$FIXDIR/sub/深" "$FIXDIR/notes" "$FIXDIR/node_modules" "$FIXDIR/many" "$FIXDIR/empty" "$FIXDIR2" "$OUTSIDE"
printf 'hello\n' > "$FIXDIR/a.txt"
: > "$FIXDIR/empty.txt"
printf 'unicode ok\n' > "$FIXDIR/héllo wörld.txt"
printf 'deep\n' > "$FIXDIR/sub/深/文件.txt"
printf 'in sub\n' > "$FIXDIR/sub/x.txt"
seq 1 100 > "$FIXDIR/lines.txt"
head -c 1048577 /dev/zero > "$FIXDIR/big.bin"                       # cap + 1 bytes
head -c 2097152 /dev/zero | tr '\0' 'a' > "$FIXDIR/big.txt"         # 2 MiB of text
printf '\xff\xfe\xfdnot utf8\n' > "$FIXDIR/bad.bin"
printf 'RIFF....WAVEfmt ' > "$FIXDIR/tiny.wav"
PNG_B64='iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg=='
printf '%s' "$PNG_B64" | base64 -d > "$FIXDIR/tiny.png"
printf '# top\n' > "$FIXDIR/top.md"
printf '# one\n' > "$FIXDIR/notes/one.md"
printf '# two\n' > "$FIXDIR/notes/two.md"
printf '# readme\n' > "$FIXDIR/notes/README.md"
printf 'module.exports = 1;\n' > "$FIXDIR/node_modules/x.js"
printf '# vendored\n' > "$FIXDIR/node_modules/vendored.md"
for i in $(seq 1 60); do printf '%s\n' "$i" > "$FIXDIR/many/f$i.txt"; done
chain="$FIXDIR/chain"; for i in $(seq 1 12); do chain="$chain/d$i"; done; mkdir -p "$chain"; printf 'bottom\n' > "$chain/bottom.txt"
printf 'SECRET\n' > "$OUTSIDE/secret.txt"
ln -sfn "$OUTSIDE/secret.txt" "$FIXDIR/escape.txt"
ln -sfn "$OUTSIDE" "$FIXDIR/escapedir"
ln -sfn a.txt "$FIXDIR/inner.txt"
ln -sfn sub "$FIXDIR/subln"
ln -sfn . "$FIXDIR/loop"
ln -sfn nowhere "$FIXDIR/dangling"
printf 'line one\n    indented line\nline three\n' > "$FIXDIR/edit-ws.txt"
printf 'a\n' > "$FIXDIR/edit-seq.txt"
printf 'x\r\ny\r\n' > "$FIXDIR/edit-crlf.txt"
printf 'code:\n```\nfenced\n```\n' > "$FIXDIR/edit-ticks.txt"
printf 'keep me\n' > "$FIXDIR/mv.txt"
printf 'stay\n' > "$FIXDIR2/other.txt"

# The concurrency test in framework_tests fires this tool 8x in parallel.
FIRST_TOOL_NAME=list_directory
FIRST_TOOL_ARGS='{"path":"/data"}'
FIRST_TOOL_EXPECT='\[FILE\] a.txt'   # grep regex: brackets escaped

mcp_build_if_needed "${1:-}"
mcp_harness_start \
  --dir "$FIXDIR::/data" --dir "$FIXDIR2::/data2" \
  --env FS_ALLOWED_DIRS=/data,/data2,/notes \
  --env FS_MAX_FILE_BYTES=1048576 --env FS_MAX_RESULTS=50
# Guard instance: mounted, but WITHOUT FS_ALLOWED_DIRS — the missing-config path.
mcp_harness_start_guard --dir "$FIXDIR::/data"
# Read-only instance (no HTTP fixture exists for this server; the slot is
# reused so the shared trap cleans it up).
echo "starting read-only instance on :${RO_PORT}..."
"$WASMTIME" serve -Sp3,cli,http --dir "$FIXDIR::/data" \
  --env FS_ALLOWED_DIRS=/data --env FS_READ_ONLY=true \
  --env "MCP_ALLOWED_HOSTS=127.0.0.1:${RO_PORT}" \
  --addr "127.0.0.1:${RO_PORT}" "$WASM" >"$E2E_TMP/ro.log" 2>&1 &
FIXTURE_PID=$!
mcp_wait_ready "$RO_PORT"

framework_tests read_text_file read_media_file read_multiple_files write_file edit_file \
  create_directory list_directory list_directory_with_sizes directory_tree move_file \
  search_files get_file_info list_allowed_directories
discovery_tests list_allowed_directories
skills_tests "$SKILL_NAME" "references/TOOLS.md"

echo "== configuration =="
OUT=$(mcp_call list_allowed_directories '{}')
assert_contains "list_allowed_directories lists both mounts" 'Allowed directories:\n/data\n/data2' "$OUT"
assert_contains "list_allowed_directories flags an unmounted entry" '/notes (NOT MOUNTED' "$OUT"
assert_json "list_allowed_directories structured: /data mounted, /notes not" \
  '[d["mounted"] for d in r["result"]["structuredContent"]["directories"]] == [True, True, False]' "$OUT"
assert_json "list_allowed_directories structured: FS_MAX_RESULTS honoured" \
  'r["result"]["structuredContent"]["maxResults"] == 50' "$OUT"
OUT=$(mcp_call read_text_file '{"path":"/notes/x.txt"}')
assert_contains "read under an unmounted allowed dir is a clean not-found" 'No such file or directory: /notes/x.txt' "$OUT"

echo "== missing config (guard instance) =="
for tool in list_allowed_directories read_text_file list_directory search_files; do
  case "$tool" in
    list_allowed_directories) ARGS='{}' ;;
    search_files) ARGS='{"path":"/data","pattern":"*"}' ;;
    *) ARGS='{"path":"/data/a.txt"}' ;;
  esac
  OUT=$(mcp_call_on "$GUARD_BASE" "$tool" "$ARGS")
  assert_contains "$tool without FS_ALLOWED_DIRS names the variable" 'FS_ALLOWED_DIRS is not set' "$OUT"
  assert_contains "$tool without FS_ALLOWED_DIRS points at volumeMounts" 'volumeMounts' "$OUT"
done
OUT=$(mcp_call_on "$GUARD_BASE" write_file '{"path":"/data/guard.txt","content":"x"}')
assert_contains "write_file without FS_ALLOWED_DIRS is refused with the hint" 'FS_ALLOWED_DIRS is not set' "$OUT"
[ ! -e "$FIXDIR/guard.txt" ] && pass "guard instance wrote nothing" || fail "guard instance wrote nothing" "guard.txt exists"

echo "== read_text_file =="
OUT=$(mcp_call read_text_file '{"path":"/data/a.txt"}')
assert_contains "reads a file" '"text":"hello\n"' "$OUT"
OUT=$(mcp_call read_text_file '{"path":"a.txt"}')
assert_contains "relative path resolves against the first allowed dir" '"text":"hello\n"' "$OUT"
OUT=$(mcp_call read_text_file '{"path":"/data/lines.txt","head":3}')
assert_contains "head=3 returns the first three lines" '"text":"1\n2\n3"' "$OUT"
OUT=$(mcp_call read_text_file '{"path":"/data/lines.txt","tail":3}')
assert_contains "tail=3 returns the last three lines" '"text":"98\n99\n100"' "$OUT"
OUT=$(mcp_call read_text_file '{"path":"/data/lines.txt","tail":500}')
assert_contains "tail larger than the file returns the whole file" '"text":"1\n2\n' "$OUT"
OUT=$(mcp_call read_text_file '{"path":"/data/lines.txt","head":2,"tail":2}')
assert_contains "head+tail together is the reference error" 'Cannot specify both head and tail parameters simultaneously' "$OUT"
OUT=$(mcp_call read_text_file '{"path":"/data/empty.txt"}')
assert_contains "empty file returns empty text, not an error" '"text":""' "$OUT"
assert_contains "empty file is not isError" '"isError":false' "$OUT"
OUT=$(mcp_call read_text_file '{"path":"/data/héllo wörld.txt"}')
assert_contains "unicode file name" 'unicode ok' "$OUT"
OUT=$(mcp_call read_text_file '{"path":"/data/sub/深/文件.txt"}')
assert_contains "unicode directory + file name" '"text":"deep\n"' "$OUT"
OUT=$(mcp_call read_text_file '{"path":"/data/big.txt"}')
assert_contains "over-cap file is truncated with the note" '[truncated: file is 2097152 bytes, cap is 1048576; use head/tail to read a range]' "$OUT"
assert_json "over-cap file returns exactly the cap" 'len(r["result"]["content"][0]["text"].split("\n[truncated")[0]) == 1048576' "$OUT"
OUT=$(mcp_call read_text_file '{"path":"/data/bad.bin"}')
assert_contains "invalid UTF-8 is decoded lossily with a note" '[note: file is not valid UTF-8' "$OUT"
OUT=$(mcp_call read_text_file '{"path":"/data/nope.txt"}')
assert_contains "missing file is a clean not-found" 'No such file or directory: /data/nope.txt' "$OUT"
assert_contains "missing file is isError" '"isError":true' "$OUT"
OUT=$(mcp_call read_text_file '{"path":"/data/sub"}')
assert_contains "reading a directory is a tool error" '"isError":true' "$OUT"
OUT=$(mcp_call read_text_file '{"path":"/data/../etc/passwd"}')
assert_contains "dot-dot traversal is denied with the reference message" 'Access denied - path outside allowed directories: /etc/passwd not in /data, /data2, /notes' "$OUT"
OUT=$(mcp_call read_text_file '{"path":"/etc/passwd"}')
assert_contains "absolute path outside the mounts is denied" 'Access denied - path outside allowed directories: /etc/passwd' "$OUT"
OUT=$(mcp_call read_text_file "{\"path\":\"/data/../..${OUTSIDE}/secret.txt\"}")
assert_contains "traversal to the outside dir is denied" 'Access denied - path outside allowed directories' "$OUT"
assert_not_contains "traversal never leaks the secret" 'SECRET' "$OUT"
OUT=$(mcp_call read_text_file '{"path":"/data2/../data/a.txt"}')
assert_contains "dot-dot that stays inside is allowed" '"text":"hello\n"' "$OUT"
OUT=$(mcp_call read_text_file '{"path":"/data/escape.txt"}')
assert_contains "symlink escaping the mount is denied" 'Access denied - symlink target outside allowed directories' "$OUT"
assert_not_contains "symlink escape never leaks the secret" 'SECRET' "$OUT"
OUT=$(mcp_call read_text_file '{"path":"/data/escapedir/secret.txt"}')
assert_contains "directory symlink escaping the mount is denied" 'Access denied - symlink target outside allowed directories' "$OUT"
assert_not_contains "directory symlink escape never leaks the secret" 'SECRET' "$OUT"
OUT=$(mcp_call read_text_file '{"path":"/data/inner.txt"}')
assert_contains "symlink inside the mount is followed" '"text":"hello\n"' "$OUT"
OUT=$(mcp_call read_text_file '{"path":"/data/subln/x.txt"}')
assert_contains "directory symlink inside the mount is followed" '"text":"in sub\n"' "$OUT"
OUT=$(mcp_call read_text_file '{"path":"/data/dangling"}')
assert_contains "dangling symlink is a clean not-found" 'No such file or directory' "$OUT"
OUT=$(mcp_call read_text_file '{"path":"/data/a.txt\u0000.png"}')
assert_contains "NUL byte in a path is refused" 'NUL byte' "$OUT"
OUT=$(mcp_call read_text_file '{"path":"C:\\Windows\\system.ini"}')
assert_contains "Windows drive path is refused" 'Access denied - Windows-style path received on a POSIX host' "$OUT"
OUT=$(mcp_call read_text_file '{"path":"/data/a.txt","head":0}')
assert_contains "head=0 is refused" 'head must be a positive integer' "$OUT"
OUT=$(mcp_call read_text_file '{"path":"/data/a.txt","head":-1}')
assert_rejected "head=-1 is refused" "$OUT"
OUT=$(mcp_call read_text_file '{"path":"/data/a.txt","head":"x"}')
assert_rejected "head='x' is refused" "$OUT"
OUT=$(mcp_call read_text_file '{}')
assert_rejected "missing path is refused" "$OUT"
OUT=$(mcp_call read_text_file '{"path":""}')
assert_contains "empty path is refused" 'path must not be empty' "$OUT"
OUT=$(mcp_call read_text_file "{\"path\":\"/data/$(printf 'x%.0s' $(seq 1 5000))\"}")
assert_contains "5000-byte path is refused" 'path is too long' "$OUT"
OUT=$(mcp_call read_text_file '{"path":"/data/a.txt; rm -rf / #"}')
assert_contains "injection-shaped path is just a missing file" 'No such file or directory' "$OUT"

echo "== read_media_file =="
OUT=$(mcp_call read_media_file '{"path":"/data/tiny.png"}')
assert_contains "png comes back as an image block" '"type":"image"' "$OUT"
assert_contains "png mime type" '"mimeType":"image/png"' "$OUT"
assert_contains "png base64 equals the seeded bytes" "\"data\":\"$PNG_B64\"" "$OUT"
assert_json "png structured content names the uri and size" 'r["result"]["structuredContent"]["uri"] == "file:///data/tiny.png" and r["result"]["structuredContent"]["size"] == 70' "$OUT"
OUT=$(mcp_call read_media_file '{"path":"/data/tiny.wav"}')
assert_contains "wav comes back as an audio block" '"type":"audio"' "$OUT"
assert_contains "wav mime type" '"mimeType":"audio/wav"' "$OUT"
OUT=$(mcp_call read_media_file '{"path":"/data/a.txt"}')
assert_contains "other files come back as an embedded resource" '"type":"resource"' "$OUT"
assert_contains "embedded resource carries the blob" '"blob":"aGVsbG8K"' "$OUT"
assert_contains "embedded resource mime type" 'application/octet-stream' "$OUT"
assert_contains "embedded resource file:// uri" '"uri":"file:///data/a.txt"' "$OUT"
OUT=$(mcp_call read_media_file '{"path":"/data/big.bin"}')
assert_contains "over-cap media is refused naming the cap" 'File is 1048577 bytes; read_media_file cap is FS_MAX_FILE_BYTES=1048576' "$OUT"
OUT=$(mcp_call read_media_file '{"path":"/data/escape.txt"}')
assert_contains "media through an escaping symlink is denied" 'Access denied - symlink target outside allowed directories' "$OUT"
OUT=$(mcp_call read_media_file '{"path":"/data/sub"}')
assert_contains "media on a directory is refused" 'is a directory, not a file' "$OUT"
OUT=$(mcp_call read_media_file '{"path":123}')
assert_rejected "non-string path is refused" "$OUT"

echo "== read_multiple_files =="
OUT=$(mcp_call read_multiple_files '{"paths":["/data/a.txt","/data/nope.txt","/data/sub/x.txt"]}')
assert_contains "batch keeps going past a failure" '/data/nope.txt: Error - ' "$OUT"
assert_contains "batch separates entries with ---" '\n---\n' "$OUT"
assert_contains "batch includes good files" '/data/a.txt:\nhello\n' "$OUT"
assert_contains "batch with one failure is not isError" '"isError":false' "$OUT"
OUT=$(mcp_call read_multiple_files '{"paths":["/data/nope.txt","/data/escape.txt"]}')
assert_contains "batch where every file fails is isError" '"isError":true' "$OUT"
assert_contains "batch reports the escaping symlink per file" 'Access denied - symlink target outside allowed directories' "$OUT"
OUT=$(mcp_call read_multiple_files '{"paths":[]}')
assert_contains "empty batch is refused" 'At least one file path must be provided' "$OUT"
OUT=$(mcp_call read_multiple_files "$(python3 -c 'import json; print(json.dumps({"paths": ["/data/a.txt"] * 101}))')")
assert_contains "batch is clamped to 100 paths" '[truncated: 101 paths requested, only the first 100 were read]' "$OUT"
OUT=$(mcp_call read_multiple_files '{"paths":"/data/a.txt"}')
assert_rejected "paths must be an array" "$OUT"

echo "== write_file =="
OUT=$(mcp_call write_file '{"path":"/data/new.txt","content":"fresh\n"}')
assert_contains "writes a new file" 'Successfully wrote to /data/new.txt' "$OUT"
[ "$(cat "$FIXDIR/new.txt")" = "fresh" ] && pass "new file content lands on the host" || fail "new file content lands on the host" "$(cat "$FIXDIR/new.txt" 2>&1)"
OUT=$(mcp_call write_file '{"path":"/data/new.txt","content":"overwritten\n"}')
assert_contains "overwrites an existing file" 'Successfully wrote to /data/new.txt' "$OUT"
[ "$(cat "$FIXDIR/new.txt")" = "overwritten" ] && pass "overwrite is visible on the host" || fail "overwrite is visible on the host" "$(cat "$FIXDIR/new.txt")"
[ -z "$(ls -A "$FIXDIR" | grep '\.tmp$')" ] && pass "no temp file left behind" || fail "no temp file left behind" "$(ls -A "$FIXDIR" | grep '\.tmp$')"
OUT=$(mcp_call write_file '{"path":"/data/nope/child.txt","content":"x"}')
assert_contains "missing parent is the reference error" 'Parent directory does not exist: /data/nope' "$OUT"
BIG=$(python3 -c 'import json; print(json.dumps({"path": "/data/unicode-100k.txt", "content": ("héllo ☃ wörld 文件\n" * 4300)}))')
OUT=$(printf '%s' "$BIG" | { read -r body; mcp_call write_file "$body"; })
assert_contains "100 KiB unicode content is written" 'Successfully wrote to /data/unicode-100k.txt' "$OUT"
python3 -c 'import sys; sys.exit(0 if open(sys.argv[1], encoding="utf-8").read() == ("héllo ☃ wörld 文件\n" * 4300) else 1)' "$FIXDIR/unicode-100k.txt" \
  && pass "unicode content round-trips byte-exact" || fail "unicode content round-trips byte-exact" "mismatch"
OUT=$(mcp_call write_file '{"path":"/data/escape.txt","content":"pwned\n"}')
assert_contains "write through an escaping symlink is denied" 'Access denied - symlink target outside allowed directories' "$OUT"
[ "$(cat "$OUTSIDE/secret.txt")" = "SECRET" ] && pass "outside secret untouched after symlink write attempt" || fail "outside secret untouched after symlink write attempt" "$(cat "$OUTSIDE/secret.txt")"
OUT=$(mcp_call write_file '{"path":"/data/escapedir/new.txt","content":"pwned\n"}')
assert_contains "write under an escaping directory symlink is denied" 'Access denied - symlink target outside allowed directories' "$OUT"
[ ! -e "$OUTSIDE/new.txt" ] && pass "nothing written outside via directory symlink" || fail "nothing written outside via directory symlink" "file exists"
OUT=$(mcp_call write_file '{"path":"/data/../evil.txt","content":"pwned\n"}')
assert_contains "traversal write is denied" 'Access denied - path outside allowed directories: /evil.txt' "$OUT"
[ ! -e "$E2E_TMP/evil.txt" ] && pass "traversal write created nothing" || fail "traversal write created nothing" "evil.txt exists"
OUT=$(mcp_call write_file '{"path":"/data/sub","content":"x"}')
assert_contains "writing onto a directory is refused" 'is a directory, not a file' "$OUT"
OUT=$(mcp_call write_file '{"path":"/data/inner.txt","content":"via link\n"}')
assert_contains "write through an inside symlink succeeds" 'Successfully wrote to /data/inner.txt' "$OUT"
[ "$(cat "$FIXDIR/a.txt")" = "via link" ] && pass "inside symlink write lands on its target" || fail "inside symlink write lands on its target" "$(cat "$FIXDIR/a.txt")"
printf 'hello\n' > "$FIXDIR/a.txt"
OUT=$(mcp_call write_file '{"path":"/data/x.txt"}')
assert_rejected "write_file without content is refused" "$OUT"

echo "== edit_file =="
OUT=$(mcp_call edit_file '{"path":"/data/new.txt","edits":[{"oldText":"overwritten","newText":"edited"}],"dryRun":true}')
assert_contains "dryRun returns a fenced diff" '```diff\n' "$OUT"
assert_contains "dryRun diff shows the removal" '\n-overwritten\n' "$OUT"
assert_contains "dryRun diff shows the addition" '\n+edited\n' "$OUT"
[ "$(cat "$FIXDIR/new.txt")" = "overwritten" ] && pass "dryRun leaves the file untouched" || fail "dryRun leaves the file untouched" "$(cat "$FIXDIR/new.txt")"
OUT=$(mcp_call edit_file '{"path":"/data/new.txt","edits":[{"oldText":"overwritten","newText":"edited"}]}')
assert_contains "apply returns the diff" '+edited' "$OUT"
[ "$(cat "$FIXDIR/new.txt")" = "edited" ] && pass "apply changes the file on the host" || fail "apply changes the file on the host" "$(cat "$FIXDIR/new.txt")"
OUT=$(mcp_call edit_file '{"path":"/data/new.txt","edits":[{"oldText":"does not exist","newText":"x"}]}')
assert_contains "unmatched edit is the reference error" 'Could not find exact match for edit:\ndoes not exist' "$OUT"
[ "$(cat "$FIXDIR/new.txt")" = "edited" ] && pass "unmatched edit writes nothing" || fail "unmatched edit writes nothing" "$(cat "$FIXDIR/new.txt")"
OUT=$(mcp_call edit_file '{"path":"/data/new.txt","edits":[{"oldText":"edited","newText":"ok"},{"oldText":"does not exist","newText":"x"}]}')
assert_contains "one bad edit fails the whole call" 'Could not find exact match' "$OUT"
[ "$(cat "$FIXDIR/new.txt")" = "edited" ] && pass "all-or-nothing: first edit not applied either" || fail "all-or-nothing: first edit not applied either" "$(cat "$FIXDIR/new.txt")"
OUT=$(mcp_call edit_file '{"path":"/data/edit-ws.txt","edits":[{"oldText":"  indented line  ","newText":"replaced line"}]}')
assert_contains "whitespace-tolerant line match applies" '+    replaced line' "$OUT"
[ "$(sed -n 2p "$FIXDIR/edit-ws.txt")" = "    replaced line" ] && pass "fallback match preserves the original indentation" || fail "fallback match preserves the original indentation" "$(sed -n 2p "$FIXDIR/edit-ws.txt")"
OUT=$(mcp_call edit_file '{"path":"/data/edit-seq.txt","edits":[{"oldText":"a","newText":"b"},{"oldText":"b","newText":"c"}]}')
assert_contains "sequential edits see the previous edit" '+c' "$OUT"
[ "$(cat "$FIXDIR/edit-seq.txt")" = "c" ] && pass "sequential edits end state" || fail "sequential edits end state" "$(cat "$FIXDIR/edit-seq.txt")"
OUT=$(mcp_call edit_file '{"path":"/data/edit-crlf.txt","edits":[{"oldText":"x\ny","newText":"z"}]}')
assert_contains "CRLF file is matched after normalisation" '+z' "$OUT"
OUT=$(mcp_call edit_file '{"path":"/data/edit-ticks.txt","edits":[{"oldText":"fenced","newText":"still fenced"}],"dryRun":true}')
assert_contains "fence grows past backticks inside the diff" '````diff\n' "$OUT"
OUT=$(mcp_call edit_file '{"path":"/data/new.txt","edits":[]}')
assert_contains "empty edits array is refused" 'edits must contain at least one' "$OUT"
OUT=$(mcp_call edit_file '{"path":"/data/new.txt","edits":[{"oldText":"","newText":"x"}]}')
assert_contains "empty oldText is refused" 'oldText must not be empty' "$OUT"
OUT=$(mcp_call edit_file '{"path":"/data/nope.txt","edits":[{"oldText":"a","newText":"b"}]}')
assert_contains "editing a missing file is a clean not-found" 'No such file or directory' "$OUT"
OUT=$(mcp_call edit_file '{"path":"/data/sub","edits":[{"oldText":"a","newText":"b"}]}')
assert_contains "editing a directory is refused" 'is a directory, not a file' "$OUT"
OUT=$(mcp_call edit_file '{"path":"/data/big.txt","edits":[{"oldText":"a","newText":"b"}],"dryRun":true}')
assert_contains "editing an over-cap file is refused naming the cap" 'edit_file cap is FS_MAX_FILE_BYTES=1048576' "$OUT"
OUT=$(mcp_call edit_file '{"path":"/data/bad.bin","edits":[{"oldText":"a","newText":"b"}],"dryRun":true}')
assert_contains "editing a non-UTF-8 file is refused" 'is not valid UTF-8 text' "$OUT"
OUT=$(mcp_call edit_file '{"path":"/data/new.txt","edits":[{"oldText":"a"}]}')
assert_rejected "edit without newText is refused" "$OUT"
OUT=$(mcp_call edit_file "$(python3 -c 'import json; print(json.dumps({"path": "/data/new.txt", "dryRun": True, "edits": [{"oldText": "zzz", "newText": "y"}] * 201}))')")
assert_contains "more than 200 edits is refused" 'edit_file accepts at most 200 edits per call' "$OUT"

echo "== create_directory =="
OUT=$(mcp_call create_directory '{"path":"/data/made/deep/er"}')
assert_contains "creates nested directories" 'Successfully created directory /data/made/deep/er' "$OUT"
[ -d "$FIXDIR/made/deep/er" ] && pass "nested directories exist on the host" || fail "nested directories exist on the host" "missing"
OUT=$(mcp_call create_directory '{"path":"/data/made/deep/er"}')
assert_contains "create_directory is idempotent" 'Successfully created directory /data/made/deep/er' "$OUT"
OUT=$(mcp_call create_directory '{"path":"/data/a.txt"}')
assert_contains "create_directory over a file is refused" 'already exists and is not a directory' "$OUT"
OUT=$(mcp_call create_directory '{"path":"/data/../outside-dir"}')
assert_contains "create_directory outside is denied" 'Access denied - path outside allowed directories' "$OUT"
[ ! -e "$E2E_TMP/outside-dir" ] && pass "denied create_directory made nothing" || fail "denied create_directory made nothing" "exists"
OUT=$(mcp_call create_directory '{"path":"/data/escapedir/sub"}')
assert_contains "create_directory under an escaping symlink is denied" 'Access denied - symlink target outside allowed directories' "$OUT"
OUT=$(mcp_call create_directory '{"path":"/notes/x"}')
assert_contains "create_directory under an unmounted allowed dir fails cleanly" 'Parent directory does not exist' "$OUT"

echo "== list_directory =="
OUT=$(mcp_call list_directory '{"path":"/data"}')
assert_contains "lists files with the [FILE] prefix" '[FILE] a.txt' "$OUT"
assert_contains "lists directories with the [DIR] prefix" '[DIR] sub' "$OUT"
assert_contains "symlink to a directory is reported by its target type" '[DIR] subln' "$OUT"
assert_contains "escaping symlink is listed as a file (not followed)" '[FILE] escape.txt' "$OUT"
assert_json "structured entries carry isSymlink" 'any(e["name"] == "inner.txt" and e["isSymlink"] for e in r["result"]["structuredContent"]["entries"])' "$OUT"
OUT=$(mcp_call list_directory '{"path":"/data/many"}')
assert_contains "listing is capped with the FS_MAX_RESULTS note" '[truncated to FS_MAX_RESULTS=50 entries]' "$OUT"
assert_json "capped listing returns exactly FS_MAX_RESULTS entries" 'len(r["result"]["structuredContent"]["entries"]) == 50 and r["result"]["structuredContent"]["truncated"]' "$OUT"
OUT=$(mcp_call list_directory '{"path":"/data/empty"}')
assert_contains "empty directory returns empty text" '"text":""' "$OUT"
assert_contains "empty directory is not an error" '"isError":false' "$OUT"
OUT=$(mcp_call list_directory '{"path":"/data/a.txt"}')
assert_contains "listing a file is a tool error" '"isError":true' "$OUT"
OUT=$(mcp_call list_directory '{"path":"/data/escapedir"}')
assert_contains "listing an escaping directory symlink is denied" 'Access denied - symlink target outside allowed directories' "$OUT"
OUT=$(mcp_call list_directory '{"path":"/"}')
assert_contains "listing / is denied (not an allowed dir)" 'Access denied - path outside allowed directories: / not in' "$OUT"

echo "== list_directory_with_sizes =="
OUT=$(mcp_call list_directory_with_sizes '{"path":"/data"}')
assert_contains "sizes listing has the summary line" 'Total: ' "$OUT"
assert_contains "sizes listing has the combined size" 'Combined size: ' "$OUT"
assert_contains "sizes listing pads names and right-aligns sizes" '[FILE] a.txt                                 6 B' "$OUT"
OUT=$(mcp_call list_directory_with_sizes '{"path":"/data","sortBy":"size"}')
assert_json "sortBy=size puts the largest file first" 'r["result"]["content"][0]["text"].startswith("[FILE] big.txt")' "$OUT"
assert_contains "sizes use the reference units" '2.00 MB' "$OUT"
OUT=$(mcp_call list_directory_with_sizes '{"path":"/data","sortBy":"colour"}')
assert_rejected "unknown sortBy is refused" "$OUT"
OUT=$(mcp_call list_directory_with_sizes '{"path":"/data/empty"}')
assert_contains "empty directory summary" 'Total: 0 files, 0 directories\nCombined size: 0 B' "$OUT"

echo "== directory_tree =="
OUT=$(mcp_call directory_tree '{"path":"/data/notes"}')
assert_json "tree text is valid JSON" 'isinstance(__import__("json").loads(r["result"]["content"][0]["text"]), list)' "$OUT"
TREE=$(tree_text "$OUT")
assert_contains "tree nodes carry name" '"name": "one.md"' "$TREE"
assert_contains "tree nodes carry type" '"type": "file"' "$TREE"
assert_json "tree node key order is name, type (children)" 'list(__import__("json").loads(r["result"]["content"][0]["text"])[0].keys()) == ["name", "type"]' "$OUT"
# `many` (60 files) alone would hit FS_MAX_RESULTS=50 before node_modules is
# reached, so it is excluded here; the cap itself is tested below.
OUT=$(mcp_call directory_tree '{"path":"/data","maxDepth":2,"excludePatterns":["many"]}')
TREE=$(tree_text "$OUT")
assert_contains "tree lists node_modules when not excluded" '"name": "node_modules"' "$TREE"
assert_contains "tree reaches depth 2" '"name": "d1"' "$TREE"
assert_not_contains "tree does not descend past maxDepth=2" '"name": "d2"' "$TREE"
assert_contains "symlink loop terminates as an empty directory" '"name": "loop"' "$TREE"
assert_json "loop node is a directory with empty children" 'any(n["name"] == "loop" and n["type"] == "directory" and n["children"] == [] for n in __import__("json").loads(r["result"]["content"][0]["text"]))' "$OUT"
assert_json "escaping symlink is listed as a file node" 'any(n["name"] == "escape.txt" and n["type"] == "file" and "children" not in n for n in __import__("json").loads(r["result"]["content"][0]["text"]))' "$OUT"
assert_json "directories always carry children, files never" 'all(("children" in n) == (n["type"] == "directory") for n in __import__("json").loads(r["result"]["content"][0]["text"]))' "$OUT"
OUT=$(mcp_call directory_tree '{"path":"/data","maxDepth":2,"excludePatterns":["node_modules","many"]}')
TREE=$(tree_text "$OUT")
assert_not_contains "bare-name exclude drops node_modules" '"name": "node_modules"' "$TREE"
assert_not_contains "bare-name exclude drops many" '"name": "many"' "$TREE"
assert_contains "excluded tree still lists the rest" '"name": "notes"' "$TREE"
OUT=$(mcp_call directory_tree '{"path":"/data","maxDepth":2,"excludePatterns":["**/*.md"]}')
TREE=$(tree_text "$OUT")
assert_not_contains "glob exclude drops nested markdown" '"name": "one.md"' "$TREE"
assert_contains "glob exclude keeps other files" '"name": "a.txt"' "$TREE"
OUT=$(mcp_call directory_tree '{"path":"/data/chain","maxDepth":999}')
TREE=$(tree_text "$OUT")
assert_contains "maxDepth above FS_MAX_TREE_DEPTH is clamped (d10 present)" '"name": "d10"' "$TREE"
assert_not_contains "maxDepth above FS_MAX_TREE_DEPTH is clamped (d11 absent)" '"name": "d11"' "$TREE"
OUT=$(mcp_call directory_tree '{"path":"/data/many"}')
TREE=$(tree_text "$OUT")
assert_contains "tree is capped with a marker node" '"name": "…"' "$TREE"
assert_contains "tree cap note is a second text block" 'truncated to FS_MAX_RESULTS=50 entries' "$OUT"
assert_json "capped tree structured flag" 'r["result"]["structuredContent"]["truncated"] is True' "$OUT"
OUT=$(mcp_call directory_tree '{"path":"/data/a.txt"}')
assert_contains "tree on a file is refused" 'is not a directory' "$OUT"
OUT=$(mcp_call directory_tree '{"path":"/data","excludePatterns":["["]}')
assert_contains "invalid exclude glob is a clean error" 'Invalid glob pattern' "$OUT"
OUT=$(mcp_call directory_tree '{"path":"/data","maxDepth":"deep"}')
assert_rejected "non-integer maxDepth is refused" "$OUT"
OUT=$(mcp_call directory_tree '{"path":"/data/escapedir"}')
assert_contains "tree on an escaping symlink is denied" 'Access denied - symlink target outside allowed directories' "$OUT"

echo "== move_file =="
OUT=$(mcp_call move_file '{"source":"/data/mv.txt","destination":"/data/mv2.txt"}')
assert_contains "renames within a directory" 'Successfully moved /data/mv.txt to /data/mv2.txt' "$OUT"
[ ! -e "$FIXDIR/mv.txt" ] && [ "$(cat "$FIXDIR/mv2.txt")" = "keep me" ] && pass "rename visible on the host" || fail "rename visible on the host" "$(ls "$FIXDIR")"
OUT=$(mcp_call move_file '{"source":"/data/mv2.txt","destination":"/data2/mv3.txt"}')
assert_contains "moves across allowed mounts" 'Successfully moved /data/mv2.txt to /data2/mv3.txt' "$OUT"
[ "$(cat "$FIXDIR2/mv3.txt")" = "keep me" ] && pass "cross-mount move visible on the host" || fail "cross-mount move visible on the host" "$(ls "$FIXDIR2")"
OUT=$(mcp_call move_file '{"source":"/data2/mv3.txt","destination":"/data/a.txt"}')
assert_contains "existing destination is refused" 'Destination already exists: /data/a.txt' "$OUT"
[ "$(cat "$FIXDIR/a.txt")" = "hello" ] && pass "refused move did not overwrite" || fail "refused move did not overwrite" "$(cat "$FIXDIR/a.txt")"
OUT=$(mcp_call move_file '{"source":"/data2/mv3.txt","destination":"/data/inner.txt"}')
assert_contains "symlink at the destination counts as existing" 'Destination already exists: /data/inner.txt' "$OUT"
OUT=$(mcp_call move_file '{"source":"/data2/mv3.txt","destination":"/data/../stolen.txt"}')
assert_contains "destination outside is denied" 'Access denied - path outside allowed directories' "$OUT"
OUT=$(mcp_call move_file '{"source":"/data2/mv3.txt","destination":"/data/escapedir/stolen.txt"}')
assert_contains "destination under an escaping symlink is denied" 'Access denied - symlink target outside allowed directories' "$OUT"
[ ! -e "$OUTSIDE/stolen.txt" ] && pass "nothing moved outside" || fail "nothing moved outside" "exists"
OUT=$(mcp_call move_file '{"source":"/data/nope.txt","destination":"/data/x.txt"}')
assert_contains "missing source is a clean not-found" 'No such file or directory: /data/nope.txt' "$OUT"
OUT=$(mcp_call move_file '{"source":"/data2/mv3.txt","destination":"/data/nope/x.txt"}')
assert_contains "destination parent must exist" 'Parent directory does not exist: /data/nope' "$OUT"
OUT=$(mcp_call move_file '{"source":"/data/made","destination":"/data/made/deep/inside"}')
assert_contains "moving a directory into itself is refused" 'Cannot move /data/made into itself' "$OUT"
OUT=$(mcp_call move_file '{"source":"/data/made","destination":"/data2/made-moved"}')
assert_contains "moves a directory" 'Successfully moved /data/made to /data2/made-moved' "$OUT"
[ -d "$FIXDIR2/made-moved/deep/er" ] && pass "directory move visible on the host" || fail "directory move visible on the host" "$(ls -R "$FIXDIR2")"
OUT=$(mcp_call move_file '{"source":"/data/a.txt"}')
assert_rejected "move_file without destination is refused" "$OUT"

echo "== search_files =="
OUT=$(mcp_call search_files '{"path":"/data","pattern":"**/*.md","excludePatterns":["**/node_modules/**"]}')
assert_contains "recursive glob finds nested markdown" '/data/notes/one.md' "$OUT"
assert_contains "recursive glob finds top-level markdown too" '/data/top.md' "$OUT"
assert_not_contains "excludePatterns drop node_modules" 'node_modules' "$OUT"
OUT=$(mcp_call search_files '{"path":"/data","pattern":"**/*.md","excludePatterns":["node_modules"]}')
assert_not_contains "bare-name exclude drops node_modules" 'vendored.md' "$OUT"
OUT=$(mcp_call search_files '{"path":"/data","pattern":"*.md"}')
assert_contains "top-level glob matches the top level" '/data/top.md' "$OUT"
assert_not_contains "top-level glob does not recurse" '/data/notes/one.md' "$OUT"
OUT=$(mcp_call search_files '{"path":"/data","pattern":"README.md"}')
assert_contains "bare file name is found at any depth" '/data/notes/README.md' "$OUT"
OUT=$(mcp_call search_files '{"path":"/data","pattern":"*.zzz"}')
assert_contains "zero hits is the reference text" '"text":"No matches found"' "$OUT"
assert_contains "zero hits is not an error" '"isError":false' "$OUT"
assert_json "zero hits structured matches is empty" 'r["result"]["structuredContent"]["matches"] == []' "$OUT"
OUT=$(mcp_call search_files '{"path":"/data/many","pattern":"*"}')
assert_contains "search is capped with the FS_MAX_RESULTS note" '[truncated to FS_MAX_RESULTS=50 entries]' "$OUT"
OUT=$(mcp_call search_files '{"path":"/data","pattern":"["}')
assert_contains "invalid glob is a clean error" 'Invalid glob pattern' "$OUT"
OUT=$(mcp_call search_files "{\"path\":\"/data\",\"pattern\":\"'; rm -rf / #\"}")
assert_contains "injection-shaped pattern just finds nothing" 'No matches found' "$OUT"
OUT=$(mcp_call search_files '{"path":"/data","pattern":"**/*","maxDepth":1}')
assert_not_contains "escaping symlinks are skipped by search" 'escape.txt' "$OUT"
assert_contains "search results are absolute guest paths" '/data/a.txt' "$OUT"
OUT=$(mcp_call search_files '{"path":"/data","pattern":"深"}')
assert_contains "unicode bare-name search" '/data/sub/深' "$OUT"
OUT=$(mcp_call search_files '{"path":"/data","pattern":""}')
assert_contains "empty pattern is refused" 'pattern must not be empty' "$OUT"
OUT=$(mcp_call search_files '{"path":"/data/../etc","pattern":"passwd"}')
assert_contains "search outside is denied" 'Access denied - path outside allowed directories' "$OUT"

echo "== get_file_info =="
OUT=$(mcp_call get_file_info '{"path":"/data/a.txt"}')
assert_contains "info reports size" 'size: 6\n' "$OUT"
assert_contains "info reports isFile" 'isFile: true' "$OUT"
assert_contains "info reports isSymlink false" 'isSymlink: false' "$OUT"
assert_contains "info renders permissions as n/a (WASI has no mode bits)" 'permissions: n/a' "$OUT"
assert_json "info structured size" 'r["result"]["structuredContent"]["size"] == 6 and r["result"]["structuredContent"]["modified"].endswith("Z")' "$OUT"
OUT=$(mcp_call get_file_info '{"path":"/data/inner.txt"}')
assert_contains "info reports isSymlink true for a link" 'isSymlink: true' "$OUT"
OUT=$(mcp_call get_file_info '{"path":"/data/sub"}')
assert_contains "info reports isDirectory for a directory" 'isDirectory: true' "$OUT"
OUT=$(mcp_call get_file_info '{"path":"/data/nope"}')
assert_contains "info on a missing path is a clean not-found" 'No such file or directory' "$OUT"
OUT=$(mcp_call get_file_info '{"path":"/data/escape.txt"}')
assert_contains "info through an escaping symlink is denied" 'Access denied - symlink target outside allowed directories' "$OUT"

echo "== read-only instance (FS_READ_ONLY=true) =="
OUT=$(mcp_call_on "$RO_BASE" write_file '{"path":"/data/ro.txt","content":"x"}')
assert_contains "read-only refuses write_file" 'This server is read-only (FS_READ_ONLY=true); write_file is disabled.' "$OUT"
[ ! -e "$FIXDIR/ro.txt" ] && pass "read-only wrote nothing" || fail "read-only wrote nothing" "exists"
OUT=$(mcp_call_on "$RO_BASE" edit_file '{"path":"/data/new.txt","edits":[{"oldText":"edited","newText":"nope"}]}')
assert_contains "read-only refuses edit_file" 'This server is read-only (FS_READ_ONLY=true); edit_file is disabled.' "$OUT"
OUT=$(mcp_call_on "$RO_BASE" edit_file '{"path":"/data/new.txt","edits":[{"oldText":"edited","newText":"preview"}],"dryRun":true}')
assert_contains "read-only still allows edit_file dryRun" '+preview' "$OUT"
[ "$(cat "$FIXDIR/new.txt")" = "edited" ] && pass "read-only dryRun changed nothing" || fail "read-only dryRun changed nothing" "$(cat "$FIXDIR/new.txt")"
OUT=$(mcp_call_on "$RO_BASE" create_directory '{"path":"/data/ro-dir"}')
assert_contains "read-only refuses create_directory" 'This server is read-only (FS_READ_ONLY=true); create_directory is disabled.' "$OUT"
OUT=$(mcp_call_on "$RO_BASE" move_file '{"source":"/data/a.txt","destination":"/data/b.txt"}')
assert_contains "read-only refuses move_file" 'This server is read-only (FS_READ_ONLY=true); move_file is disabled.' "$OUT"
OUT=$(mcp_call_on "$RO_BASE" read_text_file '{"path":"/data/a.txt"}')
assert_contains "read-only still reads" '"text":"hello\n"' "$OUT"
OUT=$(mcp_call_on "$RO_BASE" list_allowed_directories '{}')
assert_contains "read-only is announced by list_allowed_directories" '(read-only: FS_READ_ONLY=true)' "$OUT"
assert_json "read-only structured flag" 'r["result"]["structuredContent"]["readOnly"] is True' "$OUT"

if [ "${E2E_LIVE:-0}" = "1" ]; then
  echo "== live: Cosmonic Desktop deployment =="
  LIVE_BASE="${LIVE_BASE:-http://official-filesystem-mcp.localhost:8200/}"
  LIVE_HOST_DIR="${LIVE_HOST_DIR:-$HOME/.local/share/cosmonic/volumes/official-filesystem-mcp}"
  OUT=$(curl -sS --max-time 20 "$LIVE_BASE")
  assert_contains "live GET / is ok" '"status": "ok"' "$OUT"
  OUT=$(mcp_call_on "$LIVE_BASE" list_allowed_directories '{}')
  assert_contains "live list_allowed_directories shows /data mounted" 'Allowed directories:\n/data' "$OUT"
  assert_not_contains "live /data is mounted" 'NOT MOUNTED' "$OUT"
  OUT=$(mcp_call_on "$LIVE_BASE" list_directory '{"path":"/data"}')
  assert_contains "live list_directory sees README.txt" '[FILE] README.txt' "$OUT"
  STAMP="e2e-live-$$"
  OUT=$(mcp_call_on "$LIVE_BASE" write_file "{\"path\":\"/data/$STAMP.txt\",\"content\":\"$STAMP\\n\"}")
  assert_contains "live write_file succeeds" "Successfully wrote to /data/$STAMP.txt" "$OUT"
  [ "$(cat "$LIVE_HOST_DIR/$STAMP.txt" 2>/dev/null)" = "$STAMP" ] && pass "live write lands in the host folder" || fail "live write lands in the host folder" "$(ls "$LIVE_HOST_DIR" 2>&1)"
  OUT=$(mcp_call_on "$LIVE_BASE" read_text_file "{\"path\":\"/data/$STAMP.txt\"}")
  assert_contains "live read_text_file reads it back" "$STAMP" "$OUT"
  OUT=$(mcp_call_on "$LIVE_BASE" read_text_file '{"path":"/data/../etc/passwd"}')
  assert_contains "live traversal is denied" 'Access denied - path outside allowed directories' "$OUT"
  rm -f "$LIVE_HOST_DIR/$STAMP.txt"
fi

guard_tests
mcp_harness_report
