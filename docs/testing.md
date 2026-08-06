# Testing

## Complete local gate

Run these commands from the repository root with Rust `1.92.0`:

```console
cargo fmt --all -- --check
cargo check --locked --workspace --all-targets
cargo test --locked --workspace --all-targets
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo build --locked --release -p termglide
```

The integration tests use local loopback fixtures. Tests that require Chrome return without running
when no executable is discovered, so a green test command alone does not prove browser readiness.

## Browser runtime check

Run the public diagnostic and confirm `external_chrome.status` is `ready`:

```console
./target/release/termglide doctor
```

Then run a real interactive PTY smoke against a local data URL:

```console
./target/release/termglide \
  'data:text/html,<title>TermGlide 1.0</title><body>TermGlide 1.0</body>'
```

Verify that a frame appears, input and resize work, `Ctrl-C` exits, the terminal is restored, the
Chrome child exits, and its temporary profile is removed.

For the focused renderer journey:

```console
env -u NO_COLOR TERM=xterm-256color cargo test --locked -p termglide \
  --test external_terminal_backend_matrix_chrome \
  external_terminal_backend_matrix_chrome -- --exact --nocapture
```
