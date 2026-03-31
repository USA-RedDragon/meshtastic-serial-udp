use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let proto_root = PathBuf::from("proto/meshtastic-protobufs");

    let proto_files = [
        "meshtastic/mesh.proto",
        "meshtastic/portnums.proto",
        "meshtastic/channel.proto",
        "meshtastic/config.proto",
        "meshtastic/device_ui.proto",
        "meshtastic/module_config.proto",
        "meshtastic/telemetry.proto",
        "meshtastic/xmodem.proto",
        "meshtastic/atak.proto",
    ];

    let proto_paths: Vec<PathBuf> = proto_files
        .iter()
        .map(|f| proto_root.join(f))
        .collect();

    prost_build::Config::new()
        .compile_protos(&proto_paths, &[&proto_root])?;

    Ok(())
}
