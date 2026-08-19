# sandboxed-webhook

A webhook (a public HTTP endpoint something calls when an event happens: a git push, a payment, a form submit) receiver that verifies an HMAC-SHA256 signature, then forwards the payload to **exactly one** host. Built as a WebAssembly component (`wasi:http`, `wasm32-wasip2`) for Cosmonic Desktop / wasmCloud.

The point of the example is the host's egress policy, not the code. The workload's `allowedHosts` names the single forward target and denies every other destination, so even a fully compromised handler physically cannot exfiltrate the payload anywhere you did not name. Least-privilege egress, enforced by the sandbox.

- `GET /` renders an info page and a ready-to-run `curl` with a valid signature for the configured secret.
- `POST /` verifies `X-Hub-Signature-256: sha256=<hex>` over the raw body, then forwards the body to `WEBHOOK_FORWARD_URL`. A missing or wrong signature gets `401` and never reaches out.

## Configuration

Read from the workload environment:

| Variable | Default | Meaning |
| --- | --- | --- |
| `WEBHOOK_SIGNING_SECRET` | a Cosmonic secret (see below) | HMAC key. Registered as a Cosmonic secret, never inline. Falls back to a built-in dev default under local `wash dev`. |
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

This is a required step before launch, not a one-click default: a webhook that verifies its callers needs a real secret. (For local `wash dev`, where no secret is injected, the component falls back to a built-in dev default so the example still runs.)

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

`wash dev` runs deny-all, so the forward is denied there by design (proof the gate is real). To see a forward succeed, deploy with an allow-list: apply [`manifests/workload.yaml`](manifests/workload.yaml) on Cosmonic Desktop, which sets `allowedHosts: [postman-echo.com]`:

```shell
BODY='{"event":"ping","from":"cosmonic"}'
SIG=$(printf '%s' "$BODY" | openssl dgst -sha256 -hmac "cosmonic-demo-secret" | sed 's/^.*= //')
curl -H 'Host: sandboxed-webhook.localhost' -X POST http://127.0.0.1:8200/ \
  -H 'content-type: application/json' \
  -H "x-hub-signature-256: sha256=$SIG" \
  -d "$BODY"
# {"verified":true,"forwarded_to":"https://postman-echo.com/post","downstream_status":200}
```

Change one byte of the body and the signature no longer matches: `401`, no egress.

## License

Apache-2.0.
