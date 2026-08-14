//! Threat-intel MCP server — look up known vulnerabilities for an agent.
//!
//! Two tools (`lookup_package_vulnerabilities`, `get_vulnerability`) each make
//! an outbound HTTPS call to the OSV (Open Source Vulnerabilities) API (see
//! [`crate::osv`]). The only host they reach is `api.osv.dev`, which is also
//! the sole entry in the workload's outbound `allowedHosts` allowlist — the
//! egress boundary.
//!
//! No authentication is required: the OSV API is public and needs no API key.

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerInfo};
use rmcp::{tool, tool_handler, tool_router, ErrorData, ServerHandler};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::osv;

/// Threat-intel MCP server. Stateless per request.
#[derive(Clone)]
pub struct ThreatIntelServer {
    tool_router: ToolRouter<Self>,
}

/// Arguments for
/// [`lookup_package_vulnerabilities`](ThreatIntelServer::lookup_package_vulnerabilities).
#[derive(Debug, Deserialize, JsonSchema)]
pub struct LookupPackageParams {
    /// The OSV ecosystem name for the package. Case-sensitive, using OSV's
    /// spelling: `npm`, `PyPI`, `crates.io`, `Go`, `Maven`, `RubyGems`,
    /// `NuGet`, `Packagist`, `Pub`, `Hex`, etc.
    pub ecosystem: String,
    /// The package name as it appears in that ecosystem (e.g. `jinja2`,
    /// `lodash`, `log4j-core`).
    pub package: String,
    /// Optional exact package version (e.g. `2.4.1`). When given, only
    /// vulnerabilities affecting that version are returned; when omitted, all
    /// known vulnerabilities for the package are returned.
    #[serde(default)]
    pub version: Option<String>,
}

/// Arguments for [`get_vulnerability`](ThreatIntelServer::get_vulnerability).
#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetVulnerabilityParams {
    /// The OSV vulnerability identifier to fetch. Accepts an OSV id or any of
    /// its aliases: `GHSA-xxxx-xxxx-xxxx`, `CVE-2021-44228`,
    /// `RUSTSEC-2020-0001`, `PYSEC-2021-xxx`, `GO-2022-xxxx`, etc.
    pub id: String,
}

#[tool_router]
impl ThreatIntelServer {
    pub fn new() -> Self {
        Self {
            tool_router: Self::tool_router(),
        }
    }

    /// Look up known vulnerabilities for one open-source package.
    #[tool(
        description = "Look up known vulnerabilities for an open-source package in the OSV \
                       database. Requires the OSV 'ecosystem' (e.g. PyPI, npm, crates.io, Go, \
                       Maven, RubyGems, NuGet) and 'package' name; pass an exact 'version' to \
                       filter to advisories affecting that version. Returns each matching \
                       advisory's id, summary, aliases (CVE/GHSA), CVSS severity, affected \
                       version ranges, and reference URLs — or a clear 'no known vulnerabilities' \
                       result."
    )]
    #[tracing::instrument(name = "tool.lookup_package_vulnerabilities", skip(self))]
    async fn lookup_package_vulnerabilities(
        &self,
        Parameters(params): Parameters<LookupPackageParams>,
    ) -> Result<CallToolResult, ErrorData> {
        match osv::lookup_package_vulnerabilities(
            &params.ecosystem,
            &params.package,
            params.version.as_deref(),
        )
        .await
        {
            Ok(value) => Ok(structured_text(value)),
            Err(err) => Ok(err.into_tool_result()),
        }
    }

    /// Fetch the full record for one vulnerability id.
    #[tool(
        description = "Fetch the full OSV record for a single vulnerability id or alias \
                       (GHSA-…, CVE-…, RUSTSEC-…, PYSEC-…, GO-…). Returns id, summary, details \
                       (truncated), aliases, CVSS severity, the affected ecosystems/packages \
                       with fixed versions, and reference URLs. Unknown ids return a friendly \
                       'no such vulnerability id' message."
    )]
    #[tracing::instrument(name = "tool.get_vulnerability", skip(self))]
    async fn get_vulnerability(
        &self,
        Parameters(params): Parameters<GetVulnerabilityParams>,
    ) -> Result<CallToolResult, ErrorData> {
        match osv::get_vulnerability(&params.id).await {
            Ok(value) => Ok(structured_text(value)),
            Err(err) => Ok(err.into_tool_result()),
        }
    }
}

/// Emits a JSON value as both `structuredContent` and a pretty-printed text
/// block (so plain clients see readable output).
fn structured_text(value: serde_json::Value) -> CallToolResult {
    let text = serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string());
    let mut result = CallToolResult::structured(value);
    result.content = vec![ContentBlock::text(text)];
    result
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for ThreatIntelServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                env!("CARGO_PKG_NAME"),
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(
                "Look up known vulnerabilities for open-source software from the OSV (Open Source \
                 Vulnerabilities) database. lookup_package_vulnerabilities takes an OSV ecosystem \
                 (PyPI, npm, crates.io, Go, Maven, RubyGems, NuGet, …), a package name, and an \
                 optional exact version, and returns the matching advisories (id, summary, \
                 CVE/GHSA aliases, CVSS severity, affected version ranges, references) or a clear \
                 'no known vulnerabilities' result. get_vulnerability fetches one advisory's full \
                 record by its OSV id or alias (GHSA-…, CVE-…, RUSTSEC-…, PYSEC-…, GO-…). This \
                 server reaches only api.osv.dev and needs no API key.",
            )
    }
}
