# Values and parameters

Supporting file of the `postgres-mcp` skill
(`skill://postgres-mcp/references/TYPES.md`). The host
(`wasmcloud:postgres@0.2.0` in Cosmonic Desktop) converts each result cell
into a typed value and binds each parameter in Postgres' **binary** wire
format. This page is what that means for you.

## How result cells are rendered

| Postgres type | JSON | Notes |
|---|---|---|
| `int2`, `int4`, `int8`, `serial*` | number | exact |
| `float4`, `float8` | number | `NaN`, `Infinity`, `-Infinity` become **strings**; `-0.0` stays `-0.0` |
| `numeric`, `decimal`, `money` | — | **cannot be returned reliably** (the host reads the binary bytes as text); the server decodes the few values that survive and refuses the rest. Cast: `col::text` (exact, a string) or `col::float8` (a number) |
| `bool` | boolean | |
| `text`, `varchar`, `name`, `xml`, `citext` | string | cells > 32 768 chars are cut with `…[truncated: N of M chars shown]` |
| `char(n)` | — | unsupported by the host — `col::text` (`"char"` works) |
| `bytea` | string | base64 (standard alphabet, padded) |
| `uuid` | string | canonical lower-case |
| `date` | `"YYYY-MM-DD"` | BC dates as `"YYYY-MM-DD BC"`; `"infinity"` / `"-infinity"` |
| `time` | `"HH:MM:SS[.ffffff]"` | |
| `timestamp` | `"YYYY-MM-DDTHH:MM:SS[.ffffff]"` | no zone (as stored) |
| `timestamptz` | `"YYYY-MM-DDTHH:MM:SS[.ffffff]Z"` | always converted to UTC by the host |
| `timetz` | — | unsupported — `col::text` |
| `interval` | — | unsupported — `col::text` (`"1 day"`) or `EXTRACT(EPOCH FROM col)` (seconds) |
| `json`, `jsonb` | parsed value | a cell that is not valid JSON is returned as a string |
| arrays (`text[]`, `int4[]`, `int8[]`, `float8[]`, `bool[]`, `uuid[]`, …) | array | one dimension; element rules as above |
| `hstore` | object | values may be `null` |
| `inet`, `cidr`, `macaddr`, `macaddr8` | string | |
| `bit(n)`, `varbit` | `{"bits": n, "hex": "…"}` | big-endian bit order |
| `point`, `box`, `lseg`, `path`, `polygon` | arrays of `[x, y]` | `circle` and `line` are unsupported |
| `pg_lsn` | `"X/Y"` string | |
| `oid`, `regclass`, `regproc`, `regtype`, … | — | unsupported — `col::int` or `col::text` |
| enums | — | unsupported — `col::text` (`describe_table` shows `enum_values`) |
| domains (e.g. `information_schema` columns) | as the base type | the wire protocol reports the base type, so these just work |
| ranges, multiranges, `record`, `void`, `tsquery`, `tsvector` | — | unsupported — cast to text (`ROW(...)::text`, `to_jsonb(...)`) |

An unsupported column fails the **entire** call with `the host cannot convert
a result column …` naming the column (by position and, when known, name).
The safe pattern for an unfamiliar table is `describe_table`, then select
named columns with casts where the table above says so.

## How parameters are bound

`params` is a JSON array; entry *N* binds `$N`. Its count must equal the
highest placeholder used (`08P01 … expected N parameters` otherwise).

### Bare JSON values

| JSON | First encoding | Automatic retries when Postgres rejects it |
|---|---|---|
| integer | `numeric` | `int8`, `int4`, `int2` (each only if the value fits) |
| float | `numeric` | `float8` |
| string | `text` | `uuid` (if it looks like one), `numeric`, `timestamptz`/`timestamp` (if ISO-8601), `time`, `int8`/`float8`, `date`, `int4`, `int2`, `bool` (`true`/`false`/`t`/`f`/`yes`/`no`/`on`/`off`/`1`/`0`) |
| boolean | `bool` | — |
| null | `NULL` | — |
| array of strings / integers / booleans / numbers | `text[]` / `int8[]` / `bool[]` / `float8[]` | — |
| object (without `type`+`value`) | `jsonb` | — |

Retries are driven by the database's rejection. The chain is ordered from
the longest wire encoding to the shortest so that every rejection names its
parameter — with two exceptions that Postgres reads **silently** because the
byte lengths match:

- a bare **integer** bound where a `float8` (or `float4`) is expected becomes
  a garbage float → pass `3.0`, or `{"type": "float8", "value": 3}`;
- a bare **float** bound where an `int8`/`int4` is expected becomes a garbage
  integer → pass an integer.

When a rejection does not name the parameter (`08P01`) and more than one
parameter could be re-encoded, the server stops rather than guess and lists
the encodings it tried; use typed params.

### Typed parameters

`{"type": "<name>", "value": <json>}` binds exactly that type (a `value` of
`null` binds NULL in any type):

| `type` | `value` |
|---|---|
| `int2`, `int4`/`int`/`integer`, `int8`/`bigint` | integer (or numeric string) |
| `float4`/`real`, `float8`/`double` | number |
| `numeric`/`decimal`/`money` | number or numeric string (`"12345678901234567890.12"` keeps precision) |
| `text`/`varchar`/`char`/`name`/`string` | string |
| `bool`/`boolean` | boolean or `"true"`/`"false"` |
| `uuid` | canonical 36-char string |
| `json`, `jsonb` | any JSON value (a string that is itself JSON is sent as-is) |
| `date` | `"YYYY-MM-DD"` (`"infinity"`, `"-infinity"` accepted) |
| `time` | `"HH:MM[:SS[.ffffff]]"` |
| `timestamp` | `"YYYY-MM-DD[T ]HH:MM[:SS[.ffffff]]"` |
| `timestamptz` | ISO-8601 with `Z` or `±HH[:MM]` (no zone = UTC); converted to UTC before binding |
| `bytea` | base64 string |
| `inet`, `cidr` | address / network string |
| `xml` | string |
| `text[]`, `int4[]`, `int8[]`, `bool[]`, `float8[]`, `uuid[]`, `numeric[]` | JSON array of the element type |
| `null` | — |

`interval`, `char(n)`, enums, domains, ranges and other types have no typed
form: bind them as text and cast in SQL — `$1::text::interval`,
`$1::text::my_enum`.

### Casts in SQL

`WHERE created_at > $1::text::timestamptz` with a string param is always
safe: the slot becomes `text`, the string binds cleanly, and Postgres does
the conversion. `WHERE id = $1::int` is **not** the same thing — it types the
slot as `int4` and still expects binary int4 bytes.
