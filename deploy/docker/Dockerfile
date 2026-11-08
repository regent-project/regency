# regency OCI images (D15), all from one multi-stage build. Build from the
# repository root (regency/):
#
#   docker build -f deploy/docker/Dockerfile --target <image> -t regency-<image> .
#
# targets: scheduler runner dbwriter backend endpoint sourcesync regencyctl
#          frontend aio
#
# Every image runs as the unprivileged user `regency` (uid 10001), reads
# /etc/regency/<service>.toml when present, and takes every key from
# REGENCY_<SECTION>_<KEY> environment variables (D15), so most deployments
# need no file at all.

ARG RUST_VERSION=1.99
ARG DEBIAN=bookworm

# ---------------------------------------------------------------- toolchain
FROM rust:${RUST_VERSION}-${DEBIAN} AS toolchain
# protoc: exact version required by the protobuf crate (D6)
ARG PROTOC_VERSION=36.1
RUN apt-get update \
 && apt-get install -y --no-install-recommends unzip cmake clang \
 && rm -rf /var/lib/apt/lists/* \
 && case "$(uname -m)" in \
      x86_64) arch=x86_64 ;; \
      aarch64) arch=aarch_64 ;; \
      *) echo "unsupported architecture" >&2; exit 1 ;; \
    esac \
 && curl -fsSL -o /tmp/protoc.zip \
      "https://github.com/protocolbuffers/protobuf/releases/download/v${PROTOC_VERSION}/protoc-${PROTOC_VERSION}-linux-${arch}.zip" \
 && unzip -q /tmp/protoc.zip -d /usr/local \
 && rm /tmp/protoc.zip \
 && protoc --version
WORKDIR /src

# ---------------------------------------------------------------- services
FROM toolchain AS build
COPY . .
RUN cargo build --release --locked --workspace --exclude frontend \
 && mkdir -p /out \
 && cp target/release/regency-scheduler target/release/regency-runner \
       target/release/regency-dbwriter target/release/regency-backend \
       target/release/regency-endpoint target/release/regency-sourcesync \
       target/release/regencyctl target/release/regency-aio /out/

# ---------------------------------------------------------------- web client
FROM toolchain AS web
ARG TRUNK_VERSION=0.22.1
RUN rustup target add wasm32-unknown-unknown \
 && cargo install --locked trunk --version "${TRUNK_VERSION}"
COPY . .
RUN cd frontend && trunk build --release --dist /web

# ---------------------------------------------------------------- runtime base
FROM debian:${DEBIAN}-slim AS runtime
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --uid 10001 --home-dir /var/lib/regency --shell /usr/sbin/nologin regency \
 && mkdir -p /etc/regency /var/lib/regency \
 && chown regency:regency /var/lib/regency
ENV REGENCY_LOG_FORMAT=json
WORKDIR /var/lib/regency
USER regency

FROM runtime AS scheduler
COPY --from=build /out/regency-scheduler /usr/bin/
EXPOSE 9090
ENTRYPOINT ["/usr/bin/regency-scheduler", "--config", "/etc/regency/scheduler.toml"]

FROM runtime AS runner
COPY --from=build /out/regency-runner /usr/bin/
EXPOSE 9090
ENTRYPOINT ["/usr/bin/regency-runner", "--config", "/etc/regency/runner.toml"]

FROM runtime AS dbwriter
COPY --from=build /out/regency-dbwriter /usr/bin/
EXPOSE 9090
ENTRYPOINT ["/usr/bin/regency-dbwriter", "--config", "/etc/regency/dbwriter.toml"]

FROM runtime AS backend
COPY --from=build /out/regency-backend /usr/bin/
# the web client, served on the API port when frontend_dir is set
COPY --from=web /web /usr/share/regency/web
ENV REGENCY_BACKEND_LISTEN=0.0.0.0:8080 \
    REGENCY_BACKEND_FRONTEND_DIR=/usr/share/regency/web
EXPOSE 8080 9090
ENTRYPOINT ["/usr/bin/regency-backend", "--config", "/etc/regency/backend.toml"]

FROM runtime AS endpoint
COPY --from=build /out/regency-endpoint /usr/bin/
ENV REGENCY_ENDPOINT_LISTEN=0.0.0.0:8443
EXPOSE 8443 9090
ENTRYPOINT ["/usr/bin/regency-endpoint", "--config", "/etc/regency/endpoint.toml"]

FROM runtime AS sourcesync
COPY --from=build /out/regency-sourcesync /usr/bin/
EXPOSE 9090
ENTRYPOINT ["/usr/bin/regency-sourcesync", "--config", "/etc/regency/sourcesync.toml"]

FROM runtime AS regencyctl
COPY --from=build /out/regencyctl /usr/bin/
ENTRYPOINT ["/usr/bin/regencyctl", "--config", "/etc/regency/regencyctl.toml"]
CMD ["init"]

# ---------------------------------------------------------------- frontend
# The web client alone (static files), for a CDN or a separate web server.
# REGENCY_BACKEND_URL: backend's gRPC-Web URL (empty = same origin).
FROM nginxinc/nginx-unprivileged:stable-alpine AS frontend
COPY --from=web /web /usr/share/nginx/html
COPY deploy/docker/frontend/default.conf /etc/nginx/conf.d/default.conf
COPY deploy/docker/frontend/40-regency-backend.sh /docker-entrypoint.d/40-regency-backend.sh
USER root
RUN chmod 0755 /docker-entrypoint.d/40-regency-backend.sh \
 && chown -R nginx /usr/share/nginx/html
USER nginx
EXPOSE 8080

# ---------------------------------------------------------------- all-in-one
# Every regency service and the web client in one container, supervised by
# regency-aio (D15). RabbitMQ and CouchDB are NOT included: see
# deploy/compose/quickstart.yaml. Not HA, not for production.
FROM runtime AS aio
USER root
RUN apt-get update \
 && apt-get install -y --no-install-recommends curl \
 && rm -rf /var/lib/apt/lists/*
COPY --from=build /out/ /usr/bin/
COPY --from=web /web /usr/share/regency/web
COPY deploy/docker/aio/aio.toml /etc/regency/aio.toml
COPY deploy/docker/aio/regency/ /etc/regency/
USER regency
VOLUME ["/var/lib/regency"]
EXPOSE 8080 8443
HEALTHCHECK --interval=15s --timeout=3s --start-period=60s \
  CMD curl -fsS http://127.0.0.1:9094/readyz >/dev/null || exit 1
ENTRYPOINT ["/usr/bin/regency-aio", "--config", "/etc/regency/aio.toml"]
