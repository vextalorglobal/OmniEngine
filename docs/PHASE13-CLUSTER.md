# Phase 13: Distributed Cluster Execution

## Protocol

`omni-cluster` is a connection-per-task protocol over TCP:

1. a fresh OS-random nonce handshake derives separate client/worker keys;
2. every frame is length-capped and ChaCha20-Poly1305 sealed;
3. sequence numbers prevent replay within a connection; and
4. the worker sends one signed task reply and closes the connection.

The implementation is PSK-authenticated and AEAD-encrypted, not a TLS
certificate stack. Deploy workers behind a TLS 1.3 termination proxy for
network links that require standard certificate verification. Bind the
terminated proxy only to the trusted cluster network and keep the PSK inside
the protected network segment.

## Worker Isolation

Each task runs on a fork of a startup template engine. The worker does not
expose engine mutation or host callback registration through the wire. The
node operator alone sets:

- the maximum source size;
- the gas cap;
- filesystem read roots; and
- the pre-shared key.

Remote imports and undocumented permission grants are fail-closed.

## FFI and SDK

`omni_exec_remote(engine, nodes, node_count, psk, psk_len, source, len, gas,
retries, out, error, error_cap)` dispatches one task. It is synchronous and
returns `OMNI_ERR_CLUSTER` for pool/protocol failures.

Python:

```python
with omni.Cluster(["worker-a:4545", "worker-b:4545"], psk) as cluster:
    result = cluster.run_remote("40 + 2", gas=10_000, retries=2)
```

Node.js:

```javascript
const cluster = new Cluster(["worker-a:4545", "worker-b:4545"], psk);
const result = cluster.runRemote("40 + 2", 10_000, 2);
cluster.close();
```
