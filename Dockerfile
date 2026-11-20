FROM rust:1.85-bookworm AS gateway-build
WORKDIR /workspace
COPY . .
RUN rm rust-toolchain.toml \
    && cargo build --locked --release -p omni-gateway --bin omni-gateway

FROM debian:bookworm-slim AS gateway
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --system --gid 10001 omni \
    && useradd --system --uid 10001 --gid omni --home-dir /var/lib/omni omni \
    && install -d -o omni -g omni /var/lib/omni
WORKDIR /app
COPY --from=gateway-build /workspace/target/release/omni-gateway /usr/local/bin/omni-gateway
COPY --chown=omni:omni config/cloud-api.example.toml /app/config/cloud-api.example.toml
COPY --chown=omni:omni policies/usage-tiers.toml /app/policies/usage-tiers.toml
ENV OMNI_GATEWAY_CONFIG=/app/config/cloud-api.example.toml \
    OMNI_USAGE_POLICY=/app/policies/usage-tiers.toml \
    OMNI_GATEWAY_STATE=/var/lib/omni/gateway-state.json
USER omni
EXPOSE 8443
HEALTHCHECK --interval=15s --timeout=3s --start-period=5s --retries=3 \
    CMD curl --fail --silent http://127.0.0.1:8443/v1/health || exit 1
ENTRYPOINT ["/usr/local/bin/omni-gateway"]

FROM node:22-alpine AS dashboard
WORKDIR /app/ui
COPY --chown=node:node ui/ ./
ENV HOST=0.0.0.0 PORT=3000
USER node
EXPOSE 3000
HEALTHCHECK --interval=15s --timeout=3s --start-period=5s --retries=3 \
    CMD node -e "fetch('http://127.0.0.1:3000/api/config').then(r=>process.exit(r.ok?0:1)).catch(()=>process.exit(1))"
CMD ["node", "server.js"]
