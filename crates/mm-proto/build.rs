fn main() -> Result<(), Box<dyn std::error::Error>> {
    let protos = [
        "../../proto/mmrpc/v1/media.proto",
        "../../proto/mmrpc/v1/ops.proto",
        "../../proto/mmrpc/v1/job.proto",
        "../../proto/mmrpc/v1/stitch.proto",
        "../../proto/mmrpc/videomerge/v1/videomerge.proto",
        "../../proto/mmrpc/v1/service.proto",
    ];
    tonic_prost_build::configure()
        .build_client(true)
        .build_server(true)
        .compile_protos(&protos, &["../../proto"])?;
    Ok(())
}
