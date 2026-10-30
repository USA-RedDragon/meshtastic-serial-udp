//! Find a Meshtastic radio by probing the serial ports on the system.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serialport::SerialPort;

use crate::serial::{self, ProbeTimeouts, RadioInfo};

/// A serial device to probe. `alias` is the /dev/serial/by-id link that pointed to it, if any.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub device: PathBuf,
    pub alias: Option<PathBuf>,
}

impl Candidate {
    fn describe(&self) -> String {
        match &self.alias {
            Some(alias) => format!("{} ({})", self.device.display(), alias.display()),
            None => self.device.display().to_string(),
        }
    }
}

/// A port that answered the Meshtastic handshake, still open.
pub struct Found {
    pub port: Box<dyn SerialPort>,
    pub info: RadioInfo,
}

/// Probe the system's serial ports and return the first one that answers the handshake.
pub fn detect(baud: u32, serial_timeout: Duration) -> Option<Found> {
    let candidates = system_candidates();
    let busy = busy_devices(Path::new("/proc"), std::process::id(), &candidates);
    scan(
        &candidates,
        &busy,
        |path| {
            serialport::new(path.to_string_lossy(), baud)
                .timeout(serial_timeout)
                .open()
        },
        serial::PROBE_TIMEOUTS,
    )
}

#[cfg(target_os = "linux")]
fn system_candidates() -> Vec<Candidate> {
    candidates(Path::new("/dev"))
}

#[cfg(not(target_os = "linux"))]
fn system_candidates() -> Vec<Candidate> {
    serialport::available_ports()
        .map(|ports| {
            ports
                .into_iter()
                .map(|p| Candidate {
                    device: PathBuf::from(p.port_name),
                    alias: None,
                })
                .collect()
        })
        .unwrap_or_default()
}

/// List serial devices under `dev` in probe order: the targets of
/// `serial/by-id/*`, then `ttyACM*`, then `ttyUSB*`, each sorted naturally
/// (ttyACM2 before ttyACM10). A device reached by more than one path is listed once.
pub fn candidates(dev: &Path) -> Vec<Candidate> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for alias in sorted_entries(&dev.join("serial/by-id"), |_| true) {
        let Ok(device) = fs::canonicalize(&alias) else {
            continue;
        };
        if seen.insert(device.clone()) {
            out.push(Candidate {
                device,
                alias: Some(alias),
            });
        }
    }
    for prefix in ["ttyACM", "ttyUSB"] {
        let numbered = |name: &str| {
            name.strip_prefix(prefix)
                .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
        };
        for path in sorted_entries(dev, numbered) {
            let device = fs::canonicalize(&path).unwrap_or(path);
            if seen.insert(device.clone()) {
                out.push(Candidate {
                    device,
                    alias: None,
                });
            }
        }
    }
    out
}

fn sorted_entries(dir: &Path, keep: impl Fn(&str) -> bool) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| keep(n))
        .collect();
    names.sort_by(|a, b| natural_key(a).cmp(&natural_key(b)));
    names.into_iter().map(|n| dir.join(n)).collect()
}

fn natural_key(name: &str) -> (&str, Option<u64>, &str) {
    let digits = name.bytes().rev().take_while(u8::is_ascii_digit).count();
    let (stem, number) = name.split_at(name.len() - digits);
    (stem, number.parse().ok(), name)
}

/// Map each candidate device that some other process has open to that process's PID,
/// by reading the `fd` links under `proc_root`. Unreadable entries are ignored.
pub fn busy_devices(
    proc_root: &Path,
    own_pid: u32,
    candidates: &[Candidate],
) -> HashMap<PathBuf, u32> {
    let wanted: HashSet<&Path> = candidates.iter().map(|c| c.device.as_path()).collect();
    let mut busy = HashMap::new();
    let Ok(procs) = fs::read_dir(proc_root) else {
        return busy;
    };
    for entry in procs.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<u32>().ok())
        else {
            continue;
        };
        if pid == own_pid {
            continue;
        }
        let Ok(fds) = fs::read_dir(entry.path().join("fd")) else {
            continue;
        };
        for fd in fds.flatten() {
            if let Ok(target) = fs::read_link(fd.path())
                && wanted.contains(target.as_path())
            {
                busy.entry(target).or_insert(pid);
            }
        }
    }
    busy
}

/// Try each candidate in order and return the first that answers the handshake.
/// Skips devices in `busy` without opening them.
pub fn scan<O>(
    candidates: &[Candidate],
    busy: &HashMap<PathBuf, u32>,
    mut open: O,
    timeouts: ProbeTimeouts,
) -> Option<Found>
where
    O: FnMut(&Path) -> serialport::Result<Box<dyn SerialPort>>,
{
    if candidates.is_empty() {
        log::warn!(
            "no serial ports found (looked for /dev/serial/by-id/*, /dev/ttyACM*, /dev/ttyUSB*)"
        );
        return None;
    }
    for candidate in candidates {
        let name = candidate.describe();
        if let Some(pid) = busy.get(&candidate.device) {
            log::info!("skipping {name}: already open by PID {pid}");
            continue;
        }
        log::info!("probing {name} for a Meshtastic radio");
        let mut port = match open(&candidate.device) {
            Ok(port) => port,
            Err(e) => {
                log::info!("skipping {name}: cannot open: {e}");
                continue;
            }
        };
        match serial::probe(&mut *port, timeouts) {
            Ok(info) => {
                log::info!("using {name}: it answered the Meshtastic handshake");
                return Some(Found { port, info });
            }
            Err(e) => log::info!("skipping {name}: {e}"),
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::meshtastic_proto;
    use crate::serial_framing::{self, FrameReader};
    use crate::test_util::MockSerialPort;
    use prost::Message;
    use std::io::{Read as _, Write as _};
    use std::os::unix::fs::symlink;

    const FAST: ProbeTimeouts = ProbeTimeouts {
        first_frame: Duration::from_millis(300),
        resend: Duration::from_millis(100),
        total: Duration::from_secs(2),
    };

    fn touch(path: &Path) {
        fs::write(path, b"").unwrap();
    }

    fn fake_radio(node: u32) -> MockSerialPort {
        MockSerialPort::with_responder(move |written| {
            let mut out = Vec::new();
            for frame in FrameReader::new().feed_bytes(written) {
                let Ok(to_radio) = meshtastic_proto::ToRadio::decode(frame.as_slice()) else {
                    continue;
                };
                if let Some(meshtastic_proto::to_radio::PayloadVariant::WantConfigId(id)) =
                    to_radio.payload_variant
                {
                    use meshtastic_proto::from_radio::PayloadVariant as V;
                    for variant in [
                        V::MyInfo(meshtastic_proto::MyNodeInfo {
                            my_node_num: node,
                            ..Default::default()
                        }),
                        V::ConfigCompleteId(id),
                    ] {
                        let fr = meshtastic_proto::FromRadio {
                            id: 0,
                            payload_variant: Some(variant),
                        };
                        out.extend(serial_framing::frame_payload(&fr.encode_to_vec()));
                    }
                }
            }
            out
        })
    }

    fn cand(device: &str) -> Candidate {
        Candidate {
            device: PathBuf::from(device),
            alias: None,
        }
    }

    #[test]
    fn test_candidates_order_and_dedupe() {
        let dir = tempfile::tempdir().unwrap();
        let dev = fs::canonicalize(dir.path()).unwrap();
        for name in [
            "ttyACM10", "ttyACM2", "ttyUSB1", "ttyUSB0", "ttyACM0", "ttyS0", "ttyACM", "ttyUSBx",
        ] {
            touch(&dev.join(name));
        }
        let by_id = dev.join("serial/by-id");
        fs::create_dir_all(&by_id).unwrap();
        symlink("../../ttyUSB1", by_id.join("usb-b_radio-if00-port0")).unwrap();
        symlink("../../ttyACM10", by_id.join("usb-a_radio-if00")).unwrap();
        symlink("../../ttyACM10", by_id.join("usb-a_radio_again-if00")).unwrap();
        symlink("../../gone", by_id.join("usb-dangling")).unwrap();

        let got = candidates(&dev);
        let expected = vec![
            Candidate {
                device: dev.join("ttyACM10"),
                alias: Some(by_id.join("usb-a_radio-if00")),
            },
            Candidate {
                device: dev.join("ttyUSB1"),
                alias: Some(by_id.join("usb-b_radio-if00-port0")),
            },
            Candidate {
                device: dev.join("ttyACM0"),
                alias: None,
            },
            Candidate {
                device: dev.join("ttyACM2"),
                alias: None,
            },
            Candidate {
                device: dev.join("ttyUSB0"),
                alias: None,
            },
        ];
        assert_eq!(got, expected);
    }

    #[test]
    fn test_candidates_empty_dev() {
        let dir = tempfile::tempdir().unwrap();
        assert!(candidates(dir.path()).is_empty());
        assert!(candidates(&dir.path().join("missing")).is_empty());
    }

    #[test]
    fn test_natural_key() {
        let mut names = vec!["ttyACM10", "ttyACM9", "ttyACM1", "ttyACM"];
        names.sort_by(|a, b| natural_key(a).cmp(&natural_key(b)));
        assert_eq!(names, vec!["ttyACM", "ttyACM1", "ttyACM9", "ttyACM10"]);
    }

    #[test]
    fn test_busy_devices() {
        let dir = tempfile::tempdir().unwrap();
        let proc_root = dir.path();
        let acm0 = cand("/dev/ttyACM0");
        let usb0 = cand("/dev/ttyUSB0");
        for (pid, target) in [
            ("100", "/dev/ttyACM0"),
            ("200", "/dev/null"),
            ("300", "/dev/ttyUSB0"),
        ] {
            let fd = proc_root.join(pid).join("fd");
            fs::create_dir_all(&fd).unwrap();
            symlink(target, fd.join("3")).unwrap();
        }
        fs::create_dir_all(proc_root.join("self")).unwrap();
        let busy = busy_devices(proc_root, 300, &[acm0.clone(), usb0.clone()]);
        assert_eq!(busy.len(), 1);
        assert_eq!(busy.get(&acm0.device), Some(&100));
    }

    #[test]
    fn test_scan_picks_first_answering_port() {
        let candidates = [
            cand("/dev/busy"),
            cand("/dev/missing"),
            cand("/dev/silent"),
            cand("/dev/echo"),
            cand("/dev/radio1"),
            cand("/dev/radio2"),
        ];
        let busy = HashMap::from([(PathBuf::from("/dev/busy"), 42)]);
        let mut opened = Vec::new();
        let found = scan(
            &candidates,
            &busy,
            |path| {
                opened.push(path.to_path_buf());
                match path.to_str().unwrap() {
                    "/dev/missing" => Err(serialport::Error::new(
                        serialport::ErrorKind::NoDevice,
                        "no such device",
                    )),
                    "/dev/silent" => Ok(Box::new(MockSerialPort::new())),
                    "/dev/echo" => Ok(Box::new(MockSerialPort::with_responder(|w| w.to_vec()))),
                    "/dev/radio1" => Ok(Box::new(fake_radio(0x1111))),
                    "/dev/radio2" => Ok(Box::new(fake_radio(0x2222))),
                    other => panic!("unexpected open of {other}"),
                }
            },
            FAST,
        )
        .expect("radio1 should be found");
        assert_eq!(found.info.my_node_num, 0x1111);
        assert_eq!(
            opened,
            ["/dev/missing", "/dev/silent", "/dev/echo", "/dev/radio1"]
                .map(PathBuf::from)
                .to_vec()
        );
    }

    #[test]
    fn test_scan_none_answer() {
        let candidates = [cand("/dev/silent")];
        let found = scan(
            &candidates,
            &HashMap::new(),
            |_| Ok(Box::new(MockSerialPort::new())),
            FAST,
        );
        assert!(found.is_none());
        assert!(scan(&[], &HashMap::new(), |_| unreachable!(), FAST).is_none());
    }

    #[test]
    fn test_probe_answers_after_resend() {
        let radio = fake_radio(0x3333);
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let late = MockSerialPort::with_responder(move |w| {
            if calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                return Vec::new();
            }
            let mut port = radio.try_clone().unwrap();
            port.write_all(w).unwrap();
            let mut out = vec![0u8; port.bytes_to_read().unwrap() as usize];
            port.read_exact(&mut out).unwrap();
            out
        });
        let mut port: Box<dyn SerialPort> = Box::new(late);
        let info = serial::probe(&mut *port, FAST).expect("radio answers the resent request");
        assert_eq!(info.my_node_num, 0x3333);
    }

    #[test]
    fn test_probe_rejects_chatty_device() {
        let chatty =
            MockSerialPort::with_responder(|_| b"AT\r\nOK\r\n\x94\xc3\x00\x02\xff\xff".to_vec());
        let mut port: Box<dyn SerialPort> = Box::new(chatty);
        let err = serial::probe(&mut *port, FAST).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
    }

    #[test]
    fn test_probe_only_writes_want_config() {
        let mock = MockSerialPort::new();
        let mut port: Box<dyn SerialPort> = mock.try_clone().unwrap();
        let err = serial::probe(&mut *port, FAST).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
        let frames = FrameReader::new().feed_bytes(&mock.take_written());
        assert!(frames.len() >= 2, "should resend while the port is silent");
        assert!(
            frames.windows(2).all(|w| w[0] == w[1]),
            "every write is the same request"
        );
        let to_radio = meshtastic_proto::ToRadio::decode(frames[0].as_slice()).unwrap();
        assert!(matches!(
            to_radio.payload_variant,
            Some(meshtastic_proto::to_radio::PayloadVariant::WantConfigId(_))
        ));
    }
}
