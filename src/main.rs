mod udp;

mod meshtastic_proto {
    #![allow(dead_code)]
    include!(concat!(env!("OUT_DIR"), "/meshtastic.rs"));
}

use std::net::Ipv4Addr;
use std::process;

use clap::Parser;

#[derive(Parser)]
#[command(name = "meshtastic-serial-udp")]
#[command(about = "Bridge a USB-serial Meshtastic radio to UDP multicast (Meshtastic over LAN)")]
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
}

fn main() {
    env_logger::init();
    let cli = Cli::parse();

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
}
