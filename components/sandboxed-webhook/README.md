# sandboxed-webhook

A webhook (a public HTTP endpoint something calls when an event happens: a git push, a payment, a form submit) receiver that verifies an HMAC-SHA256 signature, then forwards the payload to **exactly one** host. Built as a WebAssembly component (`wasi:http`, `wasm32-wasip2`) for Cosmonic Desktop / wasmCloud.

The point of the example is the host's egress policy, not the code. The workload's `allowedHosts` names the single forward target and denies every other destination, so even a fully compromised handler physically cannot exfiltrate the payload anywhere you did not name. Least-privilege egress, enforced by the sandbox.

- `GET /` renders an info page. Once a secret is configured it prints a ready-to-run `curl` carrying a valid signature; until then it reports what is missing and returns `503`.
- `POST /` verifies `X-Hub-Signature-256: sha256=<hex>` over the raw body, then forwards the body to `WEBHOOK_FORWARD_URL`. A missing or wrong signature gets `401` and never reaches out. Bodies over 1 MiB get `413`.

## Configuration

Read from the workload environment:

| Variable | Default | Meaning |
| --- | --- | --- |
| `WEBHOOK_SIGNING_SECRET` | none, required | HMAC key. Registered as a Cosmonic secret, never inline. Unset, every `POST` is refused with `503` and nothing is forwarded. |
| `WEBHOOK_FORWARD_URL` | `https://postman-echo.com/post` | The single downstream. Its host **must** be in the workload's `allowedHosts`. |

## Set the signing secret

The webhook verifies its caller with an HMAC signature, so it needs a signing secret you control. Never inline it in a manifest: register it as a **Cosmonic secret**, flattened into the `WEBHOOK_SIGNING_SECRET` environment variable.

1. Register the secret with the `cosmonic_set_secret` MCP tool (the value goes into your OS keychain, never a manifest):

   | Field | Value |
   |---|---|
   | `name` | `webhook-signing-secret`, matches `secretFrom` in the workload |
   | `uri` | `keychain://cosmonic/webhook-signing-secret` |
   | `env` | `WEBHOOK_SIGNING_SECRET`, the variable injected into the component |
   | `value` | `<a strong random secret you generate>` |

2. The workload already references it, under `components[].localResources.environment`:

   ```yaml
   secretFrom:
     - name: webhook-signing-secret
   ```

This is a required step before launch, including under local `wash dev`. There is deliberately no fallback secret: a default written into this file would be published with it, and anyone reading the repository could sign a request that a deployment which skipped this step would accept as genuine. Without the variable set, the receiver refuses every `POST` and says so.

## Prerequisites

- [`wash`](https://wasmcloud.com/docs/installation) (tested against wash 2.5.1 / wash-runtime 2.7.0)
- The Rust toolchain and the `wasm32-wasip2` target: `rustup target add wasm32-wasip2`

## Build

```shell
wash build
```

The component is written to `target/wasm32-wasip2/release/sandboxed_webhook.wasm`.

## Run and try it

```shell
wash dev
```

`wash dev` runs deny-all, so the forward is denied there by design (proof the gate is real). Set `WEBHOOK_SIGNING_SECRET` in that environment too, or the receiver returns `503` before it gets as far as the egress check. To see a forward succeed, deploy with an allow-list: apply [`manifests/workload.yaml`](manifests/workload.yaml) on Cosmonic Desktop, which sets `allowedHosts: [postman-echo.com]`:

Sign with the same secret you registered above. `$WEBHOOK_SECRET` here is that
value, in your shell rather than in the manifest:

```shell
BODY='{"event":"ping","from":"cosmonic"}'
SIG=$(printf '%s' "$BODY" | openssl dgst -sha256 -hmac "$WEBHOOK_SECRET" | sed 's/^.*= //')
curl -H 'Host: sandboxed-webhook.localhost' -X POST http://127.0.0.1:8200/ \
  -H 'content-type: application/json' \
  -H "x-hub-signature-256: sha256=$SIG" \
  -d "$BODY"
# {"verified":true,"forwarded_to":"https://postman-echo.com/post","downstream_status":200}
```

Or open `GET /` in a browser, which prints the same `curl` with the signature already computed. Change one byte of the body and the signature no longer matches: `401`, no egress.

## License

Apache-2.0.
