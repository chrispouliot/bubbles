# Bubbles

A native GTK4 / libadwaita client for [OpenBubbles](https://github.com/OpenBubbles/openbubbles-app),
written in Rust. The goal is to replace the Flutter desktop app with a 
Rust/GTK client that links the same [`rustpush`](https://github.com/OpenBubbles/rustpush) for the backend


## Develop

The dev environment is a Nix flake devshell, wired for `direnv`:

```sh
direnv allow
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


