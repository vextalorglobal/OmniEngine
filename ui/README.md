# OmniEngine Local Development

## Start everything

From `D:\OmniEngine`, run this one command in PowerShell:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\dev-up.ps1
```

The bootstrap builds the gateway and its Rust-native certificate helper,
generates fresh self-signed PEM files for `localhost` and `127.0.0.1`, writes
`.dev\gateway.toml`, creates a persistent random local API key, then starts the
dashboard bridge. The bridge supervises the TLS gateway, while the gateway
itself handles generated-key creation and revocation without restarting.
The bootstrap opens the dashboard at <http://127.0.0.1:3000>; close the one
PowerShell session with Ctrl+C to stop both services. No OpenSSL, mkcert, npm
install, or second terminal is required.

The generated API key is stored in `.dev\api-key.txt`, restricted to the
current Windows user, and automatically supplied to the dashboard for the
current browser session. A separate random administrator token is stored in
`.dev\key-admin.txt` with the same ACL. `.dev/` is gitignored. These credentials
and the self-signed certificate are for local development only.

For hidden/background startup (no persistent terminal window):

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\dev-up-background.ps1
```

Stop it with:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\dev-down-background.ps1
```

The background helper logs to `.dev\bootstrap.stdout.log` and
`.dev\bootstrap.stderr.log`. Its hidden supervisor restarts the gateway and
dashboard after unexpected startup/runtime exits, with a bounded backoff.
`dev-down-background.ps1` requests a graceful stop without terminating an
unrelated process.

The Studio's **Gateway mode** selector switches between
`https://127.0.0.1:8443` and a custom/cloud URL. The selected URL is kept in
browser `localStorage`; API keys remain in `sessionStorage`. In hosted
deployments, set `OMNI_PUBLIC_GATEWAY_URL` to the public HTTPS origin and
`OMNI_GATEWAY_URL` to the dashboard bridge's private gateway upstream. The
bridge maps the public origin to that upstream and, when
`OMNI_GATEWAY_PROXY_LOCK=1`, refuses unconfigured proxy targets.

## Unicode source

Source files, comments, string literals, JSON inputs, and results use UTF-8;
the dashboard's Unicode preset includes Bengali and other scripts. Source
limits are measured in UTF-8 bytes (shown below the editor), not displayed
characters. Identifiers remain ASCII-only; use ASCII variable names with
Unicode text values.

## Language runners

The Studio defaults to `omni`, so existing requests and presets continue to
work. `lua` runs in the gateway's embedded Lua 5.4 VM with only base and math
libraries, plus per-request instruction, time, and VM memory limits. Lua chunks
use standard `return` syntax.

`wasm` runs WebAssembly Text (WAT) in the embedded Wasmi interpreter. A task
must be a self-contained module (no imports) exporting a no-argument function
named `run` that returns zero or one scalar (`i32`, `i64`, `f32`, or `f64`).
The gateway enforces instruction fuel, linear-memory and table caps, and a
bounded interpreter stack. No WASI, filesystem, network, or host functions are
linked. The arithmetic, gas-limit, and memory presets demonstrate this
contract. Wasm is gateway-local and does not support worker dispatch or
deterministic quorum verification.

`javascript` and `python` remain recognized but unavailable
(`503 runtime_unavailable`). This build has no embedded interpreter that can
enforce the gateway's gas/CPU/memory limits while disabling filesystem,
imports, network, and host APIs. The gateway deliberately never launches
`node`, `python`, or another host executable for untrusted source.
`GET /v1/languages` is authoritative for availability and capabilities.

## Generate and manage local keys

Open **API Key Generator**, add a label, and generate a Free Tier key
(100,000 requests/month, 50ms CPU, 128MB memory, 10 concurrent requests). The
bridge forwards the request to the gateway's authenticated `POST /v1/keys`
endpoint. The gateway generates the token, activates it immediately, and
persists only its SHA-256 digest in the gateway state registry. The raw token
is returned once, put in the Studio's API key field, and listed in browser
`sessionStorage`; use **Copy** to save it securely. It cannot be recovered
from the gateway after that response. Generated keys survive a gateway
restart because the digest registry is persisted under `.dev\`.

The gateway API key and key-administration token are separate. The local
bootstrap supplies the administrator token only to loopback requests; cloud
dashboards require an operator to enter the provisioning token in the panel.
The cloud token is used only in request memory and is not saved in browser
storage. The gateway currently assigns the configured Free Tier policy to all
generated keys and allows at most 25 managed keys. Revoke a generated key
from its session row or revoke all session-listed keys; revocation is handled
by `POST /v1/keys/revoke` at the gateway.

## Verify the gateway and execute a task

The bootstrap waits until the dashboard's health proxy reports the gateway
online. You can also verify it in another PowerShell window:

```powershell
Invoke-RestMethod http://127.0.0.1:3000/api/health
$config = Invoke-RestMethod http://127.0.0.1:3000/api/config
Invoke-RestMethod http://127.0.0.1:3000/api/execute `
  -Method Post `
  -Headers @{ Authorization = "Bearer $($config.api_key)" } `
  -ContentType 'application/json' `
  -Body '{"source":"40 + 2"}'
Invoke-RestMethod http://127.0.0.1:3000/api/languages
Invoke-RestMethod http://127.0.0.1:3000/api/execute `
  -Method Post `
  -Headers @{ Authorization = "Bearer $($config.api_key)" } `
  -ContentType 'application/json' `
  -Body '{"language":"lua","source":"return 40 + 2"}'
```

The health JSON reports `"status": "ok"` and the execution result should
contain numeric value `42`. Language discovery shows each runtime's status and
capabilities. The Lua response identifies `"runtime.language": "lua"` and also
contains numeric value `42`. The dashboard itself also fetches the local API
key from the loopback-only bridge; no key entry or environment setup is needed
by hand.

## Configuration and diagnostics

The gateway has no CLI flags: it reads `OMNI_USAGE_POLICY`,
`OMNI_GATEWAY_CONFIG`, `OMNI_GATEWAY_STATE`, and the configured API-key
environment variable (`OMNI_API_KEYS=tenant:key`). The bootstrap sets these
consistently and binds the gateway to `127.0.0.1:8443`.

`omni serve` is a separate cluster-worker command, not the REST gateway. It
accepts `--psk` or `--psk-file`, `--bind`, `--gas-cap`, `--allow-read`,
`--cpu-ms-cap`, `--memory-mib-cap`, `--tls-cert`, and `--tls-key`.

If port `8443` or `3000` is already occupied, the bootstrap reports which port
to free and leaves the existing process untouched. Startup logs are written
under `.dev\`. To start without opening a browser, add `-NoBrowser` to the
PowerShell command.

## Hosted key management

For cloud deployments, configure both `OMNI_KEY_ADMIN_ENABLED=1` on the
dashboard bridge and a separate `OMNI_KEY_ADMIN_TOKEN` on the gateway. Keep the
admin token in the hosting provider's secret store, never in the UI build,
query string, or checked-in configuration. The Studio asks for this token
before enabling key issuance; normal execution still uses the generated
tenant API key. See the [Cloud API key lifecycle](../docs/CLOUD-API.md#managed-api-key-lifecycle)
and [deployment guide](../docs/DEPLOYMENT.md#render-and-custom-hosted-domains).
