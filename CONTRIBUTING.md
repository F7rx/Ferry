# Contributing to Ferry

Thanks for helping. Bug reports, fixes, translations and design feedback are
all welcome.

## Before you start

- For anything bigger than a small fix, open an issue first so we can agree on
  the approach before you spend time on it.
- Security problems go through [SECURITY.md](SECURITY.md), not public issues.
- Ferry must keep working with LocalSend devices. Changes to discovery or the
  wire protocol need to stay compatible with the LocalSend v2 protocol.

## Setup

You need Rust 1.97 and Node 22 or newer. On Windows, install the MSVC build
tools; WebView2 ships with Windows 11. On Linux, the desktop shell needs the
[Tauri system packages](https://v2.tauri.app/start/prerequisites/).

The Rust toolchain is pinned in `rust-toolchain.toml` (1.97, with rustfmt and
clippy), so rustup picks it up automatically, and CI builds with the same
version. The workspace crates declare it as their `rust-version`. Bump all
three together: `rust-toolchain.toml`, `rust-version` in the root
`Cargo.toml`, and the toolchain in `.github/workflows/ci.yml` and
`release.yml`.

```sh
npm install
npm run dev                         # UI in the browser, open http://localhost:5173/?demo
cd apps/app && npx tauri dev        # desktop app
cargo run -p ferry-cli -- --help    # CLI
```

The `?demo` mode shows the UI with simulated devices, which is the quickest way
to work on screens without a second device.

## Checks

CI (`.github/workflows/ci.yml`) runs these on every pull request and push to
`main`. Run the ones your change touches before you push.

**Rust** (Ubuntu, Windows and macOS; formatting on Ubuntu only):

```sh
cargo fmt -p ferry-core -p ferry-cli -p ferry-signal -p ferry-desktop
cargo clippy --locked -p ferry-core -p ferry-cli -p ferry-signal --all-targets -- -D warnings
cargo test --locked -p ferry-core -p ferry-cli -p ferry-signal -p ferry-localsend --features ferry-localsend/full
```

**Web app** (Ubuntu):

```sh
npm run typecheck -w apps/app
npm run test -w apps/app
npm run build -w apps/app
```

**Desktop shell** (Windows, Ubuntu 22.04 and macOS). The shell embeds the
built frontend, so build it first:

```sh
npm run build:desktop -w apps/app
cargo clippy --locked -p ferry-desktop --all-targets -- -D warnings
cargo test --locked -p ferry-desktop
```

**LocalSend interop** (Ubuntu): unmodified upstream LocalSend, pinned in
`scripts/fetch-upstream.sh`, against Ferry over loopback TLS. `tests/interop`
is a separate Cargo workspace with its own lock file.

```sh
sh scripts/fetch-upstream.sh
cd tests/interop && cargo test --locked
```

**End to end** (Ubuntu): real Chrome runs the production PWA build against a
real `ferry-signal` and the real `ferry` CLI over WebRTC on loopback. Each
script builds the PWA itself, runs from any directory, prints PASS or FAIL per
check, exits 1 on any failure, and writes screenshots to the folder you pass
(CI uploads them when a run fails). They need Google Chrome; set
`FERRY_E2E_CHANNEL=chromium` after `npx playwright install chromium` to use
Playwright's browser instead.

```sh
cargo build --release -p ferry-cli -p ferry-signal
FERRY_SIGNAL_BIN=target/release/ferry-signal node apps/app/scripts/e2e-web.mjs out/web   # browser to browser
node apps/app/scripts/e2e-native-web.mjs out/native-web                                  # browser and CLI, both ways
node apps/app/scripts/e2e-pwa.mjs out/pwa                                                # manifest, service worker, share target, offline
```

`e2e-web.mjs` uses `target/debug/ferry-signal` unless `FERRY_SIGNAL_BIN` is
set (`.exe` on Windows); `e2e-native-web.mjs` uses the release binaries
(`FERRY_BIN` and `FERRY_SIGNAL_BIN` override them).

## Guidelines

- Keep pull requests focused: one change per PR is easier to review and revert.
- Add a test with every bug fix, and with new behavior in the engine.
- UI work follows [docs/design-system.md](docs/design-system.md): use the
  existing tokens and components, check light and dark themes, phone width,
  keyboard navigation and reduced motion.
- Write interface text in sentence case, and start buttons with a verb.
- `crates/localsend` is vendored from upstream. Keep changes there minimal, mark
  them with a "Modified by the Ferry authors" comment and list them in
  `crates/localsend/UPSTREAM.md`.

## License

By contributing, you agree that your contributions are licensed under the
[Apache License 2.0](LICENSE).
