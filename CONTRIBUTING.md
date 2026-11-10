# Contributing

Thanks for helping. A few rules keep the project consistent.

## Language

The project is English only: code, comments, commit messages and docs. Brazilian Portuguese exists only as a UI translation: `ui/src/i18n/pt.json`, plus the pt/en text pairs in `crates/app/src/shell/tray.rs` and `crates/app/src/shell/notify.rs`. Keep the i18n keys the same in `en.json` and `pt.json`.

## Commits

Use the `type(scope): summary` format, for example `fix(mcp): reject empty campaign names` or `docs: clarify install steps`.

## Before sending changes

Rust (from the repo root):

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-targets
```

UI (from `ui/`; lint must have no warnings):

```sh
npm run lint
npm run typecheck
npm test
npm run e2e
```

After changing a command in `crates/app`, run `npm --prefix ui run bindings` and commit the updated `ui/src/bindings.ts`. Never edit it by hand.
