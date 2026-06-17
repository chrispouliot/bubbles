# AGENTS.md — openbubbles-gtk

Rust + GTK4 iMessage client built on rustpush. Project-specific facts only; the global
behavioral rules (editing, recovery, verify, footguns) layer underneath these.

## Build & verify

Run from the workspace root:

- build: `cargo build`
- test:  `cargo test` (focused: `cargo test <module>` / `cargo test <name>`)
- lint:  `cargo clippy --all-targets -- -D warnings`

A test that references a not-yet-written function or type fails to **compile** — in Rust
that compile error is the expected "red" for a fresh test. Leave it red; don't stub
production code to make it compile.

## Tests

The global "don't set up a test via shared/global state" footgun, in Rust, means
`std::env::set_var`: never set `XDG_DATA_HOME` (or any env var) to redirect paths in a test
— it races the parallel test threads and is `unsafe` in edition 2024. Pass an explicit path
/ temp dir into the type under test instead; `tempfile` is fine as a dev-dependency.

## GTK

- Single-threaded main loop: do UI/widget updates on the main thread, and never block it
  with synchronous I/O, network, or DB calls — offload to tokio and hand results back via
  the existing channel / idle-callback pattern.
- Reuse the existing widget and builder patterns rather than a new construction style.

## rustpush

- `third_party/rustpush` is vendored: read-only reference unless the unit explicitly targets it.
- Incoming protocol data (messages, reactions, link metadata, attachments) arrives
  **decoded** — consume those types directly; don't re-parse the wire format or re-fetch
  off the network to reconstruct what rustpush already gives you.
