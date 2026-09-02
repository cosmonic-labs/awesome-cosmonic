---
name: official-filesystem-mcp
description: Read, search, write, edit, move and inspect files in the host folder(s) mounted into this sandboxed filesystem server (guest paths like /data). Use it whenever a task needs to look at, create or change files on the user's machine through MCP, and read it before any edit_file or move_file call or when a path is refused with "Access denied", "Parent directory does not exist", or "Operation not permitted".
---

# Using the official-filesystem-mcp server

A WebAssembly port of the official `@modelcontextprotocol/server-filesystem`
reference server: the same thirteen tools, the same argument names and the
same error strings — but sandboxed. The component sees **only** the host
folders the workload manifest mounts into it, nothing else on the machine,
and it never touches the network.

Full argument/result reference: [references/TOOLS.md](references/TOOLS.md).

## Start here: paths are GUEST paths

1. **Call `list_allowed_directories` first.** It returns the guest paths the
   tools accept (typically `/data`) and flags any that are configured but not
   actually mounted (`/notes (NOT MOUNTED — check volumeMounts)`).
2. **Build every path from what it returned.** `/data/notes/todo.md`, never
   `~/Documents/notes/todo.md` or `/home/…` — host paths are always denied
   with `Access denied - path outside allowed directories: X not in /data`.
   On the host, `/data` is the folder named by `spec.volumes[].hostPath` in
   the workload manifest (by default
   `~/.local/share/cosmonic/volumes/official-filesystem-mcp`); files you write
   appear there immediately.
3. A relative path (`notes/todo.md`) resolves against the **first** allowed
   directory. The guest's `/` itself is not readable — `list_directory` on
   `/` is denied — so do not try to "walk up" to discover mounts.
4. This server is stateless streamable HTTP: it **cannot** take allowed
   directories from the MCP `roots` protocol the way the Node reference
   server does. `FS_ALLOWED_DIRS` in the manifest is the only source of truth.

## Sequencing that works

- **Orient**: `list_allowed_directories` → `directory_tree` (with
  `excludePatterns` such as `node_modules`, `.git`, and a small `maxDepth`)
  or `list_directory` → `search_files` for names → `read_text_file`.
- **Read big files in ranges**: files over `FS_MAX_FILE_BYTES` (default
  1 MiB) come back truncated with
  `[truncated: file is N bytes, cap is M; use head/tail to read a range]`.
  Use `head`/`tail` (mutually exclusive) instead of retrying; there is no
  offset parameter. Binary files go through `read_media_file`, which refuses
  over-cap files outright because base64 inflates them by 4/3.
- **Edit safely**: `read_text_file` the region → `edit_file` with
  `dryRun: true` → inspect the diff → same call with `dryRun: false`.
  `edit_file` is exact-match, sequential and all-or-nothing:
  - `oldText` must match byte-for-byte after CRLF→LF normalisation; a
    whitespace-trimmed line-by-line fallback exists (it re-indents `newText`
    to the matched line) but there is no regex.
  - Each edit is matched against the file **as modified by the previous
    edits**, and only the first occurrence is replaced.
  - Any unmatched edit fails the whole call with
    `Could not find exact match for edit:\n<oldText>` and nothing is written.
- **Create before you write**: `write_file` and `move_file` need an existing
  parent (`Parent directory does not exist: /data/x`); `create_directory`
  creates the whole chain and is idempotent.
- **Replace, do not delete**: there is no delete tool (the reference server
  has none either). To replace a file, `write_file` over it. To "remove"
  something, `move_file` it into a scratch folder you created. `move_file`
  never overwrites — `Destination already exists: …` — and a symlink at the
  destination counts as existing.
- **Batch reads**: `read_multiple_files` keeps going past per-file failures
  (`<path>: Error - <message>` blocks separated by `---`); it is `isError`
  only when every file failed. At most 100 paths per call.

## Search patterns (minimatch-style globs)

`search_files` matches globs against the path **relative to `path`**:
`*.md` matches the top level only, `**/*.md` recurses, `*` never crosses `/`,
dot-files are matched. A bare name with no glob characters (`README.md`) is
also tried as `**/README.md`, so it is found at any depth. `excludePatterns`
are globs too; a bare `node_modules` also excludes `**/node_modules` and
everything beneath it. Zero hits returns the text `No matches found` — a
normal result, not an error. Symlinks that escape the mount are skipped.

## Limits (set by the deployment, visible in `list_allowed_directories`)

| Setting | Default | Effect |
|---|---|---|
| `FS_MAX_FILE_BYTES` | 1 MiB | Read cap for text/media/edit; larger text is truncated with a note |
| `FS_MAX_RESULTS` | 1000 | Entries per listing/search/tree; output ends with `[truncated to FS_MAX_RESULTS=N entries]` |
| `FS_MAX_TREE_DEPTH` | 10 | Recursion for `directory_tree`/`search_files`; a per-call `maxDepth` may lower it, never exceed it |
| `FS_READ_ONLY` | false | `true` disables `write_file`, `edit_file` (except `dryRun`), `create_directory`, `move_file` |

Per-call clamps: 100 paths per `read_multiple_files`, 200 edits per
`edit_file`, `head`/`tail` ≤ 1,000,000 lines, 64 MiB per `write_file`
(the transport's request cap is lower).

## Error catalogue — what it means and what to do

| You see | Meaning | Do |
|---|---|---|
| `FS_ALLOWED_DIRS is not set. Mount a host folder with spec.volumes …` | The workload has no allow-list; nothing is reachable even if a volume is mounted. | Deployment fix: add `spec.volumes` + `volumeMounts` and `FS_ALLOWED_DIRS=<mountPath>` to `deploy/workload.yaml`, re-apply. Do not retry. |
| `/x (NOT MOUNTED — check volumeMounts)` from `list_allowed_directories`, or `No such file or directory: /x/...` for the allowed dir itself | `FS_ALLOWED_DIRS` names a guest path no `volumeMounts` entry provides. | Deployment fix: make `mountPath` and `FS_ALLOWED_DIRS` identical. |
| `Access denied - path outside allowed directories: X not in /data` | After normalisation the path is outside every allowed directory (`..` climb, a host path, `/`). | Use a path under a listed directory. Never retry with the same path. |
| `Access denied - symlink target outside allowed directories: …` | A symlink inside the mount points outside it; the runtime refuses to follow it regardless of host permissions. | Treat the entry as unreadable. If the target is needed, the user must mount that folder as its own volume and add it to `FS_ALLOWED_DIRS`. |
| `Operation not permitted (the sandbox refused …)` / errno 63 | Same cause surfacing from a raw file operation (rare). | Same as above — not a chmod problem; changing host permissions will not help. |
| `Access denied - Windows-style path received on a POSIX host: C:\…` / `path contains a NUL byte` | Not a valid guest POSIX path. | Use forward-slash absolute guest paths. |
| `Parent directory does not exist: /data/x` | `write_file`/`move_file` destination's parent is missing. | `create_directory` the parent, then retry. |
| `No such file or directory: /data/x` | The file is not there (also for a dangling symlink). | Check spelling with `list_directory`/`search_files`. |
| `Cannot specify both head and tail parameters simultaneously` | `read_text_file` got both. | Send one, or two calls. |
| `[truncated: file is N bytes, cap is M; use head/tail to read a range]` | File exceeds `FS_MAX_FILE_BYTES`; you got the first M bytes. | Use `head`/`tail` for the part you need; the user can raise the cap in the manifest (max 64 MiB). |
| `File is N bytes; read_media_file cap is FS_MAX_FILE_BYTES=M` (same for `edit_file`) | Too large to base64 / edit in one result. | `get_file_info` for metadata; raise the cap only if the client can take the payload. |
| `[note: file is not valid UTF-8; …]` | Binary or non-UTF-8 content decoded lossily. | Use `read_media_file` for the raw bytes. |
| `Could not find exact match for edit:\n<oldText>` | `oldText` did not match (after CRLF normalisation and the trimmed-line fallback); nothing was written. | Re-read the region, copy it verbatim, retry with `dryRun: true`. Remember edits are sequential. |
| `Destination already exists: <path>` | `move_file` refuses to overwrite (files, dirs and symlinks all count). | Pick another destination, or `write_file` to replace contents in place. |
| `This server is read-only (FS_READ_ONLY=true); <tool> is disabled.` | Writes are disabled by configuration — a policy decision. | Do not retry. Use the read tools, or ask the user for a writable deployment. |
| `Read-only file system` on a write with no `FS_READ_ONLY` message | The `volumeMounts` entry is `readOnly: true`. | Policy decision; the user can flip the mount or set `FS_READ_ONLY=true` for the clearer message. |
| `No matches found` | The glob matched nothing (often `*.ext` when files are in subdirectories). | Try `**/*.ext`, check `excludePatterns`, or `list_directory`. |
| `[truncated to FS_MAX_RESULTS=N entries]` / a `…` node in `directory_tree` | The listing hit the result cap. | Narrow the path/pattern, lower `maxDepth`, add excludes. |
| `Invalid glob pattern '…'` | The pattern does not compile (unbalanced `[` or `{`). | Fix the glob; escape literals with `\`. |
| `created: unknown` in `get_file_info` | The host does not expose birth time through WASI. | Rely on `modified`; not an error. |
| JSON-RPC `-32602` / `isError: true` naming a field type | Malformed arguments (`head: -1`, `sortBy: "colour"`, `paths` not an array). | Fix the call; the server is fine. |

## Things that differ from the Node reference server

- No `roots` support (stateless HTTP) — see above.
- `get_file_info` prints `permissions: n/a`: WASI exposes no mode bits, and an
  overwritten file keeps whatever mode the runtime gives new files.
- `tail` does not count a trailing newline as an empty last line (`tail: 3`
  on a 100-line file returns lines 98–100).
- `head`/`tail` of `0` is refused instead of being silently ignored;
  `edit_file` refuses an empty `oldText` instead of prepending.
- `list_directory` reports a symlink by its **target's** type (`[DIR]` for a
  link to a directory); `directory_tree` and `search_files` follow directory
  symlinks that stay inside the mount, with cycle detection, and skip ones
  that escape.
- Unicode-equivalent path resolution (NFC/NFD) is not performed: paths are
  byte-exact, as on Linux.
- Moving between two volumes on different host filesystems falls back to
  copy-then-delete, which is not atomic.

## Server metadata without a protocol handshake

`GET /` returns a JSON discovery document: server name and version, MCP spec
version, endpoint paths, tool names, and the skills served. It is the cheapest
way to confirm the deployment is live before opening an MCP session.
