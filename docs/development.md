# Development

## Build

Use Rust 1.95 or later with edition 2024 support. The default StreamingCommunity client uses
BoringSSL and bindgen, which require CMake, a C/C++ compiler, libclang, pkg-config and Perl.
CI installs or verifies these dependencies in [.github/workflows/ci.yml](../.github/workflows/ci.yml).

```sh
cargo build --locked -p aulos-server
```

Running the binary also requires Python with yt-dlp and the download tools needed by the selected
providers. The [Dockerfile](../docker/Dockerfile) defines the packaged runtime and its tool pins.
The Python shim is [ytdlp_runner.py](../crates/aulos-provider-ytdlp/python/ytdlp_runner.py).

For a complete local runtime, build the image:

```sh
docker build -f docker/Dockerfile -t aulos-server:dev .
```

Use temporary download and state directories for local checks. Keep production tokens and keys
out of test environments. Never run a second Telegram bot with the production token.

## Checks

The Rust checks used by CI are:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --locked
cargo test -p aulos-provider-sc --no-default-features --locked
cargo test -p aulos-workspace-tests --locked
```

The workspace tests check dependency direction, packaging, and cross-crate behavior. Keep
`unwrap` out of production paths and handle errors with typed library errors. `anyhow` belongs in
the server crate.

CI also checks shell scripts, the Python shim, provider fixtures, release-tag logic and the web UI.
Use [ci.yml](../.github/workflows/ci.yml) for the exact commands and tool versions.

## Web UI

The shipped page is in `crates/aulos-api/web`. It uses plain HTML, CSS and JavaScript and is
embedded in the Rust binary. Node is needed only for development tests.

```sh
cd tools/web
npm ci
npx playwright install chromium
npm test
```

The suite uses a local protocol mock and produces screenshots. CI uploads them as
`web-screenshots`. The combined unminified `app.js` and `app.css` budget is 96 KB.
See [web tooling](../tools/web/README.md) for mock options and live-server checks.

## Provider fixtures

`tests/golden` holds format, option and progress regression fixtures. They remain active even
though the MeTube API is retired. Validate them with:

```sh
python3 tools/capture/verify.py
```

Regeneration needs a separate legacy source checkout. See
[fixture tooling](../tools/capture/README.md). Keep fixtures offline and check in any input needed
by a test.

## Container end-to-end checks

The container suite performs real downloads and is opt-in:

```sh
AULOS_E2E=1 AULOS_E2E_BUILD=1 tests/e2e/run.sh
```

On an arm64 Mac, use the release platform when checking the published architecture:

```sh
AULOS_E2E=1 AULOS_E2E_PLATFORM=linux/amd64 AULOS_E2E_BUILD=1 tests/e2e/run.sh
```

The suite covers health, WebSocket events, file access, ownership, restart recovery and explicit
legacy import. It is a developer-machine check because upstream sites can reject datacenter IPs.
CI runs offline image checks. See [run.sh](../tests/e2e/run.sh) for URL, port, image and cleanup
options.

## Releases

Add the `release` label before merging a PR into `main` to publish a GitHub release. The merge is
tagged `vYYYY.MM.DD` using its UTC date, with `.1`, `.2` and later suffixes for additional releases
that day. The tag triggers binary, checksum and container publication. Rerunning the tag job
reuses the existing tag for that merge.

The tag job requires `AULOS_REPO_PAT` with permission to create tags. Tags created with the
default `GITHUB_TOKEN` do not trigger the downstream workflows. Ordinary merges still publish
container images without creating a GitHub release.

Docker PR builds run for packaging-related changes or when the PR has the `docker-build` label.
Main and version-tag builds publish images. Manual dispatch can build without publishing.
See [docker.yml](../.github/workflows/docker.yml) and [release.yml](../.github/workflows/release.yml).

The yt-dlp update workflow checks the master build channel on its scheduled runs. Its single
version pin is `YTDLP_VERSION` in the Dockerfile. See
[update-yt-dlp.yml](../.github/workflows/update-yt-dlp.yml) for update validation.

## Documentation changes

Describe supported behavior in the present tense. Keep setup steps separate from API contracts
and implementation details. Use short sentences and no em dashes. Update the guide that owns the
topic instead of appending status notes or another copy of the same explanation.

Commit and PR text should describe the change and validation. Do not add AI attribution trailers.
