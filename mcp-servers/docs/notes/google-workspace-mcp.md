# google-workspace-mcp — deferred (design notes)

**Status:** DEFERRED (2026-09-02). Not built.
**Reason:** every credential path Google offers for Drive/Docs/Calendar/Gmail
needs an *interactive* OAuth 2.0 consent step (browser + redirect/loopback
callback) that cannot run inside a stateless wasi:http component, and Google's
own hosted MCP servers push that same flow onto the MCP client. The only
non-interactive path is a long-lived **refresh token** (or a service-account
RS256 JWT with domain-wide delegation) that the user must mint *outside* the
sandbox and register as a secret. That is feasible (design below, everything
after the one-time mint is plain HTTPS POST) but the setup burden — GCP
project, consent screen with *restricted* Gmail/Drive scopes, 7-day token
expiry while the consent screen is in "Testing" — is the reason to wait for a
platform-level OAuth story (see "What Desktop could add").

Everything below is enough to build it without re-researching.

---

## 1. What exists upstream (checked 2026-09-02)

| Project | License | Transport | Auth | Notes |
|---|---|---|---|---|
| **Google Workspace MCP servers** (official, Google-hosted): `https://gmailmcp.googleapis.com/mcp/v1`, `drivemcp.googleapis.com/mcp/v1`, `docsmcp.googleapis.com/mcp`, `sheetsmcp…`, `slidesmcp…`, `calendarmcp.googleapis.com/mcp/v1`, `chatmcp…`, `people.googleapis.com/mcp/v1` | n/a (hosted, closed) | streamable HTTP (remote) | OAuth 2.0 done by the MCP *client*; GCP project enrolled in the **Workspace Developer Preview Program**, per-product API + "MCP service" enabled, OAuth client + consent screen. Developer preview since 2026-04-22 (Calendar/Drive/Gmail/Chat), public preview 2026-05-01 (Workspace Updates blog). Docs page last updated 2026-09-01. | Drive tools: `search_files`, `list_recent_files`, `get_file_metadata`, `read_file_content`, `download_file_content`, `create_file`, `copy_file`, `get_file_permissions`. Gmail: `search_threads`, `get_message`, `get_thread`, `create_draft`, `list_drafts`, `list_labels`, `label_/unlabel_message`, `label_/unlabel_thread` (no `send`). Docs: `read_doc`, `update_doc` (thin wrappers on `documents.get` / `batchUpdate`). Bearer token in `Authorization`. Cosmonic Desktop could only front these if *it* acted as the OAuth client. |
| **google/mcp** (index repo) | Apache-2.0 | – | – | Lists official servers; Workspace source is *not* open — it points at the Gemini CLI extension below. |
| **gemini-cli-extensions/workspace** (Google, "official" open source) | Apache-2.0 | stdio (Gemini CLI extension, TypeScript) | OAuth via Google account; "headless login" pastes a credentials JSON | Docs/Drive/Calendar/Sheets/Slides/Gmail/Chat. Tied to Gemini CLI; nothing reusable for a Rust port except tool naming. |
| **taylorwilsdon/google_workspace_mcp** (most used community, ~3.1k stars) | MIT | stdio + streamable HTTP (Python/FastMCP) | OAuth 2.0 confidential client (`GOOGLE_OAUTH_CLIENT_ID/SECRET`), OAuth 2.1 PKCE multi-user, service account + DWD, external OAuth provider, trusted-gateway identity | 120+ tools, 12 services, three tiers (core/extended/complete), `--read-only`. **Borrow: core-tier tool surface and naming** (`search_gmail_messages`, `get_gmail_message_content`, `send_gmail_message`, `search_drive_files`, `get_drive_file_content`, `create_drive_file`, `list_calendars`, `get_events`, `manage_event`, `get_doc_content`, `create_doc`, `modify_doc_text`). |
| **dguido/google-workspace-mcp** | MIT | stdio (TypeScript, npx) | OAuth 2.0 PKCE "Desktop app" client; tokens under `~/.config/google-workspace-mcp/` | 91 tools. **Archived 2026-03-11**; forks: danielrosehill/google-workspace-mcp (multi-workspace). |
| aaronsb/google-workspace-mcp, guinacio/mcp-google-workspace, ngs/google-mcp-server, orvice/google-workspace-mcp | MIT (check each) | stdio | OAuth (browser) | Same shape; nothing extra to borrow. |

Common denominator: **all of them run a browser consent flow on the host**
(loopback redirect on `http://localhost:<port>/oauth2callback` or an
out-of-band code paste) and persist a refresh token on disk. None can be
ported as-is.

---

## 2. Design that WOULD work on Cosmonic Desktop

### 2.1 Auth: refresh-token grant, no interactive flow in the component

Google's token endpoint accepts a refresh token over a plain HTTPS POST and
returns a ~1 h access token. That is the whole runtime auth story:

```
POST https://oauth2.googleapis.com/token
Content-Type: application/x-www-form-urlencoded

client_id=…&client_secret=…&refresh_token=…&grant_type=refresh_token
→ 200 {"access_token":"ya29.…","expires_in":3599,"scope":"https://www.googleapis.com/auth/drive.readonly …","token_type":"Bearer"}
→ 400 {"error":"invalid_grant","error_description":"Token has been expired or revoked."}
→ 401 {"error":"invalid_client", …}
```

Component-side (`src/google/auth.rs`):

- `static TOKEN: OnceLock<Mutex<Option<CachedToken{access_token, expires_at, scopes}>>>`
  — statics survive across requests on a warm instance (poolSize 1), so one
  refresh per instance per hour. Refresh when `now >= expires_at - 60s`.
- Every API call: `Authorization: Bearer <access_token>`. On a `401` from any
  API, invalidate the cache, refresh **once**, retry once; a second 401 is
  reported (see catalogue).
- Keep the `scope` string from the token response. Before a tool runs, check
  the scope it needs is present and fail with "the refresh token was minted
  without `<scope>`; re-run the one-time consent with it" instead of letting
  Google answer `403 insufficientPermissions`.
- Optional escape hatch `GOOGLE_ACCESS_TOKEN` (secret): if set, skip the
  exchange entirely. This is what a future OAuth-terminating ingress (or a
  host-side helper) would inject, and it is what the e2e guard instance uses.
- Optional path B (enterprise): `GOOGLE_SERVICE_ACCOUNT_KEY_JSON` (secret, the
  downloaded key file) + `GOOGLE_IMPERSONATE_USER` (named config). Build
  JWT `{alg:RS256,typ:JWT}` / claims `{iss:<sa email>, sub:<user>, scope:"<space-separated>",
  aud:"https://oauth2.googleapis.com/token", iat, exp:iat+3600}` signed with the
  key's PKCS#8 RSA private key, then
  `POST /token grant_type=urn:ietf:params:oauth:grant-type:jwt-bearer&assertion=<jwt>`.
  Needs domain-wide delegation granted in the Admin console for the same
  scopes. Pure-Rust signing: `jsonwebtoken = { version = "10", default-features = false, features = ["rust_crypto"] }`
  (RustCrypto backend, no `ring`/`aws-lc` — those do not build for
  wasm32-wasip2) or `rsa` 0.9 + `sha2` 0.10 + `pkcs8` directly.

### 2.2 Configuration (per CONVENTIONS.md)

| Env var | Kind | Required | Default | Purpose |
|---|---|---|---|---|
| `GOOGLE_CLIENT_ID` | named config | yes* | – | OAuth client id (`….apps.googleusercontent.com`); not secret. |
| `GOOGLE_CLIENT_SECRET` | secret ref `google-workspace-mcp-client-secret` | yes* | – | OAuth client secret (Google treats desktop-app secrets as non-confidential, but keep it out of manifests). |
| `GOOGLE_REFRESH_TOKEN` | secret ref `google-workspace-mcp-refresh-token` | yes* | – | Long-lived refresh token minted once by the user (§3). |
| `GOOGLE_ACCESS_TOKEN` | secret ref `google-workspace-mcp-access-token` | no | – | Pre-minted bearer; bypasses refresh (tests, future ingress). |
| `GOOGLE_SERVICE_ACCOUNT_KEY_JSON` | secret ref `google-workspace-mcp-service-account-key` | no | – | Path B: service-account key JSON (contains `private_key`, `client_email`). |
| `GOOGLE_IMPERSONATE_USER` | named config | no | – | Path B: user to impersonate with domain-wide delegation. |
| `GOOGLE_WORKSPACE_WRITE_ENABLED` | named config | no | `false` | Gate for every mutating tool (create/append/replace doc, create/update event, draft, modify labels). |
| `GOOGLE_WORKSPACE_GMAIL_SEND_ENABLED` | named config | no | `false` | Second gate, only for `gmail_send_message`. |
| `GOOGLE_WORKSPACE_DEFAULT_CALENDAR` | named config | no | `primary` | `calendarId` when a tool omits it. |
| `GOOGLE_WORKSPACE_TIMEZONE` | named config | no | `UTC` | IANA zone used to render/interpret calendar times when the caller gives none. |
| `GOOGLE_OAUTH_TOKEN_URL` | test-override | no | `https://oauth2.googleapis.com/token` | Fixture override. |
| `GOOGLE_API_BASE_URL` | test-override | no | `https://www.googleapis.com` | Drive v3 + Calendar v3. |
| `GOOGLE_GMAIL_BASE_URL` | test-override | no | `https://gmail.googleapis.com` | Gmail v1. |
| `GOOGLE_DOCS_BASE_URL` | test-override | no | `https://docs.googleapis.com` | Docs v1. |

\* required unless `GOOGLE_ACCESS_TOKEN` or the service-account pair is set.
Missing-credential error text: "`GOOGLE_REFRESH_TOKEN` is not set. Follow
docs/setup.md (GCP OAuth client → consent → OAuth Playground) and register
the token as the `google-workspace-mcp-refresh-token` secret."

Secret registration:
```console
$ curl --unix-socket "$SOCK" -X POST http://localhost/v1/secrets/refs -H 'Content-Type: application/json' \
    -d '{"name":"google-workspace-mcp-client-secret","uri":"keychain://cosmonic/google-workspace-mcp-client-secret","env":"GOOGLE_CLIENT_SECRET","value":"<secret>"}'
$ curl --unix-socket "$SOCK" -X POST http://localhost/v1/secrets/refs -H 'Content-Type: application/json' \
    -d '{"name":"google-workspace-mcp-refresh-token","uri":"keychain://cosmonic/google-workspace-mcp-refresh-token","env":"GOOGLE_REFRESH_TOKEN","value":"<1//0g…>"}'
```

### 2.3 `deploy/workload.yaml` essentials

```yaml
spec:
  hostInterfaces:
    - {namespace: wasi, package: http, interfaces: [handler], config: {host: "google-workspace-mcp.localhost"}}
  components:
    - name: google-workspace-mcp
      poolSize: 1
      localResources:
        environment:
          config:
            RUST_LOG: info
            MCP_ALLOWED_HOSTS: "google-workspace-mcp.localhost"
            GOOGLE_CLIENT_ID: "123-abc.apps.googleusercontent.com"
            GOOGLE_WORKSPACE_WRITE_ENABLED: "false"
            GOOGLE_WORKSPACE_GMAIL_SEND_ENABLED: "false"
          secretFrom:
            - {name: google-workspace-mcp-client-secret}
            - {name: google-workspace-mcp-refresh-token}
        allowedHosts:
          - https://oauth2.googleapis.com
          - https://www.googleapis.com      # Drive v3, Calendar v3, export/alt=media downloads
          - https://gmail.googleapis.com
          - https://docs.googleapis.com
```

Grants: **none** — no loopback ports, no volumes, no extra host interfaces.
(Optional later: `wasi:keyvalue` to share the access-token cache across
instances if poolSize > 1.)

### 2.4 Tool surface (curated) → exact REST calls

Common: all calls send `Authorization: Bearer`, `Accept: application/json`;
query strings built with `form_urlencoded`; every list tool clamps
`page_size`, returns `next_page_token`, and never auto-paginates.
Gated tools refuse with "writes are disabled; set
`GOOGLE_WORKSPACE_WRITE_ENABLED=true`" when the gate is off.

| Tool | Params (clamps) | Upstream | Scope needed | Gate |
|---|---|---|---|---|
| `drive_search_files` | `query` (Drive `q`, ≤1 KB), `page_size` 1..100 (20), `page_token`, `order_by` (`modifiedTime desc` default; refuse `createdTime` with `fullText`), `include_shared_drives` (false) | `GET /drive/v3/files?q=&pageSize=&pageToken=&orderBy=&supportsAllDrives=true&includeItemsFromAllDrives=<b>&corpora=<user|allDrives>&fields=nextPageToken,incompleteSearch,files(id,name,mimeType,modifiedTime,size,webViewLink,parents,owners(emailAddress),shortcutDetails)`. Server appends ` and trashed = false` unless the query mentions `trashed`. | `drive.readonly` (or `drive.metadata.readonly`) | – |
| `drive_get_file` | `file_id` | `GET /drive/v3/files/{id}?supportsAllDrives=true&fields=id,name,mimeType,size,modifiedTime,createdTime,webViewLink,parents,owners,permissions(role,emailAddress,type),exportLinks` | `drive.readonly` | – |
| `drive_read_file` | `file_id`, `format` (`markdown`\|`text`\|`html`\|`csv`, default markdown), `max_bytes` 1..1 000 000 (200 000), `offset` | 1) metadata (`fields=mimeType,name,size`); 2) Google-native (`application/vnd.google-apps.*`) → `GET /drive/v3/files/{id}/export?mimeType=<map>` (Docs: `text/markdown`\|`text/plain`\|`text/html`; Sheets: `text/csv` — first sheet only; Slides: `text/plain`; export capped at **10 MB** by Google); other → `GET /drive/v3/files/{id}?alt=media&supportsAllDrives=true` only when mimeType is `text/*`, `application/json`, `application/xml`, `*/csv`; refuse binaries (return `webViewLink`). Slice `[offset, offset+max_bytes)` on UTF-8 boundaries, report `truncated`. | `drive.readonly` | – |
| `docs_get_document` | `document_id`, `include_tabs` (false) | `GET /v1/documents/{id}?includeTabsContent=<b>`; flatten `body.content[].paragraph.elements[].textRun.content` (+ headings via `paragraphStyle.namedStyleType`, tables, lists) into Markdown-ish text; return `revision_id` and `end_index` (needed for edits). | `documents.readonly` | – |
| `docs_create_document` | `title` (≤512), `body_text` (≤200 KB) | `POST /v1/documents {"title"}` then, if body, `POST /v1/documents/{id}:batchUpdate {"requests":[{"insertText":{"endOfSegmentLocation":{},"text":…}}]}` | `documents` (+`drive.file` implied) | write |
| `docs_append_text` | `document_id`, `text` (≤200 KB), `required_revision_id?` | `POST /v1/documents/{id}:batchUpdate {"requests":[{"insertText":{"endOfSegmentLocation":{"segmentId":""},"text":…}}],"writeControl":{"requiredRevisionId":…}}` | `documents` | write |
| `docs_replace_text` | `document_id`, `find` (≤1 KB), `replace` (≤64 KB), `match_case` (true) | `…:batchUpdate {"requests":[{"replaceAllText":{"containsText":{"text":find,"matchCase":b},"replaceText":replace}}]}` → return `replies[0].replaceAllText.occurrencesChanged` | `documents` | write |
| `calendar_list_calendars` | – | `GET /calendar/v3/users/me/calendarList?fields=items(id,summary,primary,accessRole,timeZone)` | `calendar.readonly` | – |
| `calendar_list_events` | `calendar_id` (default cfg), `time_min`/`time_max` RFC3339 **with offset** (default: now → now+7d), `query`, `page_size` 1..250 (25), `page_token`, `time_zone` | `GET /calendar/v3/calendars/{id}/events?timeMin=&timeMax=&q=&maxResults=&pageToken=&singleEvents=true&orderBy=startTime&timeZone=&fields=nextPageToken,items(id,status,summary,start,end,location,htmlLink,attendees(email,responseStatus),organizer,hangoutLink,recurringEventId)` | `calendar.readonly` / `calendar.events.readonly` | – |
| `calendar_get_event` | `calendar_id`, `event_id` | `GET /calendar/v3/calendars/{cid}/events/{eid}` | same | – |
| `calendar_create_event` | `summary`, `start`, `end` (RFC3339 or `YYYY-MM-DD` all-day), `time_zone`, `description`, `location`, `attendees[]` (≤100 emails), `send_updates` (`none`\|`all`\|`externalOnly`, default none) | `POST /calendar/v3/calendars/{id}/events?sendUpdates=<v>` body `{summary,start:{dateTime,timeZone}|{date},end:{…},description,location,attendees:[{email}]}`; validate `end > start`. | `calendar.events` | write |
| `calendar_update_event` | `calendar_id`, `event_id`, any subset of the above | `PATCH /calendar/v3/calendars/{cid}/events/{eid}?sendUpdates=` (PATCH = partial) | `calendar.events` | write |
| `gmail_search_messages` | `query` (Gmail search syntax, ≤1 KB), `page_size` 1..50 (20), `page_token`, `label_ids[]`, `include_spam_trash` (false) | `GET /gmail/v1/users/me/messages?q=&maxResults=&pageToken=&labelIds=&includeSpamTrash=`, then per id `GET …/messages/{id}?format=metadata&metadataHeaders=From&metadataHeaders=To&metadataHeaders=Subject&metadataHeaders=Date` (N+1; 5 + 20·N quota units — hence the 50 cap; optimisation: `POST /batch/gmail/v1` multipart/mixed). Return `id, thread_id, date, from, to, subject, snippet, label_ids`. | `gmail.readonly` | – |
| `gmail_get_message` | `message_id`, `max_bytes` 1..500 000 (100 000), `prefer_html` (false) | `GET /gmail/v1/users/me/messages/{id}?format=full`; walk `payload.parts` recursively, pick `text/plain` (else `text/html` → `html2text`), base64url-decode `body.data` (**no padding**), list attachments (`filename, mimeType, size, attachmentId`) without fetching. | `gmail.readonly` | – |
| `gmail_get_thread` | `thread_id`, `max_messages` 1..100 (25) | `GET /gmail/v1/users/me/threads/{id}?format=metadata&metadataHeaders=From&…` | `gmail.readonly` | – |
| `gmail_list_labels` | – | `GET /gmail/v1/users/me/labels` | `gmail.readonly`/`gmail.labels` | – |
| `gmail_create_draft` | `to[]`, `cc[]`, `bcc[]`, `subject`, `body_text` (≤1 MB), `body_html?`, `reply_to_message_id?` | Build RFC 5322 message (`mail-builder`; RFC 2047 for non-ASCII subject; for replies fetch original `Message-ID`/`References` and set `In-Reply-To`/`References` + `threadId`), base64url-encode → `POST /gmail/v1/users/me/drafts {"message":{"raw":…,"threadId":…}}` | `gmail.compose` | write |
| `gmail_send_message` | same as draft | `POST /gmail/v1/users/me/messages/send {"raw":…,"threadId":…}` (≤35 MB; 100 quota units) | `gmail.send` | write **and** `GMAIL_SEND_ENABLED` |
| `gmail_modify_labels` | `message_id`, `add_label_ids[]`, `remove_label_ids[]` (e.g. `UNREAD`, `INBOX`, `STARRED`) | `POST /gmail/v1/users/me/messages/{id}/modify {"addLabelIds":[…],"removeLabelIds":[…]}` | `gmail.modify` | write |

Minimum scope set to request at consent time (read + gated writes):
`https://www.googleapis.com/auth/drive.readonly https://www.googleapis.com/auth/drive.file https://www.googleapis.com/auth/documents https://www.googleapis.com/auth/calendar.events https://www.googleapis.com/auth/gmail.readonly https://www.googleapis.com/auth/gmail.compose https://www.googleapis.com/auth/gmail.send https://www.googleapis.com/auth/gmail.modify`.
Read-only variant: `drive.readonly documents.readonly calendar.readonly gmail.readonly`.
`gmail.*` and `drive.readonly`/`drive` are **restricted** scopes: fine for the
account's own OAuth client in Testing or for a Workspace-internal app, but a
published external app needs Google's verification (+ CASA assessment).

### 2.5 Crates (all pure Rust, MIT/Apache)

`base64` 0.22 (`URL_SAFE_NO_PAD` for Gmail `raw`/`body.data`),
`form_urlencoded` 1.2 + `percent-encoding` 2 (token POST body, query strings,
path ids), `serde`/`serde_json` (present), `jiff` 0.2 (RFC 3339 parsing,
offset validation, `now + 7d`), `mail-builder` 0.5 (RFC 5322 + MIME,
alternative: hand-write headers), `html2text` 0.17 (HTML mail → text,
optional), `jsonwebtoken` 10 `rust_crypto` (path B only). Nothing needs
`ring`, `openssl`, `reqwest`, or `tokio::net`.

### 2.6 Module layout

`src/google/{auth.rs, http.rs (bearer + retry-once + error mapping),
drive.rs, docs.rs (render + batchUpdate builders), calendar.rs, gmail.rs
(MIME walk + RFC 5322 build)}`; `server.rs` = tool defs + rendering.

---

## 3. One-time user setup (what the README would say)

1. **GCP project** → APIs & Services → enable *Google Drive API*, *Google Docs
   API*, *Google Calendar API*, *Gmail API*.
2. **OAuth consent screen**: user type *External* (personal Gmail) or
   *Internal* (Workspace org); add the scopes from §2.4; add yourself as a
   *test user*. Leave in **Testing** for personal use (refresh tokens then
   **expire after 7 days** — re-mint weekly) or click *Publish* (unverified
   external apps show a warning screen but tokens then persist; restricted
   Gmail/Drive scopes on a published app eventually require verification).
   Internal (Workspace) apps have neither problem.
3. **Credentials → Create OAuth client ID → Web application**, authorised
   redirect URI `https://developers.google.com/oauthplayground`. Copy client
   id + secret.
4. **Mint the refresh token** at <https://developers.google.com/oauthplayground>:
   gear icon → *Use your own OAuth credentials* → paste id/secret → Step 1:
   paste the scope list → *Authorize APIs* (consent; `access_type=offline`
   and `prompt=consent` are set by the playground) → Step 2: *Exchange
   authorization code for tokens* → copy `refresh_token` (`1//0g…`).
   (Do **not** use the playground's default client — those refresh tokens are
   revoked after 24 h.) Alternative: a 30-line script hitting
   `https://accounts.google.com/o/oauth2/v2/auth?access_type=offline&prompt=consent&response_type=code&redirect_uri=http://localhost:1/&…`
   and exchanging the code with `POST /token grant_type=authorization_code`.
5. Register the two secrets (§2.2), put `GOOGLE_CLIENT_ID` in the manifest,
   flip the write gates if wanted, apply `deploy/workload.yaml`.
6. Verify: `curl http://google-workspace-mcp.localhost:8200/` then
   `calendar_list_calendars`.

Enterprise alternative: service account + domain-wide delegation (Admin
console → Security → API controls → Domain-wide delegation → client id +
scopes), register the key JSON as `google-workspace-mcp-service-account-key`,
set `GOOGLE_IMPERSONATE_USER`.

---

## 4. Effort estimate

| Piece | Effort |
|---|---|
| Auth module (refresh grant, cache, retry-once, scope check, missing-secret errors) | 0.5 day |
| Drive (3 tools incl. export/alt=media/slicing) | 0.5 day |
| Docs (4 tools incl. structural render + batchUpdate builders) | 1 day |
| Calendar (5 tools, RFC3339 validation) | 0.5 day |
| Gmail (7 tools incl. MIME walk, RFC 5322 build, base64url) | 1 day |
| Python fixture for 4 hosts + e2e cases + SKILL.md/references | 1 day |
| Service-account JWT path (optional) | +0.5–1 day |
| **Total** | **~4.5 days (5.5 with path B)** for one engineer familiar with the template |

---

## 5. Risks

- **Interactive mint remains a manual step**; Testing-mode tokens die after
  7 days (`invalid_grant`), which will read as "the server broke".
- **Restricted scopes**: Gmail and full-Drive scopes trigger the unverified-app
  warning and, for published external apps, Google verification/CASA. Only
  personal-project or Workspace-internal use is realistic.
- 100 refresh tokens per client/user: repeated minting silently invalidates the
  oldest.
- Workspace admins may block third-party OAuth apps (Admin → API controls);
  the error is a consent-screen failure, invisible to the component.
- Prompt injection: mail and doc bodies are untrusted input; the SKILL.md must
  say so and writes are gated for that reason.
- Gmail quota: 250 units/s per user; `gmail_search_messages` costs 5+20·N;
  concurrency tests must hit the fixture, never live.
- Google-native file export is hard-capped at 10 MB and Sheets export is first
  sheet only; large docs need `offset`/`max_bytes` paging.
- The hosted Google MCP servers may make this port moot once Desktop can act as
  an OAuth client — but they are still Developer-Preview gated.

---

## 6. Error catalogue (real upstream behaviour)

| Condition | Meaning | Action |
|---|---|---|
| Env `GOOGLE_REFRESH_TOKEN`/`GOOGLE_CLIENT_SECRET` missing | secret not registered | tool error with the §3 steps and the ref names |
| `POST /token` → 400 `invalid_grant` ("Token has been expired or revoked") | refresh token dead: Testing-mode 7-day expiry, user revoked, password change (Gmail scopes), 6 months unused, >100 tokens, admin session policy | re-mint (§3 step 4) and re-register the secret; nothing to retry |
| `POST /token` → 401/400 `invalid_client` | client id/secret mismatch or deleted client | check `GOOGLE_CLIENT_ID` matches the secret's client |
| `POST /token` → 400 `unauthorized_client` / `invalid_scope` (path B) | DWD not granted for those scopes / `sub` not allowed | fix Admin console delegation |
| API 401 `authError` / `UNAUTHENTICATED` | access token expired or revoked mid-flight | invalidate cache, refresh once, retry once; second 401 → report |
| API 403 `insufficientPermissions` ("Request had insufficient authentication scopes") | token lacks the scope | re-consent with the scope; **not** a refresh problem (server pre-checks via token `scope`) |
| API 403 `accessNotConfigured` / `SERVICE_DISABLED` | that API not enabled in the GCP project | enable Drive/Docs/Calendar/Gmail API in the project |
| API 403 `userRateLimitExceeded` / `rateLimitExceeded`, 429 `rateLimitExceeded`/`usageLimits` | per-user/per-project sliding-window quota (Drive 325k units/min/user; Calendar 600 req/min/user; Gmail 6000 units/min/user) | truncated exponential backoff (`min(2^n + jitter, 32–64 s)`); server retries GETs ≤2×, never retries send |
| API 403 `dailyLimitExceeded` | project daily cap | wait / raise quota in Cloud console |
| API 403 `insufficientFilePermissions` / `appNotAuthorizedToFile` / `domainPolicy` | no write access / `drive.file` scope only sees files the app created / admin blocked | ask owner, use broader scope, or admin |
| Drive 404 `notFound` | no read access **or** shared-drive item without `supportsAllDrives=true` | server always sends `supportsAllDrives`; otherwise wrong id / no access |
| Drive export 403 `exportSizeLimitExceeded` | > 10 MB export | choose `text/plain`, or read via Docs API in ranges |
| Drive `alt=media` on `application/vnd.google-apps.*` → 403 `fileNotDownloadable` | native file needs export | server routes to export automatically |
| Docs 400 `INVALID_ARGUMENT` "Index N must be less than the end index of the referenced segment" | inserting at `endIndex` (trailing `\n`) | use `endOfSegmentLocation` or `endIndex-1` |
| Docs 400 with `writeControl.requiredRevisionId` mismatch | doc changed since read | re-read, retry |
| Calendar 400 "Missing time zone offset / The specified time range is empty" | `timeMin`/`timeMax` without offset, or `orderBy=startTime` without `singleEvents` | server validates RFC 3339 with offset and always sets `singleEvents=true` |
| Calendar 403 `forbiddenForNonOrganizer` | editing someone else's event | only organiser can PATCH |
| Gmail 400 "Invalid `raw` value" / "Recipient address required" | raw not base64url / missing `To` | server builds and encodes the message itself |
| Gmail 429 / 403 "User-rate limit exceeded" | > 250 units/s | backoff; lower `page_size` |
| 5xx / 502 / 503 / 504 | Google backend | retry idempotent calls with backoff |

---

## 7. Skill points (what an agent gets wrong without SKILL.md)

1. The refresh token is never sent to an API — exchange it first; access
   tokens last ~1 h; a 401 means refresh-once-and-retry, an `invalid_grant`
   from the token endpoint means "go re-consent", and a 403
   `insufficientPermissions` means "the consent lacked a scope".
2. Google-native files (`application/vnd.google-apps.document|spreadsheet|presentation`)
   cannot be downloaded with `alt=media`; they must be *exported*
   (Docs → `text/markdown`, Sheets → `text/csv` first sheet only, ≤10 MB).
   Everything else is `alt=media`, and binaries should be linked, not read.
3. Drive `q` needs single-quoted strings with `\'` escaping, explicit
   `trashed = false`, `supportsAllDrives=true`+`includeItemsFromAllDrives`
   for shared drives, and `fullText contains` must not be sorted by
   `createdTime`.
4. Calendar: `timeMin`/`timeMax` are RFC 3339 **with an offset**
   (`…Z` or `-07:00`); `orderBy=startTime` requires `singleEvents=true`;
   all-day events use `date`, timed ones `dateTime`+`timeZone`.
5. Gmail `messages.list` returns only ids → one `get` per message (20 quota
   units each); `q` dates (`after:2026/01/01`) are PST midnight; body parts
   are nested `multipart/*` with base64url **unpadded** `body.data`; replies
   need `In-Reply-To`/`References` headers *and* `threadId`, or Gmail starts
   a new thread.
6. Docs edits: read `revisionId`/`endIndex` first; append with
   `endOfSegmentLocation` (never insert at `endIndex`); `batchUpdate` is
   atomic; `replaceAllText` counts occurrences.
7. Writes are gated twice (`WRITE_ENABLED`, `GMAIL_SEND_ENABLED`); drafts
   are the safe default; `send` is never retried.

---

## 8. Hermetic e2e fixture plan

One `ThreadingHTTPServer` on `127.0.0.1:<port>` receives all four base URLs
(`GOOGLE_OAUTH_TOKEN_URL=http://127.0.0.1:P/token`, the three base-URL
overrides = `http://127.0.0.1:P`). Routes and assertions:

- `POST /token`: require `Content-Type: application/x-www-form-urlencoded`,
  parse `grant_type/client_id/client_secret/refresh_token`; refresh
  `good` → `{"access_token":"fx-<counter>","expires_in":<env FX_EXPIRES, default 3600>,"scope":"<env FX_SCOPES>","token_type":"Bearer"}`;
  `expired` → 400 `invalid_grant`; wrong secret → 401 `invalid_client`.
  Expose `GET /_fixture/stats` (`token_posts`, last Authorization seen) so
  the test proves **one** exchange per instance across N tool calls, a
  re-exchange after `FX_EXPIRES=1`, and the single retry on a scripted 401.
- Every other route: 401 `authError` JSON unless `Authorization: Bearer fx-*`;
  scripted failures via header/`?_fx=429|403rate|404|500` echoing Google's
  `{"error":{"code","message","errors":[{"reason"}],"status"}}` shape.
- Drive: `GET /drive/v3/files` (echo `q`, `pageSize`, `pageToken`, `orderBy`,
  `supportsAllDrives`, `fields` in `_echo`; two canned pages), `GET /files/{id}`
  (metadata; `?alt=media` streams a text body or 403 `fileNotDownloadable`
  for `gdoc1`), `GET /files/{id}/export` (echo `mimeType`; returns a 300 KB
  markdown body to test `max_bytes`/`offset`; `big1` → 403 `exportSizeLimitExceeded`).
- Docs: `GET /v1/documents/{id}` (canned doc with headings, list, table,
  unicode), `POST /v1/documents`, `POST /v1/documents/{id}:batchUpdate`
  (echo `requests`, `writeControl`; `bad1` → 400 index error; returns
  `occurrencesChanged`).
- Calendar: `GET /calendar/v3/users/me/calendarList`, `GET|POST
  /calendar/v3/calendars/{id}/events` (echo `timeMin/timeMax/singleEvents/orderBy/maxResults/sendUpdates`
  and the POST body; reject missing offset with Google's 400 text),
  `GET|PATCH …/events/{eid}`.
- Gmail: `GET /gmail/v1/users/me/messages` (echo `q/maxResults/labelIds`),
  `GET …/messages/{id}` (canned `format=full` multipart with base64url
  `text/plain` + `text/html` + attachment part; `format=metadata` variant),
  `GET …/threads/{id}`, `GET …/labels`, `POST …/drafts` and
  `POST …/messages/send` (base64url-decode `raw`, parse headers, echo
  `to/cc/subject/in_reply_to/references/threadId` so tests assert RFC 2047
  subject encoding and threading), `POST …/messages/{id}/modify` (echo).
- Guard instance: started without the secrets → asserts the actionable
  missing-secret error for one read and one gated tool; a second guard with
  `GOOGLE_WORKSPACE_WRITE_ENABLED` unset asserts the gate message.
- Adversarial cases: 1 MB `query`, `'` and `\` in Drive `q`, unicode/RTL
  subject, header-injection `\r\n` in `to`/`subject` (must be rejected),
  `page_size=0/999999`, `time_min` without offset, `max_bytes` mid-codepoint.

Live option: nothing keyless exists; `E2E_LIVE=1` with `GOOGLE_CLIENT_ID/SECRET/REFRESH_TOKEN`
from the shell env runs `calendar_list_calendars` + `drive_search_files`
(`q="name contains 'e2e'"`) against Google. The podman/Postgres/node on this
machine are irrelevant to this server.

---

## 9. What Cosmonic Desktop could add to make this first-class

1. **OAuth-terminating ingress / connected-accounts store (docs/auth.md
   option A, upstream side).** Desktop runs the browser consent flow once
   (it owns a loopback callback and a keychain), stores the refresh token,
   and injects a fresh `Authorization: Bearer` (or `x-mcp-upstream-token`)
   into requests to workloads labelled e.g.
   `mcp.ai/upstream-auth: google-oauth`. The component then needs only
   `GOOGLE_ACCESS_TOKEN`-style header reading — zero OAuth code, no 7-day
   surprises because Desktop can re-prompt in its UI.
2. **`cosmonic auth google` helper**: a host-side CLI/UI step that performs
   the loopback flow with the user's client id and writes the result straight
   into the `google-workspace-mcp-refresh-token` secret ref — turning §3 into
   one click.
3. **Secret refs with a refresh hook / `wasmcloud:secrets` rotation**, so an
   `invalid_grant` can surface as "reconnect account" in the Desktop UI.
4. **`wasi:keyvalue` for the token cache** when poolSize > 1 (optional).
5. With (1) in place, Desktop could equally front Google's hosted
   `*mcp.googleapis.com` servers as a proxy workload, which would make this
   port unnecessary for Drive/Gmail/Docs/Calendar.
