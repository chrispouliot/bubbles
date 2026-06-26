# Bubbles

A native GTK4 / libadwaita client for [OpenBubbles](https://github.com/OpenBubbles/openbubbles-app),
written in Rust. The goal is to replace the Flutter desktop app with a faster,
more native-feeling client that links the same [`rustpush`](https://github.com/OpenBubbles/rustpush)
protocol crate the upstream app uses — so the Apple-protocol layer is reused, not
reimplemented.

## Status

Scaffold only. Right now this is a minimal libadwaita window to prove out the
toolchain and devshell. `rustpush` is **not** linked yet.

## Develop

The dev environment is a Nix flake devshell, wired for `direnv`:

```sh
direnv allow      # picks up .envrc -> `use flake`
# or, without direnv:
nix develop
```

Then:

```sh
cargo run
```

The devshell provides the Rust toolchain (via fenix), the GTK4 / libadwaita
stack, and the build-time deps `rustpush` will need (`perl` for vendored
OpenSSL, `protobuf` for the prost-build glue).

## Planned architecture

- `rustpush` as a git/path dependency — APNs/IDS/iMessage/etc., reused as-is.
- Lift the relevant parts of upstream `rust/src/api/api.rs` as the call surface.
- Port the runtime glue from `lib/services/rustpush/rustpush_service.dart`
  (the `PushMessage` event loop, send orchestration, MMCS attachments) into Rust.
- SQLite (rusqlite/sqlx) for the local store, replacing the app's ObjectBox.
- gtk4-rs + libadwaita for the UI.

## License

The moment `rustpush` is linked, this crate becomes a "covered work" under
`rustpush`'s **Server Side Public License v1 (SSPL-1.0)**, which carries
GPLv3-style whole-work copyleft. Distributing the client (including hosting
source here on GitHub) requires the entire repo to be SSPL-licensed; it cannot
be permissive or proprietary. Add the full SSPL v1 text as `LICENSE` before the
first dependency on `rustpush` lands.

This is not legal advice.
