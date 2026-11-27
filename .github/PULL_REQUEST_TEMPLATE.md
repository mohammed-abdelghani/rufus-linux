## Summary

<!-- What does this change and why? -->

## Checklist

- [ ] `cargo build --locked --release` passes
- [ ] `cargo test` passes (tests need no root and no real devices)
- [ ] `cargo clippy -- -D warnings` passes with zero warnings
- [ ] Relevant documentation updated (README, CHANGELOG, etc.)
- [ ] No test or code path writes to real block devices or needs root
- [ ] Destructive operations still require explicit confirmation
