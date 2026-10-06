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

```sh
npm install
npm run dev                         # UI in the browser, open http://localhost:5173/?demo
cd apps/app && npx tauri dev        # desktop app
cargo run -p ferry-cli -- --help    # CLI
```

The `?demo` mode shows the UI with simulated devices, which is the quickest way
to work on screens without a second device.

## Checks

CI runs these on every pull request. Run them locally before you push:

```sh
cargo fmt -p ferry-core -p ferry-cli -p ferry-signal -p ferry-desktop
cargo clippy -p ferry-core -p ferry-cli -p ferry-signal --all-targets -- -D warnings
cargo test -p ferry-core -p ferry-cli -p ferry-signal -p ferry-localsend --features ferry-localsend/full

npm run typecheck -w apps/app
npm run test -w apps/app
```

Interop tests against unmodified upstream LocalSend:

```sh
sh scripts/fetch-upstream.sh
cd tests/interop && cargo test
```

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
