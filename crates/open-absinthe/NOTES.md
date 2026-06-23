# open-absinthe NAC port — v1 (native validation-data generation)

This fills in the previously-stubbed `ValidationCtx` so `MacOSConfig`'s
`generate_validation_data()` works **without** the Python+Unicorn sidecar. It is
a Rust port of the sidecar harness (`jelly.py` + `nac.py`); it does **not**
reimplement Apple's algorithm — it still runs Apple's `IMDAppleServices`
machine code under an emulator, exactly as the Python did.

## What's here
- `Cargo.toml`   — adds `goblin`, `unicorn-engine`, `log`.
- `src/lib.rs`   — `AbsintheError` + `From<uc_error>`/`From<goblin>`; wires `jelly`/`hooks`.
- `src/jelly.rs` — Mach-O load + Unicorn x86-64 setup + hook page + SysV `call()`.
- `src/hooks.rs` — CF object model + all ~37 IOKit/CF/libc handlers, sourced from `HardwareConfig`.
- `src/nac.rs`   — `HardwareConfig` (unchanged) + `ValidationCtx` over the three offsets.

## How it runs
1. `ValidationCtx::new(cert, &mut req, hw)` → loads the binary, runs `nacInit` (0xB1DB0), returns the request.
2. rustpush POSTs the request to `id-initialize-validation` (unchanged; open-absinthe stays network-free).
3. `key_establishment(session_info)` → `nacKeyEstablishment` (0xB1DD0).
4. `sign()` → `nacSign` (0xB1DF0) → validation data.

## Verified in-sandbox (against the real binary, sha1 e1181cc…)
- goblin parses the fat container, yields the x86-64 slice (offset 0x32b000).
- `macho.imports()` resolves all 1415 binds to `(name, address)` with leading
  underscores intact — this replaces jelly.py's bind-opcode parser.
- All hooked symbols are present as imports.
- All four source files parse cleanly (rustfmt).

## NOT yet verified (needs your devshell + a real run)
- **Compilation**: `unicorn-engine` builds libunicorn (C) — can't build here.
  Expect to shake out `unicorn-engine` 2.x API specifics first: `Unicorn::new`
  return/lifetime, `add_code_hook` closure signature, `Prot`/`Mode`/`RegisterX86`
  import paths, `mem_read_as_vec`, `reg_write` arg type.
- **`product-name` / `board-id` CFData framing**: currently `bytes + NUL`. The
  sample plist had len 14/21; if `nacSign` errors, this framing is the first
  suspect.
- **`CFDictionary` key/value resolution** (`resolve_key`/`resolve_val` in
  hooks.rs) is the loosest part; fine for the volume-UUID path but may need
  shake-out if the binary builds dicts itself.

## To use
The `IMDAppleServices` binary is fetched automatically: on first use it downloads
from the community mirror to `$XDG_CACHE_HOME/open-absinthe/` (or `~/.cache/...`),
sha1-verifies it, and reuses the cache thereafter. No manual step. To override
(offline / a local copy / tests), set `OPEN_ABSINTHE_IMD=/path/to/IMDAppleServices`.

Then in rustpush, use `MacOSConfig` (not `RelayConfig`) — no client protocol
changes once `ValidationCtx` works. Enable open-absinthe via its
`macos-validation-data` feature as before.

## Caveats
- `unicorn-engine` is **GPL-2.0**; linking it pulls the client under GPL-2.0.
  Interacts with the earlier plan to settle the project license.
- `unicorn`/`goblin` are currently hard deps of open-absinthe; can be
  feature-gated behind `macos-validation-data` later so the crate still builds
  without them.

## Next phases
1. Get it compiling + a successful end-to-end sign (one Apple round-trip).
2. Auto-fetch + dyld-cache-extract `IMDAppleServices` on first run (zero-touch install).
3. Feature-gate the C deps.
