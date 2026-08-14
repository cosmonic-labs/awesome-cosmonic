//! PII redactor MCP server — pure-compute, zero-egress redaction.
//!
//! One tool, [`redact`](PiiRedactorServer::redact), strips sensitive values
//! from text entirely on-device. It performs **no** outbound network calls, and
//! the workload ships with an empty `allowedHosts` (deny-all), so the text this
//! server sees can never be exfiltrated — the sandbox holds no network at all.

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Implementation, ServerCapabilities, ServerInfo};
use rmcp::{tool, tool_handler, tool_router, ErrorData, ServerHandler};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::redact::{self, RedactError};

/// PII redactor MCP server. Stateless per request.
#[derive(Clone)]
pub struct PiiRedactorServer {
    tool_router: ToolRouter<Self>,
}

/// Arguments for [`redact`](PiiRedactorServer::redact).
#[derive(Debug, Deserialize, JsonSchema)]
pub struct RedactParams {
    /// The text to redact. Capped at 256 KiB; larger input is rejected.
    pub text: String,
    /// Optional subset of PII categories to redact. When omitted, every
    /// category is scanned. Valid names: `email`, `us_ssn`, `phone`,
    /// `credit_card`, `ipv4`, `aws_access_key_id`.
    #[serde(default)]
    pub types: Option<Vec<String>>,
}

#[tool_router]
impl PiiRedactorServer {
    pub fn new() -> Self {
        Self {
            tool_router: Self::tool_router(),
        }
    }

    /// Redact PII from a string, on-device, with no network access.
    #[tool(
        description = "Redact personally identifiable information (PII) from text, entirely \
                       on-device with no network access. Detects and replaces emails \
                       ([REDACTED_EMAIL]), US SSNs ([REDACTED_SSN]), NANP phone numbers \
                       ([REDACTED_PHONE]), Luhn-valid credit card numbers ([REDACTED_CC]), IPv4 \
                       addresses ([REDACTED_IP]), and AWS access key ids ([REDACTED_AWS_KEY]). \
                       Pass 'text' to scan; pass an optional 'types' array to restrict the scan \
                       to specific categories (email, us_ssn, phone, credit_card, ipv4, \
                       aws_access_key_id). Returns the redacted text, per-category counts, and a \
                       total."
    )]
    #[tracing::instrument(name = "tool.redact", skip(self, params), fields(text_len = params.text.len()))]
    async fn redact(
        &self,
        Parameters(params): Parameters<RedactParams>,
    ) -> Result<CallToolResult, ErrorData> {
        match redact::redact(&params.text, params.types.as_deref()) {
            Ok(result) => Ok(CallToolResult::structured(serde_json::json!({
                "redacted": result.redacted,
                "counts": result.counts,
                "total": result.total,
            }))),
            Err(err @ RedactError::TooLarge { .. }) => {
                Err(ErrorData::invalid_params(err.to_string(), None))
            }
            Err(err @ RedactError::UnknownTypes { .. }) => {
                Err(ErrorData::invalid_params(err.to_string(), None))
            }
        }
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for PiiRedactorServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                env!("CARGO_PKG_NAME"),
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(
                "PII redactor: strips sensitive values from text entirely on-device. This server \
                 holds NO network access — its workload's outbound allowedHosts is empty \
                 (deny-all), so the text it sees cannot be exfiltrated. The single 'redact' tool \
                 detects and replaces six categories, each with a distinct placeholder: email \
                 ([REDACTED_EMAIL]), US SSN ([REDACTED_SSN]), phone ([REDACTED_PHONE]), credit \
                 card ([REDACTED_CC], validated with the Luhn checksum), IPv4 address \
                 ([REDACTED_IP]), and AWS access key id ([REDACTED_AWS_KEY]). Pass an optional \
                 'types' array to redact only chosen categories. Returns the redacted text, \
                 per-category counts, and a total.",
            )
    }
}
