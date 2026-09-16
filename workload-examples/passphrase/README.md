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
| `GET /api` | A batch of passwords as JSON, with the entropy of each. |
| `GET /healthz` | `ok` |

```console
$ curl http://passphrase.localhost:8200/api
{"passwords":["d^;Z&HsKhK)7UQCD4R[c", ...], ...}
```

## Build

```console
$ wash build
```

The component is written to `target/wasm32-wasip2/release/passphrase.wasm`. It is
a WASI p3 component: it exports `wasi:http/handler`, not the p2
`incoming-handler`.

## Run on Cosmonic Desktop

Apply [`manifests/workload.yaml`](manifests/workload.yaml), then open
`http://passphrase.localhost:8200`.

That manifest runs the published image. To run your own build instead,
`wash build`, promote it, and swap the `image` reference for the one promote
gives you.
