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
| `AG_GOOGLE_PROJECT_ID` | `[[google.accounts]]` project_id of account `main` | optional Cloud Code project override |
| `AG_UPSTREAM_BASE_URL` | `[upstream] base_url` | upstream API base |
| `AG_TUNNEL_ENABLED` | `[tunnel] enabled` | `true`/`false` |
| `AG_OPENAI_ENABLED` | `[openai] enabled` | enable the Chat Completions adapter; default `false` |

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

1. Stop the Client's proxy and close the Client. If it was used before on
   Windows, remove its placeholder credential with
   `cmdkey /delete:gemini:antigravity`. Log into the Antigravity IDE
   **natively** (proxy disabled) at least once so the real Google token is in
   the OS keyring.
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

The entry is a *generic credential* named `gemini:antigravity` with user
`antigravity`. The client writes it with the Windows API (`CredWriteW`).
**Do not use `cmdkey /pass:<json>`**: for a JSON payload it silently stores
an *empty* blob, which leaves Antigravity signed out. `cmdkey /list` only
confirms existence, never reveals the secret. WinRT `PasswordVault` does
not see generic credentials either.

Read or write the blob with the bundled helper (no extra installs):

```powershell
# Read the current payload (prints the JSON; take .token.refresh_token)
python server/tools/extract-creds.py --keyring

# Write a payload yourself (e.g. after a native sign-in elsewhere).
# Put the JSON in a local file, never in chat or shell history:
@'
import ctypes, json
from ctypes import wintypes
from pathlib import Path
payload = Path("payload.json").read_bytes()   # {"token":{...},"auth_method":"consumer"}
adv = ctypes.windll.advapi32
class ATTR(ctypes.Structure):
    _fields_=[("Keyword",wintypes.LPWSTR),("Flags",wintypes.DWORD),("ValueSize",wintypes.DWORD),("Value",ctypes.c_void_p)]
class CRED(ctypes.Structure):
    _fields_=[("Flags",wintypes.DWORD),("Type",wintypes.DWORD),("TargetName",wintypes.LPWSTR),("Comment",wintypes.LPWSTR),("LastWritten",wintypes.FILETIME),("CredentialBlobSize",wintypes.DWORD),("CredentialBlob",ctypes.c_void_p),("Persist",wintypes.DWORD),("AttributeCount",wintypes.DWORD),("Attributes",ctypes.POINTER(ATTR)),("TargetAlias",wintypes.LPWSTR),("UserName",wintypes.LPWSTR)]
buf=ctypes.create_string_buffer(payload)
c=CRED(); c.Type=1; c.TargetName="gemini:antigravity"; c.UserName="antigravity"
c.CredentialBlobSize=len(payload); c.CredentialBlob=ctypes.cast(buf,ctypes.c_void_p); c.Persist=2
assert adv.CredWriteW(ctypes.byref(c),0), ctypes.GetLastError()
print("written", len(payload), "bytes")
'@ | python -
```

Alternatively the `CredentialManager` PowerShell module works for reading:
`Get-StoredCredential -Target 'gemini:antigravity' -AsPSCredential`.
Run from an elevated prompt only if the credential was created in another
logon session (normal user-session reads work without elevation).

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
python server/tools/extract-creds.py            # only the Antigravity 2.0 OAuth pair
python server/tools/extract-creds.py --keyring  # also read your refresh_token
python server/tools/extract-creds.py --json     # machine-readable output
python server/tools/extract-creds.py --all-clients  # diagnostic: include unrelated clients
```

It auto-detects `~/.gemini/bin/agy[.exe]` (override with `--binary PATH`),
streams the binary, and reports only the recommended Antigravity 2.0 consumer
OAuth pair by default. Use `--all-clients` only when diagnosing binary string
matches; unrelated enterprise OAuth clients are not valid alternatives.

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

If requests return HTTP 502 and the service journal reports
`oauth refresh failed` with `invalid_grant`, Google rejected the refresh token.
Sign in natively again with the proxy disabled, then extract the new token
before activating the Client. Update `AG_GOOGLE_REFRESH_TOKEN` in the protected
environment file and restart the service. Extraction scripts cannot renew a
revoked token without a new interactive sign-in. Do not paste tokens into chat.

To install a newly extracted token on the VPS without exposing it in shell
history, pipe it over SSH stdin and rewrite only that variable:

```sh
# local machine: token.txt holds ONLY the refresh_token value
ssh root@<vps> 'tmp=$(mktemp) && cat > "$tmp" && \
  sed -i "/^AG_GOOGLE_REFRESH_TOKEN=/d" /etc/antigravity-server.env && \
  printf "AG_GOOGLE_REFRESH_TOKEN=%s\n" "$(cat "$tmp")" >> /etc/antigravity-server.env && \
  rm -f "$tmp" && chmod 600 /etc/antigravity-server.env && \
  systemctl restart antigravity-server' < token.txt
```

Then verify with a real chat request (see below); `/health` alone does not
prove the token works.

Antigravity 2.0 serves its desktop UI over loopback HTTPS. Chromium proxy
settings must bypass `localhost`, `127.0.0.1` and `[::1]`; forcing loopback
through the gateway can cause a black window with `ERR_EMPTY_RESPONSE`.

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

## OpenAI-compatible Chat Completions

The optional adapter translates text chat, streaming responses and function
tool calls to the Cloud Code API. It does not implement the OpenAI Responses
API, embeddings, image input or audio. A client must return tool-call IDs
unchanged when it sends tool results.

Enable the adapter in the server configuration or environment and restart the
service:

```sh
# In the server's environment file:
AG_OPENAI_ENABLED=true
# Optional if automatic Cloud Code project discovery fails:
# AG_GOOGLE_PROJECT_ID=your-cloud-code-project-id
```

When disabled, `/v1/chat/completions` keeps the existing passthrough behavior.
When enabled, the server uses the configured account's `project_id` or asks
Cloud Code for it. Personal Antigravity accounts use the managed project
returned by `loadCodeAssist` (normally `aicode-consumers`); do not configure a
Google Cloud Project ID for them. `AG_GOOGLE_PROJECT_ID` is an override for
enterprise tiers that explicitly require a user-owned Cloud project. The server
identifies itself with the Antigravity Hub User-Agent because other clients may
be rejected from the personal free tier. The model list is configured by the
server; it does not prove that a model is available to the Google account.
End-to-end compatibility with a particular account and model must be verified
against the upstream.

Use the gateway API key, not the Google refresh token. Use HTTPS for remote
connections; plain HTTP exposes both the key and conversation contents.

```sh
# Set AG_CLIENT_KEY locally without putting the real key in this command.
curl --fail-with-body 'https://gateway.example/v1/models' \
  -H "Authorization: Bearer $AG_CLIENT_KEY"

curl --fail-with-body 'https://gateway.example/v1/chat/completions' \
  -H "Authorization: Bearer $AG_CLIENT_KEY" \
  -H 'Content-Type: application/json' \
  -d '{"model":"gemini-3-flash","messages":[{"role":"user","content":"Reply with OK"}]}'
```

### VS Code Chat / GitHub Copilot

Open **Chat: Manage Language Models** from the Command Palette, select
**Add Models**, then **Custom Endpoint** and **Chat Completions**. Enter the
gateway key when prompted. In the generated `chatLanguageModels.json`, set the
model endpoint to the full `/v1/chat/completions` URL and enable tool calling:

```json
[
  {
    "name": "Antigravity",
    "vendor": "customendpoint",
    "apiKey": "${input:antigravityApiKey}",
    "apiType": "chat-completions",
    "models": [
      {
        "id": "gemini-3-flash",
        "name": "Antigravity model",
        "url": "https://gateway.example/v1/chat/completions",
        "toolCalling": true,
        "vision": false,
        "maxInputTokens": 32768,
        "maxOutputTokens": 8192
      }
    ]
  }
]
```

Replace the URL and model ID, and adjust token limits to the selected model.
Store the key in VS Code's secret input, not in the file. Select the model in
Chat. BYOK supports Chat and agent workflows, not inline code completions.

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

Caddy example (bind backend to localhost with `AG_LISTEN_ADDR=127.0.0.1:8045` so port 8045 is not exposed without TLS; `<vps-ip>.sslip.io` works without a custom domain):

```
gateway.example.com {
    reverse_proxy 127.0.0.1:8045 {
        flush_interval -1
    }
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
| GET | `/v1/models` | Bearer key | OpenAI-style model list (`object:"list"`, `data[]`) fetched from `fetchAvailableModels` (15s cache, `?refresh=1`) with reasoning-effort variants consolidated |
| POST | `/v1/chat/completions` | Bearer key | OpenAI-style text chat, tool calls, and `reasoning_effort` variant routing when `[openai] enabled = true`; otherwise passthrough |
| GET | `/v1/quota` | Bearer key | quota snapshot: `plan_name`/`tier_id` from `loadCodeAssist`, `models`/`resets` and `quota_groups` (5h + weekly) from `retrieveUserQuotaSummary` (`?refresh=1`) |
| POST | `/v1/quota/config` | Bearer key | body `{"enable_credit_overages": bool}`; flips the flag (persisted to `state_file`) |
| GET | `/v1/client-update?platform=...` | none | update manifest; empty `version` when unconfigured; `platform=macos` normalizes to `mac-arm` |
| GET | `/v1/client-update/download/{asset_id}` | none | streamed update asset; unknown id → 404 |
| GET | `/v1/proxy-connect?host=...&port=443` (WebSocket) | Bearer key | bidirectional TCP tunnel for HTTP `CONNECT` (ports `80`/`443`, public IP only; used by Antigravity 2.0) |
| GET | `/v1/tunnel?token=KEY` (WebSocket) | query token or Bearer | client tunnel worker; 401/403/404 before upgrade on auth/permission/disabled |
| ANY | `/v1internal*`, `/v1/*` (fallback) | Bearer key | streaming pass-through to upstream; token injected, provenance headers stripped, single transparent retry on upstream 401 |

Auth failures answer `401 {"error":{"message":"Invalid token"}}` (403 for
expired keys). The client key travels as `Authorization: Bearer <key>` (or
`?token=` for the WebSocket upgrade); the Google token is injected
server-side and never shown to the client.

## Non-goals

- No multi-tenant accounts, billing, or per-user quotas beyond the single
  owner's key metadata.
- No translation for the OpenAI Responses, embeddings, image or audio APIs.
- No browser OAuth flow — the server refreshes access tokens from a
  pre-extracted `refresh_token`; it cannot do an interactive sign-in.

## Run tests

```sh
cargo test --manifest-path server/Cargo.toml
```

Includes unit tests plus integration suites: `forward` (streaming
pass-through), `tunnel` (worker protocol), `e2e_contract` (acceptance
criteria AC1–AC8 against the real router on ephemeral ports).
