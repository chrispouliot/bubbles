use std::io::Result;

fn main() -> Result<()> {
    // Only the rustpush backend needs the Mac-hardware proto. Guard on the
    // feature so the default build doesn't require protoc.
    if std::env::var("CARGO_FEATURE_RUSTPUSH").is_ok() {
        let mut prost_build = prost_build::Config::new();
        prost_build.protoc_arg("--experimental_allow_proto3_optional");
        prost_build.compile_protos(&["src/mac_hw_info.proto"], &["src/"])?;
    }
    Ok(())
}
