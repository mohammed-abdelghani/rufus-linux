# Contributing

Thanks for your interest in rufus-linux. This document covers how to build,
test, and propose changes.

## Building

You need a Rust toolchain (`stable`) and the X11 development libraries:

```sh
# Debian/Ubuntu
sudo apt install build-essential pkg-config libx11-dev libx11-xcb-dev
# Fedora
sudo dnf install gcc pkgconf-devel libX11-devel libX11-xcb-devel

cargo build --locked --release
# binary: target/release/rufus-linux
```

CI runs the same commands plus `cargo fmt --check` on `ubuntu-latest`, and a
non-blocking `cargo audit` job and a `cargo deny` license check in separate
jobs.

## Testing

```sh
cargo test
```

The test suite (133 unit tests) must run **without root and without real
block devices** — it uses in-memory images and fixture files only. Please keep
it that way: never add a test that writes to a real device or requires
privileges.

**Warning:** rufus-linux performs destructive disk operations. If you test
write paths manually, do it only on a **disposable USB drive** — never on a
drive containing data you need, and never on the device hosting your running
system.

## Linting and formatting

```sh
cargo clippy -- -D warnings   # zero-warning policy; CI enforces it
cargo fmt --check             # rustfmt-clean tree; CI enforces it
```

Run `cargo fmt` before committing, and keep formatting-only rewrites out of
feature changes (the tree-wide formatting commit is listed in
`.git-blame-ignore-revs` so `git blame` stays clean). The tree uses default
rustfmt style; the generated language table in `src/locale/langs.rs` is
excluded via `#[rustfmt::skip]`.

## Proposing changes

1. Open an issue describing the problem or idea first for anything
   non-trivial.
2. Fork the repository, create a branch, and open a pull request against
   `main`.
3. Keep pull requests focused; one logical change per PR.
4. Ensure `cargo build --locked --release`, `cargo clippy -- -D warnings`,
   and `cargo test` all pass before requesting review.

## License

By contributing, you agree that your contributions are licensed under
**GPL-3.0-only**, the same license as this project (see [LICENSE](LICENSE)).
