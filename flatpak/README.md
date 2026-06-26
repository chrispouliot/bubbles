# Flatpak packaging

Local build/install/run. Not Flathub-ready yet (the build needs network — see
"Going offline / Flathub" below).

## One-time: install runtime, SDK, and the Rust extension

Match the versions to the manifest's `runtime-version`. Check what's installed:

    flatpak list --runtime | grep -E 'gnome|rust-stable'

Then (for GNOME 50 / freedesktop base 25.08):

    flatpak install flathub \
      org.gnome.Platform//50 \
      org.gnome.Sdk//50 \
      org.freedesktop.Sdk.Extension.rust-stable//25.08

The `//50` and `//25.08` intentionally differ — the `rust-stable` branch tracks
the freedesktop base of the GNOME SDK, not the GNOME version number. The
manifest pins `//25.08` explicitly so `flatpak-builder` doesn't fall back to the
runtime's branch (which would point at a non-existent `rust-stable//50`).

The `codecs-extra` extension (libx264, libx265, nvenc) at branch `25.08-extra`
is also pulled in automatically when the runtime installs — no `add-extensions`
stanza is needed in the manifest for HEVC/H.264 video to work.

## Build + install + run

    flatpak-builder --user --install --force-clean build-dir io.github.chrispouliot.Bubbles.yml
    flatpak run io.github.chrispouliot.Bubbles

## Pointing at your self-hosted relay

The relay host is a runtime env var (`BUBBLES_RELAY_HOST`, optional
`BUBBLES_RELAY_TOKEN`), so no rebuild is needed:

    # one-off
    flatpak run --env=BUBBLES_RELAY_HOST=http://nas:8085 io.github.chrispouliot.Bubbles

    # persistent (preferred for a fixed sidecar)
    flatpak override --user --env=BUBBLES_RELAY_HOST=http://nas:8085 io.github.chrispouliot.Bubbles
    flatpak run io.github.chrispouliot.Bubbles

The LAN sidecar is reachable because the manifest grants `--share=network`.
A custom host omits the hosted-relay token automatically; set
`BUBBLES_RELAY_TOKEN` too if your sidecar wants one.

Demo mode (no network/onboarding): `--env=BUBBLES_DEMO=1`.

## What the build does

`flatpak/build.sh` runs inside the sandbox and:

1. downloads a prebuilt `protoc` (the freedesktop SDK has none; `build.rs`
   needs it for `mac_hw_info.proto`, and so does rustpush),
2. `cargo build --release` (rustpush is on by default). rustpush is vendored
   under `third_party/rustpush` in this repo — submodules (`apple-private-apis`,
   `open-absinthe`, and `apple-private-apis/clearadi`) and the seeded FairPlay
   certs (`certs/fairplay/`, copies of the public `certs/legacy-fairplay/`
   placeholders) are committed, so there is no build-time clone or
   cert-seeding step,
3. installs the binary, `.desktop`, icon, and metainfo into `/app`.

## Going offline / Flathub (later)

Flathub forbids network during build. rustpush is already vendored, so only
two network uses remain: fetching crates and downloading `protoc`. To finish:

- replace the `--share=network` build-arg by vendoring crates: run
  `flatpak-cargo-generator.py Cargo.lock -o cargo-sources.json` and add those
  sources (this also covers the one cargo git dep, `android-loader`),
- vendor `protoc` as a `protobuf` module or an `extra-data` binary instead of
  curling it.

## Notes

- The `.desktop` basename matches the app id, so GNOME associates the
  notifications the app posts (the click-to-open + cross-device withdraw paths)
  with this app — under bare `cargo run` that association is flaky.
- `project_license` is SSPL-1.0 (rustpush is a covered work once its feature is
  on). Keep the metainfo in sync, and drop the standalone LICENSE per PHASE_A.md.
- Replace the placeholder icon at
  `assets/icons/hicolor/scalable/apps/io.github.chrispouliot.Bubbles.svg`.
- First build is slow (clones rustpush + submodules, compiles the whole tree).
  Re-runs reuse `.cargo-home` inside the build dir.
