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

use std::net::{Ipv4Addr, UdpSocket};
use std::process;
use std::time::Duration;

use clap::Parser;

use bridge::{Bridge, BridgeConfig, Ingress, SessionEnd};

const SERIAL_TIMEOUT: Duration = Duration::from_millis(500);
const RECONNECT_DELAY: Duration = Duration::from_secs(2);
const MAX_REOPEN_DELAY: Duration = Duration::from_secs(30);
const MQTT_PROXY_ONLY_ADDRESS: &str = "127.0.0.1";
const MQTT_REBOOT_TIMEOUT: Duration = Duration::from_secs(30);

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

    #[arg(
        long,
        value_enum,
        default_value_t = Ingress::MqttProxy,
        help = "How packets from the LAN are handed to the radio: mqtt-proxy keeps each packet's original sender, phone makes the radio re-send them as its own messages"
    )]
    ingress: Ingress,
}

#[derive(clap::ValueEnum, Clone, Debug)]
enum Platform {
    #[value(name = "openwrt")]
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

    let mut ingress = cli.ingress;
    let mut mqtt_setup_attempted = false;
    let mut serial: Option<Box<dyn serialport::SerialPort>> = None;
    loop {
        let port = match serial.take() {
            Some(port) => port,
            None => open_serial(&cli),
        };
        match run_session(&cli, port, &udp_socket, &mut ingress, &mut mqtt_setup_attempted) {
            Ok(SessionEnd::RadioRebooted(port)) => serial = Some(port),
            Ok(SessionEnd::SerialLost(e)) => {
                log::error!("serial connection lost: {e}; reconnecting");
                std::thread::sleep(RECONNECT_DELAY);
            }
            Ok(SessionEnd::UdpFailed(e)) => {
                log::error!("bridge exited with UDP error: {e}");
                process::exit(1);
            }
            Err(e) => {
                log::error!("radio session failed: {e}; reconnecting");
                std::thread::sleep(RECONNECT_DELAY);
            }
        }
    }
}

fn open_serial(cli: &Cli) -> Box<dyn serialport::SerialPort> {
    let mut delay = RECONNECT_DELAY;
    loop {
        log::info!("opening serial port {} at {} baud", cli.port, cli.baud);
        match serialport::new(&cli.port, cli.baud)
            .timeout(SERIAL_TIMEOUT)
            .open()
        {
            Ok(port) => return port,
            Err(e) => {
                log::error!("failed to open serial port {}: {e}; retrying in {delay:?}", cli.port);
                std::thread::sleep(delay);
                delay = (delay * 2).min(MAX_REOPEN_DELAY);
            }
        }
    }
}

fn run_session(
    cli: &Cli,
    mut serial: Box<dyn serialport::SerialPort>,
    udp_socket: &UdpSocket,
    ingress: &mut Ingress,
    mqtt_setup_attempted: &mut bool,
) -> std::io::Result<SessionEnd> {
    log::info!("performing serial handshake...");
    let info = serial::handshake(&mut *serial)?;
    if info.region == meshtastic_proto::config::lo_ra_config::RegionCode::Unset as i32 {
        log::error!(
            "the radio has no LoRa region set, so it will not transmit or receive anything; \
             set one with the Meshtastic app or CLI (e.g. `meshtastic --set lora.region US`)"
        );
    }
    let mut channels = info.channel_keys();
    let mut to_install = Vec::new();

    if matches!(cli.platform, Platform::OpenWrt) {
        match raven::load_raven_channels() {
            Ok(Some(raven_channels)) => {
                log::info!("loaded {} channel(s) from raven.conf", raven_channels.len());
                let merge_result =
                    raven::merge_channels(&channels, &raven_channels, info.modem_preset);
                to_install = merge_result.to_install;
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

    if *ingress == Ingress::MqttProxy {
        for ch in &info.channels {
            let downlink = ch.settings.as_ref().is_some_and(|s| s.downlink_enabled);
            if !downlink && !to_install.iter().any(|c| c.index == ch.index) {
                to_install.push(ch.clone());
            }
        }
        for ch in &mut to_install {
            if let Some(settings) = ch.settings.as_mut() {
                settings.downlink_enabled = true;
            }
        }
    }

    for ch in to_install {
        log::info!("installing channel {} on device", ch.index);
        serial::send_set_channel(&mut *serial, info.my_node_num, ch)?;
        std::thread::sleep(Duration::from_millis(100));
    }

    if *ingress == Ingress::MqttProxy {
        let mqtt = info.mqtt.clone().unwrap_or_default();
        if !(mqtt.enabled && mqtt.proxy_to_client_enabled) {
            if *mqtt_setup_attempted {
                log::error!(
                    "radio did not keep the MQTT client proxy enabled (firmware built without MQTT?); \
                     falling back to --ingress phone: LAN packets will be re-sent as this radio's own messages"
                );
                *ingress = Ingress::Phone;
            } else {
                *mqtt_setup_attempted = true;
                log::warn!(
                    "enabling the MQTT client proxy on the radio so LAN packets keep their original sender; \
                     the radio will reboot"
                );
                let mut mqtt = meshtastic_proto::module_config::MqttConfig {
                    enabled: true,
                    proxy_to_client_enabled: true,
                    ..mqtt
                };
                if !info.mqtt.as_ref().is_some_and(|m| m.enabled) && mqtt.address.is_empty() {
                    mqtt.address = MQTT_PROXY_ONLY_ADDRESS.to_string();
                }
                serial::send_set_mqtt_config(&mut *serial, info.my_node_num, mqtt)?;
                match serial::wait_for_reboot(&mut *serial, MQTT_REBOOT_TIMEOUT) {
                    Ok(true) => return Ok(SessionEnd::RadioRebooted(serial)),
                    Ok(false) => {
                        log::error!(
                            "radio did not reboot after enabling the MQTT client proxy; \
                             falling back to --ingress phone"
                        );
                        *ingress = Ingress::Phone;
                    }
                    Err(e) => return Ok(SessionEnd::SerialLost(e)),
                }
            }
        }
    }

    let config = BridgeConfig {
        multicast_addr: cli.udp_addr,
        udp_port: cli.udp_port,
        ingress: *ingress,
        my_node_num: info.my_node_num,
    };
    let bridge = Bridge::new(serial, udp_socket.try_clone()?, config, channels);
    log::info!("bridge running (ingress: {:?})", *ingress);
    bridge.run()
}
