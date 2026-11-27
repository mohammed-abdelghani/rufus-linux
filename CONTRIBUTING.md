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

CI runs the same commands on `ubuntu-latest`.

## Testing

```sh
cargo test
```

The test suite (128 unit tests) must run **without root and without real
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
```

Formatting is not currently enforced: `cargo fmt --check` fails on the
existing tree, so do not reformat unrelated code in a feature change.

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
