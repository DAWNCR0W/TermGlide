# Architecture

TermGlide 1.0 has a single rendering pipeline:

```text
CLI/config -> isolated Chrome process -> loopback CDP -> PNG/DOM/accessibility data
           -> bounded terminal projection -> ANSI output
```

## Workspace ownership

| Package | Responsibility |
|---|---|
| `apps/termglide` | CLI parsing, product policy, automation output, and interactive orchestration |
| `tg-browser` | Chrome/Chromium discovery, supervised process lifecycle, and input translation |
| `tg-network` | Bounded CDP WebSocket transport and typed CDP operations |
| `tg-terminal` | PNG decoding, RGBA projection, terminal-cell diffing, and ANSI encoding |
| `tg-platform` | Configuration paths, TTY ownership, terminal restoration, and shutdown signals |
| `tg-config` | Strict versioned TOML configuration |
| `tg-url` | Address/search classification and URL policy |
| `tg-core` | Shared limits, cancellation, clock, identifiers, and error categories |

## Session lifecycle

1. The CLI resolves configuration, URL/search input, renderer, terminal geometry, and limits.
2. `tg-browser` discovers or validates a local executable and launches it with a unique temporary
   profile and an ephemeral loopback debugging port.
3. `tg-network` attaches to the first page target and performs bounded CDP requests.
4. Chrome screenshots are decoded and projected by `tg-terminal`; interactive input is translated
   back into CDP keyboard, mouse, wheel, focus, and resize operations.
5. The CDP connection, Chrome process, temporary profile, and terminal state are cleaned up on every
   exit path. Combined operation/cleanup failures retain both causes.

## Product boundaries

- Chrome owns web standards, JavaScript, network behavior, media, and sandboxing.
- TermGlide never reads or reuses a normal Chrome profile.
- Only local loopback CDP endpoints created by the supervised process are accepted.
- Screenshots, DOM, accessibility data, events, and output are bounded by explicit quotas.
- Interactive mode uses button/wheel mouse capture without pointer-motion flooding.
- The terminal renderer retains a last accepted frame and publishes only validated complete output.
