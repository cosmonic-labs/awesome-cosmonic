//! Observability wiring.
//!
//! Baseline (always on): structured JSON logs on **stderr** with tracing
//! spans. The wasmCloud / Cosmonic Desktop host captures component stderr and,
//! when OpenTelemetry is enabled on the host
//! (`WASMCLOUD_OBSERVABILITY_ENABLED=true` / Cosmonic Control's
//! `opentelemetry.workload`), exports it to the configured OTLP collector as
//! structured log records. Nothing in the guest talks to a collector directly,
//! which keeps the component portable and its capability surface minimal.
//!
//! Native OTEL (`--features wasi-otel`): spans created with `tracing` are
//! additionally reported to the host through the `wasi:otel@0.2.0-rc.2`
//! interfaces (via [`opentelemetry-wasi`]), where they join the host's own
//! traces — the request span is parented on the host's active span. Requires
//! a host that implements `wasi:otel` (wasmCloud v2 host plugin, Cosmonic
//! Control); plain `wasmtime serve` does not link it.
//!
//! W3C trace context: the host propagates `traceparent` on incoming HTTP
//! requests. [`request_span`] records it on the request span so guest logs can
//! be correlated with host traces in the collector even without `wasi-otel`.
//!
//! [`opentelemetry-wasi`]: https://github.com/bytecodealliance/opentelemetry-wasi

use std::sync::Once;

/// Initializes the tracing subscriber exactly once.
///
/// Log level is controlled with the standard `RUST_LOG` environment variable
/// (via `wasi:cli/environment`), defaulting to `info`.
pub fn init() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        use tracing_subscriber::layer::SubscriberExt as _;
        use tracing_subscriber::util::SubscriberInitExt as _;
        use tracing_subscriber::EnvFilter;

        let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
        let fmt_layer = tracing_subscriber::fmt::layer()
            .with_writer(std::io::stderr)
            .with_ansi(false)
            .json()
            .flatten_event(true)
            .with_current_span(true);

        let registry = tracing_subscriber::registry().with(filter).with(fmt_layer);

        #[cfg(feature = "wasi-otel")]
        let registry = registry.with(wasi_otel_layer());

        registry.init();
    });
}

/// Builds the `tracing` → `wasi:otel` bridge layer: spans stream to the host
/// per-span (`on-start`/`on-end`), no batching or background threads needed.
#[cfg(feature = "wasi-otel")]
fn wasi_otel_layer<S>() -> impl tracing_subscriber::Layer<S>
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    use opentelemetry::trace::TracerProvider as _;

    let provider = opentelemetry_sdk::trace::SdkTracerProvider::builder()
        .with_span_processor(opentelemetry_wasi::WasiSpanProcessor::new())
        .build();
    let tracer = provider.tracer(env!("CARGO_PKG_NAME"));
    opentelemetry::global::set_tracer_provider(provider);
    tracing_opentelemetry::layer().with_tracer(tracer)
}

/// Creates the root span for one MCP HTTP exchange.
///
/// Records the HTTP method, path, and (if present) the W3C `traceparent`
/// header injected by the host, so stderr log records can be joined with the
/// host's OTLP traces. With `wasi-otel` enabled the span is also parented on
/// the host's active span, joining the host trace natively.
pub fn request_span<B>(request: &http::Request<B>) -> tracing::Span {
    let traceparent = request
        .headers()
        .get("traceparent")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    let span = tracing::info_span!(
        "mcp.request",
        http.method = %request.method(),
        http.path = %request.uri().path(),
        mcp.method = request
            .headers()
            .get("mcp-method")
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default(),
        traceparent,
    );

    #[cfg(feature = "wasi-otel")]
    {
        use opentelemetry_wasi::WasiPropagator as _;
        use tracing_opentelemetry::OpenTelemetrySpanExt as _;
        let host_context = opentelemetry_wasi::TraceContextPropagator::new()
            .extract(&opentelemetry::Context::new());
        if let Err(err) = span.set_parent(host_context) {
            tracing::debug!("failed to parent span on host context: {err}");
        }
    }

    span
}
