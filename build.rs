// Compiles the Nearby Share protobufs (proto/, from Chromium via NearDrop) without protoc.
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let files = [
        "proto/offline_wire_formats.proto",
        "proto/wire_format.proto",
        "proto/sharing_enums.proto",
        "proto/ukey.proto",
        "proto/securemessage.proto",
        "proto/securegcm.proto",
        "proto/device_to_device_messages.proto",
    ];
    let fds = protox::compile(files, ["proto"])?;
    prost_build::Config::new().compile_fds(fds)?;
    println!("cargo:rerun-if-changed=proto");
    Ok(())
}
