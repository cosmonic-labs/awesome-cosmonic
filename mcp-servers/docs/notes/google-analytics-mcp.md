# google-analytics-mcp — deferred (design notes)

**Status:** DEFERRED (2026-09-02). **Reason:** the only auth path Google offers a
*user* (Application Default Credentials / OAuth 2.0 authorization-code flow with a
loopback callback) needs an interactive browser round-trip and a callback
server, which the sandboxed component cannot run. The two non-interactive paths
(service-account JWT-bearer, or a pre-minted refresh token exchanged over plain
HTTPS POST) are in scope for the platform, and everything below is designed
around them; the deferral is purely a "not in this batch" decision, not a
platform blocker. Google Workspace (`google-workspace-mcp`) is deferred for the
same reason and should share the auth module described in §4.

Researched 2026-09-02 from primary sources (Google API reference pages, the
official repo, PyPI). Everything needed to build without re-researching is here.

---

## 1. What exists upstream (2026-09)

| Project | License | Lang / transport | Auth | Tools | Notes |
|---|---|---|---|---|---|
| [googleanalytics/google-analytics-mcp](https://github.com/googleanalytics/google-analytics-mcp) (**official**, PyPI `analytics-mcp`) | Apache-2.0 | Python 3.10+, stdio (`pipx run analytics-mcp`) | ADC: `gcloud auth application-default login --scopes analytics.readonly,cloud-platform` or `GOOGLE_APPLICATION_CREDENTIALS` (service-account JSON); needs `GOOGLE_PROJECT_ID` (quota project) | `get_account_summaries`, `get_property_details`, `list_google_ads_links`, `list_property_annotations` (Admin v1alpha), `run_report`, `run_realtime_report`, `run_funnel_report` (Data v1alpha), `get_custom_dimensions_and_metrics` | v0.7.0 (2026-07-29); v0.2.0 2026-03-11 … v0.6.0 2026-05-21. Marked "experimental"; **read-only by design**. Calls Data API v1beta (funnel: v1alpha) and Admin API v1beta (annotations: v1alpha). |
| [surendranb/google-analytics-mcp](https://github.com/surendranb/google-analytics-mcp) | MIT | Python, stdio (+npm wrapper) | service-account JSON via `GOOGLE_APPLICATION_CREDENTIALS` or ADC | `get_ga4_data` (dims/metrics/date_ranges/limit, JSON or Markdown), `list_accounts`, `list_properties`, `get_property_metadata`, `run_realtime_report`, plus skills tools | ~240 stars; most-used community server; ships a "schema discovery + safe defaults" skill approach worth copying. |
| [luminarylane/ga4-mcp](https://github.com/luminarylane/ga4-mcp) | MIT | Python 3.11+, stdio | service-account key file `GA4_CREDENTIALS_PATH`, default `GA4_PROPERTY_ID` | `ga4_list_properties`, `ga4_get_report`, `ga4_get_top_pages`, `ga4_get_traffic_sources`, `ga4_get_conversions`, `ga4_get_realtime`, `ga4_compare_periods` | Good example of "preset" convenience tools. |
| [gomarble-ai/google-analytics-mcp-server](https://github.com/gomarble-ai/google-analytics-mcp-server) | MIT | Python, stdio or HTTP | OAuth 2.0 browser flow, stores `google_analytics_token.json`, auto-refresh | `list_properties`, `get_page_views`, `get_active_users`, `get_events`, `get_traffic_sources`, `get_device_metrics`, `run_report` | 18 stars. Its refresh-token handling is the shape we need (minus the browser step). |
| [ruchernchong/mcp-server-google-analytics](https://github.com/ruchernchong/mcp-server-google-analytics) | MIT | TypeScript, stdio | `GOOGLE_CLIENT_EMAIL` + `GOOGLE_PRIVATE_KEY` (service account) + `GA_PROPERTY_ID` | `runReport`, `getPageViews`, `getActiveUsers`, `getEvents`, `getUserBehavior` | **Archived 2025-10-05.** Env-var-only service-account config is exactly our shape. |
| [harshfolio/mcp-server-ga4](https://github.com/harshfolio/mcp-server-ga4) | MIT | Python, stdio | ADC | `run-report`, `run-realtime-report`, `get-metadata` | **Archived 2025-11-01.** |

Borrow: the official server's tool surface and parameter names (Apache-2.0,
attribution in README); surendranb's "discover schema first, small defaults"
skill guidance (MIT); ruchernchong's env-var service-account config (MIT).
No code is borrowed as such — our port is Rust from the template.

## 2. Upstream API facts (verified 2026-09-02)

### Data API v1beta — host `analyticsdata.googleapis.com`

| Method | Call | Notes |
|---|---|---|
| runReport | `POST /v1beta/properties/{id}:runReport` | body: `dateRanges[]` (≤4; `startDate`/`endDate` as `YYYY-MM-DD`, `today`, `yesterday`, `NdaysAgo`), `dimensions[]{name}` (≤9), `metrics[]{name}` (≤10), `dimensionFilter`/`metricFilter` (FilterExpression: `filter{fieldName, stringFilter{matchType: EXACT\|BEGINS_WITH\|ENDS_WITH\|CONTAINS\|FULL_REGEXP\|PARTIAL_REGEXP, value, caseSensitive}, inListFilter{values[]}, numericFilter{operation, value{int64Value\|doubleValue}}, betweenFilter}`, `andGroup`, `orGroup`, `notExpression`), `orderBys[]{metric{metricName}\|dimension{dimensionName}, desc}`, `limit` (default 10 000, max 250 000), `offset`, `keepEmptyRows`, `currencyCode`, `metricAggregations[]` (`TOTAL`,`MINIMUM`,`MAXIMUM`,`COUNT`), `returnPropertyQuota`. Response: `dimensionHeaders[]`, `metricHeaders[]{name,type}`, `rows[]{dimensionValues[]{value}, metricValues[]{value}}`, `rowCount` (total, for paging), `totals[]`, `metadata{currencyCode,timeZone,dataLossFromOtherRow,…}`, `propertyQuota`. |
| runRealtimeReport | `POST /v1beta/properties/{id}:runRealtimeReport` | no dateRanges; `minuteRanges[]{startMinutesAgo (default 29, max 29 std / 59 GA360), endMinutesAgo}` ≤2; same dims/metrics/filters/orderBys/limit (default 10 000, max 250 000); only realtime-compatible fields (`activeUsers`, `eventCount`, `screenPageViews`, `keyEvents`; dims `country`, `city`, `deviceCategory`, `unifiedScreenName`, `eventName`, `platform`, `minutesAgo`, …). |
| getMetadata | `GET /v1beta/properties/{id}/metadata` | `properties/0/metadata` = universal (no custom fields). Response `dimensions[]`/`metrics[]{apiName, uiName, description, customDefinition, category, type, deprecatedApiNames[]}`, `comparisons[]`. Custom fields appear as `customEvent:<param>`, `customUser:<param>`, `customItem:<param>`. |
| checkCompatibility | `POST /v1beta/properties/{id}:checkCompatibility` | body `dimensions[]`, `metrics[]`, filters, `compatibilityFilter` (`COMPATIBLE`\|`INCOMPATIBLE`); response `dimensionCompatibilities[]{dimensionMetadata, compatibility}`, `metricCompatibilities[]`. Cheap way to pre-validate a report. |
| runFunnelReport | `POST /v1alpha/properties/{id}:runFunnelReport` | **v1alpha** — `funnel{steps[]{name, filterExpression}, isOpenFunnel}`, `funnelBreakdown`, `funnelNextAction`, `funnelVisualizationType`, `segments[]` (≤4), `dateRanges`, `limit`. Response `funnelTable`, `funnelVisualization`. Optional for v2 of our server. |

Scopes: `https://www.googleapis.com/auth/analytics.readonly` (all of the above).

Quotas (standard property): 200 000 tokens/day, 40 000/hour, 14 000/hour per
project-per-property; 10 concurrent requests; **10 server errors (500/503) per
hour** — after that the property is locked out for the hour, so never
hot-loop retries. `returnPropertyQuota: true` adds
`propertyQuota{tokensPerDay,tokensPerHour,concurrentRequests,serverErrorsPerProjectPerHour,potentiallyThresholdedRequestsPerHour}{consumed,remaining}`.
Daily quotas reset at midnight Pacific. Realtime shares the same quotas.

Errors (all Google APIs): `{"error":{"code":<http>,"message":…,"status":<CANONICAL>,"details":[…]}}`.

### Admin API v1beta — host `analyticsadmin.googleapis.com`

| Method | Call | Notes |
|---|---|---|
| accountSummaries.list | `GET /v1beta/accountSummaries?pageSize=200&pageToken=` | pageSize default 50, max 200. `accountSummaries[]{name, account, displayName, propertySummaries[]{property: "properties/<id>", displayName, propertyType (PROPERTY_TYPE_ORDINARY\|SUBPROPERTY\|ROLLUP), parent}}`, `nextPageToken`. The one call that turns "which property?" into an id. |
| properties.get | `GET /v1beta/properties/{id}` | `Property{name, propertyType, createTime, updateTime, parent, displayName, industryCategory, timeZone, currencyCode, serviceLevel (GOOGLE_ANALYTICS_STANDARD\|_360), account, dataRetentionSettings?}`. timeZone matters for date ranges. |
| properties.customDimensions.list | `GET /v1beta/properties/{id}/customDimensions?pageSize=200` | `customDimensions[]{name, parameterName, displayName, description, scope (EVENT\|USER\|ITEM), disallowAdsPersonalization}`. Data API name = `custom<Scope>:<parameterName>`. |
| properties.customMetrics.list | `GET /v1beta/properties/{id}/customMetrics?pageSize=200` | `customMetrics[]{name, parameterName, displayName, description, measurementUnit, scope, restrictedMetricType[]}`. Data API name = `customEvent:<parameterName>`. |
| properties.googleAdsLinks.list | `GET /v1beta/properties/{id}/googleAdsLinks` | `googleAdsLinks[]{name, customerId, canManageClients, adsPersonalizationEnabled, createTime}`. Low value; include only if cheap. |

Scopes: `analytics.readonly` (or `analytics.edit`, not needed).

### Token endpoint — host `oauth2.googleapis.com`

`POST https://oauth2.googleapis.com/token` (`application/x-www-form-urlencoded`).

- **Service account (JWT-bearer):** `grant_type=urn:ietf:params:oauth:grant-type:jwt-bearer&assertion=<JWT>`.
  JWT header `{"alg":"RS256","typ":"JWT"}` (optional `kid` = `private_key_id`);
  claims `{iss: client_email, scope: "https://www.googleapis.com/auth/analytics.readonly", aud: "https://oauth2.googleapis.com/token", iat, exp (≤ iat+3600)}`;
  signature RSASSA-PKCS1-v1_5/SHA-256 with the key JSON's `private_key`
  (PEM, PKCS#8 `BEGIN PRIVATE KEY`). Response `{access_token, token_type:"Bearer", expires_in: 3600, scope}`.
- **Refresh token:** `grant_type=refresh_token&client_id=…&client_secret=…&refresh_token=…`.
  Same response (no new refresh token). Errors: `400 {"error":"invalid_grant"}`
  (revoked / expired — OAuth clients in *Testing* publishing status get
  7-day refresh tokens; 100 refresh tokens per user per client, oldest
  silently revoked), `401 {"error":"invalid_client"}` (wrong id/secret).

## 3. Concrete design for Cosmonic Desktop

Name: `google-analytics-mcp` (crate `google_analytics_mcp`, wasm
`google_analytics_mcp.wasm`, ingress `http://google-analytics-mcp.localhost:8200/`,
skill `skill://google-analytics-mcp/SKILL.md`). Labels as the template
(`mcp.ai/domain: "google-analytics"`, `mcp.ai/auth-type: none`).

### 3.1 Configuration (per CONVENTIONS.md)

| Env var | Kind | Required | Default | Purpose |
|---|---|---|---|---|
| `GOOGLE_AUTH_MODE` | named config | no | `service_account` | `service_account` \| `refresh_token`. Selects which secrets are read. |
| `GOOGLE_SERVICE_ACCOUNT_KEY` | secret (`google-analytics-mcp-service-account-key`) | when mode=service_account | — | The **whole service-account key JSON** (one env value; contains `client_email`, `private_key`, `private_key_id`, `token_uri`). One secret instead of three keeps registration to one command. |
| `GOOGLE_OAUTH_CLIENT_ID` | named config | when mode=refresh_token | — | Desktop-app OAuth client id (not secret per Google for installed apps, but fine either way). |
| `GOOGLE_OAUTH_CLIENT_SECRET` | secret (`google-analytics-mcp-oauth-client-secret`) | when mode=refresh_token | — | OAuth client secret. |
| `GOOGLE_OAUTH_REFRESH_TOKEN` | secret (`google-analytics-mcp-refresh-token`) | when mode=refresh_token | — | Refresh token minted once out-of-band (§5). |
| `GA_DEFAULT_PROPERTY_ID` | named config | no | — | Numeric property id used when a tool call omits `property_id`. |
| `GA_DATA_BASE_URL` | named config | no | `https://analyticsdata.googleapis.com` | e2e override. |
| `GA_ADMIN_BASE_URL` | named config | no | `https://analyticsadmin.googleapis.com` | e2e override. |
| `GOOGLE_TOKEN_URL` | named config | no | `https://oauth2.googleapis.com/token` | e2e override (also overrides the JWT `aud`). |
| `GA_MAX_ROWS` | named config | no | `1000` | Hard clamp on `limit` for report tools (upstream max 250 000; agents never need that). |
| `RUST_LOG`, `MCP_ALLOWED_HOSTS` | named config | — | `info`, `google-analytics-mcp.localhost` | template. |

Secret registration (service-account mode):

```console
$ curl --unix-socket "$SOCK" -X POST http://localhost/v1/secrets/refs \
    -H 'Content-Type: application/json' \
    -d "{\"name\":\"google-analytics-mcp-service-account-key\",\"uri\":\"keychain://cosmonic/google-analytics-mcp-service-account-key\",\"env\":\"GOOGLE_SERVICE_ACCOUNT_KEY\",\"value\":$(jq -c . sa-key.json | jq -Rs .)}"
```

Manifest fragment:

```yaml
localResources:
  environment:
    config:
      RUST_LOG: info
      MCP_ALLOWED_HOSTS: "google-analytics-mcp.localhost"
      GOOGLE_AUTH_MODE: service_account
      GA_DEFAULT_PROPERTY_ID: "123456789"
    secretFrom:
      - name: google-analytics-mcp-service-account-key
  allowedHosts:
    - "https://analyticsdata.googleapis.com"
    - "https://analyticsadmin.googleapis.com"
    - "https://oauth2.googleapis.com"
```

Grants: none (no loopback ports, no volumes, no hostInterfaces beyond
wasi:http ingress). Everything is outbound HTTPS to Google (webpki roots OK).

### 3.2 Auth module (shared with google-workspace-mcp)

`src/google_auth.rs` — copy-paste identical between the two servers (or a tiny
workspace crate `google-auth-wasi` under `mcp-servers/` later):

```rust
pub enum Mode { ServiceAccount { key: ServiceAccountKey }, RefreshToken { client_id, client_secret, refresh_token } }
pub struct TokenCache { access_token: String, expires_at: u64 }   // static Mutex<Option<TokenCache>>; survives on a warm instance (poolSize 1)
pub async fn access_token(scopes: &[&str]) -> Result<String, AuthError>  // refresh when < 60 s left
```

- Service account: build header+claims, base64url, sign with `rsa` crate
  (`rsa::pkcs8::DecodePrivateKey` for the PEM, `rsa::pkcs1v15::SigningKey<sha2::Sha256>`,
  `signature::Signer` — deterministic, no RNG needed, pure Rust, compiles for
  `wasm32-wasip2`). `iat` from `std::time::SystemTime::now()` (wasi clocks
  work). `exp = iat + 3600`. POST form to `GOOGLE_TOKEN_URL` via
  `bridge::outbound::fetch`. Avoid `jsonwebtoken` (pulls `ring`, awkward on
  wasip2) and `openssl`.
- Refresh token: POST form with the four fields; same cache.
- Map `invalid_grant` → actionable message (re-mint refresh token / key
  deleted); `invalid_client` → client id/secret mismatch; missing env → the
  CONVENTIONS "missing secret" message naming the ref.
- Scope for GA: `https://www.googleapis.com/auth/analytics.readonly`.
  Workspace would pass its own scopes; the JWT `scope` claim is space-joined.

### 3.3 Tool surface

All read-only (Google's own server is read-only by policy; nothing gated).
`property_id` accepts `123456789` or `properties/123456789`; falls back to
`GA_DEFAULT_PROPERTY_ID`, else a clear error listing `list_properties`.

| Tool | Upstream | Params | Clamps / behaviour |
|---|---|---|---|
| `list_properties` | Admin `GET /v1beta/accountSummaries?pageSize=200` (+ follow `nextPageToken` up to 5 pages) | none | Flattened table: account id/name → property id/name/type. First tool to call. |
| `get_property` | Admin `GET /v1beta/properties/{id}` | `property_id?` | Returns displayName, timeZone, currencyCode, serviceLevel, createTime, industryCategory. |
| `get_metadata` | Data `GET /v1beta/properties/{id}/metadata` | `property_id?`, `search?` (substring on apiName/uiName), `kind?` (dimensions\|metrics\|all), `custom_only?` | Response is ~800 entries; default cap 100 rows after filtering; returns apiName, uiName, type, category, customDefinition. `property_id=0` allowed for universal metadata. |
| `list_custom_definitions` | Admin `GET /v1beta/properties/{id}/customDimensions` + `/customMetrics` (pageSize=200) | `property_id?` | Emits the exact Data-API names (`customEvent:x`) next to display names. |
| `check_compatibility` | Data `POST /v1beta/properties/{id}:checkCompatibility` | `property_id?`, `dimensions[]`, `metrics[]`, `dimension_filter?`, `metric_filter?` | `compatibilityFilter: INCOMPATIBLE` by default so the answer is short. |
| `run_report` | Data `POST /v1beta/properties/{id}:runReport` | `property_id?`, `date_ranges[]{start_date,end_date,name?}` (default `[{28daysAgo, yesterday}]`), `dimensions[]` (≤9), `metrics[]` (1..10), `dimension_filter?`, `metric_filter?` (pass-through FilterExpression JSON), `order_bys?`, `limit?` (default 50, clamp 1..GA_MAX_ROWS), `offset?` (≥0), `keep_empty_rows?`, `currency_code?`, `return_quota?` | Always sets `returnPropertyQuota: true` internally and appends a one-line quota summary; renders as a Markdown table + `rowCount` and next `offset` hint. Rejects >4 date ranges / >9 dims / >10 metrics client-side with the upstream-style message. |
| `run_realtime_report` | Data `POST /v1beta/properties/{id}:runRealtimeReport` | `property_id?`, `dimensions[]?`, `metrics[]` (default `["activeUsers"]`), `minute_ranges?[]{start_minutes_ago,end_minutes_ago}` (≤2; clamp 0..29), `dimension_filter?`, `order_bys?`, `limit?` (default 50, clamp) | Same rendering. |
| `run_funnel_report` (v2, optional) | Data `POST /v1alpha/properties/{id}:runFunnelReport` | `property_id?`, `date_ranges[]`, `steps[]{name, event_name}` (sugar → `filterExpression.funnelEventFilter.eventName`), `breakdown_dimension?`, `limit?` | v1alpha; include only if a user asks. |

Preset convenience tools (`top_pages`, `traffic_sources`, `compare_periods`)
are deliberately *not* tools — they are SKILL.md recipes over `run_report`
(the skill shows the exact dims/metrics/orderBys), which keeps the tool list
short and the recipes editable without a rebuild.

### 3.4 Skill points (what an agent gets wrong without SKILL.md)

1. **Property id first.** Every Data-API call needs a numeric GA4 property id
   (`properties/123456789`), not the `G-XXXXXXX` measurement id, not the
   account id, not a UA `UA-…` id. Call `list_properties`; if the account has
   several properties, ask the user which. A `G-` id in `property_id` yields
   `400 INVALID_ARGUMENT` "Property ID … is not a valid property".
2. **Discover names before reporting.** Dimension/metric names are exact
   camelCase apiNames (`screenPageViews`, not `pageviews`; `sessionSource`,
   not `source`; `keyEvents` — `conversions` is the legacy alias since the
   2024 rename). Custom fields are `customEvent:<param>` / `customUser:<param>`.
   Unknown names → `400 INVALID_ARGUMENT "Field <x> is not a valid dimension"`.
   Use `get_metadata search=…` or `list_custom_definitions`.
3. **Compatibility.** Not every dimension goes with every metric (e.g. item-
   scoped `itemName` with `sessions`, or user-scoped with event-scoped).
   Failure is `400` "The dimensions and metrics are incompatible". Run
   `check_compatibility` when mixing scopes.
4. **Dates.** `startDate`/`endDate` are `YYYY-MM-DD`, `today`, `yesterday`,
   `NdaysAgo` — no ISO datetimes, no "last week". The `date` dimension comes
   back as `YYYYMMDD`. Dates are in the property's `timeZone` (`get_property`).
   Today's data is partial; GA4 intraday freshness is hours, so prefer
   `yesterday` as the end date for stable numbers.
5. **Realtime is a different schema.** `run_realtime_report` has no date
   ranges, only `minuteRanges` (last 30 min standard / 60 min GA360) and a
   small set of compatible fields; `sessions`/`pagePath` are *not* realtime
   fields (`unifiedScreenName` is).
6. **Quota is per property and shared with the GA UI/other tools.** A
   report with many dimensions and a long date range costs many tokens;
   `propertyQuota` in the result says how much is left. `429
   RESOURCE_EXHAUSTED` → wait for the hourly reset; do not retry in a loop.
   500/503 count against a 10-per-hour server-error quota — one retry with
   backoff, then stop.
7. **Paging.** `rowCount` is the total; page with `offset` + `limit`. The
   server clamps `limit` to `GA_MAX_ROWS`; the agent should aggregate with
   `orderBys` + a small limit rather than paging through everything.
8. **Filters are JSON FilterExpression objects**, not strings:
   `{"filter":{"fieldName":"pagePath","stringFilter":{"matchType":"BEGINS_WITH","value":"/blog"}}}`.
   `metricFilter` applies after aggregation (HAVING); `dimensionFilter`
   before (WHERE).
9. **`(other)` row.** High-cardinality reports may collapse into an
   `(other)` row (`metadata.dataLossFromOtherRow: true`); narrow the date
   range or add a filter rather than trusting the totals.

### 3.5 Error catalogue

| Condition | Meaning | Action |
|---|---|---|
| `GOOGLE_SERVICE_ACCOUNT_KEY` / refresh-token env unset | secret not registered or not listed in `secretFrom` | Register `google-analytics-mcp-service-account-key` (§5) and redeploy. Server returns the actionable message, never a 401. |
| Key JSON parses but `private_key` isn't PKCS#8 PEM | wrong file (e.g. OAuth client JSON `{"installed":…}` instead of a service-account key) | Download a *service account* key (`"type":"service_account"`). |
| token endpoint `400 invalid_grant` (service account) | clock skew > few min, key deleted/disabled, `aud` mismatch | Check key still exists in IAM; check host clock. |
| token endpoint `400 invalid_grant` (refresh token) | token revoked, expired (7 days if OAuth consent screen is in *Testing*), or >100 tokens minted | Re-mint the refresh token (§5.2); publish the OAuth app to *Production* (internal) to stop 7-day expiry. |
| token endpoint `401 invalid_client` | client id/secret mismatch | Fix `GOOGLE_OAUTH_CLIENT_ID` / secret ref. |
| `401 UNAUTHENTICATED` from Data/Admin API | access token expired/invalid (cache stale) | Server drops cache and retries once automatically; if persistent, scope missing → re-mint with `analytics.readonly`. |
| `403 PERMISSION_DENIED` "User does not have sufficient permissions for this property" | the service-account email (or OAuth user) is not added to the GA property | GA Admin → Property → Property access management → add the `client_email` as **Viewer**. |
| `403 PERMISSION_DENIED` "Google Analytics Data API has not been used in project … or it is disabled" | API not enabled in the key's GCP project | Enable *Google Analytics Data API* and *Google Analytics Admin API* in that project. |
| `400 INVALID_ARGUMENT` "Field X is not a valid dimension/metric" | bad apiName | `get_metadata search=X`. |
| `400 INVALID_ARGUMENT` "… are incompatible" | dimension/metric scope clash | `check_compatibility`, drop the offending field. |
| `400 INVALID_ARGUMENT` "Property ID … is not a valid property" / 404 | measurement id, UA id, or wrong number | `list_properties`. |
| `429 RESOURCE_EXHAUSTED` | tokens/hour, tokens/day, concurrent (10), or server-error quota hit | Stop; report `propertyQuota`; hourly quota refills within the hour, daily at midnight PT. |
| `500 INTERNAL` / `503 UNAVAILABLE` | transient | One retry after 2 s; each counts against the 10/h server-error quota. |
| `dataLossFromOtherRow: true` in metadata | cardinality collapse | Narrow the query. |

## 4. Shared Google auth with google-workspace-mcp

Both servers need the same two token flows and the same error mapping; only
scopes and hosts differ. Build `google_auth.rs` once with:

- `Scopes` parameter (GA: `analytics.readonly`; Workspace: `gmail.readonly`,
  `calendar.readonly`, `drive.readonly`, …),
- `sub` claim support (domain-wide delegation — Workspace needs it to act as
  a user; GA does not),
- the same env-var names prefixed `GOOGLE_*` (mode, SA key JSON, OAuth client
  id/secret, refresh token) so a user with one OAuth client can register the
  same secret values under two ref names, and the `oauth2.googleapis.com`
  allowedHost entry.

Do the GA server first: its API is smaller, and a service account works for
GA with zero delegation (just add the SA email as a property Viewer), which
gives the auth module a live test without Workspace's admin-console steps.

## 5. User setup

### 5.1 Service account (recommended, non-interactive)

1. GCP Console → create/select a project → APIs & Services → enable
   **Google Analytics Data API** and **Google Analytics Admin API**.
2. IAM → Service Accounts → create (no GCP roles needed) → Keys → *Add key* →
   JSON → download `sa-key.json`.
3. GA4 Admin → Property → **Property access management** → add the
   service-account `client_email` with role **Viewer** (Account-level access
   makes `list_properties` show every property).
4. Register the secret (command in §3.1), set `GA_DEFAULT_PROPERTY_ID`
   (optional), apply `deploy/workload.yaml`, `claude mcp add --transport http
   google-analytics-mcp http://google-analytics-mcp.localhost:8200/`.

### 5.2 Refresh token (when a personal Google account must be used)

1. GCP Console → OAuth consent screen (Internal if Workspace, else External
   + add yourself as test user — note the 7-day refresh-token expiry in
   *Testing*; publish to *Production* to lift it) → Credentials → OAuth client
   id → **Desktop app** → note client id + secret.
2. Mint a refresh token once, on the host, outside the sandbox — any of:
   `gcloud auth application-default login --client-id-file=client.json
   --scopes=https://www.googleapis.com/auth/analytics.readonly` then read
   `refresh_token` from `~/.config/gcloud/application_default_credentials.json`;
   or the OAuth Playground with "use your own credentials"; or a 20-line
   Python/Node loopback script. Include `access_type=offline&prompt=consent`
   or no refresh token is returned.
3. Register `google-analytics-mcp-oauth-client-secret` and
   `google-analytics-mcp-refresh-token`, set `GOOGLE_AUTH_MODE=refresh_token`
   and `GOOGLE_OAUTH_CLIENT_ID`.

## 6. Hermetic e2e fixture

Python `ThreadingHTTPServer` on 127.0.0.1 impersonating all three hosts
(the server selects it via `GA_DATA_BASE_URL`, `GA_ADMIN_BASE_URL`,
`GOOGLE_TOKEN_URL`):

- `POST /token`: parse the form; for `grant_type=urn:…:jwt-bearer` split the
  assertion, base64url-decode header+claims, **echo** them
  (`iss`, `scope`, `aud`, `exp-iat`) in an `X-Fixture-Claims` header/JSON body
  so tests assert alg=RS256, scope=analytics.readonly, aud=fixture token URL,
  lifetime ≤3600; optionally verify the signature with the fixture's test
  keypair (`cryptography` is not stdlib — verify with a pure-Python RSA check
  against the public modulus/exponent, or skip verification and only assert
  structure). For `grant_type=refresh_token` echo client_id/secret/refresh
  presence. Return `{"access_token":"fixture-<n>","expires_in":3600,"token_type":"Bearer"}`;
  a special refresh token `revoked` → `400 {"error":"invalid_grant"}`.
  Count calls to assert token caching (2 tool calls → 1 token call).
- `GET /v1beta/accountSummaries`: two pages (`nextPageToken`), echoes
  `pageSize`, asserts `Authorization: Bearer fixture-*`.
- `GET /v1beta/properties/{id}`, `/customDimensions`, `/customMetrics`,
  `/metadata` (canned ~30-entry metadata incl. a `customEvent:` entry and a
  `deprecatedApiNames` entry).
- `POST /v1beta/properties/{id}:runReport`, `:runRealtimeReport`,
  `:checkCompatibility`: **echo the JSON body** under `metadata.fixtureEcho`
  (so tests assert `limit` clamped to `GA_MAX_ROWS`, default date range,
  `returnPropertyQuota: true`, filter pass-through, unicode/injection-shaped
  dimension names forwarded verbatim), and return 3 canned rows + `rowCount`
  + `propertyQuota`. Property `400400400` → `400 INVALID_ARGUMENT`,
  `403403403` → `403 PERMISSION_DENIED` (both message variants),
  `429429429` → `429 RESOURCE_EXHAUSTED`, `503503503` → 503 once then 200
  (asserts single retry), `999` → 404.
- Guard instance: started without `GOOGLE_SERVICE_ACCOUNT_KEY` → missing-
  secret message names the ref; a second guard with an OAuth-client JSON in
  place of the SA key → the "wrong file type" message.
- Test SA key: generate a throwaway 2048-bit RSA key at fixture start
  (`openssl genpkey -algorithm RSA -pkeyopt rsa_keygen_bits:2048` is
  available on the box) and hand it to wasmtime via env.

Live option: none keyless. `E2E_LIVE=1` with `GOOGLE_SERVICE_ACCOUNT_KEY`
and `GA_LIVE_PROPERTY_ID` from the developer's env runs `list_properties`,
`get_metadata property_id=0` and a 7-day `activeUsers` report. Nothing on
this machine (podman 2375, Postgres 5432, node 22) substitutes for Google.

## 7. Effort estimate

~1.5–2 developer-days: 0.5 d auth module (RS256 JWT with `rsa`+`sha2`+
`base64`, token cache, error mapping, form-encoded POST through the bridge),
0.5 d client + 7 tools + Markdown table rendering, 0.5 d SKILL.md +
references (field cheat-sheet, filter grammar, recipes), 0.5 d fixture/e2e.
Add 0.5 d if the Workspace server is built at the same time to factor the
auth module out.

## 8. Risks

- **`rsa` crate advisory RUSTSEC-2023-0071 (Marvin timing side channel)** —
  irrelevant for a single-user local signer but `cargo audit`/deny will flag
  it; document the accept. Alternative: `ring` with `wasm32` — unverified on
  wasip2.
- Clock: JWT `iat` depends on the host clock via WASI; skew >5 min → `invalid_grant`.
- Key JSON as a single env secret: ~2.3 KB with newlines-as-`\n`; verify the
  keychain backend round-trips it intact (it is JSON, so `\n` stays escaped).
- Refresh tokens from *Testing* OAuth apps die after 7 days — users will hit
  `invalid_grant` and blame the server; the error text must say so.
- Quota: an agent looping over pages or dimensions can burn the hourly
  property quota shared with the GA UI; the `GA_MAX_ROWS` clamp and the
  quota footer mitigate.
- `runFunnelReport`/annotations are v1alpha and may change; keep them out of
  v1.
- The official server adds tools each release (0.2→0.7 in 2026); track its
  `tools/` directory for new surface (funnel, annotations were added this way).
- Google has said the Data API v1beta will graduate to v1; endpoints then
  change path only — keep the version segment in one constant.

## 9. What Desktop could add to make this easy

- **Host-side OAuth broker**: a `wasmcloud:oauth`/`wasmcloud:secrets`
  extension that runs the browser authorization-code + loopback flow in
  Desktop (which *can* open a browser and listen on loopback), stores the
  refresh token in the keychain, and hands the component a fresh access token
  via `secretFrom` (rotating value) — this removes step 5.2 entirely and would
  unblock both Google servers and every other OAuth-only API (Slack user
  tokens, Microsoft Graph, HubSpot…).
- A **file-backed secret** URI (`file://~/Downloads/sa-key.json` → env) so a
  service-account JSON never has to be pasted into a shell command.
- An `allowedHosts` preset/alias for `*.googleapis.com` families.
- A shared "Google auth" library component (WASI p3 middleware) that injects
  `Authorization` on outbound calls, so individual MCP servers carry no
  crypto.
