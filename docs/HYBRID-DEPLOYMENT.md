# Vextalor Global Hybrid Deployment Architecture

## Deployment Modes

| Mode | Where code executes | Network hop | Data boundary | Usage |
| --- | --- | --- | --- | --- |
| Local SDK / self-hosted | The customer's device or server | None | Data never leaves the local execution environment unless the caller sends it elsewhere | Free and unlimited by OmniEngine quotas |
| Managed Cloud / SaaS | Vextalor-managed isolated workers | REST/gRPC API request | Only data included in the API request and gateway-generated metadata is processed by the managed service | Tiered API usage |

Local SDKs must not upload source or results to the managed service automatically.
Managed execution must not access host-device files or unrelated customer state.
File, module, network, and host-function permissions remain fail-closed at both
the worker and API gateway layers.

## Data and Privacy Boundary

For local execution:

- source, bindings, results, logs, and profiling data remain in the caller's
  process or private server;
- local SDK packages are free and unlimited by OmniEngine SaaS request quotas;
- data residency is controlled by the customer's own deployment policy; and
- the operator chooses filesystem roots and module permissions.

For managed cloud execution:

- the API gateway receives only the request body, authentication metadata, and
  explicitly attached configuration;
- workers execute only that request in a fresh or similarly isolated context;
- the sandbox receives no implicit access to the caller's local files or devices;
- cloud response data is limited to the result, status, diagnostics, usage, and
  request metadata permitted by the API contract; and
- retention, audit, and regional-routing behavior is governed by the deployed
  SaaS policy rather than by the local SDK.

## Execution and Routing

```text
Local SDK/host
  +-- Rust engine / C FFI / Python / Node
  +-- 1-5 ms internal target
  +-- zero OmniEngine network latency

Managed SaaS
  Client
    -> TLS
    -> API gateway: token auth, routing, quotas, request validation
    -> isolated OmniEngine worker pool
    -> sandbox result
    -> gateway response
  30-80 ms target end-to-end
```

The Rust VM is the deterministic baseline. The Cranelift JIT accelerates eligible
JIT-safe code; end-to-end latency targets include network, TLS, gateway, parsing,
execution, and worker scheduling. Sub-millisecond execution is an internal
execution target, not a guarantee for gateway round trips.

## Isolation

The cloud gateway and worker nodes enforce independent boundaries:

- bearer or scoped API tokens at the gateway;
- tenant and key quota accounting;
- source-size and request-size limits;
- per-request CPU and memory ceilings;
- concurrency and queue ceilings;
- worker capability allowlists;
- no implicit filesystem, module, network, or host callback access;
- deterministic result verification for routed worker pools; and
- dead, unhealthy, or non-agreeing nodes excluded from dispatch.
