# official-filesystem-mcp

A Rust/WebAssembly port of the official
[`@modelcontextprotocol/server-filesystem`](https://github.com/modelcontextprotocol/servers/tree/main/src/filesystem)
reference server for [Cosmonic Desktop](https://cosmonic.com/docs/desktop):
the same thirteen tools, argument names, output formats and error strings —
but running as a sandboxed component whose only filesystem is the host folder
the workload manifest mounts into it. No network, no credentials, no
subprocesses: `allowedHosts: []`.

Built from [mcp-server-template-rs](https://github.com/cosmonic-labs/mcp-server-template-rs):
rmcp 3.x, MCP spec 2026-07-28, exports `wasi:http/handler@0.3.0`, serves a
discovery document on `GET /` and `GET /health`, and publishes its playbook
as a skill at `skill://official-filesystem-mcp/SKILL.md`.

Reachable on Cosmonic Desktop at <http://official-filesystem-mcp.localhost:8200/>.

## How it works

```
 MCP client ──HTTP──▶ Desktop ingress ──▶ official-filesystem-mcp (Wasm)
                                             │ std::fs on the WASI preopen /data
                                             ▼
                      spec.volumes hostPath: ~/.local/share/cosmonic/volumes/official-filesystem-mcp
```

The tools see **guest** paths (`/data/notes.md`), never host paths. Inside the
component `/data` is the WASI preopen the manifest's `volumeMounts` creates; on
the host it is the `hostPath` folder, and files the tools write appear there
immediately (same user). Everything else on the machine is invisible: the
runtime refuses `..` climbs and symlinks that leave the mount (errno 63,
"Operation not permitted"), and the server turns those into the reference
server's `Access denied - …` messages before they happen.

## Tools

| Tool | Parameters | Output | Gated by `FS_READ_ONLY` |
|---|---|---|---|
| `list_allowed_directories` | — | `Allowed directories:` + one guest path per line (unmounted ones flagged `NOT MOUNTED`); structured `{directories, readOnly, maxFileBytes, maxResults, maxTreeDepth}` | no |
| `read_text_file` | `path`, `head?`, `tail?` | file text; truncated with a note above `FS_MAX_FILE_BYTES` | no |
| `read_media_file` | `path` | `image`/`audio` block, or embedded `resource` (`file://` uri + base64 blob) | no |
| `read_multiple_files` | `paths[]` (≤100) | `<path>:\n<content>` blocks joined by `---`; per-file `Error -` lines | no |
| `write_file` | `path`, `content` | `Successfully wrote to <path>` (atomic create-or-replace) | **yes** |
| `edit_file` | `path`, `edits[{oldText,newText}]` (≤200), `dryRun?` | unified diff in a ```` ```diff ```` fence | **yes** (not for `dryRun`) |
| `create_directory` | `path` | `Successfully created directory <path>` (`mkdir -p`, idempotent) | **yes** |
| `list_directory` | `path` | `[DIR]`/`[FILE]` lines; structured entries | no |
| `list_directory_with_sizes` | `path`, `sortBy?` | padded listing + `Total:` / `Combined size:` | no |
| `directory_tree` | `path`, `excludePatterns?`, `maxDepth?` | JSON `[{name, type, children?}]` | no |
| `move_file` | `source`, `destination` | `Successfully moved …` (never overwrites) | **yes** |
| `search_files` | `path`, `pattern`, `excludePatterns?`, `maxDepth?` | matching guest paths or `No matches found` | no |
| `get_file_info` | `path` | `size/created/modified/accessed/isDirectory/isFile/isSymlink/permissions` lines | no |

Full semantics, limits and error strings: [skills/server/references/TOOLS.md](skills/server/references/TOOLS.md).
Operating knowledge for agents (sequencing, error catalogue):
[skills/server/SKILL.md](skills/server/SKILL.md), served at
`skill://official-filesystem-mcp/SKILL.md` (catalog at `skill://index.json`).

## Configuration

All named config (no secrets — the server holds no credential):

| Env var | Kind | Default | Required | Meaning |
|---|---|---|---|---|
| `FS_ALLOWED_DIRS` | named config | — | **yes** | Comma-separated absolute **guest** paths the tools may touch; each must be a `volumeMounts[].mountPath`. Missing → every tool returns `FS_ALLOWED_DIRS is not set. Mount a host folder with spec.volumes (hostPath) + localResources.volumeMounts (mountPath) …`. Relative entries are rejected. |
| `FS_READ_ONLY` | named config | `false` | no | `true`/`1`/`yes` disables `write_file`, `edit_file` (except `dryRun`), `create_directory`, `move_file` with `This server is read-only (FS_READ_ONLY=true); <tool> is disabled.` |
| `FS_MAX_FILE_BYTES` | named config | `1048576` | no | Read cap per file (text reads are truncated with a note; `read_media_file`/`edit_file` refuse larger files). Clamped to 1024..=67108864. |
| `FS_MAX_RESULTS` | named config | `1000` | no | Max entries from listings, searches and trees; output ends with `[truncated to FS_MAX_RESULTS=N entries]`. Clamped to 1..=100000. |
| `FS_MAX_TREE_DEPTH` | named config | `10` | no | Recursion depth for `directory_tree`/`search_files`; a per-call `maxDepth` may lower it, never exceed it. Clamped to 1..=64. |
| `MCP_ALLOWED_HOSTS` | named config | `official-filesystem-mcp.localhost` | yes | DNS-rebinding guard; must match the ingress host. |
| `RUST_LOG` | named config | `info` | no | Log filter. |

There is no secret to register (`secretFrom` is empty). `allowedHosts` is
`[]`: the tools never dial out.

## Grant: the host folder (`spec.volumes`)

The only authority this server has is the folder the manifest mounts. Three
steps, in this order:

1. **Create the host folder first** — the daemon never creates `hostPath`
   directories, and a missing one is a *permanent* start failure
   (`HostPath volume '<path>' does not exist or is not a directory`) that is
   not retried until you re-apply:
   ```console
   $ mkdir -p ~/.local/share/cosmonic/volumes/official-filesystem-mcp
   $ echo 'Files here are visible to the official-filesystem-mcp server as /data.' \
       > ~/.local/share/cosmonic/volumes/official-filesystem-mcp/README.txt
   ```
2. **Mount it** in [`deploy/workload.yaml`](deploy/workload.yaml) (already
   done for the default folder — write the absolute host path, `~` is not
   expanded):
   ```yaml
   spec:
     volumes:
       - name: data
         hostPath: { path: /home/<you>/.local/share/cosmonic/volumes/official-filesystem-mcp }
     components:
       - localResources:
           volumeMounts:
             - { name: data, mountPath: /data, readOnly: false }
   ```
3. **Point the server at it**: `FS_ALLOWED_DIRS: /data`.

To expose more folders add one `volumes` entry + one `volumeMounts` entry per
folder (`name: notes`, `hostPath: /home/<you>/Documents/notes` →
`mountPath: /notes`) and list every mount path: `FS_ALLOWED_DIRS: /data,/notes`.
A Desktop named config (`configFrom`) can carry `FS_ALLOWED_DIRS` per folder
set. For a read-only server set **both** `readOnly: true` on the mount and
`FS_READ_ONLY: "true"` — the mount is the enforcement, the env var is what
gives agents a clear message instead of `Read-only file system`.

**The mount is a two-way door.** Anything an agent writes lands in that folder
on the host immediately. Mount a dedicated folder, not `~` or a repository
root; keep secrets out of it; prefer read-only when reads suffice.

Note: the `cosmonic://schema/workload` MCP resource does not mention
`volumes`; the field is supported (Desktop's own `oci-registry` workload uses
it) — this manifest is the reference shape.

## Build and test

```console
$ cargo build --release           # target/wasm32-wasip2/release/official_filesystem_mcp.wasm
$ cargo fmt --check && cargo clippy --all-features -- -D warnings
$ wasm-tools component wit target/wasm32-wasip2/release/official_filesystem_mcp.wasm \
    | grep 'export wasi:http/handler@0.3.0'
$ scripts/e2e.sh                  # hermetic; wasmtime 46/47 + curl + python3
$ E2E_LIVE=1 scripts/e2e.sh --no-build   # adds a smoke against the Desktop deployment
```

The suite needs no network and no HTTP fixture: it builds a directory tree
under `/tmp/mcp-e2e-<port>/` (unicode names, symlinks that escape the mount,
a cycle, over-cap files, a 12-deep chain) and hands it to `wasmtime serve
--dir` as preopens — the same thing a Desktop `volumeMount` becomes. Three
instances run: the primary (two mounts plus one deliberately unmounted
allowed dir), a read-only one, and a guard instance started **without**
`FS_ALLOWED_DIRS` for the missing-config path. `cargo test` is not used (wasm
target).

## Deploy on Cosmonic Desktop

```console
$ SOCK=/run/user/$(id -u)/cosmonic/cosmonicd.sock         # Linux
$ mkdir -p ~/.local/share/cosmonic/volumes/official-filesystem-mcp   # step 1 above
$ cd mcp-servers/official-filesystem-mcp && cargo build --release
$ curl -sS --unix-socket "$SOCK" -X POST http://localhost/v1/projects \
    -H 'Content-Type: application/json' -d "{\"path\":\"$PWD\"}"
$ curl -sS --unix-socket "$SOCK" -X POST http://localhost/v1/projects/official-filesystem-mcp/promote \
    -H 'Content-Type: application/json' -d '{"ref":"official-filesystem-mcp:0.1.0","rebuild":true}'
# → {"image":"oci.localhost:8200/apps/official-filesystem-mcp:0.1.0@sha256:…", …}
$ IMAGE=oci.localhost:8200/apps/official-filesystem-mcp:0.1.0@sha256:…
$ python3 -c 'import yaml,json,sys; d=yaml.safe_load(open("deploy/workload.yaml")); d["spec"]["components"][0]["image"]=sys.argv[1]; print(json.dumps(d))' "$IMAGE" \
    | curl -sS --unix-socket "$SOCK" -X POST http://localhost/v1/workloads \
        -H 'Content-Type: application/json' --data-binary @-
$ curl -s http://official-filesystem-mcp.localhost:8200/ | jq .status
```

Or apply `deploy/workload.yaml` as-is for the published image, or through the
`cosmonic_apply_workload` MCP tool. Never apply a name that already exists
unless you mean to replace it (`GET /v1/workloads` first).

### Talk to it

```console
$ curl -s http://official-filesystem-mcp.localhost:8200/ | jq '.status, .capabilities.tools'
"ok"
["create_directory","directory_tree","edit_file","get_file_info","list_allowed_directories", …]

$ curl -s -X POST http://official-filesystem-mcp.localhost:8200/ \
    -H 'Content-Type: application/json' -H 'Accept: application/json, text/event-stream' \
    -H 'MCP-Protocol-Version: 2026-07-28' -H 'Mcp-Method: tools/list' \
    -d '{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}}}}'

$ curl -s -X POST http://official-filesystem-mcp.localhost:8200/ \
    -H 'Content-Type: application/json' -H 'Accept: application/json, text/event-stream' \
    -H 'MCP-Protocol-Version: 2026-07-28' -H 'Mcp-Method: tools/call' -H 'Mcp-Name: list_directory' \
    -d '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"list_directory","arguments":{"path":"/data"},"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}}}}'
data: {"jsonrpc":"2.0","id":2,"result":{"resultType":"complete","content":[{"type":"text","text":"[FILE] README.txt"}],"structuredContent":{…},"isError":false}}
```

### Connect a client

```console
$ claude mcp add --transport http official-filesystem-mcp http://official-filesystem-mcp.localhost:8200/
```

Claude Desktop (`claude_desktop_config.json`):
`{"mcpServers":{"official-filesystem-mcp":{"type":"http","url":"http://official-filesystem-mcp.localhost:8200/"}}}`.
Cosmonic Desktop also detects the `mcp.ai/*` labels and can register the
server into detected coding agents from its UI.

## Borrowed from

- [modelcontextprotocol/servers — `src/filesystem`](https://github.com/modelcontextprotocol/servers/tree/main/src/filesystem)
  (`@modelcontextprotocol/server-filesystem` 0.6.3, **MIT**; the project is
  transitioning to Apache-2.0): the entire tool surface, argument names,
  tool descriptions and annotations, output formats, error strings, the
  path-validation rules, the atomic write scheme, and the `edit_file`
  matching/diff algorithm — re-implemented in Rust (`src/fsops.rs`,
  `src/server.rs`) and re-licensed Apache-2.0 with this attribution.
- Rust crates: [`similar`](https://crates.io/crates/similar) (Apache-2.0)
  for the unified diff, [`globset`](https://crates.io/crates/globset)
  (MIT/Unlicense) for minimatch-style globs, [`base64`](https://crates.io/crates/base64)
  (MIT/Apache-2.0).
- Surveyed, nothing borrowed: mark3labs/mcp-filesystem-server (Go, MIT) and
  cyanheads/filesystem-mcp-server (TypeScript, Apache-2.0).

## Known limitations and divergences

- **No MCP `roots`.** The reference server can take its allowed directories
  from the client's `roots/list`; a stateless streamable-HTTP component
  cannot, so `FS_ALLOWED_DIRS` is the only source. Clients that rely on roots
  must mount the folder instead.
- **No delete tool** (the reference server has none either): move things into
  a scratch folder instead.
- **`permissions: n/a`** in `get_file_info` — WASI exposes no mode bits, and an
  overwritten file keeps whatever mode the runtime gives new files (the
  reference server restores the original `chmod`).
- **`created`** may be `unknown` where the host does not expose a birth time.
- **Symlinks must be relative and stay inside the mount.** The runtime refuses
  absolute symlink targets outright, even ones that point back into the same
  folder; the server reports them as `Access denied - symlink target outside
  allowed directories`.
- `tail` does not count a trailing newline as an empty last line; `head`/`tail`
  of `0` and an empty `oldText` are refused rather than silently ignored.
- `list_directory` reports a symlink by its target's type; `directory_tree`
  and `search_files` follow in-mount directory symlinks with cycle detection
  (the reference server treats every symlink as a file).
- No NFC/NFD Unicode-equivalent path matching (Linux is byte-exact); a macOS
  deployment of the same manifest will differ from the Node server there.
- A `move_file` between two volumes on different host filesystems falls back
  to copy-then-delete, which is not atomic.
- `readOnly: true` on the mount without `FS_READ_ONLY` surfaces as a raw
  `Read-only file system` error on writes; set both for a clear message.
- The dev loop (`.wash/config.yaml`) has not been verified to honour
  `volumeMounts`; test under `scripts/e2e.sh` and deploy with
  `deploy/workload.yaml`.

## License

Apache-2.0 (see `LICENSE`).
