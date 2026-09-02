# Tool reference

Supporting file of the `official-filesystem-mcp` skill, served at
`skill://official-filesystem-mcp/references/TOOLS.md`. Argument names, output
formats and error strings match the official
`@modelcontextprotocol/server-filesystem` reference server unless noted.

Every `path` is an absolute **guest** path under one of the directories
`list_allowed_directories` returns (a relative path resolves against the first
one). Every tool returns the same validation errors before doing anything:
`Access denied - path outside allowed directories: X not in A, B`,
`Access denied - symlink target outside allowed directories: …`,
`Access denied - Windows-style path received on a POSIX host: …`,
`Access denied - path contains a NUL byte: …`, and — on an unconfigured
deployment — `FS_ALLOWED_DIRS is not set. …`.

## Summary

| Tool | Arguments | Returns | Gated by `FS_READ_ONLY` |
|---|---|---|---|
| `list_allowed_directories` | none | `Allowed directories:` + one guest path per line (unmounted ones flagged); structured `{directories:[{path,mounted}], readOnly, maxFileBytes, maxResults, maxTreeDepth}` | no |
| `read_text_file` | `path`, `head?`, `tail?` | File text (UTF-8; lossy with a note); truncated with a note above `FS_MAX_FILE_BYTES` | no |
| `read_media_file` | `path` | `image`/`audio` content block, or an embedded `resource` (`file://` uri, mimeType, base64 `blob`); structured `{uri, mimeType, size, encoding}` | no |
| `read_multiple_files` | `paths[]` (1–100) | `<path>:\n<content>\n` blocks joined by `\n---\n`; failures as `<path>: Error - <msg>` | no |
| `write_file` | `path`, `content` | `Successfully wrote to <path>` | **yes** |
| `edit_file` | `path`, `edits[{oldText,newText}]` (1–200), `dryRun?` | Unified diff in a ```` ```diff ```` fence | **yes** (unless `dryRun: true`) |
| `create_directory` | `path` | `Successfully created directory <path>` (also when it existed) | **yes** |
| `list_directory` | `path` | `[DIR] name` / `[FILE] name` lines sorted by name; structured `{entries:[{name,type,isSymlink}], truncated}` | no |
| `list_directory_with_sizes` | `path`, `sortBy?` (`name`\|`size`) | `[FILE] <name padded to 30> <size padded to 10>` lines, blank line, `Total: N files, M directories`, `Combined size: X`; structured `{entries:[{name,type,size}], totalFiles, totalDirectories, combinedSize, truncated}` | no |
| `directory_tree` | `path`, `excludePatterns?[]`, `maxDepth?` | 2-space-indented JSON array of `{name, type: "file"\|"directory", children?}`; structured `{entries, maxDepth, truncated}` | no |
| `move_file` | `source`, `destination` | `Successfully moved <source> to <destination>` | **yes** |
| `search_files` | `path`, `pattern`, `excludePatterns?[]`, `maxDepth?` | Absolute guest paths one per line, or `No matches found`; structured `{matches[], truncated}` | no |
| `get_file_info` | `path` | `size`, `created`, `modified`, `accessed` (ISO-8601 UTC or `unknown`), `isDirectory`, `isFile`, `isSymlink`, `permissions: n/a` as `key: value` lines; structured object of the same | no |

`maxDepth` is this port's addition (the reference server has no depth
limit); it defaults to the deployment's `FS_MAX_TREE_DEPTH` and is clamped to
it. Tool annotations follow the reference server: the nine readers carry
`readOnlyHint: true`; `write_file` and `move_file` are `destructiveHint: true`;
`create_directory` is `idempotentHint: true`; every tool is
`openWorldHint: false`.

## `read_text_file`

```json
{"path": "/data/notes/todo.md", "head": 20}
```

- `head` and `tail` are positive integers (≤ 1,000,000), mutually exclusive:
  both → `Cannot specify both head and tail parameters simultaneously`;
  `0` → `head must be a positive integer`.
- Lines are returned joined by `\n` without a trailing newline; a file's
  final line terminator is not counted as an empty line.
- Whole-file reads stop at `FS_MAX_FILE_BYTES`, cut on a UTF-8 boundary, and
  append `\n[truncated: file is N bytes, cap is M; use head/tail to read a range]`.
  `head`/`tail` never read more than the cap either.
- Invalid UTF-8 is decoded lossily and flagged with
  `[note: file is not valid UTF-8; undecodable bytes were replaced with U+FFFD]`.
- Missing file → `No such file or directory: <path>`; a directory → an
  `isError` read failure.

## `read_media_file`

```json
{"path": "/data/img/logo.png"}
```

MIME by extension: `.png .jpg .jpeg .gif .webp .bmp .svg` → `image/*`,
`.mp3 .wav .ogg .flac` → `audio/*`, anything else →
`application/octet-stream` returned as an embedded resource
`{type: "resource", resource: {uri: "file:///data/…", mimeType, blob}}`.
Files over `FS_MAX_FILE_BYTES` → `File is N bytes; read_media_file cap is
FS_MAX_FILE_BYTES=M`; directories → `<path> is a directory, not a file`.

## `read_multiple_files`

```json
{"paths": ["/data/a.md", "/data/b.md"]}
```

Empty array → `At least one file path must be provided`. More than 100 paths:
the first 100 are read and a `[truncated: N paths requested, only the first 100
were read]` block is appended. Each file is subject to the same cap and notes
as `read_text_file`. `isError` only when every file failed.

## `write_file`

```json
{"path": "/data/out/report.md", "content": "# Report\n"}
```

Creates the file with exclusive-create (so it never writes *through* a
pre-existing symlink); if the file exists it writes a temp file beside it and
renames it over the target atomically. Parent must exist
(`Parent directory does not exist: <dir>`); a directory at `path` →
`<path> is a directory, not a file`. `content` is capped at 64 MiB in-guest
(the transport's request-body cap of a few MiB applies first).

## `edit_file`

```json
{
  "path": "/data/src/main.rs",
  "edits": [{"oldText": "fn main() {", "newText": "fn main() {\n    init();"}],
  "dryRun": true
}
```

1. The file is loaded (must be ≤ `FS_MAX_FILE_BYTES` and valid UTF-8) and
   CRLF is normalised to LF (in the file and in every `oldText`/`newText`).
2. Each edit, in order: exact substring match (first occurrence) — else a
   line-window match comparing whitespace-trimmed lines, re-indenting
   `newText`'s first line to the matched line's indentation and preserving
   relative indentation of the rest — else
   `Could not find exact match for edit:\n<oldText>` and nothing is written.
3. Output: ```` ```diff\n--- <path>\n+++ <path>\n@@ … @@\n…``` ```` with 3
   context lines; the fence grows (```` ```` ````) past any backtick run in
   the diff. No changes → an empty fence.
4. Unless `dryRun`, the result is written atomically (temp + rename).

`edits` must be non-empty (≤ 200) and no `oldText` may be empty.

## `create_directory`

```json
{"path": "/data/out/2026/q3"}
```

`mkdir -p` semantics. The nearest existing ancestor must be inside an
allowed directory and not be an escaping symlink. A file at `path` →
`<path> already exists and is not a directory`.

## `list_directory` / `list_directory_with_sizes`

```json
{"path": "/data", "sortBy": "size"}
```

Entries are sorted by name (case-insensitive); `sortBy: "size"` sorts
descending by size. A symlink is reported by its target's type; one the
sandbox refuses to follow is listed as `[FILE]` with size `0 B`. Sizes use
`B`/`KB`/`MB`/`GB`/`TB` with two decimals (`0 B` for zero). Listings stop at
`FS_MAX_RESULTS` with `[truncated to FS_MAX_RESULTS=N entries]`; the
`Total:`/`Combined size:` summary counts every entry. An empty directory
returns empty text.

## `directory_tree`

```json
{"path": "/data", "excludePatterns": ["node_modules", "**/.git/**"], "maxDepth": 3}
```

Nodes: `{"name", "type": "file"|"directory", "children": […]}` — directories
always carry `children` (possibly empty), files never do. `excludePatterns`
match the path relative to `path`; a pattern without `*` is also tried as
`**/<p>` and `**/<p>/**`. Directory symlinks inside the mount are followed
with cycle detection (a revisited directory has empty `children`); ones that
escape are listed as files. When `FS_MAX_RESULTS` nodes have been emitted the
tree ends with a `{"name": "…", "type": "file"}` marker and a second text
block explains the cut. Not a directory → `<path> is not a directory`.

## `move_file`

```json
{"source": "/data/draft.md", "destination": "/data/archive/draft.md"}
```

`source` must exist; `destination` must not (`Destination already exists:
<destination>` — checked with `lstat`, so a symlink counts) and its parent
must. Moving a directory into itself → `Cannot move <source> into itself`.
Across two volumes on different host filesystems the rename fails with
EXDEV and the server copies then deletes (not atomic).

## `search_files`

```json
{"path": "/data", "pattern": "**/*.md", "excludePatterns": ["node_modules"]}
```

Globs are matched against each entry's path relative to `path`
(files and directories both count): `*.md` top level only, `**/*.md`
recursive, `*` never crosses `/`, dot-files included, `\` escapes. A bare
name with no glob characters is also tried as `**/<name>`. Excludes are
expanded like `directory_tree`'s and prune whole subtrees. Entries the
sandbox refuses (escaping symlinks) are skipped silently. Results are
absolute guest paths; none → `No matches found` (not an error); more than
`FS_MAX_RESULTS` → the list ends with `[truncated to FS_MAX_RESULTS=N entries]`.
Unbalanced patterns → `Invalid glob pattern '<p>': …`.

## `get_file_info`

```json
{"path": "/data/report.pdf"}
```

```
size: 12345
created: 2026-09-02T13:58:48Z
modified: 2026-09-02T13:58:48Z
accessed: 2026-09-02T13:58:50Z
isDirectory: false
isFile: true
isSymlink: false
permissions: n/a
```

Timestamps are ISO-8601 UTC (`unknown` when the host does not expose one).
`isSymlink` describes the path as given; `size`/`isFile` describe the
target. `permissions` is always `n/a` — WASI exposes no mode bits.
