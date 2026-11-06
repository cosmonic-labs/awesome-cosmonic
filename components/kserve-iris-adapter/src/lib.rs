#![cfg(any(target_arch = "wasm32", test))]

use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Features {
    instances: Vec<[f32; 4]>,
}

fn inference_request(bytes: &[u8]) -> Result<Value, &'static str> {
    let features: Features =
        serde_json::from_slice(bytes).map_err(|_| "expected instances of four finite numbers")?;
    if features.instances.is_empty() || features.instances.len() > 128 {
        return Err("expected 1 to 128 instances");
    }
    if features
        .instances
        .iter()
        .flatten()
        .any(|n| !n.is_finite() || *n < 0.0)
    {
        return Err("features must be finite and nonnegative");
    }
    Ok(
        json!({"inputs": [{"name": "input-0", "shape": [features.instances.len(), 4],
        "datatype": "FP32", "data": features.instances.into_iter().flatten().collect::<Vec<_>>()}]}),
    )
}

#[cfg(target_arch = "wasm32")]
mod server {
    use super::*;
    use http_body_util::{BodyExt, Limited};
    use wstd::http::{Body, Client, Method, Request, Response};

    fn reply(status: u16, body: Value) -> Result<Response<Body>, wstd::http::Error> {
        Ok(Response::builder()
            .status(status)
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))?)
    }

    #[wstd::http_server]
    async fn main(req: Request<Body>) -> Result<Response<Body>, wstd::http::Error> {
        if req.method() == Method::GET && req.uri().path() == "/healthz" {
            return reply(200, json!({"status": "live"}));
        }
        if req.uri().path() != "/classify" {
            return reply(404, json!({"error": "unknown route"}));
        }
        if req.method() != Method::POST {
            return reply(405, json!({"error": "POST required"}));
        }
        let bytes = match Limited::new(req.into_body().into_boxed_body(), 16 * 1024)
            .collect()
            .await
        {
            Ok(body) => body.to_bytes(),
            Err(_) => return reply(413, json!({"error": "body unreadable or exceeds 16 KiB"})),
        };
        let payload = match inference_request(&bytes) {
            Ok(payload) => payload,
            Err(error) => return reply(400, json!({"error": error})),
        };
        // The operator owns the full URL; caller input never selects an upstream.
        let url = match std::env::var("KSERVE_INFER_URL") {
            Ok(url) if url.starts_with("http://") || url.starts_with("https://") => url,
            _ => return reply(503, json!({"error": "KSERVE_INFER_URL is not configured"})),
        };
        let payload = payload.to_string();
        let outgoing = Request::post(url)
            .header("content-type", "application/json")
            .header("content-length", payload.len())
            .body(Body::from(payload))?;
        let mut client = Client::new();
        client.set_connect_timeout(std::time::Duration::from_secs(5));
        client.set_first_byte_timeout(std::time::Duration::from_secs(60));
        client.set_between_bytes_timeout(std::time::Duration::from_secs(10));
        let response = match client.send(outgoing).await {
            Ok(response) => response,
            Err(_) => return reply(502, json!({"error": "inference transport failed"})),
        };
        let status = response.status();
        let bytes = match Limited::new(response.into_body().into_boxed_body(), 1024 * 1024)
            .collect()
            .await
        {
            Ok(body) => body.to_bytes(),
            Err(_) => {
                return reply(
                    502,
                    json!({"error": "inference body unreadable or exceeds 1 MiB"}),
                );
            }
        };
        if !status.is_success() {
            return reply(
                status.as_u16(),
                json!({"error": "inference refused", "upstream_status": status.as_u16()}),
            );
        }
        match serde_json::from_slice::<Value>(&bytes) {
            Ok(body) if body.get("outputs").is_some_and(Value::is_array) => reply(200, body),
            _ => reply(502, json!({"error": "invalid inference response"})),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flattens_batch_in_row_order() {
        let request = inference_request(br#"{"instances":[[1,2,3,4],[5,6,7,8]]}"#).unwrap();
        assert_eq!(request["inputs"][0]["shape"], json!([2, 4]));
        assert_eq!(
            request["inputs"][0]["data"],
            json!([1., 2., 3., 4., 5., 6., 7., 8.])
        );
        assert_eq!(request["inputs"][0]["datatype"], "FP32");
    }

    #[test]
    fn rejects_invalid_and_unbounded_inputs() {
        for input in [
            r#"{"instances":[]}"#,
            r#"{"instances":[[1,2,3]]}"#,
            r#"{"instances":[[-1,2,3,4]]}"#,
            r#"{"instances":[[1e100,2,3,4]]}"#,
            r#"{"instances":[[1,2,3,4]],"url":"http://attacker"}"#,
        ] {
            assert!(inference_request(input.as_bytes()).is_err(), "{input}");
        }
        assert!(
            inference_request(
                json!({"instances": vec![[1.;4];129]})
                    .to_string()
                    .as_bytes()
            )
            .is_err()
        );
    }
}
