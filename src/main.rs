mod bridge;
mod crypto;
mod raven;
mod serial;
mod serial_framing;
mod udp;

#[cfg(test)]
pub(crate) mod test_util;

mod meshtastic_proto {
    #![allow(dead_code)]
    include!(concat!(env!("OUT_DIR"), "/meshtastic.rs"));
}

use std::net::Ipv4Addr;
use std::process;
use std::time::Duration;

use clap::Parser;
use bridge::{Bridge, BridgeConfig};

#[derive(Parser)]
#[command(name = "meshtastic-serial-udp")]
#[command(about = "Bridge a USB-serial Meshtastic radio to UDP multicast (Meshtastic over LAN)")]
#[command(version = concat!(env!("PKG_VERSION"), " (", env!("GIT_COMMIT"), ")"))]
struct Cli {
    /// Serial port path (e.g. /dev/ttyUSB0 or COM3)
    #[arg(short, long)]
    port: String,

    /// Serial baud rate
    #[arg(short, long, default_value_t = 115200)]
    baud: u32,

    /// UDP multicast address
    #[arg(long, default_value = "224.0.0.69")]
    udp_addr: Ipv4Addr,

    /// UDP multicast port
    #[arg(long, default_value_t = 4403)]
    udp_port: u16,

    /// Network interface IP to bind multicast socket to (optional)
    #[arg(long)]
    interface: Option<Ipv4Addr>,

    /// Platform
    #[arg(long, default_value = "other")]
    platform: Platform,
}

#[derive(clap::ValueEnum, Clone, Debug)]
enum Platform {
    OpenWrt,
    Other,
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let cli = Cli::parse();

    log::info!(
        "meshtastic-serial-udp v{} ({})",
        env!("PKG_VERSION"),
        env!("GIT_COMMIT"),
    );

    log::info!(
        "opening serial port {} at {} baud",
        cli.port,
        cli.baud
    );

    let serial = match serialport::new(&cli.port, cli.baud)
        .timeout(Duration::from_millis(500))
        .open()
    {
        Ok(port) => port,
        Err(e) => {
            log::error!("failed to open serial port {}: {e}", cli.port);
            process::exit(1);
        }
    };

    log::info!(
        "setting up UDP multicast on {}:{}",
        cli.udp_addr,
        cli.udp_port
    );

    let udp_socket = match udp::setup_multicast_socket(cli.udp_addr, cli.udp_port, cli.interface) {
        Ok(s) => s,
        Err(e) => {
            log::error!("failed to setup UDP multicast socket: {e}");
            process::exit(1);
        }
    };

    let config = BridgeConfig {
        multicast_addr: cli.udp_addr,
        udp_port: cli.udp_port,
    };

    log::info!("performing serial handshake...");
    let mut serial = serial;
    let (my_node_num, modem_preset, mut channels) = match serial::handshake(&mut *serial) {
        Ok(result) => result,
        Err(e) => {
            log::error!("handshake failed: {e}");
            process::exit(1);
        }
    };

    if matches!(cli.platform, Platform::OpenWrt) {
        match raven::load_raven_channels() {
            Ok(Some(raven_channels)) => {
                log::info!("loaded {} channel(s) from raven.conf", raven_channels.len());
                let merge_result = raven::merge_channels(&channels, &raven_channels, modem_preset);
                for ch in &merge_result.to_install {
                    log::info!("installing channel {} on device", ch.index);
                    if let Err(e) = serial::send_set_channel(&mut *serial, my_node_num, ch.clone()) {
                        log::error!("failed to install channel {}: {e}", ch.index);
                        process::exit(1);
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                channels = merge_result.channels;
            }
            Ok(None) => {
                log::info!("raven.conf not found, using device channels only");
            }
            Err(e) => {
                log::warn!("failed to read raven.conf: {e}, using device channels only");
            }
        }
    }

    let bridge = Bridge::new(serial, udp_socket, config, channels);

    log::info!("bridge running");
    if let Err(e) = bridge.run() {
        log::error!("bridge exited with error: {e}");
        process::exit(1);
    }
}
