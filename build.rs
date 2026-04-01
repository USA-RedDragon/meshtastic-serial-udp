use std::path::PathBuf;
use std::process::Command;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Embed version from PKG_VERSION env var (set by CI / build.sh)
    let version = std::env::var("PKG_VERSION").unwrap_or_else(|_| "0.0.0".to_string());
    println!("cargo:rustc-env=PKG_VERSION={version}");
    println!("cargo:rerun-if-env-changed=PKG_VERSION");

    // Embed git commit hash
    let commit = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=GIT_COMMIT={commit}");

    let proto_root = PathBuf::from("meshtastic-protobufs");

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
        "meshtastic/admin.proto",
        "meshtastic/connection_status.proto",
    ];

    let proto_paths: Vec<PathBuf> = proto_files
        .iter()
        .map(|f| proto_root.join(f))
        .collect();

    prost_build::Config::new()
        .compile_protos(&proto_paths, &[&proto_root])?;

    Ok(())
}
