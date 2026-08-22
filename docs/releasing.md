# Releasing Kevin

How a `vX.Y.Z` release is cut, what CI does with it, how to check that what you
downloaded is what CI built, and how to get back to the previous version when
it is not.

The workflow is [`.github/workflows/release.yml`](../.github/workflows/release.yml).
It never runs on pull requests, so it cannot affect the `ci` gate.

## Versioning policy

The cargo workspace has **one version**: `workspace.package.version` in the root
`Cargo.toml`, inherited by every `kevin-*` crate and by the `kevin` binary.
There are no per-crate versions to reconcile, and a release covers everything.

Semver, read from the operator's point of view rather than the library's — no
Kevin crate is published, so the public surface is the CLI, the HTTP API, the
config schema and the database schema:

| Bump | When |
|---|---|
| **major** | A config key or CLI flag is removed or changes meaning; an API endpoint or DTO field is removed or changes type; a migration is not backwards compatible with the previous binary. |
| **minor** | New commands, endpoints, config keys with defaults, new worker adapters, additive migrations. Adjacent versions still coexist. |
| **patch** | Bug fixes, dependency bumps, docs, performance. No schema or interface change. |

Two rules that outrank the table:

- **Adjacent versions must coexist.** N and N+1 read the same database schema,
  so a rollback does not need a down-migration. Migrations are additive
  (expand → backfill → switch → contract *across releases*); never stop writing
  a column and drop it in the same release.
- **A migration that needs exclusive access** (long lock, rewrite) is called out
  in the release notes with the drain requirement, and gets its own release with
  nothing else in it.

Pre-1.0 (`0.y.z`), the minor position carries breaking changes.

## Cutting a release

Everything happens on `main`, through a PR like any other change.

1. **Decide the version** from the commits since the last tag. Conventional
   commits make this mechanical: any `feat!:`/`BREAKING CHANGE` → major (minor
   while `0.y.z`), any `feat:` → minor, otherwise patch.

2. **Open a release PR** that does exactly two things:

   ```bash
   # workspace.package.version in the root Cargo.toml
   sed -i '' 's/^version = ".*"$/version = "0.2.0"/' Cargo.toml
   cargo check --workspace          # refresh Cargo.lock with the new version
   ```

   and turns `## [Unreleased]` in `CHANGELOG.md` into
   `## [0.2.0] - YYYY-MM-DD`, leaving a fresh empty `## [Unreleased]` above it.
   The workflow refuses to release when the tag and
   `workspace.package.version` disagree, and warns when the changelog has no
   section for the version.

3. **Merge it, then tag the merge commit on `main`:**

   ```bash
   git fetch origin && git switch main && git pull --ff-only
   git tag -a v0.2.0 -m "v0.2.0" && git push origin v0.2.0
   ```

   With jj:

   ```bash
   jj git fetch
   jj bookmark create v0.2.0 -r main@origin && jj git push -b v0.2.0 --allow-new
   ```

   Signed tags (`git tag -s`) are preferred but not enforced.

4. **Watch the workflow.** `gh run watch --exit-status $(gh run list --workflow=release.yml -L1 --json databaseId --jq '.[0].databaseId')`.

## What CI does with the tag

| Job | What it produces |
|---|---|
| `prepare` | Resolves the tag, checks it against `workspace.package.version`, decides whether this run may publish (a fork cannot). |
| `crates.io policy` | Asserts the publish decision below; runs `cargo install --path crates/kevin-cli --locked` so the documented source install is proven on every release. |
| `create release` | Creates the GitHub release **as a draft**, body = the `CHANGELOG.md` section for this version + GitHub's generated commit/PR notes. |
| `kevin (<target>)` ×4 | `cargo build --locked --release`, strip, `kevin-<target>.tar.gz` (binary + `README.md` + `CHANGELOG.md` under a leading directory) with a `.sha256` sidecar, attached to the draft. |
| `SHA256SUMS` | Downloads the four archives back from the release and attaches one aggregate `SHA256SUMS`. |
| `container image` | Multi-arch `linux/amd64,linux/arm64` image → `ghcr.io/ligerian-labs/kevin`, with SBOM, SLSA provenance (`mode=max`), a GitHub build-provenance attestation and a keyless cosign signature. |
| `publish release` | Undrafts the release and marks it latest — only after every job above succeeded. |

Targets: `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`,
`x86_64-apple-darwin`, `aarch64-apple-darwin`. macOS builds run natively on
`macos-latest`; the aarch64 Linux build cross-compiles on the host toolchain via
`taiki-e/setup-cross-toolchain-action` (not in a `cross` container) so `build.rs`
can still read the git checkout and stamp the commit id. The container image
cross-compiles inside its builder stage for the same reason plus speed: a
`linux/arm64` build under QEMU emulation would take hours.

The one sharp edge on every Linux target is `fastembed`: `ort` downloads a
prebuilt ONNX Runtime for the target and links it statically, so the linker
needs a libstdc++ at least as new as the one that archive was built against.
That is why `deploy/Dockerfile` is on Debian trixie rather than bookworm — on
bookworm the link fails with undefined `std::__cxx11::…_M_replace_cold` and
`__cxa_call_terminate`. If a Linux job starts failing at the link step with
undefined C++ symbols, that is this, and the fix is a newer toolchain image or
runner, not a Rust change.

macOS binaries are **not** notarized or codesigned. Gatekeeper will quarantine a
downloaded archive; `xattr -d com.apple.quarantine kevin` clears it. Notarization
needs an Apple Developer identity and is not set up.

### crates.io

Kevin is **not published to crates.io**. Every crate inherits
`publish = false` from `workspace.package`: the crates are internal bounded
contexts that are only useful as a whole, and `kevin-cli` is a binary, not a
library anyone should depend on. `cargo install kevin-cli` therefore does not
work; the supported source install is

```bash
cargo install --path crates/kevin-cli --locked
# or, without a checkout:
cargo install --git https://github.com/Ligerian-labs/kevin --locked kevin-cli
```

The `crates.io policy` job exercises exactly that path on every release. If the
policy is ever reversed, flipping `publish` turns that job into a real
`cargo publish --dry-run` without any other change.

Homebrew and `cargo-binstall` are M5 items (`plan/13-roadmap.md`);
`cargo-binstall` will work off the release archives once the naming below is
declared in `[package.metadata.binstall]`.

## Dry runs

`workflow_dispatch` with an empty `tag` input builds all four binaries and the
image and publishes nothing — use it to check a workflow change without burning
a version number. `workflow_dispatch` with an existing tag re-runs a release
(assets are uploaded with `--clobber`).

Locally:

```bash
cargo build --release -p kevin-cli --bin kevin && ./target/release/kevin --version
podman build -f deploy/Dockerfile -t kevin:dev .
```

The image build is deliberately **not** part of `just ci`: it compiles the whole
workspace a second time inside a container and would add minutes to every PR.

## Verifying a download

Binaries:

```bash
curl -fsSLO https://github.com/Ligerian-labs/kevin/releases/download/v0.2.0/kevin-aarch64-apple-darwin.tar.gz
curl -fsSLO https://github.com/Ligerian-labs/kevin/releases/download/v0.2.0/SHA256SUMS
shasum -a 256 --ignore-missing -c SHA256SUMS      # sha256sum on Linux
tar xzf kevin-aarch64-apple-darwin.tar.gz
./kevin-aarch64-apple-darwin/kevin --version      # kevin 0.2.0 (<sha> <date>)
```

The commit id in `--version` is the commit the release was built from; it must
match the tag's commit.

Image signature — keyless cosign, so verification asserts *which workflow* on
*which repository* produced the image, not a key we hold:

```bash
IMAGE=ghcr.io/ligerian-labs/kevin:0.2.0
cosign verify "$IMAGE" \
  --certificate-identity-regexp '^https://github\.com/Ligerian-labs/kevin/\.github/workflows/release\.yml@refs/tags/v' \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com

# SLSA provenance and SBOM that ride along with the image:
cosign verify-attestation --type slsaprovenance "$IMAGE" \
  --certificate-identity-regexp '^https://github\.com/Ligerian-labs/kevin/' \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com
docker buildx imagetools inspect "$IMAGE" --format '{{ json .SBOM }}'

# GitHub's own attestation, if you prefer gh:
gh attestation verify "oci://$IMAGE" --repo Ligerian-labs/kevin
```

Pin by digest in production (`ghcr.io/ligerian-labs/kevin@sha256:…`); tags move,
digests do not.

## Rolling back

Kevin's rollback story rests on the coexistence rule: N and N+1 read the same
schema, so going back one version needs no down-migration. Do **not** roll back
across a release whose notes flag an exclusive-access migration — restore from a
`pg_dump` instead.

1. Drain and stop the current version:
   ```bash
   curl -fsS -XPOST -H "Authorization: Bearer $KEVIN_TOKEN" http://kevin:7777/api/v1/maintenance/drain
   # wait for in-flight attempts; then stop the unit / container
   systemctl stop kevin
   ```
2. Put the previous artifact back — the binary from the previous release's
   archive, or the image *by digest*:
   ```bash
   podman run ... ghcr.io/ligerian-labs/kevin@sha256:<previous-digest>
   ```
3. Start it and check: `kevin db status` (no unexpected pending migrations),
   `/readyz` green, `kevin --version` reports the version you meant, then a
   smoke run with the fake worker.
4. In-flight attempts from the aborted version are terminalised on startup as
   `task.attempt_failed { class: RuntimeRestarted }` — that is expected, not a
   symptom. Re-drive those runs.

If a release must be withdrawn: mark the GitHub release as a pre-release (do not
delete it — people have the digests), open a fix PR, and cut a new patch
version. Never move a tag that has been pushed; a moved tag invalidates every
checksum and signature already published against it.
