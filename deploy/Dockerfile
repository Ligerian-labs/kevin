# Kevin daemon image — `kevin serve` and nothing else.
#
# WHAT IS IN HERE
#   * the `kevin` binary (release profile, stripped, statically-known
#     migrations are compiled in via `sqlx::migrate!`);
#   * `git` — `kevin-workspace` prepares a git worktree per task attempt
#     (plan/adr/0007-workspace-isolation.md); without it only `in-place` and
#     `none` repo kinds work;
#   * `ca-certificates` — TLS roots for Postgres (rustls) and for the one-off
#     embedding-model download;
#   * `curl` — only so `HEALTHCHECK` can call `/healthz`.
#
# WHAT IS **NOT** IN HERE, on purpose
#   * the coding-agent CLIs (`claude`, `codex`, `pi`, `opencode`) and their
#     Node/Bun runtimes. This image is the plain daemon: it orchestrates,
#     it does not host workers. The image that bundles the agent CLIs is the
#     Kohral image (`deploy/kohral/`, WS-23) — use that one to actually run
#     agent tasks in a container.
#   * `jj` — jj workspaces need the binary; use the Kohral image or add it in
#     a derived image.
#   * Postgres. Kevin has no embedded store; point `KEVIN__DATABASE__URL` at a
#     Postgres 16 with pgvector (see `deploy/compose/postgres.yml`).
#
# Build (from the repository root, this file is not self-contained):
#   podman build -f deploy/Dockerfile -t kevin:dev .
# Multi-arch, as the release workflow does it:
#   docker buildx build -f deploy/Dockerfile --platform linux/amd64,linux/arm64 .
#
# The builder stage always runs on the *build* platform and cross-compiles to
# the target, so `linux/arm64` images do not compile the whole workspace under
# QEMU emulation (hours). Only the tiny runtime stage is target-native.
#
# Debian *trixie* on both stages, not bookworm: `fastembed` statically links the
# prebuilt ONNX Runtime that `ort` downloads, and that archive references
# libstdc++ symbols (`_M_replace_cold`, `__cxa_call_terminate`) that bookworm's
# GCC 12 runtime does not export — linking fails outright on bookworm.

# ---------------------------------------------------------------- builder
FROM --platform=$BUILDPLATFORM rust:1-trixie AS builder

# Provided by BuildKit; defaulted so a plain `docker build` still works.
ARG BUILDARCH
ARG TARGETARCH
# Stamped into `kevin --version` when the build context has no git metadata.
ARG KEVIN_BUILD_GIT_SHA=""
ARG SOURCE_DATE_EPOCH=""

ENV CARGO_TERM_COLOR=always \
    CARGO_INCREMENTAL=0 \
    KEVIN_BUILD_GIT_SHA=${KEVIN_BUILD_GIT_SHA} \
    SOURCE_DATE_EPOCH=${SOURCE_DATE_EPOCH}

# Map the OCI arch name to a Rust target triple and install the cross toolchain
# only when we are actually cross-compiling.
RUN set -eux; \
    case "${TARGETARCH:-${BUILDARCH}}" in \
      amd64) target='x86_64-unknown-linux-gnu'; pkg='gcc-x86-64-linux-gnu g++-x86-64-linux-gnu'; linker='x86_64-linux-gnu-gcc' ;; \
      arm64) target='aarch64-unknown-linux-gnu'; pkg='gcc-aarch64-linux-gnu g++-aarch64-linux-gnu'; linker='aarch64-linux-gnu-gcc' ;; \
      *) echo "unsupported TARGETARCH=${TARGETARCH}" >&2; exit 1 ;; \
    esac; \
    echo "${target}" >/rust-target; \
    apt-get update; \
    apt-get install -y --no-install-recommends pkg-config; \
    if [ "${TARGETARCH:-${BUILDARCH}}" != "${BUILDARCH}" ]; then \
      apt-get install -y --no-install-recommends ${pkg}; \
      var="CARGO_TARGET_$(echo "${target}" | tr 'a-z-' 'A-Z_')_LINKER"; \
      printf '%s=%s\n' "${var}" "${linker}" >/cross-env; \
    else \
      : >/cross-env; \
    fi; \
    rm -rf /var/lib/apt/lists/*

WORKDIR /src
COPY . .

# `--locked` so the image is built from the committed Cargo.lock, exactly like
# the release binaries. `-C strip=symbols` is rustc's own stripping, which works
# the same whether or not we are cross-compiling (a host `strip` cannot touch a
# foreign-architecture binary).
RUN set -eux; \
    target="$(cat /rust-target)"; \
    set -a; . /cross-env; set +a; \
    rustup target add "${target}"; \
    RUSTFLAGS="${RUSTFLAGS:-} -C strip=symbols" \
      cargo build --locked --release --target "${target}" -p kevin-cli --bin kevin; \
    install -m 0755 "target/${target}/release/kevin" /usr/local/bin/kevin

# ---------------------------------------------------------------- runtime
FROM debian:trixie-slim AS runtime

LABEL org.opencontainers.image.title="kevin" \
      org.opencontainers.image.description="Kevin daemon (kevin serve): autonomous agent runtime. Does not bundle the coding-agent CLIs." \
      org.opencontainers.image.source="https://github.com/Ligerian-labs/kevin" \
      org.opencontainers.image.licenses="Apache-2.0"

RUN set -eux; \
    apt-get update; \
    apt-get install -y --no-install-recommends ca-certificates curl git; \
    rm -rf /var/lib/apt/lists/*; \
    useradd --system --create-home --home-dir /var/lib/kevin --shell /usr/sbin/nologin kevin

COPY --from=builder /usr/local/bin/kevin /usr/local/bin/kevin

# `kevin.data_dir` (artifacts, worker transcripts, embedding-model cache) and
# the API bind address. `server.bind` defaults to 127.0.0.1, which is useless
# in a container, so the image binds all interfaces; publish the port
# deliberately and keep the API behind `server.auth_token_file`.
ENV HOME=/var/lib/kevin \
    KEVIN__KEVIN__DATA_DIR=/var/lib/kevin \
    KEVIN__KEVIN__PROFILE=server \
    KEVIN__SERVER__BIND=0.0.0.0:7777

VOLUME ["/var/lib/kevin"]
EXPOSE 7777
USER kevin
WORKDIR /var/lib/kevin

# `KEVIN__DATABASE__URL` is required and has no sane default here.
# `server` profile keeps `database.auto_migrate = false`: run `kevin db migrate`
# (a one-shot container or an init container) before starting the daemon, or
# set `KEVIN__DATABASE__AUTO_MIGRATE=true` (see plan/10-observability-ops.md).
# Liveness only, exactly as plan/10-observability-ops.md defines it: /healthz
# never touches the database. Readiness (/readyz) is the orchestrator's job and
# belongs in the scheduler's probe, not in the image.
HEALTHCHECK --interval=30s --timeout=5s --start-period=20s --retries=3 \
    CMD curl -fsS "http://127.0.0.1:${KEVIN_HEALTH_PORT:-7777}/healthz" || exit 1

ENTRYPOINT ["kevin"]
CMD ["serve"]
