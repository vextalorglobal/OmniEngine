# Managed OmniEngine Cloud API

## Base Contract

The managed API uses HTTPS and a TLS-enabled gRPC gateway. The REST shape is:

```text
POST /v1/execute
Authorization: Bearer <token>
Content-Type: application/json
```

Example REST request:

```json
{
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

## gRPC Contract

The equivalent gRPC service shape is:

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
}
```

REST and gRPC must produce the same status classes, value tags, sandbox
diagnostics, and usage accounting.

## Authentication

All public API routes require a bearer token or gRPC metadata key:

```text
authorization: Bearer <token>
```

- tokens are issued per tenant and capability scope;
- credentials must be sent only over TLS;
- keys must be rotatable and revocable;
- authentication occurs before source parsing or worker dispatch;
- failed authentication must not disclose tenant or quota existence; and
- worker-to-worker traffic uses separate cluster credentials from customer API
  tokens.

## Rate Limiting

The gateway applies both quota and concurrency controls:

- monthly request quota;
- rolling per-key request rate;
- maximum concurrent requests;
- queue depth and rejection policy; and
- retry-after metadata for throttled or capacity-rejected requests.

Recommended HTTP responses:

| Status | Meaning |
| --- | --- |
| `401` | Missing or invalid token |
| `403` | Token lacks endpoint or tenant capability |
| `429` | Per-key rate or monthly quota exceeded |
| `503` | No healthy worker or insufficient queue capacity |
| `400` | Invalid source or resource configuration |
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

## Latency Targets

| Scope | Target |
| --- | --- |
| JIT-safe internal execution | Sub-millisecond |
| VM/local SDK execution | 1-5ms internal target |
| Managed API end-to-end | 30-80ms, including network and gateway |

Cold starts, queueing, cross-region routing, and result verification are excluded
from internal execution numbers but included in end-to-end latency.

## Health and Deterministic Verification

Gateway workers expose health only to the control plane:

```text
GET /v1/health
grpc OmniExecution.Health
```

The public execute route should support deterministic verification by dispatching
eligible tasks to multiple workers and comparing status, value tag, numeric
value, and rendered text. A mismatch rejects the result rather than selecting a
majority silently. Unhealthy workers are retried or removed according to the
cluster fault policy.
