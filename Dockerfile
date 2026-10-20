# ---- build ----
FROM rust:1.98-slim-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked

# ---- runtime ----
FROM debian:bookworm-slim
ARG CLAUDE_VERSION=2.1.296
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl git ripgrep \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --create-home --uid 1000 --shell /bin/bash gateway

USER gateway
RUN curl -fsSL https://claude.ai/install.sh | bash -s ${CLAUDE_VERSION}
USER root

COPY --from=build /src/target/release/claude-gateway /usr/local/bin/claude-gateway
# The agent may write inside workspace/, but the sticky bit and root ownership stop it
# from editing or deleting its own CLAUDE.md rules.
COPY workspace/CLAUDE.md /app/workspace/CLAUDE.md
RUN chown root:gateway /app/workspace && chmod 1775 /app/workspace \
 && chown root:root /app/workspace/CLAUDE.md && chmod 0444 /app/workspace/CLAUDE.md

USER gateway
ENV PATH=/home/gateway/.local/bin:$PATH \
    CLAUDE_BIN=/home/gateway/.local/bin/claude \
    DISABLE_AUTOUPDATER=1 \
    GATEWAY_BIND=0.0.0.0:8443 \
    GATEWAY_WORKDIR=/app/workspace \
    RUST_LOG=claude_gateway=info
EXPOSE 8443
HEALTHCHECK --interval=30s --timeout=5s CMD curl -fs http://127.0.0.1:8443/health || exit 1
ENTRYPOINT ["claude-gateway"]
