# OmniEngine Deployment

OmniEngine supports loopback-only local development and a cloud deployment
behind a TLS-terminating reverse proxy. In both modes the gateway remains the
authority for authentication, quotas, resource limits, language availability,
and sandbox permissions.

## Local development

From `D:\OmniEngine`, start the gateway and dashboard together:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\dev-up.ps1
```

The script builds the gateway, creates local TLS files and `.dev\gateway.toml`,
sets a local `OMNI_API_KEYS` value, starts the gateway at
`https://127.0.0.1:8443`, and serves the Studio at
<http://localhost:3000>. The UI bridge handles the local self-signed TLS
certificate; browsers only connect to the HTTP localhost dashboard. Stop both
processes with `Ctrl+C` in the bootstrap PowerShell window.

The generated key, certificate/private key, gateway state, and logs stay under
`.dev\`; `.dev/` is gitignored. Never commit or share those files. Check the
gateway from another PowerShell window:

```powershell
Invoke-RestMethod http://127.0.0.1:3000/api/health
$config = Invoke-RestMethod http://127.0.0.1:3000/api/config
Invoke-RestMethod http://127.0.0.1:3000/api/execute `
  -Method Post `
  -Headers @{ Authorization = "Bearer $($config.api_key)" } `
  -ContentType 'application/json' `
  -Body '{"source":"40 + 2"}'
```

The UI starts in **Local** mode. **Custom / Cloud URL** lets a developer choose
another gateway; the selected URL persists in browser `localStorage`, while
bearer keys remain in session storage.

## Cloud VPS with Docker Compose and Caddy

The repository `Dockerfile` builds the Rust gateway in a builder stage and
provides a small dashboard stage. `docker-compose.yml` runs the gateway, UI
bridge, and Caddy. Caddy obtains and renews public HTTPS certificates for the
configured domain and routes `/v1/*` to the gateway and the Studio/API bridge
to the dashboard.

### Prepare the host

1. Install Docker Engine and the Docker Compose plugin.
2. Point a DNS `A`/`AAAA` record for the domain to the VPS.
3. Allow inbound TCP ports `80` and `443` (and optionally UDP `443` for HTTP/3)
   in the VPS firewall and cloud security group. Do **not** publish the
   gateway's internal port `8443` to the public internet.
4. Create an API key in a secret manager or generate a cryptographically
   random value. Use the `tenant:key` format; multiple entries are comma
   separated. Do not put production keys in a checked-in file.

For a temporary shell-based setup:

```sh
export OMNI_DOMAIN=api.example.com
export OMNI_API_KEYS="production:$(openssl rand -hex 32)"
docker compose up --build -d
```

PowerShell equivalent when deploying from Windows with Docker Desktop:

```powershell
$env:OMNI_DOMAIN = 'api.example.com'
$bytes = New-Object 'System.Byte[]' 32
$rng = [Security.Cryptography.RandomNumberGenerator]::Create()
try { $rng.GetBytes($bytes) } finally { $rng.Dispose() }
$token = [BitConverter]::ToString($bytes).Replace('-', '').ToLowerInvariant()
$env:OMNI_API_KEYS = "production:$token"
docker compose up --build -d
```

Prefer your platform's secret injection for long-running services. The Compose
file requires `OMNI_API_KEYS` and `OMNI_DOMAIN` and fails rather than starting
with missing credentials. After startup, test the proxy and API:

```sh
curl --fail https://api.example.com/v1/health
curl --fail https://api.example.com/api/health
curl --fail https://api.example.com/v1/languages
curl --fail https://api.example.com/v1/execute \
  -H "Authorization: Bearer ${OMNI_API_KEYS#*:}" \
  -H 'Content-Type: application/json' \
  --data '{"source":"40 + 2"}'
docker compose ps
docker compose logs --tail=100 gateway dashboard caddy
```

The sample key extraction command is only for a single `tenant:key` entry.
Avoid putting bearer tokens on shared command lines or in shell history;
production health and language discovery do not require a key.

### TLS termination and network boundaries

The gateway defaults to direct TLS (`OMNI_TLS_MODE=tls`) and loads the PEM paths
from its config. The Compose example sets `OMNI_TLS_MODE=plain` only because
Caddy terminates public TLS. In this mode, the gateway listens on the private
Compose network at `0.0.0.0:8443`; the port is not published to the host. Do
not use plain mode on a public interface or an untrusted network. If using a
different reverse proxy, expose the plaintext listener only on a private
interface/network, terminate HTTPS at the proxy, and forward `/v1/health` and
`/v1/execute` unchanged.

Nginx deployments should use a valid certificate and key provisioned by
Certbot or another trusted certificate manager. A minimal site configuration
shape is:

```nginx
server {
    listen 80;
    server_name api.example.com;
    return 301 https://$host$request_uri;
}

server {
    listen 443 ssl http2;
    server_name api.example.com;
    ssl_certificate     /etc/letsencrypt/live/api.example.com/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/api.example.com/privkey.pem;

    location /v1/ {
        proxy_set_header Host $host;
        proxy_set_header X-Forwarded-Proto https;
        proxy_pass http://127.0.0.1:8443;
    }
    location / {
        proxy_set_header Host $host;
        proxy_pass http://127.0.0.1:3000;
    }
}
```

For Nginx on the host, configure gateway and dashboard to bind to loopback,
set `OMNI_TLS_MODE=plain` for the private upstream, and do not expose port `8443`
in the firewall. Use an internal network instead of loopback when Nginx is in
another container. Configure the dashboard bridge with the private upstream,
public UI URL, and an exact host/origin allowlist before starting it:

```sh
export OMNI_BIND_ADDR=127.0.0.1:8443
export OMNI_PORT=8443
export OMNI_TLS_MODE=plain
export OMNI_API_KEYS='production:<inject-from-your-secret-manager>'
export OMNI_ALLOWED_ORIGINS=https://api.example.com
export OMNI_GATEWAY_CONFIG=/opt/omniengine/config/cloud-api.example.toml
export OMNI_USAGE_POLICY=/opt/omniengine/policies/usage-tiers.toml
export OMNI_GATEWAY_STATE=/var/lib/omniengine/gateway-state.json

export HOST=127.0.0.1
export PORT=3000
export OMNI_GATEWAY_URL=http://127.0.0.1:8443
export OMNI_PUBLIC_GATEWAY_URL=https://api.example.com
export OMNI_GATEWAY_ALLOW_HTTP=1
export OMNI_GATEWAY_PROXY_LOCK=1
export OMNI_ALLOWED_HOSTS=api.example.com
export OMNI_UI_ALLOWED_ORIGINS=https://api.example.com
node ui/server.js
```

Run the gateway with the same `OMNI_API_KEYS`, `OMNI_ALLOWED_ORIGINS`,
`OMNI_BIND_ADDR`, and `OMNI_TLS_MODE` values in its service environment. Inject
the API key securely rather than pasting the placeholder into a terminal.

## Gateway environment

Environment values override the corresponding gateway config at startup:

| Variable | Behavior |
| --- | --- |
| `OMNI_BIND_ADDR` | Listener address, such as `127.0.0.1:8443` or `0.0.0.0:8443` |
| `OMNI_PORT` | Listener port; overrides the port component of `OMNI_BIND_ADDR` |
| `OMNI_API_KEYS` | Comma-separated `tenant:key` credentials; API keys are never logged |
| `OMNI_TLS_MODE` | `tls` (default) for direct TLS, or `plain` behind a trusted TLS proxy |
| `OMNI_ALLOWED_ORIGINS` | Comma-separated exact browser origins, e.g. `https://studio.example.com` |
| `OMNI_GATEWAY_CONFIG` | TOML config file path |
| `OMNI_USAGE_POLICY` | Tier-policy TOML path |
| `OMNI_GATEWAY_STATE` | Durable quota/rate state path |

`OMNI_ALLOWED_ORIGINS` does not accept wildcard origins. When empty, browser
cross-origin requests are not allowed; same-origin requests and non-browser API
clients continue to work. A reverse proxy must preserve the request `Origin`
and route preflight `OPTIONS` requests to the gateway. The gateway returns
explicit CORS headers only for configured origins.

The `gateway` service keeps state in the named `gateway-state` volume. The
dashboard bridge uses `OMNI_GATEWAY_URL` for the internal gateway URL and
`OMNI_PUBLIC_GATEWAY_URL` for the URL presented in the UI. Its upstream lock
allows only those configured origins, preventing the hosted bridge from
becoming an arbitrary-URL proxy. `OMNI_ALLOWED_HOSTS` is a comma-separated
exact host/host:port allowlist for the bridge; the older
`OMNI_UI_ALLOWED_HOSTS` remains supported. `OMNI_UI_ALLOWED_ORIGINS` can
explicitly allow browser origins.

### Render and custom hosted domains

On Render, the bridge automatically permits the exact
`RENDER_EXTERNAL_HOSTNAME`, accepts same-origin HTTPS requests, and uses
`https://<RENDER_EXTERNAL_HOSTNAME>` as the dashboard's initial gateway URL
unless `OMNI_PUBLIC_GATEWAY_URL` is set. Render deployments also accept
`*.onrender.com` Host headers when `RENDER`, `RENDER_EXTERNAL_HOSTNAME`, or
`OMNI_TLS_MODE=plain` indicates a hosted deployment. For a custom domain, set
`OMNI_ALLOWED_HOSTS` to its exact hostname (for example
`omniengine.onrender.com` or `engine.example.com`); this also supplies the
default HTTPS gateway URL when no public URL is configured. If the gateway is
on a separate private address, set `OMNI_GATEWAY_URL` to that upstream and
keep `OMNI_PUBLIC_GATEWAY_URL` set to the browser-facing HTTPS URL.

The bridge still defaults to loopback hosts only outside hosted mode. Set
`OMNI_ALLOWED_HOSTS=*` only when intentionally accepting every Host header;
wildcard host access does not disable gateway bearer-token authentication.
For other cloud platforms, explicitly configure the exact host and origin:

```text
OMNI_ALLOWED_HOSTS=engine.example.com
OMNI_PUBLIC_GATEWAY_URL=https://engine.example.com
OMNI_UI_ALLOWED_ORIGINS=https://engine.example.com
```

For a non-Docker cloud service, configure `OMNI_GATEWAY_CONFIG`,
`OMNI_USAGE_POLICY`, `OMNI_API_KEYS`, `OMNI_BIND_ADDR`, and `OMNI_TLS_MODE`
explicitly. Keep all existing managed sandbox settings disabled:

```toml
[gateway.sandbox]
filesystem_reads = false
filesystem_writes = false
module_imports = false
host_functions = false
network_access = false
deterministic_verification = true
```

The Docker Compose example runs one gateway replica. Durable file locking
protects quota/rate updates among processes sharing a suitable local filesystem;
active concurrency limits are process-local. For multiple replicas, use a
shared locking-capable filesystem and an external concurrency coordinator if
the concurrency ceiling must apply cluster-wide.

## Configuration and operations

- `config/cloud-api.example.toml` is a template, not a secret store. It binds
  locally by default and has placeholder TLS paths.
- `policies/usage-tiers.toml` must remain aligned with the gateway config.
- Rotate keys by updating the injected `OMNI_API_KEYS` and restarting/recreating
  the gateway; never expose production key-generation or revocation controls to
  unauthenticated public users.
- Back up the Compose `gateway-state` volume using the host's protected backup
  process. It contains usage metadata, not raw API keys.
- Inspect startup and request errors with `docker compose logs`; local errors
  and logs are under `.dev\`.
- `401` indicates missing/invalid credentials, `403` a denied origin or
  capability, `409` a deterministic quorum mismatch, `422` invalid/unsupported
  execution input, `429` quota/rate/resource enforcement, and `503` an
  unavailable runtime or worker topology.

See [CLOUD-API.md](CLOUD-API.md) for request/response semantics and
[HYBRID-DEPLOYMENT.md](HYBRID-DEPLOYMENT.md) for the local versus managed data
boundary.
