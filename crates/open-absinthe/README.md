# Open Absinthe

This is an implementation of the open-absinthe backend which was stubbed out in the original repository
It allows for Apple NAC hardware token validation using emulation.

This fills in the previously-stubbed `ValidationCtx` so `MacOSConfig`'s
`generate_validation_data()`. It runs Apple's `IMDAppleServices`
machine code under an emulator.

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

## Verified in-sandbox
- goblin parses the fat container, yields the x86-64 slice (offset 0x32b000).
- `macho.imports()` resolves all 1415 binds to `(name, address)` with leading
  underscores intact — this replaces jelly.py's bind-opcode parser.
- All hooked symbols are present as imports.
- All four source files parse cleanly (rustfmt).

## To use
The `IMDAppleServices` binary is fetched automatically: on first use it downloads
from the community mirror to `$XDG_CACHE_HOME/open-absinthe/` (or `~/.cache/...`),
sha1-verifies it, and reuses the cache thereafter. No manual step. To override
(offline / a local copy / tests), set `OPEN_ABSINTHE_IMD=/path/to/IMDAppleServices`.

Then in rustpush, use `MacOSConfig` (not `RelayConfig`) — no client protocol
changes once `ValidationCtx` works. Enable open-absinthe via its
`macos-validation-data` feature as before.
