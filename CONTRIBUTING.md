# Contributing

Thank you for helping improve TermGlide. The project aims to stay a focused, dependable
Chrome-to-terminal application, so contributions should strengthen that product boundary instead
of expanding it by default.

## Before you start

- Search existing issues before filing a bug or proposing a feature.
- Small fixes and documentation improvements can go directly to a pull request.
- Discuss substantial features, architectural changes, or new dependencies before investing in an
  implementation.
- Report security vulnerabilities privately through
  [GitHub Security Advisories](https://github.com/DAWNCR0W/TermGlide/security/advisories/new), not a
  public issue.

When reporting a bug, include the TermGlide version or commit, operating system, terminal, Chrome
or Chromium version, reproduction steps, expected behavior, and relevant diagnostic output. Remove
credentials, cookies, tokens, local paths, and other private data before sharing logs or traces.

## Branch workflow

- `main` is the stable release branch. Do not push or force-push to it directly.
- `develop` is the integration branch and the target for every regular contribution pull request.
- Pull requests targeting `main` are reserved for maintainer release promotions from `develop`.
- Create a focused branch from the latest `develop`, using a descriptive prefix such as `feat/`,
  `fix/`, `docs/`, `test/`, or `chore/`.

Keep each branch limited to one coherent change. Avoid drive-by formatting, generated artifacts,
browser profiles, traces, credentials, and unrelated refactors.

## Local setup

Install Rust `1.92.0` and a local Chrome or Chromium executable. Fork the repository, clone your
fork, and create a branch from `develop`:

```console
git fetch origin
git switch develop
git pull --ff-only
git switch -c fix/short-description
```

Use `termglide doctor` to confirm that TermGlide can discover and launch the browser on your
machine.

## Required validation

Run the complete local gate from the repository root before requesting review:

```console
cargo fmt --all -- --check
cargo check --locked --workspace --all-targets
cargo test --locked --workspace --all-targets
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo build --locked --release -p termglide
./target/release/termglide doctor
```

Tests that require Chrome skip when no compatible executable is discovered. A green test command
therefore does not prove browser readiness: verify that `doctor` reports
`external_chrome.status = ready` and complete the relevant real-browser smoke checks in
[docs/testing.md](docs/testing.md). State any skipped or unavailable check plainly in the pull
request.

## Engineering guidelines

- Keep Chrome process ownership in `tg-browser`, CDP transport in `tg-network`, terminal projection
  in `tg-terminal`, and CLI policy in `apps/termglide`.
- Keep resource limits, cancellation, temporary-profile cleanup, and terminal restoration explicit.
- Add focused tests for observable behavior, platform differences, and failure cleanup.
- Preserve the public CLI and configuration contract unless the change intentionally documents a
  compatible evolution or a justified breaking change.
- Do not make a missing Chrome installation look like successful release evidence; `doctor` must be
  checked separately.
- Update user-facing docs and [CHANGELOG.md](CHANGELOG.md) when the CLI or supported scope changes.
- Keep unsafe Rust forbidden and resolve Clippy warnings instead of suppressing them without a clear
  reason.

## Pull requests

A reviewable pull request should:

- target `develop` and reference any related issue;
- explain what changed, why it changed, and what users will observe;
- include tests or a clear explanation of why tests are not applicable;
- list the exact commands and runtime checks performed;
- call out platform-specific behavior, compatibility risks, and known limitations;
- update documentation and the changelog when users need to know about the change; and
- contain no secrets, personal data, unrelated files, or generated build output.

Maintainers may ask for a contribution to be narrowed, split, or redesigned when that keeps the
project reliable and maintainable.

## Conduct and licensing

Participation in TermGlide project spaces is governed by the
[Code of Conduct](CODE_OF_CONDUCT.md).

By submitting a contribution, you agree that it may be distributed under TermGlide's existing
Apache-2.0 OR MIT dual-license terms.
