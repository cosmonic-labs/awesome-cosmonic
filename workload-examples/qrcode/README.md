# QR Code Generator

An HTTP service that turns text into a QR-code PNG, with a small browser UI.
Pure compute: it reaches no network, needs no configuration, and stores nothing.

Adapted from the [`qrcode` example in
wasmCloud](https://github.com/wasmCloud/wasmCloud/tree/main/examples/qrcode)
(Apache-2.0). This copy adds a styled UI, real error handling, and a size limit;
see [Changes from upstream](#changes-from-upstream).

## Routes

| Route | Response |
| --- | --- |
| `GET /` | The browser UI, served inline. |
| `POST /qrcode` | `{"payload": "..."}` in, `image/png` out. |
| anything else | `404` with a JSON body. |

Every non-2xx answer is JSON of the form `{"error": "..."}`, so the page can show
the reason rather than guessing whether it received an image or a failure.

```console
$ curl -X POST http://qrcode.localhost:8200/qrcode \
    -H 'Content-Type: application/json' \
    -d '{"payload":"https://cosmonic.com"}' --output qr.png
```

Limits, all answered with a specific status rather than a generic failure:

| Limit | Answer |
| --- | --- |
| Body over 8 KiB, by `Content-Length` | `413` |
| No `Content-Length` (a chunked body) | `411` |
| Empty or whitespace-only payload | `400` |
| Over 2,000 characters | `400` |
| Text the encoder cannot fit | `400` |

The `Content-Length` header is required because the body is read in full before
anything can inspect it, so a declared length is the only chance to refuse one
that is too large. The 2,000-character limit counts Unicode scalars; a payload
of multi-byte characters will hit the encoder's own capacity well before it,
and gets a `400` either way.

Leading and trailing whitespace is trimmed before encoding, so a pasted URL with
a stray space still produces the code you expect.

## Build

```console
$ wash build
```

The component is written to `target/wasm32-wasip2/release/qrcode.wasm`.

## Run on Cosmonic Desktop

Apply [`manifests/workload.yaml`](manifests/workload.yaml), then open
`http://qrcode.localhost:8200`.

That manifest runs the published image. To run your own build instead,
`wash build`, promote it, and swap the `image` reference for the one promote
gives you.

## Changes from upstream

- **The UI is styled and self-contained.** No webfont, no CDN, no external
  stylesheet, so it renders the same on a machine with the network denied. It
  follows the system light or dark preference, but the code itself always renders
  black on white, because inverting it would stop it scanning.
- **Errors are shown, not swallowed.** The original page called
  `response.blob()` on every response, so a failure rendered as a broken image.
  It now reads the JSON error and displays it.
- **The server distinguishes a caller's mistake from its own.** Previously every
  failure, including malformed JSON, became `500 sadness. go check logs.` Bad
  input now returns `400` with a specific message, an oversized body returns
  `413`, and `500` is reserved for genuine faults.
- **A `GET /qrcode`** returns `405` naming the fix, rather than `404`. It is the
  obvious thing to try from a browser address bar.
- **Medium error correction and a quiet zone.** The quiet zone is required by the
  spec; without the light margin many scanners will not find the code.
- **Object URLs are revoked** when replaced, instead of leaking one image per
  generation for the life of the page.
- **Whitespace is trimmed** before encoding, which upstream did not do.
