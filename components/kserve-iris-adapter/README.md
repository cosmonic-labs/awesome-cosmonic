# kserve-iris-adapter

An experimental reusable WASI HTTP component that validates Iris feature rows,
constructs a KServe V2 tensor request, and returns the predictor response. This
is an Iris-shaped adapter, not a universal KServe proxy or complete V2 server.
The browser playground and cluster deployment live in
[cosmonic/gitops](https://github.com/cosmonic/gitops/pull/144).

## Build and run locally

Requires Rust 1.96.0 or later, the `wasm32-wasip2` target, current `wash`, and
Python 3 for the optional protocol fixture. Tested with wash 2.11.0.

```sh
rustup target add wasm32-wasip2
cargo test --locked
wash build
python3 mock_predictor.py
```

In another terminal in this directory run `wash dev`, then:

```sh
curl --fail-with-body http://localhost:8000/classify \
  -H 'Content-Type: application/json' \
  -d '{"instances":[[5.1,3.5,1.4,0.2],[6.2,3.4,5.4,2.3]]}'
```

The fixture returns zero for every class and performs no inference. It is for
local protocol checks only and is not included in the Wasm artifact. Stop it
and forward a real compatible predictor to port 8081 to test model inference.
The component exports `wasi:http/incoming-handler@0.2.12`.

## Configuration and contract

The operator sets `KSERVE_INFER_URL` to the full inference URL, such as
`http://sklearn-iris-predictor.models.svc.cluster.local/v2/models/sklearn-iris/infer`,
and grants that authority in `allowedHosts`. Caller input never selects the
upstream. No model authentication is implemented; authenticated predictors need
a trusted gateway or host-mediated provider. Do not put credentials in URLs.

`POST /classify` accepts only `instances`: 1–128 rows of four finite,
nonnegative FP32 numbers in sepal-length, sepal-width, petal-length,
petal-width order. The request becomes `input-0`, datatype `FP32`, shape
`[N,4]`, with row-major data. Confirm the model metadata matches that contract.
`GET /healthz` reports local adapter liveness, not predictor readiness.

Request and response bodies are limited while reading to 16 KiB and 1 MiB.
Connect, first-byte, and inter-byte timeouts are 5, 60, and 10 seconds. There is
no single overall deadline or automatic retry. Upstream HTTP errors preserve
the status with a sanitized error; transport and malformed response errors
return 502. Successful responses must contain an `outputs` array.

## Published component

The repository's `components` workflow builds, validates, tests, and publishes
`ghcr.io/cosmonic-labs/components/kserve-iris-adapter`. Main publishes a
`sha-<12-character-commit>` tag; a `kserve-iris-adapter-v0.1.0` release tag
publishes `0.1.0`. The workflow emits the registry digest and provenance.
Neither tag exists merely because a PR has been opened.

Use the workflow's actual `sha256:…` digest in deployment manifests. The
GitOps playground chart accepts `component.repository`, `component.tag`, and
`component.digest`; the digest takes precedence. It runs the component as a
Control-managed WorkloadDeployment with a selectorless Kubernetes Service,
while nginx serves the UI separately. No embedded `wash dev` container is needed.

KServe owns model serving; this component owns application validation and
adaptation. The separate `cosmonic:agent` model-source path uses generative
OpenAI-compatible endpoints, not predictive V2 tensor translation.
