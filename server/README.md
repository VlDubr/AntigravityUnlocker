# Antigravity Gateway Server

`antigravity-server` is the self-hosted gateway component of Antigravity
Unlocker. The Antigravity Client (Tauri desktop app) points its local proxy
at this server; the server holds the Google OAuth credentials, refreshes
access tokens, and forwards IDE traffic (SSE streams included) to the
upstream Antigravity/cloudcode-pa API. It also serves the model list, the
quota snapshot, client auto-update assets, and an optional reverse tunnel
that lets the server egress through the desktop client's own network.

This is a **personal, single-owner deployment** (`own_account` mode): one
Google account behind one (or a few) client API keys. It is not a multi-tenant
provider.

## Build

```sh
cargo build --release --manifest-path server/Cargo.toml
```

The binary lands at `server/target/release/antigravity-server`.

For a Linux VPS from another machine, cross-compile a fully static `musl`
binary. The easiest path needs only Docker:

```sh
bash server/deploy/build-linux.sh
```

or `cargo-zigbuild` without Docker. Full instructions (Docker + zigbuild,
static-ELF verification, scp/systemd deploy) are in
[`deploy/README.md`](deploy/README.md). Then copy the single binary to the VPS
(see `deploy/antigravity-server.service` for a ready systemd unit).

## Configure

Copy `server/config.example.toml` to `server.toml` next to the binary (or
point `AG_CONFIG` at it) and adjust. Every setting can also be overridden by
environment variables, which win over the file:

| Variable | Overrides | Notes |
| --- | --- | --- |
| `AG_CONFIG` | config file path | default `./server.toml` |
| `AG_LISTEN_ADDR` | `[server] listen_addr` | default `0.0.0.0:8045` |
| `AG_PUBLIC_URL` | `[server] public_url` | advertised base URL, empty = auto |
| `AG_CLIENT_KEY` | `[[auth.keys]]` key of account `main` | creates the entry if absent |
| `AG_GOOGLE_CLIENT_ID` | `[google] client_id` | OAuth client id |
| `AG_GOOGLE_CLIENT_SECRET` | `[google] client_secret` | OAuth client secret |
| `AG_GOOGLE_REFRESH_TOKEN` | `[[google.accounts]]` refresh_token of account `main` | the real Google refresh token |
| `AG_UPSTREAM_BASE_URL` | `[upstream] base_url` | upstream API base |
| `AG_TUNNEL_ENABLED` | `[tunnel] enabled` | `true`/`false` |

Keep secrets out of the TOML file in production; use a root-owned
`EnvironmentFile=/etc/antigravity-server.env` with mode 0600 (the systemd
unit shows how).

## CRITICAL: extract the real Google refresh_token BEFORE launching the Client

The gateway needs the **real** Google `refresh_token` of your account. The
Antigravity Client **overwrites** the OS keyring entry `gemini:antigravity`
with the placeholder `proxy_managed_refresh_token` the moment you activate
proxy mode (`inject_real_token` in `src-tauri/src/db.rs`), because after that
the server owns token management. If you extract the token after launching
the Client against the gateway, you will read the placeholder, not the real
token.

Correct order:

1. Log into the Antigravity IDE **natively** (proxy disabled) at least once so
   the real Google token is in the OS keyring.
2. **Extract the token now — before opening/activating the Antigravity
   Client** (steps below).
3. Put the extracted value into `AG_GOOGLE_REFRESH_TOKEN` (or
   `[[google.accounts]]`) and start the gateway server.
4. Only then launch the Antigravity Client and connect it to the server.

The keyring payload is a JSON document:

```json
{
  "token": {
    "access_token": "ya29....",
    "token_type": "Bearer",
    "refresh_token": "1//....",
    "expiry": "2026-01-01T00:00:00Z"
  },
  "auth_method": "consumer"
}
```

Take the `.token.refresh_token` field.

### Windows

The entry is a *generic credential* named `gemini:antigravity` (written via
`cmdkey /generic:gemini:antigravity /user:antigravity`). `cmdkey /list`
confirms the entry exists but can never reveal its password. Read the
secret with PowerShell:

```powershell
# Option A: CredentialManager module
Install-Module CredentialManager -Scope CurrentUser
$cred = Get-StoredCredential -Target 'gemini:antigravity' -AsPSCredential
$json = $cred.GetNetworkCredential().Password
($json | ConvertFrom-Json).token.refresh_token

# Option B: no module, WinRT PasswordVault (Windows.Security.Credentials)
$vault = [Windows.Security.Credentials.PasswordVault,Windows.Security.Credentials,ContentType=WindowsRuntime]::new()
$cred  = $vault.Retrieve('gemini:antigravity', 'antigravity')
$cred.RetrievePassword()
# ...the returned string is the same JSON payload; parse .token.refresh_token
```

Either command prints the JSON payload shown above; take
`.token.refresh_token`. Run it from an elevated prompt only if the
credential was created in another logon session (normal user-session reads
work without elevation).

### macOS

```sh
security find-generic-password -s gemini -a antigravity -w \
  | python3 -c 'import sys,json;print(json.load(sys.stdin)["token"]["refresh_token"])'
```

### Linux

```sh
secret-tool lookup service gemini username antigravity \
  | python3 -c 'import sys,json;print(json.load(sys.stdin)["token"]["refresh_token"])'
```

Put the printed value into `AG_GOOGLE_REFRESH_TOKEN`.

## OAuth client_id / client_secret

The `client_id`/`client_secret` pair used against `https://oauth2.googleapis.com/token`
is configurable (`AG_GOOGLE_CLIENT_ID` / `AG_GOOGLE_CLIENT_SECRET`).

### Extract from your installed bundle (recommended)

Run the bundled extractor against your local Antigravity hub/CLI binary
(`agy`):

```sh
python server/tools/extract-creds.py            # scan binary for the OAuth pair
python server/tools/extract-creds.py --keyring  # also read your refresh_token
python server/tools/extract-creds.py --json     # machine-readable output
```

It auto-detects `~/.gemini/bin/agy[.exe]` (override with `--binary PATH`),
streams the binary, and reports every embedded `client_id` / `client_secret`.

**Confirmed in agy 1.5.16:** the consumer Antigravity desktop `client_id` is
the one found next to `GetHubOAuthAccessToken` / `CLOUD_CODE_URL` in the
binary's string table, and its `client_secret` is the `GOCSPX-…` run immediately
followed by `cloudcode-pa`. The extractor identifies this pair structurally and
marks it `recommended: true` in `--json` output. Use that pair for the default
`daily-cloudcode-pa` upstream.

These are embedded, public-by-design desktop client credentials (not user
secrets); they are read from your local binary at runtime and are **not stored
in this repository**. The binary also contains other `client_id`s
(enterprise/auth-provider variants); the extractor lists them, but their exact
secret pairing is not adjacent in the Go string table and is left unconfirmed —
prefer the recommended consumer pair.

The `refresh_token` you extract must have been issued for a client whose
scopes cover the Antigravity/cloudcode-pa API; a token minted by an unrelated
OAuth app will be rejected by the upstream.

## Upstream

Default: `https://daily-cloudcode-pa.googleapis.com` — the Antigravity
consumer endpoint. Override with `AG_UPSTREAM_BASE_URL` or
`[upstream] base_url`. Use `https://cloudcode-pa.googleapis.com` (prod) if
you operate under GCP terms instead of the consumer path. The server
normalizes duplicated gateway prefixes and the `/v1internal/{method}` →
`/v1internal:{method}` colon form before forwarding (see
`server/src/upstream.rs`).

## Google account country eligibility

The upstream gates access by the **Google account's** country association,
not by the IP it connects from. The account must be associated with a
US/EU region. A RU/CIS-associated account is rejected (the quota response
reports `ineligibleTiers`) **even when all traffic originates from a foreign
VPS**. Fix the account's country association first; no server-side setting
can work around this.

## Connect the Antigravity Client

In Antigravity Client **Settings**:

- **Server URL** = `http://<your-VPS>:8045` (or `https://…` behind a reverse
  proxy with TLS).
- **Token** = the exact value of `AG_CLIENT_KEY`.

The Client then runs a local proxy on `127.0.0.1:8047` that forwards the IDE
(profile `proxyBaseUrl`) to your server. All IDE traffic flows
IDE → client local proxy → gateway server → upstream.

## Reverse proxy warning (nginx / Caddy)

If you terminate TLS in front of the server:

- **Disable response buffering** — otherwise SSE streams are held by the
  proxy and the IDE stalls. The server already sends
  `X-Accel-Buffering: no` and `Cache-Control: no-cache` on streaming
  responses; nginx must still have `proxy_buffering off;` (and
  `proxy_read_timeout` generously high) for the streaming locations, and
  Caddy must not compress/buffer `text/event-stream`.
- **Do not inject `X-Forwarded-For` with your (e.g. RU) client IP.** The
  gateway strips provenance headers (`X-Forwarded-For`, `X-Real-IP`,
  `Forwarded`, `CF-Connecting-IP`, `True-Client-IP`) before egress, but the
  **upstream still sees the IP of whatever machine opens the TCP connection**
  — i.e. your VPS or proxy, never the gateway's config. Make sure the final
  egress IP is a foreign (US/EU) one.
- Prefer **direct TLS or Caddy's automatic HTTPS** on the VPS over a
  hand-tuned nginx config; fewer knobs to get wrong for streaming.

Caddy example:

```
gateway.example.com {
    reverse_proxy 127.0.0.1:8045
}
```

nginx example (streaming-safe):

```nginx
location / {
    proxy_pass http://127.0.0.1:8045;
    proxy_http_version 1.1;
    proxy_buffering off;
    proxy_request_buffering off;
    proxy_read_timeout 3600s;
    proxy_set_header Connection "";
}
```

## Tunnel routing (`routing_mode`)

By default (`routing_mode = "direct"`) the server egresses to the upstream
from its own (foreign VPS) IP. If you want the upstream to see the **client
machine's** IP instead, the client opens a pool of reverse-tunnel workers
(12 workers per client, `src-tauri/src/tunnel.rs`) over
`GET /v1/tunnel?token=KEY` (WebSocket), and the server routes selected
requests through them via an internal CONNECT bridge:

| `routing_mode` | Behaviour |
| --- | --- |
| `direct` (default) | always egress from the server |
| `allowlist` | tunnel only hosts listed in `route_hosts` |
| `prefer_tunnel` | tunnel when idle workers exist, else direct |
| `always` | every forwarded request egresses through a worker |

Workers need `allow_tunnel_egress = true` on their key (default). Tunnel
support can be switched off entirely with `AG_TUNNEL_ENABLED=false`
(`/v1/tunnel` then answers 404). Note that tunnel egress still requires the
*Google account* itself to be eligible — routing through a residential
client IP mainly changes what IP fingerprint the upstream observes.

## Endpoints contract

| Method | Path | Auth | Purpose |
| --- | --- | --- | --- |
| GET | `/health` | none | liveness probe; `{"status":"ok"}` |
| GET | `/v1/models` | Bearer key | OpenAI-style model list (`object:"list"`, `data[]`) |
| GET | `/v1/quota` | Bearer key | quota snapshot: `tier`, `total_credits`, `expires_at` (null or unix **seconds**), `enable_credit_overages`, `models:{}`, `resets:{}` |
| POST | `/v1/quota/config` | Bearer key | body `{"enable_credit_overages": bool}`; flips the flag (persisted to `state_file`) |
| GET | `/v1/client-update?platform=...` | none | update manifest; empty `version` when unconfigured; `platform=macos` normalizes to `mac-arm` |
| GET | `/v1/client-update/download/{asset_id}` | none | streamed update asset; unknown id → 404 |
| GET | `/v1/tunnel?token=KEY` (WebSocket) | query token or Bearer | client tunnel worker; 401/403/404 before upgrade on auth/permission/disabled |
| ANY | `/v1internal*`, `/v1/*` (fallback) | Bearer key | streaming pass-through to upstream; token injected, provenance headers stripped, single transparent retry on upstream 401 |

Auth failures answer `401 {"error":{"message":"Invalid token"}}` (403 for
expired keys). The client key travels as `Authorization: Bearer <key>` (or
`?token=` for the WebSocket upgrade); the Google token is injected
server-side and never shown to the client.

## Non-goals

- No multi-tenant accounts, billing, or per-user quotas beyond the single
  owner's key metadata.
- No OpenAI-schema translation — the gateway is a faithful pass-through of
  the native upstream API (the `/v1/models` list is the only
  OpenAI-shaped endpoint).
- No browser OAuth flow — the server refreshes access tokens from a
  pre-extracted `refresh_token`; it cannot do an interactive sign-in.

## Run tests

```sh
cargo test --manifest-path server/Cargo.toml
```

Includes unit tests plus integration suites: `forward` (streaming
pass-through), `tunnel` (worker protocol), `e2e_contract` (acceptance
criteria AC1–AC8 against the real router on ephemeral ports).
