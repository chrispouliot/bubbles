# Bubbles


<img width="1740" height="645" alt="Photo of the app chat view" src="https://github.com/user-attachments/assets/b2d316ed-bc3b-4a42-b2b5-9e1830449924" />

A native Linux iMessage client written in Rust with GTK4 / libadwaita based on [OpenBubbles](https://github.com/OpenBubbles/openbubbles-app).
The app implements the same [rustpush](https://github.com/OpenBubbles/rustpush) for the backend
as the original OpenBubbles Flutter app and uses the same Hosted Relay or Mac Hardware Token authentication.

Implements KDE Status Notifier for tray icon support and background app running without a window (On Gnome, use the Kstatus Notifier / App Indicator extension)
including red dot icon for new messages.

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

## Install (Flatpak)

A flatpak manifest is provided at `io.github.chrispouliot.Bubbles.yml`.
Building it produces a self-contained bundle that runs on any Linux
distribution with the `org.gnome.Platform//50` runtime installed.

### Prerequisites

The build needs `flatpak` and `flatpak-builder` on the host, plus the
GNOME 50 runtime and the `rust-stable` SDK extension. The vendored
crate sources for the bundled gstreamer plugin
(`gst-plugin-gtk4-sources.json`) are committed in this repo, so no
generation step is required at install time.

The Nix dev shell from the [Develop](#develop) section above provides
`flatpak-builder` and `flatpak` directly; install the runtime +
extension on the host:

```sh
flatpak remote-add --user --if-not-exists flathub https://dl.flathub.org/repo/flathub.flatpakrepo
flatpak install --user flathub org.gnome.Platform//50
flatpak install --user flathub org.freedesktop.Sdk.Extension.rust-stable//25.08
```

Without Nix, install the build tools directly: `flatpak` and
`flatpak-builder` from your package manager.

The `//25.08` suffix on the extension is mandatory — it tracks the
freedesktop SDK base, not the GNOME runtime version. Without it,
`rust-stable` would inherit the runtime's branch (50), which does not
exist on Flathub.

### Build

```sh
flatpak-builder --user --install build-dir io.github.chrispouliot.Bubbles.yml
```

`build-dir` is gitignored. The first build is slow (the `gst-plugins-rs`
module compiles its full cargo dep graph inside the sandbox);
subsequent builds cache and skip rebuilt modules.

### Run

```sh
flatpak run io.github.chrispouliot.Bubbles
```

To verify the bundled `gtk4paintablesink` plugin is discoverable
(needed for video playback in the lightbox):

```sh
flatpak run --command=bash io.github.chrispouliot.Bubbles \
  -c 'gst-inspect-1.0 gtk4paintablesink'
```

Element metadata in the output confirms the plugin is wired up. "No
such element or plugin" means the `.so` didn't land in
`/app/lib/gstreamer-1.0/`, see the `gst-plugins-rs` module's
`cargo cinstall` build-commands in `io.github.chrispouliot.Bubbles.yml`
for the `--libdir` fix.

## Install (NixOS)

A NixOS module is exposed as `nixosModules.default` from the flake,
so the recommended way to install on NixOS is to add the flake as
an input and enable the module in `configuration.nix`.

In `flake.nix`:

```nix
{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    bubbles.url = "github:chrispouliot/bubbles";
  };

  outputs = { nixpkgs, bubbles, ... }: {
    nixosConfigurations.nixos = nixpkgs.lib.nixosSystem {
      system = "x86_64-linux";
      modules = [
        bubbles.nixosModules.default
        ({ ... }: {
          programs.bubbles.enable = true;
        })
      ];
    };
  };
}
```

The module installs the bubbles package and `hicolor-icon-theme` (the
base icon theme every other theme inherits from. Without it, the app
icon silently fails to resolve in the apps grid). To pin a specific
build of the package (like one you've built locally with
`nix build .#default` from this repo) override the `package` option:

```nix
programs.bubbles = {
  enable = true;
  package = bubbles.packages.${pkgs.stdenv.hostPlatform.system}.default;
  # default; only set this if you want to override (e.g. a local
  #   `nix build` artifact or a fork tracking a different branch).
};
```

**Build note:** the package isn't published to any binary cache, so
the first `nixos-rebuild switch` builds it from source. Expect tens
of minutes on the first activation; subsequent evaluations reuse
the local store.



