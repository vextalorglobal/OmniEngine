# Managed OmniEngine Cloud API

## Base Contract

The current gateway exposes the managed API over HTTPS. The REST shape is:

```text
POST /v1/execute
Authorization: Bearer <token>
Content-Type: application/json
```

Example REST request:

```json
{
  "language": "omni",
  "source": "40 + 2",
  "gas_limit": 50000,
  "timeout_ms": 50,
  "verification": "deterministic"
}
```

Example REST success:

```json
{
  "request_id": "req_example",
  "status": "ok",
  "runtime": {
    "language": "omni",
    "name": "OmniEngine",
    "runtime": "omni-core",
    "available": true
  },
  "value": {
    "tag": "number",
    "number": 42
  },
  "usage": {
    "cpu_ms": 1,
    "peak_memory_bytes": 2048
  }
}
```

The `language` field is optional. Omitting it preserves the original contract
and selects `omni`; `"default"` is also accepted as an alias. Language IDs are
case-sensitive. Successful results include additive `runtime` metadata so
clients can identify the adapter that executed the request.

Lua example:

```json
{
  "language": "lua",
  "source": "return 40 + 2"
}
```

Lua uses an embedded Lua 5.4 VM with only the base and math libraries enabled;
file, network, package, debug, output, and dynamic code-loading APIs are
removed. Its instruction hook, VM memory limit, UTF-8 source/output bounds, and
the same gateway CPU and request limits apply. Lua is gateway-local and does
not support worker-cluster dispatch or deterministic quorum verification.

## Runtime Discovery

`GET /v1/languages` returns the available language IDs, runtime names,
capabilities, effective gateway limits, and sandbox policy. The discovery
response contains no tenant or credential data and does not require bearer
authentication. `omni`, `lua`, and `wasm` are available. The Wasm runner uses
the embedded Wasmi interpreter to execute WebAssembly Text (WAT) source. A
module must have no imports and export a no-argument `run` function returning
at most one `i32`, `i64`, `f32`, or `f64` value (or no value). Fuel, linear
memory, table, instance, and stack limits are enforced. It has no WASI,
filesystem, network, or host-function access and does not support worker
dispatch or deterministic quorum verification. The separate `omni-wasm`
crate remains a browser-binding scaffold.

`javascript` and `python` are recognized but return `503 runtime_unavailable`.
The gateway does not invoke system interpreters: no embedded interpreter with
enforced gas/memory budgets and disabled host APIs is currently configured.
The repository's Python and Node packages are client SDKs over the native C
ABI, not interpreters for submitted Python or JavaScript source.

Unknown language IDs return `422 unsupported_language`. A known runtime that
cannot execute in the configured topology returns `503` with a diagnostic
code. In particular, Lua cannot be routed through the current Omni-only worker
protocol.

## Planned gRPC Contract

An equivalent gRPC service is a planned API surface; it is not implemented by
the current `omni-gateway` crate:

```text
service OmniExecution {
  rpc Execute(ExecuteRequest) returns (ExecuteResponse);
  rpc Health(HealthRequest) returns (HealthResponse);
}

message ExecuteRequest {
  string source = 1;
  uint64 gas_limit = 2;
  uint32 timeout_ms = 3;
  bool deterministic_verification = 4;
  string language = 5;
}
```

REST and gRPC must produce the same status classes, value tags, sandbox
diagnostics, and usage accounting.

## Listener, TLS, and CORS

The local configuration binds to `127.0.0.1:8443` and serves direct TLS using
the configured certificate and private-key PEM paths. `omni-gateway` supports
the following startup overrides:

| Variable | Behavior |
| --- | --- |
| `OMNI_BIND_ADDR` | Bind address, e.g. `127.0.0.1:8443` or `0.0.0.0:8443` |
| `OMNI_PORT` | Overrides the configured/bind-address port |
| `OMNI_TLS_MODE` | `tls` (default) or `plain` behind a trusted TLS-terminating proxy |
| `OMNI_ALLOWED_ORIGINS` | Comma-separated exact browser origins |
| `OMNI_API_KEYS` | Comma-separated `tenant:key` values; overrides the configured key source |

Use `OMNI_TLS_MODE=plain` only when the gateway is reachable exclusively on a
trusted private network behind a proxy that terminates HTTPS. Do not expose a
plain listener to the public internet. `/v1/health`, `/v1/languages`, and
`/v1/execute` use the same routes in direct TLS and trusted-proxy modes.

The gateway permits browser CORS only for exact configured origins, including
preflight requests. Wildcard origins are rejected. An empty origin list
disables cross-origin browser access without affecting same-origin requests
or non-browser API clients. See [DEPLOYMENT.md](DEPLOYMENT.md) for Docker,
Caddy, and Nginx setup.

## Authentication

Execution routes require a bearer token or gRPC metadata key:

```text
authorization: Bearer <token>
```

- tokens are issued per tenant and capability scope;
- credentials must be sent over TLS between client and trusted reverse proxy or
  directly to the gateway;
- keys must be rotatable and revocable;
- authentication occurs before source parsing or worker dispatch;
- failed authentication must not disclose tenant or quota existence; and
- worker-to-worker traffic uses separate cluster credentials from customer API
  tokens.

`GET /v1/health` and `GET /v1/languages` are unauthenticated aggregate and
capability endpoints; neither returns tenant identifiers or credential data.

## Rate Limiting

The gateway applies both quota and concurrency controls:

- monthly request quota;
- rolling per-key request rate;
- maximum concurrent requests;
- queue depth and rejection policy; and
- retry-after metadata for throttled or capacity-rejected requests.

Monthly quota and rolling-window updates are serialized across gateway
processes sharing the same state path by an OS-backed sibling lock file; the
snapshot is reloaded while locked before each admission and replaced atomically.
The lock is released by the operating system if a process exits. Active-request
concurrency permits remain process-local and are reset on restart, so use an
external coordinator if the concurrency ceiling must span multiple replicas.

Recommended HTTP responses:

| Status | Meaning |
| --- | --- |
| `401` | Missing or invalid token |
| `403` | Token lacks endpoint or tenant capability |
| `429` | Per-key rate or monthly quota exceeded |
| `409` | Deterministic worker results disagree |
| `503` | No healthy worker or insufficient queue capacity |
| `400` | Invalid source or resource configuration |
| `422` | Unsupported language ID or runtime execution error |
| `413` | Request or source exceeds gateway size limits |

## Resource Enforcement

Every request is bounded independently at the gateway and worker:

- CPU execution: 50ms default for the Free Tier;
- sandbox memory: 128MiB default for the Free Tier;
- maximum concurrency: 10 requests per Free Tier tenant/key;
- gas: translated into a VM/JIT step ceiling before dispatch;
- source size: gateway and worker limits, with worker-side enforcement;
- filesystem: no implicit read or write roots;
- modules: disabled unless explicitly configured for a private deployment;
- host functions: disabled by default for managed cloud execution; and
- output: length-capped and treated as untrusted response data.

If gateway and worker limits differ, the stricter effective limit applies.
Cloud requests must never widen a worker's configured permission set.
All registered runners declare their filesystem, network, module, host-function,
resource-limit, cluster, and verification capabilities through the language
discovery endpoint. A runtime without a proven isolated adapter is reported as
unavailable rather than being launched as a subprocess.

## Latency Targets

| Scope | Target |
| --- | --- |
| JIT-safe internal execution | Sub-millisecond |
| VM/local SDK execution | 1-5ms internal target |
| Managed API end-to-end | 30-80ms, including network and gateway |

Cold starts, queueing, cross-region routing, and result verification are excluded
from internal execution numbers but included in end-to-end latency.

## Health and Deterministic Verification

When cluster dispatch is enabled, the gateway checks configured workers before
dispatch. Worker connections require certificate-verified TLS and a separate
32-byte cluster PSK; the customer API token is never forwarded to workers.
Configure a CA bundle and expected worker DNS name, and provision the PSK through
the gateway's secret environment. Worker-side filesystem roots and resource
ceilings remain operator-controlled.

The configured health endpoint (default `/v1/health`) serves unauthenticated
aggregate gateway status over HTTPS. Its JSON response includes request
success/failure, quota/rate/concurrency rejection, worker dispatch, and
deterministic quorum counters, plus cluster enablement and configured worker
count. It contains no tenant identifiers or API-key material. Health reads are
excluded from request counters. Worker health checks separately use the
authenticated cluster `Ping` protocol over the configured TLS connection.

For requests with `"verification": "deterministic"`, the gateway dispatches to
the configured quorum of distinct healthy workers and compares status, value
tag, numeric value, and rendered text. A mismatch returns `409` and rejects the
result; an unavailable quorum returns `503`. The gateway never silently accepts
a single-worker result for a verification request. Local fallback, when enabled,
applies only to transient errors on non-verification requests.

CPU deadlines are checked between VM instructions, with elapsed-time checks
after execution. The Lua runner also applies its allocator limit and checks
time/gas at each instruction. VM memory figures do not cap all Rust or gateway
process allocations, and an in-process runtime is not an OS security boundary.
Operators should isolate gateway and worker processes with OS/container limits.
