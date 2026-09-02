# linkedin-mcp — deferred (design notes)

**Status:** DEFERRED (2026-09-02). **Reason:** two stacked problems.

1. **There is no legitimate read API.** Everything the popular "LinkedIn MCP"
   servers do — profile lookup, people/company/job search, feed, inbox — is
   scraped through a logged-in browser session (`li_at` cookie) and violates
   LinkedIn's User Agreement (accounts get restricted). The official APIs that
   *read* member data (`r_member_social`, Community Management, Marketing
   Developer Platform) need partner-program approval (3–4 months, registered
   company, verified Page, screencast) and `r_member_social` is currently a
   **closed** permission ("not accepting access requests"). We will not ship a
   scraper.
2. **The legitimate write surface needs interactive OAuth.** What a developer
   *can* self-serve — post to their own feed (`w_member_social`, "Share on
   LinkedIn") and read their own basic profile (`openid profile email`, "Sign
   In with LinkedIn using OpenID Connect") — is a 3-legged authorization-code
   flow with a browser round-trip and an HTTPS redirect. Programmatic refresh
   tokens are only issued to approved Marketing Developer Platform partners, so
   the only non-interactive path for an ordinary developer is a **60-day
   static access token** minted by hand in the Developer Portal Token
   Generator and pasted into a secret. That is in scope for the platform
   (static bearer token) but is a poor experience (re-mint every 60 days), and
   the resulting server is write-only: post, comment, react, upload image,
   delete post. Worth building once Desktop has an OAuth broker (§9); until
   then it is "not in this batch".

Researched 2026-09-02 from primary sources (Microsoft Learn LinkedIn API
reference, current version `202608`; GitHub READMEs; PyPI). Everything needed
to build without re-researching is here.

---

## 1. What exists upstream (2026-09)

No vendor-official MCP server exists. LinkedIn publishes only a Postman
workspace for its Marketing APIs.

| Project | License | Lang / transport | Auth | Tools (summary) | Notes |
|---|---|---|---|---|---|
| [stickerdaniel/linkedin-mcp-server](https://github.com/stickerdaniel/linkedin-mcp-server) (PyPI `linkedin-mcp-server`) | Apache-2.0 | Python, stdio or streamable-HTTP; Docker MCP Catalog | **Browser session cookie** (`LINKEDIN_COOKIE` = `li_at`, or `--login` / `--import-from-browser`; Chromium automation) | `get_person_profile`, `get_my_profile`, `connect_with_person`, `get_inbox`, `get_conversation`, `search_conversations`, `send_message`, `get_company_profile`, `get_company_posts`, `search_companies`, `get_company_employees`, `search_jobs`, `get_saved_jobs`, `get_job_details`, `search_people`, `get_feed`, `search_posts`, `get_sidebar_profiles`, `close_session` | ~3.3k stars, 1,170+ commits; the "linkedin-mcp" people mean. README: "LinkedIn's User Agreement prohibits automated access, and accounts using automated tools can be restricted or banned." Cookie expires ~30 days. **Do not borrow.** |
| [eliasbiondo/linkedin-mcp-server](https://github.com/eliasbiondo/linkedin-mcp-server) | MIT | Python 3.12, stdio / streamable-HTTP (FastMCP + Patchright) | Browser session (`--login`, persisted at `~/.linkedin-mcp-server/browser-data`) | `get_person_profile`, `search_people`, `get_company_profile`, `get_company_posts`, `get_job_details`, `search_jobs`, `close_browser` | 163 stars. "Scraping LinkedIn may violate their Terms of Service." Do not borrow. |
| [Linked-API/linkedapi-mcp](https://github.com/Linked-API/linkedapi-mcp) | MIT | TypeScript, stdio | API key for **Linked API** (paid hosted cloud-browser automation of the user's account) | remote queue of "actions" (profile/company/search/messaging) | Third-party scraping-as-a-service; same ToS exposure, outsourced. |
| [souravdasbiswas/linkedin-mcp-server](https://github.com/souravdasbiswas/linkedin-mcp-server) | MIT | TypeScript (Node 20+), stdio | **Official OAuth 2.0 + PKCE**, scopes `openid profile email w_member_social`; env `LINKEDIN_CLIENT_ID/SECRET/REDIRECT_URI`; runs its own `localhost:3000/callback` | `linkedin_auth_start`, `linkedin_auth_callback`, `linkedin_auth_logout`, `linkedin_get_auth_status`, `linkedin_get_my_profile`, `linkedin_get_my_email`, `linkedin_get_rate_limits`, `linkedin_create_post` (text/article/image), `linkedin_delete_post`, `linkedin_create_comment`, `linkedin_react_to_post`, `linkedin_upload_image`, `linkedin_list_my_posts` (local SQLite history — because the API can't list them), `linkedin_create_event`, `linkedin_get_event` | ~5 stars, 7 commits, but the **only honest official-API design**; its tool surface is the model for ours (MIT, attribute). Adaptive `LinkedIn-Version` header. |
| [fredericbarthelet/linkedin-mcp-server](https://github.com/fredericbarthelet/linkedin-mcp-server) | not stated (treat as unlicensed — do not borrow) | TypeScript (Node 22), HTTP+SSE | Official OAuth via the MCP draft third-party-authorization flow; requires Community Management API product | `user-info`, `create-post` | Proof-of-concept; only MCP Inspector implements the auth flow it uses. |
| [quinnjr/linkedin-mcp](https://github.com/quinnjr/linkedin-mcp) | unknown (repo 404 on 2026-09-02) | — | official API (claimed "profiles, connections, skills…" which need partner scopes) | — | ~55 stars in listings; gone. |
| Hosted: Taplio MCP, ContentIn MCP (Pro plan) | proprietary | remote MCP | vendor account + vendor's approved LinkedIn partner app | posting/scheduling/analytics through the vendor | Only way to get analytics/read data legitimately today: through an approved partner. Composio / Zapier / n8n also expose "official OAuth" posting connectors. |

Borrow: souravdasbiswas's tool list and parameter shapes (MIT, attribution in
README). No code is borrowed as such — our port is Rust from the template.

## 2. Upstream API facts (verified 2026-09-02)

### 2.1 Products, scopes, what each unlocks

| Product (Developer Portal → app → Products) | Availability | Scopes | Unlocks |
|---|---|---|---|
| **Sign In with LinkedIn using OpenID Connect** | self-serve, instant | `openid`, `profile`, `email` | `GET /v2/userinfo` (sub, name, given_name, family_name, picture, locale, email?, email_verified?). `sub` is the member id used in `urn:li:person:{sub}`. |
| **Share on LinkedIn** | self-serve, instant | `w_member_social` | `POST /rest/posts` as the member; `/rest/images?action=initializeUpload`; `POST /rest/socialActions/{urn}/comments`; `POST /rest/reactions`; `DELETE /rest/posts/{urn}`. **Write-only**: tokens with only `w_member_social` cannot `GET /rest/images` on the versioned gateway (docs), and "Find posts by author" needs `r_member_social`. Rate limit (docs): **150 requests/member/day, 100,000/app/day**, UTC reset. |
| Community Management API | vetted; Development tier (500 req/app/day, 100/member/day) → Standard tier (screencast review) | `w_organization_social`, `r_organization_social`, `rw_organization_admin`, member/page analytics | Company-page posting/reading, page + post analytics. Needs a registered company and a verified Page; 3–4 months in practice. |
| Member Post Management (`r_member_social`) | **closed** ("not accepting access requests at this time") | `r_member_social` | Read the member's own posts/comments/reactions. |
| Marketing Developer Platform partner | partner program | above + `rw_ads` | Programmatic refresh tokens (365 days) are only issued here. |

No product exposes people/company/job **search**, other members' profiles,
the feed, connections, or messaging to third parties. Those are Sales
Navigator / Recruiter / Talent partner APIs, closed to the public.

### 2.2 OAuth endpoints — host `www.linkedin.com`

- Authorize (interactive, out of scope for the sandbox):
  `GET https://www.linkedin.com/oauth/v2/authorization?response_type=code&client_id=…&redirect_uri=…&state=…&scope=openid%20profile%20email%20w_member_social`.
  Redirect URL must be absolute HTTPS, registered on the app's Auth tab
  (no `#`, query ignored). Code lives 30 minutes.
- Token exchange: `POST https://www.linkedin.com/oauth/v2/accessToken`
  (`application/x-www-form-urlencoded`): `grant_type=authorization_code&code=&client_id=&client_secret=&redirect_uri=`
  → `{access_token (~500 chars, plan for 1000), expires_in: 5184000 (60 days), scope}`;
  partners also get `refresh_token`, `refresh_token_expires_in` (365 d).
- Refresh (partners only): same URL, `grant_type=refresh_token&refresh_token=&client_id=&client_secret=`
  → new `access_token` (60 d) + same `refresh_token` with remaining TTL.
  Errors: `400 invalid_request "The provided authorization grant or refresh token is invalid, expired or revoked"`.
- **Introspection** (works for any app, non-interactive):
  `POST https://www.linkedin.com/oauth/v2/introspectToken` form
  `client_id=&client_secret=&token=` → `{active, status: active|expired|revoked, scope: "openid,profile,w_member_social", client_id, created_at, expires_at (epoch s), authorized_at, auth_type: "3L"}`.
  `400` bad client id/token, `401` bad client secret; a valid-but-other-app
  token returns `200 {"active": false}`.
- **Developer Portal Token Generator** (the non-interactive-for-us path):
  <https://www.linkedin.com/developers/tools/oauth/token-generator> — pick the
  app, tick scopes, approve as the logged-in member, copy the 60-day token.
  Token Inspector at `/developers/tools/oauth/token-inspector`.
- Rule: requesting a *different* scope set invalidates every earlier token for
  that member+app. Revocation by the member (Settings → Data privacy →
  Permitted services) → `401 "The token has been revoked"`.

### 2.3 Identity — host `api.linkedin.com`

`GET /v2/userinfo` — `Authorization: Bearer …`, **no** `Linkedin-Version` or
Rest.li headers needed. Response `{sub, name, given_name, family_name,
picture, locale, email?, email_verified?}`. Person URN = `urn:li:person:{sub}`
(`sub` is an opaque string like `782bbtaQ`, not a number). Do not use
`/v2/me` — it needs the retired `r_liteprofile` scope.

### 2.4 Versioned REST — host `api.linkedin.com/rest/…`

Every `/rest/*` call needs `Linkedin-Version: YYYYMM` (latest **202608**;
each version supported ≥ 12 months; 202508 sunsets 2026-08-17) and
`X-Restli-Protocol-Version: 2.0.0`. Missing → `400 {"code":"VERSION_MISSING"}`;
sunset → `426 {"code":"NONEXISTENT_VERSION","message":"Requested version … is not active"}`.
URNs in the **path** must be URL-encoded (`urn%3Ali%3Ashare%3A123`); inside
`List(a,b)` query values the commas are not encoded.
Error body shape: `{"message","serviceErrorCode","status"}` (+ `code` on newer
endpoints). Successful creates return **201 with an empty body and the id in
the `x-restli-id` response header**.

| Operation | Call | Body / notes |
|---|---|---|
| Create post | `POST /rest/posts` | `{"author":"urn:li:person:{sub}","commentary":"<little text>","visibility":"PUBLIC"\|"CONNECTIONS"\|"LOGGED_IN","distribution":{"feedDistribution":"MAIN_FEED","targetEntities":[],"thirdPartyDistributionChannels":[]},"lifecycleState":"PUBLISHED","isReshareDisabledByAuthor":false}` + optional `content`. Article: `"content":{"article":{"source":"https://…","title":"…","description":"…","thumbnail":"urn:li:image:…"}}` — **the API does not scrape the URL**; without title/description/thumbnail the card is bare. Image: `"content":{"media":{"id":"urn:li:image:…","altText":"…","title":"…"}}`. Reshare: `"reshareContext":{"parent":"urn:li:share:…"}`. Response `201`, `x-restli-id: urn:li:share:…` (or `urn:li:ugcPost:…`). Post URL: `https://www.linkedin.com/feed/update/{urn}/`. Errors: `400 MISSING_FIELD`, `INVALID_URN_TYPE`, `INVALID_VALUE_FOR_FIELD`, `FIELD_LENGTH_TOO_LONG` (commentary > 3,000 chars), `INVALID_VALUE_BLANK_FIELD`; `403 ACCESS_DENIED`; `409 CONFLICT` (retry once); `422`; `429`. |
| Get post | `GET /rest/posts/{enc urn}?viewContext=AUTHOR` | Returns `{id, author, commentary, visibility, lifecycleState (PUBLISHED\|PUBLISH_REQUESTED\|PUBLISH_FAILED\|DRAFT), lifecycleStateInfo, content, createdAt, publishedAt, lastModifiedAt}`. Documented under the Marketing tree; for a member's *own* post with only `w_member_social` it is reported to work but is **unverified** — build it, mark best-effort, and let the e2e/live test decide whether to keep it. |
| Update post | `POST /rest/posts/{enc urn}` + header `X-RestLi-Method: PARTIAL_UPDATE`, body `{"patch":{"$set":{"commentary":"…"}}}` → `204`. | Optional v2 tool. |
| Delete post | `DELETE /rest/posts/{enc urn}` + `X-RestLi-Method: DELETE` → `204`. Idempotent (204 for an already-deleted post). No batch delete. |
| List own posts | `GET /rest/posts?q=author&author={enc person urn}&count=…` + `X-RestLi-Method: FINDER` | **Needs `r_member_social` (closed)** → 403 for us. Not a tool; the skill says so. |
| Init image upload | `POST /rest/images?action=initializeUpload` body `{"initializeUploadRequest":{"owner":"urn:li:person:{sub}"}}` → `200 {"value":{"uploadUrl":"https://www.linkedin.com/dms-uploads/…","image":"urn:li:image:…","uploadUrlExpiresAt":<ms>}}` | JPG/GIF/PNG, < 36,152,320 pixels, GIF ≤ 250 frames. No synchronous mode. |
| Upload bytes | `PUT {uploadUrl}` with `Authorization: Bearer …` (images need the token; videos must not send it), body = raw bytes, `Content-Type: application/octet-stream` → `201` empty | Host is **`www.linkedin.com`** (must be in `allowedHosts`). |
| Image status | `GET /rest/images/{urn}` → `status: PROCESSING\|AVAILABLE\|PROCESSING_FAILED\|WAITING_UPLOAD` | **403 with only `w_member_social`** ("Accessing this image resource is forbidden"). Skip; wait a few seconds after the PUT, then post. A post whose image failed ends in `lifecycleState: PUBLISH_FAILED`. |
| Comment | `POST /rest/socialActions/{enc post urn}/comments` body `{"actor":"urn:li:person:{sub}","object":"<post urn>","message":{"text":"…"}}` → `201`, `x-restli-id: <commentId>`, body echoes `commentUrn: urn:li:comment:(urn:li:activity:…,<id>)` | Reply: add `"parentComment":"urn:li:comment:(…)"`. Mentions need `message.attributes[{start,length,value:{person|organization}}]` — needs URNs we cannot look up. `429 "Comment create throttled: creation rate limit exceeded for member"` = 1-minute throttle. Delete: `DELETE /rest/socialActions/{enc post urn}/comments/{id}` → 204. |
| React | `POST /rest/reactions?actor={enc person urn}` body `{"root":"<post urn or comment urn>","reactionType":"LIKE\|PRAISE\|EMPATHY\|INTEREST\|APPRECIATION\|ENTERTAINMENT"}` → `201` body `{id: "urn:li:reaction:(actor,root)", …}` | `MAYBE` is deprecated → 400. Remove: `DELETE /rest/reactions/(actor:{enc},entity:{enc})` → 204. |

### 2.5 `little` text (the `commentary` field)

`commentary` is not plain text. Reserved characters must be backslash-escaped
or the post 400s / renders wrong: `| { } @ [ ] ( ) < > # \ * _ ~`. Elements:
mention `@[Display](urn:li:person:…)` / `@[Name](urn:li:organization:…)` (the
display text must match the entity name, case-sensitive); hashtag `#word` or
the template `{hashtag|\#|word}`. A bullet list must be written
`\* item`. Newlines are literal `\n`. Practical limit 3,000 characters
(`FIELD_LENGTH_TOO_LONG` beyond).

### 2.6 Rate limits and throttling

Documented for Share on LinkedIn: 150/member/day, 100,000/app/day, reset
00:00 UTC. No `Retry-After` / remaining-quota headers — usage is only visible
in the Developer Portal → app → Analytics (shows endpoints called today).
`429 "Resource level throttle limit for calls to this resource is reached."`
Admins get email at 75 % of the app-level quota (1–2 h delayed). LinkedIn may
also return 429 as "infrastructure protection"; do not retry in a loop.

## 3. Concrete design for Cosmonic Desktop

Name: `linkedin-mcp` (crate `linkedin_mcp`, wasm `linkedin_mcp.wasm`, ingress
`http://linkedin-mcp.localhost:8200/`, skill `skill://linkedin-mcp/SKILL.md`).
Labels `mcp.ai/domain: "linkedin"`, `mcp.ai/auth-type: none` (the MCP endpoint
itself is unauthenticated; the upstream bearer token lives in a secret).

### 3.1 Configuration (per CONVENTIONS.md)

| Env var | Kind | Required | Default | Purpose |
|---|---|---|---|---|
| `LINKEDIN_ACCESS_TOKEN` | secret (`linkedin-mcp-access-token`) | **yes** | — | 60-day member token from the Token Generator (scopes `openid profile email w_member_social`). |
| `LINKEDIN_CLIENT_ID` | named config | no | — | App client id. With the secret below, enables `introspectToken` in `check_auth` (scopes + expiry) and refresh. |
| `LINKEDIN_CLIENT_SECRET` | secret (`linkedin-mcp-client-secret`) | no | — | App client secret; never in a URL or log. |
| `LINKEDIN_REFRESH_TOKEN` | secret (`linkedin-mcp-refresh-token`) | no | — | Only for MDP partners. When set (with client id/secret), the server refreshes the access token on 401 and caches it in a static. |
| `LINKEDIN_VERSION` | named config | no | `202608` | `Linkedin-Version` header for `/rest/*`. Bump yearly at minimum. |
| `LINKEDIN_DEFAULT_VISIBILITY` | named config | no | `PUBLIC` | `PUBLIC` \| `CONNECTIONS` \| `LOGGED_IN` used when `create_post` omits `visibility`. |
| `LINKEDIN_ALLOW_DELETE` | named config | no | `false` | Gate for `delete_post` / `delete_comment` / `remove_reaction`. |
| `LINKEDIN_MAX_IMAGE_BYTES` | named config | no | `8388608` | Clamp on decoded upload size (MCP payloads are base64; 8 MiB is generous). |
| `LINKEDIN_IMAGE_DIR` | named config | no | — | Optional WASI mount (e.g. `/images`) so `upload_image` can take a `path` instead of base64. |
| `LINKEDIN_API_BASE_URL` | named config | no | `https://api.linkedin.com` | e2e override. |
| `LINKEDIN_OAUTH_BASE_URL` | named config | no | `https://www.linkedin.com` | e2e override for `/oauth/v2/*` (token, introspect). The image `uploadUrl` is followed verbatim from the API response, so the fixture returns one pointing at itself. |
| `RUST_LOG`, `MCP_ALLOWED_HOSTS` | named config | — | `info`, `linkedin-mcp.localhost` | template. |

Secret registration:

```console
$ curl --unix-socket "$SOCK" -X POST http://localhost/v1/secrets/refs \
    -H 'Content-Type: application/json' \
    -d '{"name":"linkedin-mcp-access-token","uri":"keychain://cosmonic/linkedin-mcp-access-token","env":"LINKEDIN_ACCESS_TOKEN","value":"<token from the Token Generator>"}'
```

Manifest fragment:

```yaml
metadata:
  annotations:
    desktop.cosmonic.com/credentials: >-
      [{"ref":"linkedin-mcp-access-token","env":"LINKEDIN_ACCESS_TOKEN",
        "description":"60-day member access token (Token Generator); re-mint every 60 days",
        "obtainUrl":"https://www.linkedin.com/developers/tools/oauth/token-generator",
        "scopes":["openid","profile","email","w_member_social"]},
       {"ref":"linkedin-mcp-client-secret","env":"LINKEDIN_CLIENT_SECRET",
        "description":"Optional: app client secret, enables token introspection in check_auth",
        "obtainUrl":"https://www.linkedin.com/developers/apps","scopes":[]}]
spec:
  components:
    - name: linkedin-mcp
      localResources:
        environment:
          config:
            RUST_LOG: info
            MCP_ALLOWED_HOSTS: "linkedin-mcp.localhost"
            LINKEDIN_VERSION: "202608"
            LINKEDIN_DEFAULT_VISIBILITY: PUBLIC
            LINKEDIN_ALLOW_DELETE: "false"
          secretFrom:
            - name: linkedin-mcp-access-token
            # - name: linkedin-mcp-client-secret      # optional
        allowedHosts:
          - "https://api.linkedin.com"
          - "https://www.linkedin.com"
```

Grants: none. No loopback ports, no host interfaces. Optional volume
(`spec.volumes` hostPath → `volumeMounts` at `/images`) only if the user
wants `upload_image path=…`; otherwise images travel as base64 in the tool
call. TLS: both hosts have public CA chains (webpki OK).

### 3.2 Auth module (`src/linkedin.rs`)

- `token()` → `LINKEDIN_ACCESS_TOKEN` (or the refreshed token cached in a
  `static Mutex<Option<Cached>>`). Missing → the CONVENTIONS "missing secret"
  message naming `linkedin-mcp-access-token`, `LINKEDIN_ACCESS_TOKEN`, and the
  Token Generator URL. Never call upstream without it.
- `person_urn()` → `GET /v2/userinfo` once per warm instance; cache `sub`,
  `name` in a static. Every write tool needs it (`author` / `actor` /
  `owner`).
- `rest(method, path, body)` adds `Authorization: Bearer`, `Linkedin-Version`,
  `X-Restli-Protocol-Version: 2.0.0`, `Content-Type: application/json`, plus
  `X-RestLi-Method` when given. Reads `x-restli-id` on 201.
- On `401` with a refresh token configured: one refresh (`POST
  {OAUTH}/oauth/v2/accessToken`, form-encoded), retry once, then surface.
  Without a refresh token: surface "token expired/revoked — re-mint in the
  Token Generator and update the `linkedin-mcp-access-token` secret".
- `introspect()` (only when client id + secret are set): `POST
  {OAUTH}/oauth/v2/introspectToken`; used by `check_auth` for `scopes`,
  `expires_at`, `status`.
- `escape_little(text)` — backslash-escape `| { } @ [ ] ( ) < > # \ * _ ~`
  unless the caller sets `raw_little_text: true`.

### 3.3 Tool surface

| Tool | Upstream | Params | Clamps / behaviour | Gate |
|---|---|---|---|---|
| `check_auth` | `GET /v2/userinfo` (+ `POST /oauth/v2/introspectToken` when client creds set) | none | `structuredContent {status: ok\|missing\|invalid\|insufficient, sub, name, person_urn, scopes[], expires_at, days_left, remediation}`. `insufficient` when `w_member_social` is absent from introspected scopes. Warns when `days_left < 7`. | — |
| `get_my_profile` | `GET /v2/userinfo` | none | Returns sub, name, given/family name, picture URL, locale, email (if the `email` scope was granted), `person_urn`. | — |
| `create_post` | `POST /rest/posts` | `text` (1..3000 chars after escaping), `visibility?` (`PUBLIC`\|`CONNECTIONS`\|`LOGGED_IN`, default env), `article?{url, title?, description?, thumbnail_image_urn?}`, `image_urn?` (`urn:li:image:…`), `image_alt_text?` (≤ 4,086, recommend < 120), `reshare_of?` (post URN), `disable_reshare?` (bool), `raw_little_text?` (bool, default false) | Exactly one of `article` / `image_urn` / `reshare_of` (or none). `url` must be `https?://`. Escapes `text` unless raw. Returns `{post_urn, url: https://www.linkedin.com/feed/update/{urn}/, visibility}`. Rejects text that is whitespace-only (`INVALID_VALUE_BLANK_FIELD` upstream). | write (not idempotent; MCP `destructiveHint: false`) |
| `get_post` | `GET /rest/posts/{enc}?viewContext=AUTHOR` | `post_urn` (`urn:li:share:` or `urn:li:ugcPost:`) | Best-effort (see §2.4). Returns id, commentary, visibility, lifecycleState (+ `PUBLISH_FAILED` explanation), timestamps, content summary. 403 → explains that reading needs `r_member_social`. | — |
| `delete_post` | `DELETE /rest/posts/{enc}` + `X-RestLi-Method: DELETE` | `post_urn` | 204 → `{deleted: true}` (idempotent). | `LINKEDIN_ALLOW_DELETE=true` |
| `upload_image` | `POST /rest/images?action=initializeUpload` then `PUT {uploadUrl}` | `base64?` or `path?` (under `LINKEDIN_IMAGE_DIR`), `content_type?` (`image/png`\|`image/jpeg`\|`image/gif`, sniffed from magic bytes if absent) | Decoded size ≤ `LINKEDIN_MAX_IMAGE_BYTES`; rejects non-JPG/PNG/GIF magic. Returns `{image_urn, bytes, note: "wait ~5 s before posting; status cannot be polled with w_member_social"}`. `uploadUrl` host must be `www.linkedin.com` (or the override base) — refuse anything else. | write |
| `create_comment` | `POST /rest/socialActions/{enc}/comments` | `post_urn`, `text` (1..1250), `parent_comment_urn?` | Returns `{comment_id, comment_urn}` from `x-restli-id`/body. Maps the 1-minute throttle 429 distinctly. | write |
| `react_to_post` | `POST /rest/reactions?actor={enc person}` | `target_urn` (post or comment URN), `reaction_type` (`LIKE`\|`PRAISE`\|`EMPATHY`\|`INTEREST`\|`APPRECIATION`\|`ENTERTAINMENT`, default `LIKE`) | Rejects `MAYBE` client-side. Returns reaction id. | write |
| `remove_reaction` | `DELETE /rest/reactions/(actor:{enc},entity:{enc})` | `target_urn` | 204. | `LINKEDIN_ALLOW_DELETE=true` |
| `delete_comment` (optional) | `DELETE /rest/socialActions/{enc post}/comments/{id}` | `post_urn`, `comment_id` | 204. | `LINKEDIN_ALLOW_DELETE=true` |

Deliberately **not** tools (the skill says why): search people/companies/jobs,
read feed, list own posts (`r_member_social` closed), messaging, connections,
company-page posting (needs Community Management approval — if a user has it,
add `LINKEDIN_ORGANIZATION_URN` + `author` override as a v2; the request
bodies are identical with `urn:li:organization:{id}` and scope
`w_organization_social`), video/document posts (Videos API multipart flow;
v2 if asked), polls, multi-image.

### 3.4 Skill points (what an agent gets wrong without SKILL.md)

1. **`check_auth` first, then `get_my_profile`.** The author/actor/owner of
   every write is `urn:li:person:{sub}` where `sub` comes from
   `GET /v2/userinfo` (opaque string, not numeric). `/v2/me` is dead
   (needs the retired `r_liteprofile`). Never guess a person URN.
2. **This server cannot read LinkedIn.** No search, no feed, no other
   profiles, no inbox, no list of the member's own posts (that scope is
   closed). If the user asks for those, say it needs LinkedIn partner access
   and refuse to suggest cookie/scraping tools. Keep the post URN returned by
   `create_post` — it is the only handle you will ever get.
3. **Two header sets.** `/v2/userinfo` takes only the bearer token; every
   `/rest/*` call additionally needs `Linkedin-Version: YYYYMM` and
   `X-Restli-Protocol-Version: 2.0.0` (the server adds them; a
   `400 VERSION_MISSING` / `426 NONEXISTENT_VERSION` means `LINKEDIN_VERSION`
   is unset/sunset — bump it, don't retry).
4. **Commentary is `little` text.** Parentheses, brackets, braces, `@`, `#`,
   `*`, `_`, `~`, `|`, `<`, `>` and backslash are syntax; the server escapes
   them by default. Use `raw_little_text: true` only when you intend a
   `#hashtag`, `{hashtag|\#|tag}` or an `@[Name](urn:…)` mention — and you
   cannot look up mention URNs, so mentions are effectively unavailable.
   Limit 3,000 characters. Bullet lists: `\* item` in raw mode or plain `-`.
5. **Article posts do not unfurl.** The API never scrapes the URL: supply
   `title` and `description`, and a `thumbnail_image_urn` from
   `upload_image` if you want a preview image, or the card will be bare.
6. **Image posts are a 2-step dance with no status check.** `upload_image`
   → `image_urn` → wait a few seconds → `create_post image_urn=…`. You cannot
   poll the image (403 with this scope). If `get_post` later shows
   `PUBLISH_FAILED`, the image was rejected: re-upload (JPG/PNG/GIF, under
   36 MP) and post again.
7. **Creates return no body.** The id is in the `x-restli-id` header; the
   server surfaces it as `post_urn` and a `https://www.linkedin.com/feed/update/{urn}/`
   URL. Deletes are idempotent 204s.
8. **Quotas are small and invisible.** 150 requests per member per day
   (every tool call counts, including `check_auth`), reset 00:00 UTC, no
   remaining-quota headers. Comments have an extra 1-minute throttle. On 429
   stop; do not retry in a loop. Never post the same text twice "to be safe"
   — there is no idempotency key; check `get_post` on the URN you already
   have.
9. **Tokens die every 60 days** and immediately if the app's scope set
   changes or the member revokes the app. `401 "Expired access token"` /
   `"The token has been revoked"` → tell the user to re-mint in the Token
   Generator and update the `linkedin-mcp-access-token` secret; do not
   retry. `check_auth` reports `days_left`.
10. **Posting is a real, public action.** Confirm text, visibility
    (`PUBLIC` vs `CONNECTIONS`) and the target URN with the user before
    `create_post`, `create_comment`, `react_to_post`. `delete_post` is off
    unless `LINKEDIN_ALLOW_DELETE=true`.
11. **URN shapes.** Posts: `urn:li:share:<digits>` or `urn:li:ugcPost:<digits>`;
    comments: composite `urn:li:comment:(urn:li:activity:<n>,<id>)`;
    images: `urn:li:image:<id>`. The server URL-encodes them; pass them raw.
    Activity URNs from web URLs (`urn:li:activity:…`) are accepted as
    reaction/comment roots but not as `get_post` ids.

### 3.5 Error catalogue

| Condition | Meaning | Action |
|---|---|---|
| `LINKEDIN_ACCESS_TOKEN` unset | secret not registered / not in `secretFrom` | Register `linkedin-mcp-access-token` (§5), redeploy. Server returns the actionable message, never calls upstream. |
| `401 {"serviceErrorCode":65600,"message":"Invalid access token"}` (also "Unknown authentication schema", "Empty oauth2_access_token") | token malformed/pasted wrong | Re-copy from the Token Generator; check for whitespace. |
| `401 {"serviceErrorCode":65601,"message":"Expired access token"}` / `"The token used in the request has expired"` | 60 days elapsed | Re-mint, update secret. With a partner refresh token the server retries once automatically. |
| `401 "The token has been revoked"` | member revoked the app, or the app's scope set changed | Re-mint (member re-consents). |
| `403 ACCESS_DENIED` / `"Not enough permissions to access: …"` on `/rest/posts` | token lacks `w_member_social` (Share on LinkedIn product not added, or scope not ticked when minting) | `check_auth` shows scopes; add the product, re-mint with `w_member_social`. |
| `403` on `GET /rest/posts?q=author`, `GET /rest/images/…`, `GET /rest/posts/{urn}` | read scopes closed to self-serve apps | Expected; the skill says these are unavailable. `get_post` is best-effort. |
| `400 {"code":"VERSION_MISSING"}` | no `Linkedin-Version` header | Server bug / env stripped; set `LINKEDIN_VERSION`. |
| `426 {"code":"NONEXISTENT_VERSION","message":"Requested version … is not active"}` | `LINKEDIN_VERSION` older than 12 months | Set it to a current `YYYYMM` (docs list the latest). |
| `400 INVALID_URN_TYPE "author value … must be a person URN"` | bad/missing `sub` | `get_my_profile`; don't hand-build URNs. |
| `400 INVALID_URN_ID` / `"Syntax exception in path variables"` | URN not URL-encoded or wrong kind | Pass raw URNs; server encodes. |
| `400 FIELD_LENGTH_TOO_LONG` | commentary > 3,000 (post) / comment too long | Shorten. |
| `400 INVALID_VALUE_BLANK_FIELD` | empty commentary | Provide text. |
| `400 INVALID_VALUE_FOR_FIELD` on `visibility` / `reactionType` | bad enum (e.g. `MAYBE`) | Use the documented enums. |
| `400 "Invalid query parameters passed to request"` | Rest.li 2.0 encoding of `List(…)`/composite keys | Server-side encoding bug; check the reaction/comment key builders. |
| `404 NOT_FOUND` on a post URN | wrong id, deleted, or not visible to this token | Verify the URN; deletes are idempotent so a 404 on delete is treated as success. |
| `409 CONFLICT` | write conflict | Retry once after 1 s. |
| `413` / `415` / `422` on image PUT | too large / unsupported format / corrupt | JPG/PNG/GIF under 36 MP; check magic bytes. |
| `429 "Resource level throttle limit for calls to this resource is reached."` | daily member (150) or app quota, or infra protection | Stop; quota resets 00:00 UTC. Report the count of calls made this session. |
| `429 "Comment create throttled: creation rate limit exceeded for member"` | 1-minute comment throttle | Wait 60 s. |
| `500` / `503` / `504` | LinkedIn side | One retry after 2 s; then report `x-li-uuid` from the response for support. |
| Introspect `400`/`401` | wrong client id / client secret | Fix `LINKEDIN_CLIENT_ID` / `linkedin-mcp-client-secret`; `check_auth` still works via userinfo. |
| Introspect `200 {"active": false}` | token belongs to a different app | Mint the token from the same app as the client id. |
| `lifecycleState: PUBLISH_FAILED` in `get_post` | media processing failed | Re-upload the image, post again, delete the failed post if `LINKEDIN_ALLOW_DELETE`. |

## 4. Why not the interactive flow, and what would change the decision

The component cannot open a browser, cannot host an HTTPS redirect (LinkedIn
requires absolute HTTPS redirect URLs; `http://localhost` is not accepted for
member apps), and cannot run a loopback listener. PKCE does not remove the
redirect. A desktop-side broker (§9) that performs the authorization-code
flow and hands the component a rotating `LINKEDIN_ACCESS_TOKEN` secret would
make this server a clean "build": every tool above works unchanged, and the
60-day re-mint becomes a "Reconnect LinkedIn" button.

## 5. User setup (static-token path)

1. <https://www.linkedin.com/developers/apps/new> — app name, **associate a
   LinkedIn Page** (any Page you admin; create one if needed), logo, accept
   terms. Then on the app → Settings → *Verify* the Page (sends a link to a
   Page admin; required before products unlock).
2. App → **Products** → add **Share on LinkedIn** and **Sign In with LinkedIn
   using OpenID Connect** (both instant, self-serve).
3. App → **Auth** → note *Client ID* / *Client Secret* (optional, for
   `check_auth` introspection) and add any HTTPS redirect URL (e.g.
   `https://oauth.pstmn.io/v1/callback`; the Token Generator needs one
   configured).
4. <https://www.linkedin.com/developers/tools/oauth/token-generator> → select
   the app → tick `openid`, `profile`, `email`, `w_member_social` → *Request
   access token* → approve → *Copy token*. It is valid 60 days.
5. Desktop → Secrets → paste as `linkedin-mcp-access-token` (env
   `LINKEDIN_ACCESS_TOKEN`), or the `curl` in §3.1. Optionally
   `linkedin-mcp-client-secret` + `LINKEDIN_CLIENT_ID`.
6. Apply `deploy/workload.yaml`; `claude mcp add --transport http linkedin-mcp
   http://linkedin-mcp.localhost:8200/`; run `check_auth`.
7. Every 60 days (the `check_auth` `days_left` warning): repeat step 4–5.

## 6. Hermetic e2e fixture

Python `ThreadingHTTPServer` on 127.0.0.1 impersonating both hosts; the
server selects it with `LINKEDIN_API_BASE_URL` and `LINKEDIN_OAUTH_BASE_URL`
(same fixture URL for both).

- `GET /v2/userinfo`: assert `Authorization: Bearer <token>`; tokens
  `good` → 200 canned `{sub:"782bbtaQ", name:"Test Member", …, email}`;
  `expired` → `401 {"serviceErrorCode":65601,"message":"Expired access token","status":401}`;
  `revoked` → `401 "The token has been revoked"`; `noscope` → 200 (then
  `/rest/posts` returns 403 for it). Count calls to assert `sub` caching
  (N tool calls → 1 userinfo call per instance).
- `POST /oauth/v2/introspectToken`: parse form, assert `client_secret` is not
  in the query string; echo `client_id`; return `active:true, scope:"openid,profile,email,w_member_social", expires_at: now+30d` (`noscope` token → scope without `w_member_social`; unknown → `active:false`).
- `POST /oauth/v2/accessToken`: `grant_type=refresh_token` → echo fields,
  return a new `access_token`; refresh token `dead` → `400 invalid_request`.
- `POST /rest/posts`: assert `Linkedin-Version` (== env), `X-Restli-Protocol-Version: 2.0.0`,
  `Content-Type: application/json`; **echo the JSON body** in an
  `X-Fixture-Echo` header (base64) so tests assert: `author == urn:li:person:782bbtaQ`,
  default visibility from env, `distribution` boilerplate present,
  `lifecycleState: PUBLISHED`, escaping (`(`→`\(` etc.; raw mode leaves
  `{hashtag|\#|x}` intact), article fields pass-through, `content.media.id`
  for image posts, `reshareContext.parent`. Return `201` empty body with
  `x-restli-id: urn:li:share:<counter>`. Commentary > 3000 →
  `400 {"code":"FIELD_LENGTH_TOO_LONG"}`; `noscope` token → `403 ACCESS_DENIED`;
  header `Linkedin-Version: 202401` → `426 NONEXISTENT_VERSION`; missing →
  `400 VERSION_MISSING`; commentary containing `"CONFLICT"` → 409 once then 201
  (asserts single retry); token `throttled` → 429 with the documented message.
- `GET /rest/posts/urn%3Ali%3Ashare%3A<n>`: assert `%3A` encoding and
  `viewContext=AUTHOR`; return canned post (`n == 9` → `PUBLISH_FAILED`;
  `n == 404` → 404; `n == 403` → 403).
- `DELETE /rest/posts/{enc}`: assert `X-RestLi-Method: DELETE`; 204 always.
- `POST /rest/images?action=initializeUpload`: assert body owner; return
  `uploadUrl: http://127.0.0.1:<port>/dms-uploads/<id>`, `image: urn:li:image:<id>`.
  `PUT /dms-uploads/<id>`: assert `Authorization` present, record
  `Content-Length`/first bytes; 201. Test cases: PNG magic ok; text bytes →
  server-side reject before any upstream call; > `LINKEDIN_MAX_IMAGE_BYTES`
  → server-side reject; fixture returning an `uploadUrl` on another host →
  server refuses.
- `POST /rest/socialActions/{enc}/comments`: assert path encoding, echo
  `actor`/`object`/`message.text`/`parentComment`; 201 with
  `x-restli-id: 6643206422739898368` and a body carrying `commentUrn`.
  Text `"THROTTLE"` → 429 comment throttle message.
- `POST /rest/reactions?actor=…`: assert `actor` query is encoded person URN;
  `reactionType: MAYBE` never reaches the fixture (server rejects); 201 body.
  `DELETE /rest/reactions/(actor:…,entity:…)`: assert composite-key encoding; 204.
- Guard instance: started without `LINKEDIN_ACCESS_TOKEN` → every tool
  returns the missing-secret message naming the ref; a second guard with
  `LINKEDIN_ALLOW_DELETE` unset → `delete_post` refuses without calling
  upstream.
- Adversarial: 3,000-char unicode text, injection-shaped text
  (`"); DROP TABLE`, `</script>`, `{hashtag|#|x}` in non-raw mode → escaped),
  URNs with path traversal (`urn:li:share:../../`) → 400 before upstream.

Live option: none keyless. `E2E_LIVE=1` with `LINKEDIN_ACCESS_TOKEN` in the
developer's env runs `check_auth` and `get_my_profile` only (never posts).
Nothing on this machine (podman 2375, Postgres 5432, node 22) substitutes
for LinkedIn.

## 7. Effort estimate

~1.5 developer-days once the deferral is lifted: 0.25 d auth/client module
(bearer + headers + `x-restli-id` + optional introspect/refresh, static
caches), 0.5 d 8–9 tools incl. `little` escaping, URN encoding helpers,
base64/magic-byte image handling and the 2-step upload, 0.25 d SKILL.md +
`references/little-text.md` + `references/errors.md`, 0.5 d fixture/e2e.
Add 0.5 d for a Videos API tool (multipart `finalizeUpload`) if ever asked.

## 8. Risks

- **Reputation/ToS**: the name "linkedin-mcp" carries the expectation of
  scraping tools; the README must lead with "official API, write-only, no
  scraping" so users are not surprised and do not ask us to add cookies.
- **60-day token churn** and silent invalidation when the app's scope set
  changes; users will blame the server. `check_auth` `days_left` plus the
  explicit error text mitigate; a Desktop expiry reminder would fix it.
- **Agent-driven posting** is a public, irreversible-ish action; the skill
  mandates confirmation and the server never batches posts. Consider a
  per-instance cap (e.g. 10 posts/day) as a named config.
- **`get_post` with `w_member_social` is unverified**; the docs put it under
  Community Management. If it 403s live, drop it and rely on the URL.
- **Version sunset**: `LINKEDIN_VERSION` must be bumped at least yearly or
  every `/rest` call becomes 426. Ship with the latest and document.
- **Share on LinkedIn product changes**: LinkedIn has been moving self-serve
  docs toward the versioned Posts API while the consumer page still shows
  `/v2/ugcPosts`; if `/rest/posts` were ever fenced off for self-serve apps,
  fall back to `/v2/ugcPosts` (schema in the consumer docs; no version
  header) — keep the post body builder behind a trait.
- **Quota**: 150/member/day shared with anything else using the same app;
  `check_auth` counts too.
- Community Management / organization posting is a different approval and a
  different author URN; do not promise it.

## 9. What Desktop could add to make this easy

- **Host-side OAuth broker** (`wasmcloud:oauth` or a secrets-backend feature):
  Desktop opens the browser, receives the redirect on a loopback/HTTPS
  helper (LinkedIn needs HTTPS; a `https://auth.cosmonic.app/cb` relay or a
  local TLS helper), exchanges the code, stores the token in the keychain and
  exposes it as a rotating `secretFrom` value. Turns this and every other
  3-legged-only API (Google, Microsoft Graph, Slack user tokens, HubSpot)
  into a "build".
- **Secret expiry metadata**: `expires_at` on a secret ref + a UI badge /
  notification at 7 days; `check_auth` could write it back.
- **Credential deep links in the UI** from the
  `desktop.cosmonic.com/credentials` annotation (Token Generator URL with the
  app preselected: `…/token-generator?clientId=<id>` if LinkedIn honours it —
  unverified).
- **Write-action confirmation hook**: a workload label
  (`mcp.ai/confirm-tools: create_post,delete_post`) that Desktop turns into a
  human approval prompt before the call is forwarded.
