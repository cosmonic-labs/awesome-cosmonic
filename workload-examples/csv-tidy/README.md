# CSV Tidy

Paste a messy CSV, get clean JSON and a list of what was wrong with it.

The kind of file that arrives from a bank export, a CRM, or a spreadsheet someone
edited by hand: a BOM at the front, a stray blank line, a header row with two
columns called "Notes", one row with an extra comma. It reports each of those
rather than silently coping, because on a real export the ragged row is usually
the interesting one.

It declares no outbound network access, which is the point for this particular
tool. The files people want to tidy are customer lists and bank exports, and the
host enforces that none of it can leave the sandbox. Its Launchpad card reads
`OUTBOUND none`.

## Routes

| Route | Response |
| --- | --- |
| `GET /` | The browser UI, served inline. |
| `POST /api/tidy` | CSV in the body, JSON out: parsed rows, the issues found, and stats. |
| `GET /healthz` | `ok` |

```console
$ printf 'name, age\n Alice ,30\n\nBob,25\n' \
    | curl -X POST --data-binary @- http://csv-tidy.localhost:8200/api/tidy
{"headers":["name","age"],
 "rows":[{"name":"Alice","age":"30"},{"name":"Bob","age":"25"}],
 "issues":["Dropped 1 blank row.","Trimmed whitespace from 2 values."],
 "stats":{"columns":2,"rows":2,...}}
```

Header whitespace is trimmed, values are trimmed, and blank rows are dropped,
and each of those is reported in `issues` rather than applied silently. An
unterminated quote is reported first, because everything after it is read as one
value and the resulting short row invites exactly the wrong fix.

## Build

```console
$ wash build
```

The component is written to `target/wasm32-wasip2/release/csv_tidy.wasm`. It is a
WASI p3 component: it exports `wasi:http/handler`, not the p2
`incoming-handler`.

## Run on Cosmonic Desktop

Apply [`manifests/workload.yaml`](manifests/workload.yaml), then open
`http://csv-tidy.localhost:8200`.

That manifest runs the published image. To run your own build instead,
`wash build`, promote it, and swap the `image` reference for the one promote
gives you.
