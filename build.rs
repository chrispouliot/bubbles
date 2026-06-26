use std::io::Result;
use std::process::Command;

fn main() -> Result<()> {
    // Re-run the build script when $PROTOC changes, so the protoc-availability
    // check below stays accurate across builds where the user overrides the
    // binary location.
    println!("cargo:rerun-if-envchanged=PROTOC");

    // Only the rustpush backend needs the Mac-hardware proto. Guard on the
    // feature so the default build doesn't require protoc.
    if std::env::var("CARGO_FEATURE_RUSTPUSH").is_ok() {
        // prost-build shells out to `protoc` (or the binary at $PROTOC) to
        // compile .proto files. Detect a missing toolchain up front and print
        // a clear hint, rather than letting the user wade through prost-build's
        // generic stderr.
        if let Ok(path) = std::env::var("PROTOC") {
            if !std::path::Path::new(&path).exists() {
                panic!(
                    "build.rs: $PROTOC points at `{}` but that file does not exist. \
                     Install protobuf-compiler (Debian/Ubuntu: `apt install protobuf-compiler`, \
                     Fedora: `dnf install protobuf-compiler`, macOS: `brew install protobuf`) \
                     or fix $PROTOC.",
                    path,
                );
            }
        } else if Command::new("protoc").arg("--version").output().is_err() {
            panic!(
                "build.rs: `protoc` was not found on PATH and $PROTOC is unset. \
                 Install protobuf-compiler (Debian/Ubuntu: `apt install protobuf-compiler`, \
                 Fedora: `dnf install protobuf-compiler`, macOS: `brew install protobuf`).",
            );
        }

        let mut prost_build = prost_build::Config::new();
        prost_build.protoc_arg("--experimental_allow_proto3_optional");
        prost_build.compile_protos(&["src/mac_hw_info.proto"], &["src/"])?;
    }
    Ok(())
}
