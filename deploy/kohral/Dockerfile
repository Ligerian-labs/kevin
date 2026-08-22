# Kevin **Kohral runtime image** — `kevin serve --kohral` plus the coding-agent
# CLIs Kevin drives (`plan/08-kohral-runtime.md` §6, WS-23).
#
# HOW THIS DIFFERS FROM `deploy/Dockerfile`
#   `deploy/Dockerfile` is the plain daemon: it orchestrates, it hosts no
#   workers, and it deliberately ships **no** agent CLI. This image is the one
#   Kohral deploys as an `AgentRuntimeStrategy` workload, so it must be able to
#   actually run a task: it bundles Node 24 and the four worker CLIs
#   (`claude`, `codex`, `pi`, `opencode`) at pinned versions, and it runs in
#   `sandbox.tier = "container"` where the bypass flags are allowed because the
#   Kohral stack itself is the isolation boundary (`plan/09-security.md`,
#   `deploy/kohral/README.md` "Sandbox tier").
#
# WHAT IS IN HERE
#   * `/usr/local/bin/kevin` — release build of `kevin-cli`, `--locked`;
#   * Node 24 (pinned tarball + sha256) and the four agent CLIs, pinned to the
#     exact versions in `deploy/kohral/upstreams.lock.json`;
#   * `git`, `bash`, `curl`, `ca-certificates`, `openssh-client`, `jq`,
#     `python3` (codex hooks / notify scripts) — the tools the CLIs shell out to;
#   * the fastembed model cache for `BAAI/bge-small-en-v1.5`, pre-downloaded at
#     a pinned Hugging Face revision so a fresh agent does not spend its first
#     turn pulling 130 MB (`ARG BAKE_EMBEDDING_MODEL=1`, see the `models` stage);
#   * `/opt/kevin/config` (Kohral mounts config files read-only here) and
#     `/opt/kevin/data` (the persistent volume: data_dir, MEMORY.md, CLI state,
#     workspaces, model cache).
#
# WHAT IS **NOT** IN HERE
#   * Postgres — the stack's second service (`deploy/kohral/compose.yml`);
#   * `jj` — a Kohral workload has no repository (`workspace.strategy =
#     "in_place"`), so the jj/git worktree strategies are not used;
#   * any credential. Provider keys arrive as a secret env file at runtime
#     (`/run/secrets/kevin-env`), never in a layer.
#
# Build (from the repository root — this file is not self-contained):
#   podman build -f deploy/kohral/Dockerfile -t kevin-kohral:dev .
#   podman build -f deploy/kohral/Dockerfile --build-arg BAKE_EMBEDDING_MODEL=0 …
# Multi-arch, as the release workflow does it:
#   docker buildx build -f deploy/kohral/Dockerfile --platform linux/amd64,linux/arm64 .
#
# The Rust builder always runs on the *build* platform and cross-compiles, so an
# arm64 image does not compile the workspace under emulation. The runtime stage
# is target-native on purpose: `npm install -g` resolves per-architecture
# optional dependencies (codex ships a Rust binary per target, opencode a Bun
# one), so those must be installed for the *target* architecture.
#
# Debian *trixie*, not bookworm: fastembed statically links the prebuilt ONNX
# Runtime, which references libstdc++ symbols bookworm's GCC 12 does not export.

# ---------------------------------------------------------------- builder
FROM --platform=$BUILDPLATFORM rust:1-trixie AS builder

ARG BUILDARCH
ARG TARGETARCH
ARG KEVIN_BUILD_GIT_SHA=""
ARG SOURCE_DATE_EPOCH=""

ENV CARGO_TERM_COLOR=always \
    CARGO_INCREMENTAL=0 \
    KEVIN_BUILD_GIT_SHA=${KEVIN_BUILD_GIT_SHA} \
    SOURCE_DATE_EPOCH=${SOURCE_DATE_EPOCH}

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

RUN set -eux; \
    target="$(cat /rust-target)"; \
    set -a; . /cross-env; set +a; \
    rustup target add "${target}"; \
    RUSTFLAGS="${RUSTFLAGS:-} -C strip=symbols" \
      cargo build --locked --release --target "${target}" -p kevin-cli --bin kevin; \
    install -m 0755 "target/${target}/release/kevin" /usr/local/bin/kevin

# ---------------------------------------------------------------- models
# The fastembed cache, laid out exactly the way `hf-hub` reads it:
#   <cache>/models--<org>--<name>/refs/main            → the commit sha
#   <cache>/models--<org>--<name>/snapshots/<sha>/<f>  → the files
# Downloaded at a pinned revision with per-file sha256 verification, so the
# layer is reproducible and an upstream force-push cannot change what we ship.
# Architecture-independent, hence a separate stage on the build platform.
FROM --platform=$BUILDPLATFORM debian:trixie-slim AS models

ARG BAKE_EMBEDDING_MODEL=1
# Keep in sync with deploy/kohral/upstreams.lock.json (`embedding_model`).
ARG EMBEDDING_REPO=Xenova/bge-small-en-v1.5
ARG EMBEDDING_REVISION=ea104dacec62c0de699686887e3f920caeb4f3e3

RUN set -eux; \
    mkdir -p /models/embeddings; \
    if [ "${BAKE_EMBEDDING_MODEL}" != "1" ]; then \
      echo "embedding model bake disabled; the first boot will download it"; exit 0; \
    fi; \
    apt-get update; \
    apt-get install -y --no-install-recommends ca-certificates curl; \
    rm -rf /var/lib/apt/lists/*; \
    repo_dir="/models/embeddings/models--$(echo "${EMBEDDING_REPO}" | sed 's|/|--|g')"; \
    snapshot="${repo_dir}/snapshots/${EMBEDDING_REVISION}"; \
    mkdir -p "${repo_dir}/refs" "${snapshot}/onnx"; \
    printf '%s' "${EMBEDDING_REVISION}" >"${repo_dir}/refs/main"; \
    set -- \
      "onnx/model.onnx 828e1496d7fabb79cfa4dcd84fa38625c0d3d21da474a00f08db0f559940cf35" \
      "tokenizer.json d241a60d5e8f04cc1b2b3e9ef7a4921b27bf526d9f6050ab90f9267a1f9e5c66" \
      "config.json fa73f90bf92c8cace1fbcb709626306f2bdbc9ea3e5b5f94b440df9b6aa56350" \
      "special_tokens_map.json b6d346be366a7d1d48332dbc9fdf3bf8960b5d879522b7799ddba59e76237ee3" \
      "tokenizer_config.json 9261e7d79b44c8195c1cada2b453e55b00aeb81e907a6664974b4d7776172ab3"; \
    for entry in "$@"; do \
      file="${entry%% *}"; want="${entry##* }"; \
      curl -fsSL --retry 3 -o "${snapshot}/${file}" \
        "https://huggingface.co/${EMBEDDING_REPO}/resolve/${EMBEDDING_REVISION}/${file}"; \
      echo "${want}  ${snapshot}/${file}" | sha256sum -c -; \
    done

# ---------------------------------------------------------------- runtime
FROM debian:trixie-slim AS runtime

ARG TARGETARCH
# Pins. Every one of these is recorded with its digest in
# `deploy/kohral/upstreams.lock.json`; `deploy/kohral/README.md` documents the
# update policy (upgraded in a release, never on an unattended schedule).
ARG NODE_VERSION=24.19.0
ARG NODE_SHA256_AMD64=14b342e71204f811bde6153be8e04b62aef63c236fef92b55f9c83154b409647
ARG NODE_SHA256_ARM64=01443c1e1a29e531ccad5a46fefa6df490d2189c49f7955904aecdbb0fe86fdc
ARG CLAUDE_VERSION=2.1.240
ARG CODEX_VERSION=0.149.0
ARG OPENCODE_VERSION=1.18.15
ARG PI_VERSION=0.84.2

LABEL org.opencontainers.image.title="kevin-kohral" \
      org.opencontainers.image.description="Kevin as a Kohral agent runtime: kevin serve --kohral plus the claude/codex/pi/opencode CLIs." \
      org.opencontainers.image.source="https://github.com/Ligerian-labs/kevin" \
      org.opencontainers.image.licenses="Apache-2.0" \
      io.kohral.runtime.contract="kohral.conversations.v1" \
      io.kohral.runtime.type="kevin"

# Base tools the agent CLIs shell out to. `python3` is there for codex' hook /
# notify scripts; `openssh-client` for git over ssh; `jq` because half the
# agent recipes in the wild use it.
RUN set -eux; \
    apt-get update; \
    apt-get install -y --no-install-recommends \
      bash ca-certificates curl git jq less openssh-client python3 ripgrep xz-utils; \
    rm -rf /var/lib/apt/lists/*

# Node 24 from the pinned tarball (not the distro package: Debian's node is far
# older than what the CLIs support, and the tarball is checksummable).
RUN set -eux; \
    case "${TARGETARCH:-amd64}" in \
      amd64) arch='x64'; sha="${NODE_SHA256_AMD64}" ;; \
      arm64) arch='arm64'; sha="${NODE_SHA256_ARM64}" ;; \
      *) echo "unsupported TARGETARCH=${TARGETARCH}" >&2; exit 1 ;; \
    esac; \
    tarball="node-v${NODE_VERSION}-linux-${arch}.tar.xz"; \
    curl -fsSL --retry 3 -o "/tmp/${tarball}" "https://nodejs.org/dist/v${NODE_VERSION}/${tarball}"; \
    echo "${sha}  /tmp/${tarball}" | sha256sum -c -; \
    tar -xJf "/tmp/${tarball}" -C /usr/local --strip-components=1 \
      --exclude CHANGELOG.md --exclude LICENSE --exclude README.md; \
    rm -f "/tmp/${tarball}"; \
    node --version; npm --version

# The four coding-agent CLIs Kevin drives (`plan/04-workers.md`). Exact
# versions, `--save-exact`, no lifecycle surprises beyond what each package
# needs to place its per-arch binary. `pi` is `@earendil-works/pi-coding-agent`
# — the package `plan/08` §6 guessed (`@mariozechner/…`) is the old name and is
# several minor versions behind.
RUN set -eux; \
    npm install --global --no-audit --no-fund --save-exact \
      "@anthropic-ai/claude-code@${CLAUDE_VERSION}" \
      "@openai/codex@${CODEX_VERSION}" \
      "opencode-ai@${OPENCODE_VERSION}" \
      "@earendil-works/pi-coding-agent@${PI_VERSION}"; \
    npm cache clean --force; \
    rm -rf /root/.npm /tmp/*; \
    claude --version; codex --version; opencode --version; pi --version

COPY --from=builder /usr/local/bin/kevin /usr/local/bin/kevin
COPY --from=models /models/embeddings /opt/kevin/models/embeddings
COPY deploy/kohral/entrypoint.sh /usr/local/bin/kevin-entrypoint

# uid/gid 10000 matches Kohral's volume ownership convention
# (`VolumeSpec('data', …, 10000, 10000)`); `plan/09-security.md`: even in the
# container tier Kevin's own process is never root.
RUN set -eux; \
    groupadd --gid 10000 kevin; \
    useradd --uid 10000 --gid 10000 --no-create-home --home-dir /opt/kevin/data/home --shell /bin/bash kevin; \
    mkdir -p /opt/kevin/config /opt/kevin/data/home /opt/kevin/data/work; \
    chown -R 10000:10000 /opt/kevin/data; \
    chmod 0755 /opt/kevin/config /opt/kevin/models /opt/kevin/models/embeddings; \
    chmod 0755 /usr/local/bin/kevin-entrypoint

# `kevin.profile = "kohral"` flips kohral.enabled, auto_approve_plans and the
# json/metrics defaults; the rest is what only the *image* may decide, so it
# lives here and not in the operator overlay (Kohral's protected sections:
# `server`, `kohral`, `database`, `sandbox`, `workers.*.bin`,
# `workers.*.env_passthrough`, `telemetry`).
#
# `sandbox.tier = "container"` is the whole trust argument of this image: the
# stack is the boundary, so unattended runs may use the bypass flags
# (`workers.claude.permission_mode = "bypassPermissions"`). codex deliberately
# stays at `workspace-write`, never `danger-full-access`.
ENV HOME=/opt/kevin/data/home \
    KEVIN__KEVIN__PROFILE=kohral \
    KEVIN__KEVIN__DATA_DIR=/opt/kevin/data \
    KEVIN__KOHRAL__BIND=0.0.0.0:8080 \
    KEVIN__SERVER__AUTH_TOKEN_FILE=/opt/kevin/data/api-token \
    KEVIN__DATABASE__AUTO_MIGRATE=true \
    KEVIN__SANDBOX__TIER=container \
    KEVIN__WORKERS__CLAUDE__PERMISSION_MODE=bypassPermissions \
    KEVIN__WORKSPACE__STRATEGY=in_place \
    KEVIN__WORKSPACE__INTEGRATION=none \
    KEVIN_CONFIG_DIR=/opt/kevin/config \
    KEVIN_DATA_DIR=/opt/kevin/data \
    KEVIN_MODEL_CACHE=/opt/kevin/models/embeddings

VOLUME ["/opt/kevin/data"]
# The Kohral runtime contract (`kohral.bind`). The plain API (`server.bind`,
# 7777) and the Prometheus exporter (`telemetry.metrics_bind`, 9464) stay
# inside the stack network — Kohral only ever talks to 8080.
EXPOSE 8080

USER 10000:10000
WORKDIR /opt/kevin/data

# `/health` is the Kohral liveness probe (`plan/08` §1.1): no auth, no database.
HEALTHCHECK --interval=30s --timeout=5s --start-period=30s --retries=3 \
    CMD curl -fsS "http://127.0.0.1:8080/health" || exit 1

# Kohral's `KevinRuntimeStrategy` overrides both with its own
# `/bin/sh -c "<seed MEMORY.md>; exec kevin serve --kohral"`; the entrypoint is
# what a hand-run stack (and the conformance job) gets. Both paths end in the
# same `kevin serve --kohral`, and everything the entrypoint does is either
# idempotent or also covered by the image environment — `auto_migrate = true`
# above is there precisely because the Kohral path skips `kevin db migrate`.
ENTRYPOINT ["/usr/local/bin/kevin-entrypoint"]
CMD ["serve", "--kohral"]
