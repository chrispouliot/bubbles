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
        pkgs = import nixpkgs { inherit system; };

        # Established pattern: fenix for the Rust toolchain.
        # stable.toolchain bundles rustc, cargo, clippy, rustfmt, rust-src, rust-std.
        toolchain = fenix.packages.${system}.stable.toolchain;

        nativeBuildInputs = with pkgs; [
          toolchain
          rust-analyzer
          pkg-config

          # Build-time deps needed once the rustpush submodule is linked.
          perl # vendored OpenSSL (rustpush pins openssl/vendored)
          protobuf # protoc, for the prost-build glue (bbhwinfo / cloudkit_proto)
          cmake # insurance for assorted -sys crates
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
    );
}
