# Password Generator

Generates passwords from the host's cryptographic RNG and reports the entropy of
each one, so the number on screen is a fact about the generator rather than a
claim about the characters.

The interesting property is what it cannot do. A password generator is exactly
the tool where "this cannot phone home" is what you want, and here the host
enforces it: the component declares no outbound network access at all, so its
Launchpad card reads `OUTBOUND none`. That is a statement about its capabilities,
not a promise in a privacy policy.

## Routes

| Route | Response |
| --- | --- |
| `GET /` | The browser UI, served inline. |
| `GET /api` | A batch of passwords as JSON, with the entropy of each password in `entropyBits`. |
| `GET /healthz` | `ok` |

```console
$ curl http://passphrase.localhost:8200/api
{"passwords":["d^;Z&HsKhK)7UQCD4R[c", ...],"length":20,"count":5,...,"entropyBits":...}
```

`/api` takes optional query parameters: `length` (4 to 128, default 20),
`count` (1 to 20, default 5), the character-class toggles `lower`, `upper`,
`digits` and `symbols` (`1` or `0`, all on by default), and `avoidAmbiguous`
(on by default), which drops characters that are easy to confuse, such as `l`,
`1`, `O` and `0`. Pass `avoidAmbiguous=0` to keep them.

## Build

Requires [`wash`](https://wasmcloud.com/docs/installation) and Rust 1.94 or
newer with the `wasm32-wasip2` target.

```console
$ wash build
```

The component is written to `target/wasm32-wasip2/release/passphrase.wasm`. It
targets WASI 0.3 (also called p3), so it exports `wasi:http/handler` rather
than the WASI 0.2 `incoming-handler`. The `wasip3` crate carries the WIT, so
there is no `wit/` directory to fetch.

## Run on Cosmonic Desktop

Apply [`manifests/workload.yaml`](manifests/workload.yaml), then open
`http://passphrase.localhost:8200`.

That manifest runs the published image,
`ghcr.io/cosmonic-labs/components/passphrase:0.1.2`, pinned by digest. To run
your own build instead, run `wash build`, promote the project in Cosmonic
Desktop, and swap the `image` reference for the one promote gives you.
