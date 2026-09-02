# google-ads-mcp — deferred (design notes)

**Status:** DEFERRED (2026-09-02).
**Reason:** the runtime part is buildable on Cosmonic Desktop (refresh-token
exchange over HTTPS POST + GAQL over REST), but the *credential acquisition*
is not: a Google Ads refresh token only comes out of an interactive OAuth 2.0
consent flow with a browser + loopback callback (or a Google Workspace
service account with domain-wide delegation), and the developer token must be
approved by Google (Explorer → Basic takes ~5 business days; unapproved tokens
only work against *test* accounts). Nothing in the sandbox can run that flow.
Everything below is what a builder needs once the user has those three values.

## What exists upstream (checked 2026-09-02)

| Project | License | Language / transport | Auth | Tools | Notes |
|---|---|---|---|---|---|
| [googleads/google-ads-mcp](https://github.com/googleads/google-ads-mcp) (official, Google Ads API team) | Apache-2.0 | Python (FastMCP); stdio, optional streamable-HTTP behind its own OAuth proxy (Cloud Run + Firestore/Redis) | google-ads.yaml / ADC / env; developer token (Explorer access min.); `GOOGLE_ADS_MCP_LOGIN_CUSTOMER_ID` | `list_accessible_customers()`, `search(customer_id, fields[], resource, conditions[], orderings[], limit)` (uses `search_stream`), `get_resource_metadata(resource_name)`; resources `discovery-document`, `metrics`, `segments`, `release-notes`; `tools_config.yaml` namespaces `customers/search/metadata` | Created 2025-10-03, 911 stars, pushed 2026-08-26. **Read-only.** The archived predecessor is google-marketing-solutions/google_ads_mcp_server. |
| [cohnen/mcp-google-ads](https://github.com/cohnen/mcp-google-ads) | MIT | Python, stdio | OAuth client (desktop) or service account | `list_accounts`, `execute_gaql_query`, `get_campaign_performance`, `get_ad_performance`, `run_gaql(format table/json/csv)` | 701 stars; most-used community one; read-only |
| [johnoconnor0/google-ads-mcp](https://github.com/johnoconnor0/google-ads-mcp) | MIT | Python, stdio | OAuth refresh token + dev token | 10 tools incl. `campaign_performance`, `keyword_performance`, `search_terms`, `recommendations`, `custom_query`, **writes** `update_campaign_budget`, `update_campaign_status` (no gating) | v17 API; roadmap of 161 tools |
| [promobase/google-ads-mcp](https://github.com/promobase/google-ads-mcp) | MIT | Python 3.12, stdio | OAuth refresh token + dev token | 90/103 services wrapped, grouped (core, assets, targeting, bidding, planning, reporting, conversion, …) | v20 API; too broad for an agent |
| [gomarble-ai/google-ads-mcp-server](https://github.com/gomarble-ai/google-ads-mcp-server) | MIT | Python, stdio or `--http` | browser OAuth on first call, token file | `list_accounts`, `run_gaql(customer_id, query, manager_id)`, `run_keyword_planner` | 142 stars |

Borrow: the official server's tool shape (`list_accessible_customers` → `search` → `get_resource_metadata`, and its two GoogleAdsFieldService queries), johnoconnor0's canned-report + gated-write idea. All of it is MIT/Apache — fine.

## Google Ads API facts the design depends on

- **Versions:** v25 released 2026-07-22 (sunset ~Aug 2027); v24 → May 2027; v23 → Feb 2027; v22 sunsets Oct 2026, v21 Aug 2026. Major versions ship roughly every 3–4 months and live ~12 months → the version must be a config value, default `v25`.
- **Endpoints (all `https://googleads.googleapis.com`):**
  - `GET  /v25/customers:listAccessibleCustomers` → `{"resourceNames":["customers/1234567890", …]}` (accounts the OAuth user *directly* has access to; no `login-customer-id` needed).
  - `POST /v25/customers/{cid}/googleAds:searchStream` body `{"query": "<GAQL>"}` → JSON **array** of chunks, each `{"results":[…], "fieldMask":"campaign.id,metrics.costMicros", "requestId":"…"}` (+ optional `summaryRow`, `queryResourceConsumption`). No `pageSize`; use `LIMIT` in GAQL. Chunks are up to 10,000 rows.
  - `POST /v25/customers/{cid}/googleAds:search` body `{"query", "pageToken"?, "returnTotalResultsCount"?, "validateOnly"?}` → `{"results", "nextPageToken", "totalResultsCount", "fieldMask"}`; page size is fixed at 10,000.
  - `POST /v25/googleAdsFields:search` body `{"query":"SELECT name, category, selectable, filterable, sortable, selectable_with, data_type, is_repeated, enum_values WHERE name LIKE 'campaign.%'"}` and `GET /v25/googleAdsFields/{name}` — no customer id.
  - `POST /v25/customers/{cid}/campaigns:mutate` `{"operations":[{"update":{"resourceName":"customers/{cid}/campaigns/{id}","status":"PAUSED"},"updateMask":"status"}],"validateOnly":false,"partialFailure":false}` → `{"results":[{"resourceName":…}]}`.
  - `POST /v25/customers/{cid}/campaignBudgets:mutate` same shape, `update.amountMicros`, `updateMask":"amount_micros"`.
  - `POST /v25/customers/{cid}/recommendations:apply` `{"operations":[{"resourceName":"customers/{cid}/recommendations/{id}"}],"partialFailure":false}`; `…:dismiss` `{"operations":[{"resourceName":…}]}`.
- **Headers:** `Authorization: Bearer <access_token>`, `developer-token: <token>`, `login-customer-id: <10 digits, no hyphens>` (manager id when reaching a client account), `Content-Type: application/json`. Response header `request-id` — surface it in every error. Cloud-managed access (pilot) lets you omit `developer-token` when the Cloud project's org is approved — keep the header optional.
- **OAuth:** `POST https://oauth2.googleapis.com/token` (form: `grant_type=refresh_token&client_id&client_secret&refresh_token`) → `{"access_token","expires_in":3599,"token_type":"Bearer"}`. Scope `https://www.googleapis.com/auth/adwords`. Error: HTTP 400 `{"error":"invalid_grant","error_description":"Token has been expired or revoked."}`. Refresh tokens **expire after 7 days** while the OAuth consent screen is in *Testing* status; max 100 refresh tokens per client per Google account (oldest silently revoked); 6 months unused → revoked.
- **Quota:** Test-account access 15,000 ops/day (test accounts only); Explorer 2,880 ops/day on production accounts; Basic 15,000/day; Standard unlimited. One `search`/`searchStream` call = 1 operation regardless of rows; **errors also count**. Excess → `RESOURCE_EXHAUSTED` (HTTP 429). GAQL `IN (...)` ≤ 20,000 items; gRPC response ≤ 64 MB.
- **REST error shape:** `{"error":{"code":403,"message":"The caller does not have permission","status":"PERMISSION_DENIED","details":[{"@type":"type.googleapis.com/google.ads.googleads.v25.errors.GoogleAdsFailure","errors":[{"errorCode":{"authorizationError":"USER_PERMISSION_DENIED"},"message":"…","location":{"fieldPathElements":[…]}}],"requestId":"…"}]}}`. The `errorCode` object has exactly one key naming the category (`authenticationError`, `authorizationError`, `queryError`, `quotaError`, `requestError`, `fieldError`, `mutateError`, …).
- **Response JSON is lowerCamelCase** (`metrics.costMicros`, `campaign.advertisingChannelType`) while the GAQL query is snake_case; the `fieldMask` string is camelCase too. Money is in micros (÷1,000,000). Enums are strings.

## Design that works on Cosmonic Desktop

Name/dir/crate/workload: `google-ads-mcp`; ingress `http://google-ads-mcp.localhost:8200/`; skill `skill://google-ads-mcp/SKILL.md`. Template: mcp-server-template-rs (rmcp 3.1, MCP 2026-07-28). Upstream client in `src/google_ads.rs`, OAuth in `src/google_oauth.rs`.

### Configuration

| Env var | Kind | Required | Default | Meaning |
|---|---|---|---|---|
| `GOOGLE_ADS_DEVELOPER_TOKEN` | secret ref `google-ads-mcp-developer-token` | yes (unless cloud-managed access) | — | API Center → developer token |
| `GOOGLE_ADS_CLIENT_ID` | named config | yes | — | OAuth 2.0 client id (`…apps.googleusercontent.com`, Desktop-app type) |
| `GOOGLE_ADS_CLIENT_SECRET` | secret ref `google-ads-mcp-client-secret` | yes | — | OAuth client secret |
| `GOOGLE_ADS_REFRESH_TOKEN` | secret ref `google-ads-mcp-refresh-token` | yes | — | refresh token with scope `…/auth/adwords` |
| `GOOGLE_ADS_LOGIN_CUSTOMER_ID` | named config | no | unset | manager (MCC) id, 10 digits, sent as `login-customer-id` on every customer call |
| `GOOGLE_ADS_API_VERSION` | named config | no | `v25` | path segment |
| `GOOGLE_ADS_ALLOW_MUTATE` | named config | no | `false` | gate for the three write tools |
| `GOOGLE_ADS_MAX_ROWS` | named config | no | `1000` | hard cap on rows returned per call |
| `GOOGLE_ADS_BASE_URL` | named config (test override) | no | `https://googleads.googleapis.com` | fixture points here |
| `GOOGLE_OAUTH_TOKEN_URL` | named config (test override) | no | `https://oauth2.googleapis.com/token` | fixture points here |

```console
$ curl --unix-socket "$SOCK" -X POST http://localhost/v1/secrets/refs -H 'Content-Type: application/json' \
    -d '{"name":"google-ads-mcp-developer-token","uri":"keychain://cosmonic/google-ads-mcp-developer-token","env":"GOOGLE_ADS_DEVELOPER_TOKEN","value":"<token>"}'
$ … same for google-ads-mcp-client-secret (env GOOGLE_ADS_CLIENT_SECRET) and google-ads-mcp-refresh-token (env GOOGLE_ADS_REFRESH_TOKEN)
```

`deploy/workload.yaml` component env: `config: {GOOGLE_ADS_CLIENT_ID: …, GOOGLE_ADS_LOGIN_CUSTOMER_ID: …}`, `secretFrom: [{name: google-ads-mcp-developer-token},{name: google-ads-mcp-client-secret},{name: google-ads-mcp-refresh-token}]`.

**allowedHosts:** `["https://googleads.googleapis.com", "https://oauth2.googleapis.com"]` (in both `deploy/workload.yaml` and `.wash/config.yaml`). **Grants:** none — no loopback, no volumes, no hostInterfaces.

**Access-token cache:** a `static` `Mutex<Option<(String, Instant)>>` per warm instance; refresh when < 60 s left or on a 401 `OAUTH_TOKEN_INVALID`/`GOOGLE_ACCOUNT_COOKIE_INVALID` (one retry). `expires_in` is 3599 s. Missing any of the four required values → the CONVENTIONS "missing secret" tool error naming the ref.

### Tool surface

Read tools (always on):

| Tool | Params (clamps) | Upstream |
|---|---|---|
| `list_accessible_customers` | — | `GET /{v}/customers:listAccessibleCustomers` (no `login-customer-id`) |
| `get_account_hierarchy` | `customer_id` (10 digits; hyphens stripped), `max_depth` 0..10 default 1 | `searchStream` `SELECT customer_client.id, customer_client.descriptive_name, customer_client.level, customer_client.manager, customer_client.status, customer_client.currency_code, customer_client.time_zone FROM customer_client WHERE customer_client.level <= {max_depth}` |
| `get_resource_metadata` | `resource` (`^[a-z_]{1,64}$`) | two `POST /{v}/googleAdsFields:search` queries: `SELECT name, category, selectable, filterable, sortable, data_type, is_repeated WHERE name LIKE '{resource}.%'` and `… WHERE selectable_with CONTAINS ANY('{resource}')` (official server's approach) → `{resource, attributes[], metrics[], segments[]}` |
| `search` | `customer_id`, `query` (GAQL, 1..8 KiB), `max_rows` 1..`GOOGLE_ADS_MAX_ROWS` default 200, `format` `json\|csv` | `POST /{v}/customers/{cid}/googleAds:searchStream`; if the query lacks `LIMIT`, append `LIMIT {max_rows}`; if it has a larger one, rewrite it down (`PARAMETERS include_drafts=true` is read-only and passes through). Flatten chunks, truncate at `max_rows`, report `truncated: true`, echo `field_mask` and `request_id`. |
| `search_page` | `customer_id`, `query`, `page_token`?, `return_total_count` | `POST …/googleAds:search` (10,000-row pages) → `{results, next_page_token, total_results_count}`; rows still truncated to `GOOGLE_ADS_MAX_ROWS` with the token preserved |
| `campaign_performance` | `customer_id`, `date_range` (`TODAY\|YESTERDAY\|LAST_7_DAYS\|LAST_14_DAYS\|LAST_30_DAYS\|THIS_MONTH\|LAST_MONTH` or `start`+`end` `YYYY-MM-DD` → `BETWEEN`), `status` `ENABLED\|PAUSED\|ALL` default `ENABLED`, `max_rows` | `searchStream` `SELECT campaign.id, campaign.name, campaign.status, campaign.advertising_channel_type, campaign.bidding_strategy_type, campaign_budget.amount_micros, metrics.impressions, metrics.clicks, metrics.ctr, metrics.average_cpc, metrics.cost_micros, metrics.conversions, metrics.conversions_value, metrics.cost_per_conversion FROM campaign WHERE segments.date DURING {range} [AND campaign.status = '{status}'] ORDER BY metrics.cost_micros DESC LIMIT n` |
| `keyword_performance` | `customer_id`, `date_range`, `campaign_id`?, `max_rows` | `FROM keyword_view` selecting `ad_group_criterion.keyword.text, ad_group_criterion.keyword.match_type, ad_group_criterion.quality_info.quality_score, ad_group.name, campaign.name, metrics.impressions, metrics.clicks, metrics.cost_micros, metrics.conversions` |
| `search_terms_report` | `customer_id`, `date_range`, `campaign_id`?, `max_rows` | `FROM search_term_view` selecting `search_term_view.search_term, search_term_view.status, ad_group.name, campaign.name, metrics.impressions, metrics.clicks, metrics.cost_micros, metrics.conversions` |
| `list_recommendations` | `customer_id`, `types[]`? (enum strings), `max_rows` | `FROM recommendation` selecting `recommendation.resource_name, recommendation.type, recommendation.campaign, recommendation.impact.base_metrics.cost_micros, recommendation.impact.potential_metrics.*` |

Money fields are additionally rendered as `cost` = micros/1e6 with the account currency when known.

Gated writes (`GOOGLE_ADS_ALLOW_MUTATE=true`, else a tool error explaining the gate; every write accepts `validate_only` default `true` so the first call is a dry run):

| Tool | Params | Upstream |
|---|---|---|
| `set_campaign_status` | `customer_id`, `campaign_id`, `status` `ENABLED\|PAUSED` (never `REMOVED`), `validate_only` | `POST …/campaigns:mutate` `{operations:[{update:{resourceName, status}, updateMask:"status"}], validateOnly}` |
| `set_campaign_budget` | `customer_id`, `budget_id`, `amount_micros` (integer, 10,000 ≤ n ≤ 10^12; Google rounds to the currency's billable unit), `validate_only` | `POST …/campaignBudgets:mutate` `{operations:[{update:{resourceName:"customers/{cid}/campaignBudgets/{id}", amountMicros}, updateMask:"amount_micros"}], validateOnly}` |
| `apply_recommendation` | `customer_id`, `recommendation_id`, `dismiss` bool | `POST …/recommendations:apply` / `…:dismiss` (`validateOnly` is not supported here — document it) |

### User setup steps (the part that cannot be automated)

1. Have (or create) a Google Ads **manager account**; open https://ads.google.com/aw/apicenter, fill the API access form → developer token (initially Test-account or Explorer access). Apply for Basic access (~5 business days) to hit production accounts with more than 2,880 ops/day.
2. Google Cloud console: enable "Google Ads API", create an OAuth 2.0 client of type **Desktop app**; set the consent screen to **In production** (Testing status = refresh tokens die after 7 days).
3. Mint a refresh token once, outside the sandbox: `oauth2l fetch --credentials client.json --scope adwords --output_format refresh_token`, or the Python client's `examples/authentication/generate_user_credentials.py`, or `gcloud auth application-default` is *not* usable (wrong scope). This is the interactive step.
4. Register the three secret refs (above), set `GOOGLE_ADS_CLIENT_ID` and, if the token belongs to a manager, `GOOGLE_ADS_LOGIN_CUSTOMER_ID`.
5. Deploy; `list_accessible_customers` is the smoke test.

Alternative without a human consent flow: a Google Workspace **service account** with domain-wide delegation impersonating an Ads user — needs RS256-signed JWT assertions (`grant_type=urn:ietf:params:oauth:grant-type:jwt-bearer`); doable in wasm with the `rsa`/`jsonwebtoken` crates but adds a private-key secret and a Workspace admin step, so not the first iteration.

### Skill points (what an agent gets wrong without SKILL.md)

- Customer ids are 10 digits with **no hyphens**; `login-customer-id` must be the *manager* that has access to the target account, not the target itself — otherwise `USER_PERMISSION_DENIED`.
- `list_accessible_customers` only shows accounts the OAuth user is a direct member of; walk the tree with `get_account_hierarchy` (`customer_client`) to find client accounts under a manager.
- One search call = one operation; Explorer tokens get 2,880/day on production accounts, and **failed calls count**. Do not poll; ask for what you need in one GAQL query with `LIMIT`.
- GAQL has exactly one `FROM` resource; metrics need a compatible resource/segment — check `get_resource_metadata(...).selectable_with` before inventing fields. Selecting `segments.date` explodes rows per day; use `DURING` without selecting the segment for totals.
- Money is micros; `ctr` is a fraction; the REST JSON is camelCase (`costMicros`) while GAQL is snake_case (`cost_micros`).
- `DEVELOPER_TOKEN_NOT_APPROVED` is not an auth bug — it means the token is at Test-account level and you queried a production account.
- `searchStream` has no paging; `search_page` is the only way past `GOOGLE_ADS_MAX_ROWS`.
- Writes start as `validate_only=true`; run once with it, read the response, then run for real.

### Error catalogue

| Condition | Meaning | Action |
|---|---|---|
| env missing | not configured | tool error naming the env var + secret ref + setup step |
| token endpoint 400 `invalid_grant` | refresh token expired/revoked (Testing consent screen → 7 days; unused 6 months; user revoked; >100 tokens) | re-mint the refresh token, publish the consent screen |
| token endpoint 401 `invalid_client` | client id/secret wrong | fix `GOOGLE_ADS_CLIENT_ID` / secret |
| 401 `authenticationError: OAUTH_TOKEN_INVALID` / `GOOGLE_ACCOUNT_COOKIE_INVALID` | access token expired mid-flight | refresh once and retry (server does it) |
| 401 `authenticationError: DEVELOPER_TOKEN_INVALID` | typo'd token | copy from API Center |
| 401 `authenticationError: NOT_ADS_USER` | the Google account behind the refresh token has no Ads account | invite that user to the manager account |
| 401 `authenticationError: CUSTOMER_NOT_FOUND` | id does not exist / just created | check the id; wait 5 min after creation |
| 403 `authorizationError: DEVELOPER_TOKEN_NOT_APPROVED` | test-level token, production account | use a test account or apply for Basic access |
| 403 `authorizationError: DEVELOPER_TOKEN_PROHIBITED` | token bound to another Cloud project | one dev token per Cloud project |
| 403 `authorizationError: USER_PERMISSION_DENIED` | wrong/missing `login-customer-id` or the user lacks access to that customer | set `GOOGLE_ADS_LOGIN_CUSTOMER_ID` to the manager id |
| 403 `authorizationError: CUSTOMER_NOT_ENABLED` | account cancelled / signup incomplete | fix in Ads UI |
| 403 `authorizationError: TWO_STEP_VERIFICATION_NOT_ENROLLED` | manager requires 2SV on the token's Google account | enrol 2SV |
| 400 `requestError: INVALID_CUSTOMER_ID` | hyphens or wrong length | server strips hyphens; must be 10 digits |
| 400 `queryError: *` (e.g. `UNRECOGNIZED_FIELD`, `PROHIBITED_FIELD_COMBINATION_IN_SELECT_CLAUSE`, `BAD_RESOURCE_TYPE_IN_FROM_CLAUSE`, `INVALID_VALUE_WITH_DATE`) | GAQL invalid | use `get_resource_metadata`; fix the query; the `message` says which token |
| 429 `quotaError: RESOURCE_EXHAUSTED` | daily ops or QPS limit | stop; retry after the `retryDelay` in details, or next day |
| 500/503/504 (`INTERNAL_ERROR`, `DEADLINE_EXCEEDED`, `UNAVAILABLE`) | transient | retry ≤ 2 with 1 s/4 s backoff; include `request-id` |
| 200 but empty `results` | no rows for that date range/filter | not an error; widen the range |
| non-JSON or hostname refused | `allowedHosts` missing an entry, or `GOOGLE_ADS_BASE_URL` wrong | check manifest |

### Hermetic e2e fixture (Python `ThreadingHTTPServer`)

Routes (base URL overrides `GOOGLE_ADS_BASE_URL` and `GOOGLE_OAUTH_TOKEN_URL` → `http://127.0.0.1:<port>`):
- `POST /token` — assert `Content-Type: application/x-www-form-urlencoded`, `grant_type=refresh_token`, `client_id`, `client_secret`, `refresh_token`; return `{"access_token":"fx-access-<n>","expires_in":3599,"token_type":"Bearer"}`; `refresh_token=expired` → 400 `invalid_grant`; count calls so the test proves the token is cached across 3 tool calls.
- `GET /v25/customers:listAccessibleCustomers` — assert `Authorization: Bearer fx-access-*`, `developer-token: fx-dev`, no `login-customer-id`; return two ids.
- `POST /v25/customers/{cid}/googleAds:searchStream` — assert headers incl. `login-customer-id`; parse the body; return a **JSON array** of 2 chunks whose first result row echoes the received query (`{"customer":{"id":cid},"echo":{"query":..., "headers":{...}}}`) plus N filler rows so `max_rows` truncation and the `LIMIT` rewrite are assertable; special `cid`s: `4030000000` → 403 `DEVELOPER_TOKEN_NOT_APPROVED`, `4010000000` → 401 `OAUTH_TOKEN_INVALID` (first call only, then 200 → proves the refresh-and-retry), `4290000000` → 429 with `retryDelay`, `4000000000` → 400 `queryError.UNRECOGNIZED_FIELD` with `location.fieldPathElements`, `5030000000` → 503 twice then 200.
- `POST /v25/customers/{cid}/googleAds:search` — honour `pageToken` (`""`→`"p2"`→ none), echo `returnTotalResultsCount`.
- `POST /v25/googleAdsFields:search` — return a fixed catalog for `campaign`, assert the two query strings.
- `POST /v25/customers/{cid}/campaigns:mutate`, `…/campaignBudgets:mutate`, `…/recommendations:apply|dismiss` — echo `operations`, `updateMask`, `validateOnly` in the response so tests assert the exact mask and that the gate (`GOOGLE_ADS_ALLOW_MUTATE` unset on the guard instance) blocks before any HTTP call (fixture records a hit counter).
- A `GET /__hits` endpoint returning counters (token calls, mutate calls) for assertions.

Test cases: happy path per tool; hyphenated customer id normalisation; unicode/injection-shaped GAQL passes through untouched; 9 KiB query rejected client-side; `max_rows` 0 / 10^9 clamped; date `2026-13-40` rejected; missing-secret path on the guard instance for each of the three secrets; `FIRST_TOOL_*` concurrency on `list_accessible_customers`.

Live option: none is keyless. Behind `E2E_LIVE=1`, a developer token at *Test-account* level plus a refresh token for a Google Ads **test** manager account is enough for `list_accessible_customers` and `campaign_performance` without any approval — that is the realistic smoke. The machine's podman/Postgres/node have no role here.

### Effort estimate

- Rust: OAuth refresh + cache (0.5 d), REST client + error mapping (0.5 d), 9 read tools + 3 gated writes with GAQL builders/clamps (1 d), fixture + e2e (0.5 d), SKILL.md/references (GAQL cheat-sheet, resource list from `gaql_resources.txt`, error table) + README (0.5 d). **≈ 3 engineer-days** once credentials exist.
- User-side: 1–7 days of Google approvals plus ~30 min of console clicking.

### Risks

- Refresh token rot (7-day Testing expiry, 6-month idle, 100-token cap) — the most common support issue; the error must tell the user exactly what to re-do.
- Developer-token tiers: Explorer's 2,880 ops/day is easy for an agent to burn; failed calls count.
- API version churn every ~3–4 months with ~12-month sunsets; `GOOGLE_ADS_API_VERSION` must be overridable and the README must say how to bump.
- searchStream responses for wide date-segmented queries can be tens of MB; the component must stream/limit (LIMIT rewrite) rather than buffer whole reports; the 16 MB-ish practical response ceiling in the bridge should be enforced.
- Writes touch real money; keep `GOOGLE_ADS_ALLOW_MUTATE` default off and `validate_only` default on.
- Cloud-managed access (no developer token) is a pilot; header must be optional but the doc should not promise it.

### What Cosmonic Desktop could add to make this easy

- An **OAuth broker** in Desktop: run the Google consent flow (loopback callback) from the Desktop UI, store the resulting refresh token in the keychain as a secret ref, and optionally expose a `wasmcloud:secrets`-backed "fresh access token" so workloads never see client secrets. Then every Google (Ads, Analytics, Search Console, Sheets) server becomes a plain build.
- A secret backend like `google-oauth://<client>/<scope>` alongside `op://` / `aws-sm://`.
- A JWT/RS256 signing helper (or a `wasmcloud:secrets` sign op) to make service-account auth trivial without shipping private keys into components.
