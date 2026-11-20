# OmniEngine

OmniEngine is a Rust-based execution platform from Vextalor Global. It includes
a compact language engine and CLI, C FFI and local SDKs, a TLS REST gateway for
policy-bounded remote execution, and a localhost Execution Studio.

The project supports two deployment models:

- **Local / self-hosted:** execute on the user's device or server through the
  CLI, C ABI, or Python and Node.js SDKs. Execution does not send source or
  results to an OmniEngine service and is not subject to the gateway's API
  quota.
- **Managed API:** send a request to the HTTPS gateway. Only the data included
  in that request is processed remotely. The gateway authenticates the request,
  applies tier and sandbox limits, and can dispatch supported work to a
  configured worker pool.

## Features

- **OmniEngine runtime:** a Rust lexer, compiler, bytecode VM, and native
  execution paths, accessible from Rust, the CLI, C, Python, and Node.js.
- **Gateway:** `omni-gateway` serves TLS REST on port `8443` by default, with
  bearer-token authentication for execution, health and language discovery
  endpoints, durable quota/rate state, and structured JSON errors.
- **Sandbox policy:** the managed Free Tier defaults to 50ms CPU, 128MiB
  sandbox memory, 100,000 requests per month, and 10 concurrent requests per
  key. Source and output are size-limited. Filesystem, network, imports, and
  host functions are disabled by default.
- **Worker cluster:** the gateway can dispatch OmniEngine work over
  certificate-verified TLS with separate cluster credentials. Deterministic
  verification requires a configured worker quorum; it does not silently
  substitute a single result.
- **Execution Studio:** the local dashboard at `http://localhost:3000` includes
  Arithmetic, Gas limit, Memory load, and Unicode text presets; round-trip,
  sandbox-usage, quota, and health telemetry; execution history; and a
  structured result/error inspector.
- **Languages and Unicode:** `omni` is the default runtime; embedded Lua 5.4 is
  also available in the gateway. JavaScript, Python, and WebAssembly are listed
  for discovery but are not currently executable by the gateway. Source,
  comments, string literals, inputs, and outputs support UTF-8, including
  Bengali and other scripts. Identifiers remain ASCII-only.

See [docs/HYBRID-DEPLOYMENT.md](docs/HYBRID-DEPLOYMENT.md) for deployment
guidance, [docs/DEPLOYMENT.md](docs/DEPLOYMENT.md) for local and Docker/Caddy
setup, and [docs/CLOUD-API.md](docs/CLOUD-API.md) for the API contract, limits,
and runtime availability.

## Quick start on Windows

Requirements: Rust/Cargo and Node.js on `PATH`. From the repository root, run:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\dev-up.ps1
```

The bootstrap builds the gateway and its native certificate helper, creates
the local TLS certificate and key, writes `.dev\gateway.toml`, creates a
random local API key, then starts the gateway and dashboard bridge. It opens
the Studio in your browser at <http://localhost:3000>. Press `Ctrl+C` in that
PowerShell session to stop both services.

The local gateway listens on `https://127.0.0.1:8443`. The development bridge
proxies requests from the browser, so the browser does not need to handle the
self-signed gateway certificate directly. The bootstrap wires the local API
key into the dashboard session. You can also generate a key in the Studio's
**API Key Generator** panel and select **Use in Studio**.

All local certificates, keys, quota state, and logs live under `.dev\`. That
directory is gitignored and must remain local; do not commit its contents.
`config\cloud-api.example.toml` is a deployment template, not a local
certificate bundle. The bootstrap derives its local configuration from that
template and binds only to loopback.

### Run a sample execution

In the Studio, leave **OmniEngine** selected, use the **Arithmetic** preset,
and press **Run Execution** (or `Ctrl+Enter`). The result should be `42`.
Choose **Lua 5.4** to try its language-specific presets, including the
multi-script Unicode example.

You can also exercise the bridge from another PowerShell window:

```powershell
$config = Invoke-RestMethod http://127.0.0.1:3000/api/config
Invoke-RestMethod http://127.0.0.1:3000/api/health
Invoke-RestMethod http://127.0.0.1:3000/api/execute `
  -Method Post `
  -Headers @{ Authorization = "Bearer $($config.api_key)" } `
  -ContentType 'application/json' `
  -Body '{"source":"40 + 2"}'
```

The API key is returned only by the loopback development bridge and should not
be copied into logs, source control, or shared terminals.

## Gateway API

The main routes are:

| Method | Route | Authentication | Purpose |
| --- | --- | --- | --- |
| `POST` | `/v1/execute` | Bearer token | Execute source under gateway policy |
| `GET` | `/v1/health` | None | Aggregate gateway health and metrics |
| `GET` | `/v1/languages` | None | Runtime availability, capabilities, and limits |

Example request:

```json
{
  "language": "omni",
  "source": "40 + 2",
  "gas_limit": 50000,
  "timeout_ms": 50
}
```

The `language` field is optional; omitting it preserves compatibility and
selects `omni`. Use `language: "lua"` with standard Lua `return` syntax for the
embedded Lua 5.4 runtime. Lua exposes only base and math libraries and has
instruction, time, and VM memory limits. It does not support worker-cluster
dispatch or deterministic quorum verification. The gateway returns
`503 runtime_unavailable` for recognized but unavailable runtimes rather than
launching a host interpreter.

Gateway configuration is read from `OMNI_GATEWAY_CONFIG` and
`OMNI_USAGE_POLICY`; the API key source is configured in TOML and supplied
through `OMNI_API_KEYS` as comma-separated `tenant:key` entries. The gateway
binary has no command-line flags. `omni serve` is a separate command for
starting a cluster worker, not the REST gateway.

## Troubleshooting

- **Gateway offline:** confirm the bootstrap is still running, then inspect
  `.dev\gateway.stderr.log`, `.dev\gateway.stdout.log`, and
  `.dev\ui.stderr.log`. The bootstrap reports conflicts on ports `8443` and
  `3000`; it leaves existing listeners untouched.
- **TLS/configuration errors:** use the generated `.dev\gateway.toml` and
  certificate/key pair together. The example configuration points to
  deployment secret paths and is not directly usable for local TLS.
- **`401 unauthorized`:** enter a key accepted by the gateway. In the local
  Studio, use a generated key with **Use in Studio**. For direct gateway
  configuration, verify `OMNI_API_KEYS` uses the configured `tenant:key`
  format.
- **`422 unsupported_language` / `422 runtime_error`:** check the language ID
  in `GET /v1/languages` and inspect the structured error body. Source
  compilation errors and unsupported language IDs have distinct diagnostics.
- **`503 runtime_unavailable` / `503 workers_unavailable`:** the selected
  language is unavailable, or the configured worker pool cannot satisfy the
  request. Check runtime discovery, worker TLS/PSK settings, quorum, and
  gateway logs.
- **`429`:** the request exceeded a rate/quota or execution resource limit
  such as CPU, gas, or memory. Check the configured tier policy and the
  `Retry-After` header when present.
- **`409`:** deterministic worker results disagreed. The gateway rejects the
  result rather than returning an unverified value.

Useful local checks:

```powershell
Invoke-RestMethod http://127.0.0.1:3000/api/health
Invoke-RestMethod http://127.0.0.1:3000/api/languages
cargo test -p omni-core -p omni-gateway
cargo check --workspace
```

## Repository layout

| Path | Contents |
| --- | --- |
| `crates/omni-core` | Language engine, lexer, compiler, bytecode VM, and value model |
| `crates/omni-cli` | `omni run`, `omni eval`, updater commands, and `omni serve` worker |
| `crates/omni-ffi` | C ABI used by native hosts and the Python/Node.js SDKs |
| `crates/omni-cluster` | TLS worker protocol, client, node, and dispatch logic |
| `crates/omni-gateway` | HTTPS REST gateway, policy enforcement, language runners, and dev certificate helper |
| `crates/omni-jit` | Cranelift JIT integration |
| `crates/omni-wasm` | WebAssembly binding scaffold; not a general untrusted Wasm host |
| `sdks/` | Python and Node.js local SDKs |
| `config/` and `policies/` | Gateway configuration template and usage-tier policy |
| `docs/` | Deployment, API, cluster, and release documentation |
| `scripts/dev-up.ps1` and `ui/` | Windows local bootstrap and browser dashboard/bridge |
| `Dockerfile`, `docker-compose.yml`, `Caddyfile` | Multi-stage cloud deployment with private gateway and automatic HTTPS proxy |

## License

OmniEngine is distributed under the Vextalor Global Public & Integration
License (VGPIL) 1.0. See [LICENSE](LICENSE).
