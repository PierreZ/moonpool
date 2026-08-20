//! Compiles the example gRPC service definition with tonic-prost-build.
//!
//! Requires `protoc` on PATH (provided by the nix dev shell).

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_prost_build::configure()
        // No tonic::transport in the sim — connections are hand-wired over
        // moonpool's simulated network, so skip the connect() helpers.
        .build_transport(false)
        .compile_protos(&["proto/echo.proto"], &["proto"])?;
    Ok(())
}
