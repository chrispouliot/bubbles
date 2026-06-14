# Phase A — the real `rustpush` backend

The adapter (`src/protocol/rustpush_backend.rs`) implements `Backend` over
`rustpush` + the vendored api glue. It is **not in the default build**; the repo
keeps compiling against `StubBackend` until you finish A1–A3.

## Pins (match these exactly — `api.rs` was written against them)

| repo               | commit                                     |
|--------------------|--------------------------------------------|
| rustpush           | `a7fab473e7a33325a760635285db2860de8e1cb0` |
| apple-private-apis | `e1c2b0b26bc0d1e9ef04191b8d31c079ee625586` |
| open-absinthe      | `1f8dc73a311e7b4d94a868972a6816c8a2c14e44` |

`clearadi` is only needed for `remote-clearadi`, which you don't enable. Use
features `macos-validation-data` + `remote-anisette-v3`.

## A1. The api glue — mostly done

Do **not** copy `api.rs` wholesale: it `include!`s a 1666-line `mirrors.rs`
(100 FRB type-mirrors of rustpush types), pulls in `frb_generated` streaming,
and carries the entire FaceTime/FindMy/CloudKit/passwords surface — none of
which onboarding needs.

`src/api/mod.rs` in this commit is the **onboarding subset already extracted and
de-FRB'd** for you: only the ~20 functions `RustpushBackend` calls plus their
private helpers (`get_login_config`, `do_login`, `reset_user`, `GSAConfig`,
`generate_udid`, `plist_to_string`, `JoinedOSConfig`, `HwExtra`, `DeviceInfo`,
`SavedHardwareState`). `mirrors.rs`, `frb_generated`, `DartFnFuture`, and
`StreamSink` are all gone; the types come from the `use rustpush::...` block at
the top.

Two small things it still needs from the upstream crate:

1. **`src/api/runtime.rs`** — copy `RUNTIME` and `init_logger` from upstream
   `rust/src/lib.rs`. (`do_first_time_init` is already in `mod.rs`.)
2. **`bbhwinfo`** (the Mac-hardware protobuf; used by `get_device_info` and
   `config_from_encoded`). Copy upstream `rust/build.rs` + `rust/src/mac_hw_info.proto`
   to your crate root, and add to `main.rs`:
   ```rust
   pub mod bbhwinfo {
       include!(concat!(env!("OUT_DIR"), "/bbhwinfo.rs"));
   }
   ```

Then `cargo build --features rustpush` and follow the compiler for any stray
private helper (copy it by name from upstream `api.rs`). The subset was checked
for crate-internal references — only `bbhwinfo` and `crate::api::runtime`
remain, so this should be a short loop, not a slog.

## A2. Cargo

```toml
[features]
rustpush = ["dep:rustpush", "dep:keystore"]

[dependencies]
rustpush = { git = "https://github.com/OpenBubbles/rustpush.git", rev = "a7fab473e7a33325a760635285db2860de8e1cb0", features = ["macos-validation-data", "remote-anisette-v3"], optional = true }
keystore = { git = "https://github.com/OpenBubbles/rustpush.git", rev = "a7fab473e7a33325a760635285db2860de8e1cb0", package = "keystore", optional = true }
prost = "0.12"
plist = "1"
log = "0.4"
rand = "0.8"
uuid = { version = "1", features = ["v4"] }
sha2 = "0.10"
base64 = "0.21"
serde = { version = "1", features = ["derive"] }
async-recursion = "1"

[build-dependencies]
prost-build = "0.12"
```

The devshell already provides `perl` (vendored OpenSSL) and `protobuf` (protoc).
**Drop the SSPL `LICENSE` in this commit** — copyleft attaches here.

## A3. Wire it

- `src/protocol/mod.rs`: `#[cfg(feature = "rustpush")] pub mod rustpush_backend;`
- crate root (`main.rs`): `mod api;` + the `bbhwinfo` module above
- `main.rs`, under `--features rustpush`:

```rust
api::runtime::do_first_time_init(&state_path); // or api::do_first_time_init
let backend: Arc<dyn Backend> =
    Arc::new(protocol::rustpush_backend::RustpushBackend::new(state_path));
```

Build with `cargo run --features rustpush`. The flow and view are unchanged.

## Known `// VERIFY` points in the adapter

- `config_from_validation_data`: map our `HwExtra` → `api::HwExtra` (relay path,
  the default, doesn't need it).
- `send_2fa_sms`: picks the first trusted number; surface `TrustedPhoneNumber`
  to the UI for parity.
- `setup_push`: passes `state: None`. Session restore is A2-proper (below).
- Mutex type: the adapter uses `rustpush::DebugMutex` for the account to match
  `api::try_auth`'s return type — confirm on build.

## A2-proper — session restore on launch

Branch in `main`: if `state_path` holds a saved session, restore it
(`setup_push(state = Some(saved_aps_state))` + `decode_identity` +
`restore_users` + `make_imclient`) and skip onboarding. Otherwise run setup.
