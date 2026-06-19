{
  description = "Native GTK4/libadwaita client for OpenBubbles (iMessage)";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    fenix = {
      url = "github:nix-community/fenix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      flake-utils,
      fenix,
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        # The package is SSPL-1.0 (per Cargo.toml), which nixpkgs classifies
        # as unfree. Even when a consumer sets `nixpkgs.config.allowUnfree =
        # true` and `inputs.nixpkgs.follows = "nixpkgs"` on this flake, the
        # follow shares the nixpkgs FLAKE — not the nixpkgs INSTANCE. The
        # fresh import below is a separate instance with default config, so
        # `meta.license = licenses.sspl` would refuse to evaluate against
        # the consumer's setting. Pass `config.allowUnfree = true` here so
        # this flake's own build can evaluate; the consumer's separate
        # nixpkgs instance (with their own `nixpkgs.config`) is unaffected.
        pkgs = import nixpkgs {
          inherit system;
          config = { allowUnfree = true; };
        };

        # Established pattern: fenix for the Rust toolchain.
        # stable.toolchain bundles rustc, cargo, clippy, rustfmt, rust-src, rust-std.
        toolchain = fenix.packages.${system}.stable.toolchain;

        nativeBuildInputs = with pkgs; [
          toolchain
          rust-analyzer
          pkg-config
          
          # Needed for absinthe apple nac emulator (rust + C code)
          rustPlatform.bindgenHook
          cmake
          gnumake

          # Build-time deps needed once the rustpush submodule is linked.
          perl # vendored OpenSSL (rustpush pins openssl/vendored)
          protobuf # protoc, for the prost-build glue (bbhwinfo / cloudkit_proto)
          cmake # insurance for assorted -sys crates

          # flatpak-builder shells out to appstreamcli for its metainfo compose
          # step; without it on PATH the build fails at the very end.
          appstream
        ];

        buildInputs = with pkgs; [
          glib
          gtk4
          libadwaita
          gdk-pixbuf
          graphene
          cairo
          pango
          openssl # for OPENSSL_NO_VENDOR=1 if you ever drop vendored
        ];
      in
      {
        # Build the GTK client as a normal Nix package so it can be consumed
        # from an external flake (e.g. `inputs.openbubbles-gtk.url =
        # "path:/work";` from a NixOS/home-manager config) and installed via
        # `environment.systemPackages` or `home.packages`. The .desktop file,
        # AppStream metainfo, and hicolor icons are dropped into the standard
        # $out/share/... locations so the entry is discoverable by xdg and
        # software centers.
        packages.default = pkgs.rustPlatform.buildRustPackage {
          pname = "openbubbles-gtk";
          version = "0.1.0";

          # `self` is the flake's own source tree. For a git-based flake,
          # Nix copies ONLY git-tracked files into the store, so untracked /
          # gitignored paths (target/, .direnv/, build-dir/, etc.) are already
          # excluded — no `cleanSource`/`fetchGit` gymnastics needed, and the
          # 5+ GB `target/` dir is never copied. We previously used
          # `builtins.fetchGit ./.` here, but that breaks pure evaluation
          # (which `nixos-rebuild` uses): when this flake is consumed as an
          # input and the tree is dirty, fetchGit has no locked rev and errors
          # with "in pure evaluation mode, 'fetchGit' doesn't fetch unlocked
          # input". `self` has no such problem.
          # Note: a file must be git-*tracked* to be visible to the build, so
          # `git add` any newly created file before rebuilding.
          src = self;
          cargoLock = {
            lockFile = ./Cargo.lock;
            # Cargo.lock pins android-loader to a git+https rev. Letting
            # cargo's libgit2 fetch it at build time is fine — the rev pin
            # in the lockfile keeps the build deterministic.
            allowBuiltinFetchGit = true;
          };

          # build.rs runs `prost-build` when the default `rustpush` feature
          # is on, so `protoc` (from the `protobuf` drv) must be on PATH.
          # rustpush pins `openssl` with the `vendored` feature, which the
          # OpenSSL build script drives through `perl`.
          # `gtk4` is in nativeBuildInputs for `gtk4-update-icon-cache`,
          # which we invoke ourselves at the end of postInstall to build
          # `$out/share/icons/hicolor/icon-theme.cache`. NOTE: gtk4's setup
          # hook does NOT generate that cache — its `dropIconThemeCache`
          # hook *deletes* any `icon-theme.cache` from `$out/share/icons`
          # in preFixup (because per-prefix caches normally only collide in
          # a merged profile). We opt out of that via `dontDropIconThemeCache`
          # below so the cache we build survives. `gdk-pixbuf` and `librsvg`
          # are here (not just buildInputs) because their setup hooks set
          # `GDK_PIXBUF_MODULE_FILE` and register the SVG loader — without
          # those, `gtk4-update-icon-cache` silently fails to load the
          # scalable SVG and writes an empty/partial cache, which is exactly
          # the "icon shows up blank in the apps grid" symptom.
          nativeBuildInputs = with pkgs; [
            pkg-config
            cmake
            gnumake
            perl
            protobuf
            rustPlatform.bindgenHook
            gtk4
            gdk-pixbuf
            librsvg
          ];

          # `hicolor-icon-theme` provides the base hicolor files
          # (share/icons/hicolor/index.theme, symbolic/apps/, etc.) that
          # every other icon theme inherits from. Pulled in via the
          # nixosModules.default below so it lands in the *system*
          # profile (and therefore in the system-wide icons union) —
          # buildInputs here wouldn't propagate.
          buildInputs = with pkgs; [
            glib
            gtk4
            libadwaita
            gdk-pixbuf
            graphene
            cairo
            pango
            openssl
            librsvg
          ];

          postInstall = ''
            install -Dm644 $src/app.openbubbles.Gtk.Devel.desktop \
              $out/share/applications/app.openbubbles.Gtk.Devel.desktop
            install -Dm644 $src/app.openbubbles.Gtk.Devel.metainfo.xml \
              $out/share/metainfo/app.openbubbles.Gtk.Devel.metainfo.xml
            install -Dm644 $src/assets/icons/hicolor/scalable/apps/app.openbubbles.Gtk.Devel.svg \
              $out/share/icons/hicolor/scalable/apps/app.openbubbles.Gtk.Devel.svg
            for sz in 64 128 256; do
              install -Dm644 \
                "$src/assets/icons/hicolor/''${sz}x''${sz}/apps/app.openbubbles.Gtk.Devel.png" \
                "$out/share/icons/hicolor/''${sz}x''${sz}/apps/app.openbubbles.Gtk.Devel.png"
            done
            # In-app action icons (splash hero, send button, etc.) live
            # under hicolor/scalable/actions/. The app references them by
            # name at runtime — from_icon_name("empty-state") in the empty
            # state StatusPage, from_icon_name("ob-send-symbolic") on the
            # send button, etc. — and GTK resolves those names against
            # $XDG_DATA_DIRS/icons/hicolor. Skipping this means broken
            # image placeholders wherever they're used.
            for icon in $src/assets/icons/hicolor/scalable/actions/*.svg; do
              install -Dm644 "$icon" \
                "$out/share/icons/hicolor/scalable/actions/$(basename "$icon")"
            done
            # hicolor self-containment: copy the base hicolor files
            # (index.theme, etc.) from hicolor-icon-theme into our own
            # $out/share/icons/hicolor/ so this package ships a complete
            # hicolor tree. This must happen BEFORE we build the cache
            # below so index.theme is present.
            cp -r ${pkgs.hicolor-icon-theme}/share/icons/hicolor/. \
              $out/share/icons/hicolor/
            # Build the hicolor cache ourselves. gtk4's setup hook does NOT
            # create this (it only ever *removes* it — see dropIconThemeCache),
            # so without this the app icon has no cache and GNOME shows a blank
            # entry in the overview/dash on install paths that don't trigger
            # the system-level `gtk.iconCache.enable` rebuild (nix profile,
            # home-manager, etc.). `dontDropIconThemeCache = true` below keeps
            # the gtk4 preFixup hook from deleting what we generate here.
            # gdk-pixbuf + librsvg (in nativeBuildInputs) provide the SVG
            # loader so the scalable icon is indexed too.
            gtk4-update-icon-cache -f -t $out/share/icons/hicolor
          '';

          # Keep the cache we build in postInstall; gtk4's setup hook would
          # otherwise strip it in preFixup. See the postInstall comment.
          dontDropIconThemeCache = true;

          meta = with pkgs.lib; {
            description = "Native GTK4/libadwaita client for OpenBubbles (iMessage)";
            license = licenses.sspl;
            platforms = platforms.linux;
            mainProgram = "openbubbles-gtk";
          };
        };

        devShells.default = pkgs.mkShell {
          inherit nativeBuildInputs buildInputs;

          shellHook = ''
            export RUST_SRC_PATH="${toolchain}/lib/rustlib/src/rust/library"

            # NixOS has no global lib dir and rustc bakes no rpath for the GTK
            # stack, so the runtime loader can't find libadwaita-1.so.0 etc.
            # Put the buildInputs' lib dirs on the loader path.
            export LD_LIBRARY_PATH="${pkgs.lib.makeLibraryPath buildInputs}:''${LD_LIBRARY_PATH:-}"

            # Make GSettings schemas (gtk4 + libadwaita) resolvable so the app
            # runs from the devshell without schema-not-found warnings.
            export XDG_DATA_DIRS="${pkgs.gtk4}/share/gsettings-schemas/${pkgs.gtk4.name}:${pkgs.libadwaita}/share/gsettings-schemas/${pkgs.libadwaita.name}:${pkgs.gsettings-desktop-schemas}/share/gsettings-schemas/${pkgs.gsettings-desktop-schemas.name}:''${XDG_DATA_DIRS:-}"

            echo "openbubbles-gtk devshell · $(rustc --version)"
          '';
        };

        formatter = pkgs.nixfmt-rfc-style;
      }
    )
    // {
      # NixOS integration module. Consumers add this to `nixosSystem.modules`
      # and toggle `programs.openbubbles-gtk.enable` instead of wiring
      # `environment.systemPackages` themselves — same pattern as
      # `programs.firefox` / `programs.gnome-terminal`. The default package
      # follows the consumer's host system so cross-platform overrides
      # (e.g. `programs.openbubbles-gtk.package = ...;`) work without
      # further plumbing.
      nixosModules.default =
        { config
        , lib
        , pkgs
        , ...
        }:
        let
          cfg = config.programs.openbubbles-gtk;
        in
        {
          options.programs.openbubbles-gtk = {
            enable = lib.mkEnableOption "OpenBubbles GTK client (native iMessage client)";
            package = lib.mkOption {
              type = lib.types.package;
              default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
              defaultText = lib.literalExpression "openbubbles-gtk.packages.\${system}.default";
              description = "The openbubbles-gtk package to install.";
            };
          };

          config = lib.mkIf cfg.enable {
            environment.systemPackages = [
              cfg.package
              # The base hicolor icon theme (index.theme, symbolic/apps/, etc.)
              # is what every other theme inherits from, and is what the openbubbles-gtk
              # .desktop + icon-theme union at /run/current-system/sw/share/icons/
              # needs to actually be a valid hicolor tree. Without this in the
              # system profile, the app icon silently fails to resolve in the
              # apps grid and the tray/system notification lookups also fall back
              # to broken-image placeholders.
              pkgs.hicolor-icon-theme
            ];
          };
        };
    };
}
