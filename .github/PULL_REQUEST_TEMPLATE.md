<!--
Regular contributions must target develop.
Pull requests to main are reserved for maintainer release promotions from develop.
-->

## Summary

<!-- What changed, why it changed, and what users will observe. -->

## Related issue

<!-- Use "Closes #123" when applicable, or write "None". -->

## Validation

<!-- List the exact commands and runtime checks you performed. -->

- [ ] `cargo fmt --all -- --check`
- [ ] `cargo check --locked --workspace --all-targets`
- [ ] `cargo test --locked --workspace --all-targets`
- [ ] `cargo clippy --locked --workspace --all-targets -- -D warnings`
- [ ] `cargo build --locked --release -p termglide`
- [ ] `./target/release/termglide doctor`
- [ ] Relevant real-browser or PTY smoke checks

## Checklist

- [ ] This PR targets `develop`, or it is a maintainer release promotion from `develop` to `main`.
- [ ] The change is focused and contains no unrelated files or generated output.
- [ ] Tests cover new or changed observable behavior, or tests are not applicable and I explained
      why.
- [ ] User-facing documentation and `CHANGELOG.md` are updated when needed.
- [ ] Platform-specific behavior, compatibility risks, skipped checks, and known limitations are
      documented above.
- [ ] No credentials, cookies, tokens, personal data, or private traces are included.
