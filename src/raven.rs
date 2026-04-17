use std::io;
use std::path::Path;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde::Deserialize;

use crate::crypto::{self, ChannelKey};
use crate::meshtastic_proto;

const RAVEN_CONF_PATH: &str = "/usr/local/raven/raven.conf";
const RAVEN_CONF_OVERRIDE_PATH: &str = "/usr/local/raven/raven.conf.override";

#[derive(Deserialize)]
struct RavenConfig {
    #[serde(default)]
    channels: Vec<RavenChannel>,
}

#[derive(Deserialize)]
struct RavenChannel {
    namekey: String,
}

/// A parsed raven channel: name and raw PSK bytes (before expansion).
pub struct ParsedRavenChannel {
    pub name: String,
    pub psk: Vec<u8>,
}

/// Deep-merge `override_val` into `base`, mirroring Raven's config.uc merge
/// semantics: objects merge recursively, null deletes the key, and everything
/// else (arrays, strings, numbers, booleans) replaces entirely.
fn deep_merge(base: &mut serde_json::Value, override_val: &serde_json::Value) {
    let (Some(base_obj), Some(over_obj)) = (base.as_object_mut(), override_val.as_object()) else {
        return;
    };
    for (k, v) in over_obj {
        if v.is_null() {
            base_obj.remove(k);
        } else if v.is_object() {
            let entry = base_obj
                .entry(k.clone())
                .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
            if !entry.is_object() {
                *entry = serde_json::Value::Object(serde_json::Map::new());
            }
            deep_merge(entry, v);
        } else {
            base_obj.insert(k.clone(), v.clone());
        }
    }
}

/// Try to load and parse raven channels from the default paths.
/// Reads raven.conf, then deep-merges raven.conf.override on top if it exists.
/// Returns None if the base file doesn't exist. Returns Err on parse failures.
pub fn load_raven_channels() -> io::Result<Option<Vec<ParsedRavenChannel>>> {
    load_raven_channels_with_override(RAVEN_CONF_PATH, RAVEN_CONF_OVERRIDE_PATH)
}

/// Load and parse raven channels from a base path, with an optional override
/// file that is deep-merged on top (following Raven's config.uc semantics).
fn load_raven_channels_with_override<P: AsRef<Path>, Q: AsRef<Path>>(
    base_path: P,
    override_path: Q,
) -> io::Result<Option<Vec<ParsedRavenChannel>>> {
    let base_content = match std::fs::read_to_string(base_path) {
        Ok(c) => c,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let mut config_value: serde_json::Value = serde_json::from_str(&base_content)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

    // Apply override if present (mirroring Raven: missing override is silently ignored).
    match std::fs::read_to_string(override_path) {
        Ok(override_content) => {
            let override_value: serde_json::Value =
                serde_json::from_str(&override_content)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            if override_value.is_object() {
                deep_merge(&mut config_value, &override_value);
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }

    let config: RavenConfig = serde_json::from_value(config_value)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

    let mut channels = Vec::with_capacity(config.channels.len());
    for ch in &config.channels {
        let parsed = parse_namekey(&ch.namekey)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        channels.push(parsed);
    }
    Ok(Some(channels))
}

/// Load and parse raven channels from a single path (no override).
#[cfg(test)]
fn load_raven_channels_from<P: AsRef<Path>>(path: P) -> io::Result<Option<Vec<ParsedRavenChannel>>> {
    load_raven_channels_with_override(path, "/nonexistent-no-override")
}

/// Parse a "name base64psk" namekey string into name and raw PSK bytes.
fn parse_namekey(namekey: &str) -> Result<ParsedRavenChannel, String> {
    let (name, b64) = namekey
        .rsplit_once(' ')
        .ok_or_else(|| format!("invalid namekey (no space separator): {namekey:?}"))?;
    let psk = BASE64
        .decode(b64)
        .map_err(|e| format!("invalid base64 in namekey {namekey:?}: {e}"))?;
    Ok(ParsedRavenChannel {
        name: name.to_string(),
        psk,
    })
}

/// Result of merging device channels with raven channels.
pub struct MergeResult {
    /// The final merged channel list for the bridge to use.
    pub channels: Vec<ChannelKey>,
    /// Protobuf Channel messages that need to be installed on the device
    /// (new channels or channels with updated PSKs).
    pub to_install: Vec<meshtastic_proto::Channel>,
}

/// Map a modem_preset integer (from LoRaConfig) to the default channel name
/// the firmware uses when the primary channel's name field is empty.
pub fn modem_preset_name(modem_preset: i32) -> &'static str {
    match modem_preset {
        0 => "LongFast",
        1 => "LongSlow",
        2 => "VeryLongSlow",
        3 => "MediumSlow",
        4 => "MediumFast",
        5 => "ShortSlow",
        6 => "ShortFast",
        7 => "LongModerate",
        8 => "ShortTurbo",
        9 => "LongTurbo",
        _ => "LongFast",
    }
}

/// Merge device-reported channels with raven-sourced channels.
/// Duplicates (by name) prefer the raven version's PSK.
/// New raven channels are assigned as SECONDARY at the next available index.
/// `modem_preset` is used to resolve the default primary channel name when the
/// device reports it as empty (which is standard Meshtastic behavior).
pub fn merge_channels(
    device_channels: &[ChannelKey],
    raven_channels: &[ParsedRavenChannel],
    modem_preset: i32,
) -> MergeResult {
    let mut merged: Vec<ChannelKey> = device_channels.to_vec();
    let mut to_install: Vec<meshtastic_proto::Channel> = Vec::new();

    // Track which device channel indices are in use.
    let mut max_index = device_channels.iter().map(|c| c.index).max().unwrap_or(0);

    let default_name = modem_preset_name(modem_preset);

    for rc in raven_channels {
        let effective = |c: &ChannelKey| -> bool {
            if c.name == rc.name {
                return true;
            }
            // Device reports primary channel (index 0) with empty name;
            // match it against the modem preset's default name.
            c.name.is_empty() && c.index == 0 && rc.name == default_name
        };
        if let Some(existing) = merged.iter_mut().find(|c| effective(c)) {
            // Duplicate — update PSK from raven, recompute key and hash.
            let new_ck = crypto::channel_key_from_proto(existing.index, &rc.name, &rc.psk);
            if existing.key != new_ck.key {
                log::info!(
                    "raven: updating channel {:?} (index {}) with raven PSK",
                    rc.name,
                    existing.index,
                );
                // Determine the role: keep whatever the device had.
                let role = if existing.index == 0 { 1 } else { 2 }; // 1=PRIMARY, 2=SECONDARY
                to_install.push(make_channel_proto(
                    existing.index as i32,
                    role,
                    &rc.name,
                    &rc.psk,
                ));
                *existing = new_ck;
            }
        } else {
            // New channel from raven — assign next index as SECONDARY.
            max_index += 1;
            let ck = crypto::channel_key_from_proto(max_index, &rc.name, &rc.psk);
            log::info!(
                "raven: adding new channel {:?} at index {} (SECONDARY)",
                rc.name,
                max_index,
            );
            to_install.push(make_channel_proto(
                max_index as i32,
                2, // SECONDARY
                &rc.name,
                &rc.psk,
            ));
            merged.push(ck);
        }
    }

    MergeResult {
        channels: merged,
        to_install,
    }
}

fn make_channel_proto(index: i32, role: i32, name: &str, psk: &[u8]) -> meshtastic_proto::Channel {
    meshtastic_proto::Channel {
        index,
        role,
        settings: Some(meshtastic_proto::ChannelSettings {
            psk: psk.to_vec(),
            name: name.to_string(),
            ..Default::default()
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    const SAMPLE_RAVEN_CONF: &str = r#"{
        "debug": 0,
        "role": "client_mute",
        "channels": [
            { "namekey": "AREDN og==", "telemetry": true, "meshtastic": true },
            { "namekey": "LongFast AQ==", "telemetry": true, "meshtastic": true },
            { "namekey": "MeshCore izOH6cXN6mrJ5e26oRXNcg==" },
            { "namekey": "OK-WX Uw==", "telemetry": true, "meshtastic": true },
            { "namekey": "PMesh DcYgnMhTKG64bXt+n8gLCzK57IpEhRnhYwpL4xowu9Y=", "telemetry": true, "meshtastic": true }
        ]
    }"#;

    #[test]
    fn test_parse_namekey_simple() {
        let parsed = parse_namekey("LongFast AQ==").unwrap();
        assert_eq!(parsed.name, "LongFast");
        assert_eq!(parsed.psk, vec![0x01]); // AQ== = [1]
    }

    #[test]
    fn test_parse_namekey_16_byte_psk() {
        let parsed = parse_namekey("MeshCore izOH6cXN6mrJ5e26oRXNcg==").unwrap();
        assert_eq!(parsed.name, "MeshCore");
        assert_eq!(parsed.psk.len(), 16);
    }

    #[test]
    fn test_parse_namekey_32_byte_psk() {
        let parsed =
            parse_namekey("PMesh DcYgnMhTKG64bXt+n8gLCzK57IpEhRnhYwpL4xowu9Y=").unwrap();
        assert_eq!(parsed.name, "PMesh");
        assert_eq!(parsed.psk.len(), 32);
    }

    #[test]
    fn test_parse_namekey_short_psk() {
        // og== decodes to [0xa2] — 1 byte
        let parsed = parse_namekey("AREDN og==").unwrap();
        assert_eq!(parsed.name, "AREDN");
        assert_eq!(parsed.psk, vec![0xa2]);
    }

    #[test]
    fn test_parse_namekey_no_space() {
        assert!(parse_namekey("nospace").is_err());
    }

    #[test]
    fn test_parse_namekey_invalid_base64() {
        assert!(parse_namekey("name !!!invalid!!!").is_err());
    }

    #[test]
    fn test_load_raven_channels_missing_file() {
        let result = load_raven_channels_from("/nonexistent/raven.conf").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_load_raven_channels_valid() {
        let mut tmp = NamedTempFile::new().unwrap();
        tmp.write_all(SAMPLE_RAVEN_CONF.as_bytes()).unwrap();
        let result = load_raven_channels_from(tmp.path()).unwrap();
        let channels = result.unwrap();
        assert_eq!(channels.len(), 5);
        assert_eq!(channels[0].name, "AREDN");
        assert_eq!(channels[1].name, "LongFast");
        assert_eq!(channels[2].name, "MeshCore");
        assert_eq!(channels[3].name, "OK-WX");
        assert_eq!(channels[4].name, "PMesh");
    }

    #[test]
    fn test_load_raven_channels_invalid_json() {
        let mut tmp = NamedTempFile::new().unwrap();
        tmp.write_all(b"not json").unwrap();
        assert!(load_raven_channels_from(tmp.path()).is_err());
    }

    #[test]
    fn test_merge_no_raven() {
        let device = vec![crypto::channel_key_from_proto(0, "LongFast", &[1])];
        let result = merge_channels(&device, &[], 0);
        assert_eq!(result.channels.len(), 1);
        assert!(result.to_install.is_empty());
    }

    #[test]
    fn test_merge_duplicate_same_psk() {
        let device = vec![crypto::channel_key_from_proto(0, "LongFast", &[1])];
        let raven = vec![ParsedRavenChannel {
            name: "LongFast".to_string(),
            psk: vec![1],
        }];
        let result = merge_channels(&device, &raven, 0);
        assert_eq!(result.channels.len(), 1);
        // Same PSK, no install needed.
        assert!(result.to_install.is_empty());
    }

    #[test]
    fn test_merge_duplicate_different_psk() {
        let device = vec![crypto::channel_key_from_proto(0, "LongFast", &[1])];
        let raven = vec![ParsedRavenChannel {
            name: "LongFast".to_string(),
            psk: vec![2],
        }];
        let result = merge_channels(&device, &raven, 0);
        assert_eq!(result.channels.len(), 1);
        assert_eq!(result.to_install.len(), 1);
        assert_eq!(result.to_install[0].index, 0);
        assert_eq!(result.to_install[0].role, 1); // PRIMARY kept
    }

    #[test]
    fn test_merge_new_channel() {
        let device = vec![crypto::channel_key_from_proto(0, "LongFast", &[1])];
        let raven = vec![ParsedRavenChannel {
            name: "AREDN".to_string(),
            psk: vec![0xa2],
        }];
        let result = merge_channels(&device, &raven, 0);
        assert_eq!(result.channels.len(), 2);
        assert_eq!(result.channels[1].name, "AREDN");
        assert_eq!(result.channels[1].index, 1);
        assert_eq!(result.to_install.len(), 1);
        assert_eq!(result.to_install[0].index, 1);
        assert_eq!(result.to_install[0].role, 2); // SECONDARY
    }

    #[test]
    fn test_merge_mixed() {
        let device = vec![
            crypto::channel_key_from_proto(0, "LongFast", &[1]),
            crypto::channel_key_from_proto(1, "AREDN", &[0xa2]),
        ];
        let raven = vec![
            ParsedRavenChannel {
                name: "AREDN".to_string(),
                psk: vec![0xa3], // Different PSK
            },
            ParsedRavenChannel {
                name: "NewChan".to_string(),
                psk: vec![5],
            },
        ];
        let result = merge_channels(&device, &raven, 0);
        assert_eq!(result.channels.len(), 3);
        // AREDN updated at index 1
        assert_eq!(result.channels[1].name, "AREDN");
        assert_eq!(result.channels[1].index, 1);
        // NewChan added at index 2
        assert_eq!(result.channels[2].name, "NewChan");
        assert_eq!(result.channels[2].index, 2);
        assert_eq!(result.to_install.len(), 2);
    }

    #[test]
    fn test_merge_empty_name_primary_matches_preset() {
        // Device reports primary channel with empty name (standard Meshtastic behavior)
        let device = vec![crypto::channel_key_from_proto(0, "", &[1])];
        let raven = vec![ParsedRavenChannel {
            name: "LongFast".to_string(),
            psk: vec![1],
        }];
        // modem_preset=0 (LONG_FAST) → default name "LongFast"
        let result = merge_channels(&device, &raven, 0);
        assert_eq!(result.channels.len(), 1, "should not duplicate");
        assert!(result.to_install.is_empty(), "same PSK, no install needed");
    }

    #[test]
    fn test_merge_empty_name_primary_wrong_preset_no_match() {
        // Device reports primary channel with empty name, but modem preset is ShortFast
        let device = vec![crypto::channel_key_from_proto(0, "", &[1])];
        let raven = vec![ParsedRavenChannel {
            name: "LongFast".to_string(),
            psk: vec![1],
        }];
        // modem_preset=6 (SHORT_FAST) → default name "ShortFast", not "LongFast"
        let result = merge_channels(&device, &raven, 6);
        assert_eq!(result.channels.len(), 2, "should add LongFast as new channel");
        assert_eq!(result.to_install.len(), 1);
    }

    // --- deep_merge tests ---

    #[test]
    fn test_deep_merge_replaces_array() {
        let mut base = serde_json::json!({
            "channels": [
                { "namekey": "A aa==" },
                { "namekey": "B bb==" }
            ]
        });
        let over = serde_json::json!({
            "channels": [
                { "namekey": "C cc==" }
            ]
        });
        deep_merge(&mut base, &over);
        let arr = base["channels"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["namekey"], "C cc==");
    }

    #[test]
    fn test_deep_merge_null_deletes_key() {
        let mut base = serde_json::json!({
            "debug": 1,
            "role": "client_mute"
        });
        let over = serde_json::json!({ "debug": null });
        deep_merge(&mut base, &over);
        assert!(base.get("debug").is_none());
        assert_eq!(base["role"], "client_mute");
    }

    #[test]
    fn test_deep_merge_objects_recurse() {
        let mut base = serde_json::json!({
            "meshtastic": { "address": "127.0.0.1", "port": 4403 }
        });
        let over = serde_json::json!({
            "meshtastic": { "port": 9999 }
        });
        deep_merge(&mut base, &over);
        assert_eq!(base["meshtastic"]["address"], "127.0.0.1");
        assert_eq!(base["meshtastic"]["port"], 9999);
    }

    #[test]
    fn test_deep_merge_empty_override() {
        let mut base = serde_json::json!({ "debug": 0, "role": "client_mute" });
        let over = serde_json::json!({});
        deep_merge(&mut base, &over);
        assert_eq!(base["debug"], 0);
        assert_eq!(base["role"], "client_mute");
    }

    #[test]
    fn test_deep_merge_adds_new_key() {
        let mut base = serde_json::json!({ "debug": 0 });
        let over = serde_json::json!({ "newkey": "value" });
        deep_merge(&mut base, &over);
        assert_eq!(base["debug"], 0);
        assert_eq!(base["newkey"], "value");
    }

    #[test]
    fn test_deep_merge_new_nested_object() {
        let mut base = serde_json::json!({ "debug": 0 });
        let over = serde_json::json!({ "meshtastic": { "address": "10.0.0.1" } });
        deep_merge(&mut base, &over);
        assert_eq!(base["meshtastic"]["address"], "10.0.0.1");
    }

    // --- override file loading tests ---

    #[test]
    fn test_load_with_override_missing_override() {
        // Override file doesn't exist — should use base channels only.
        let mut base = NamedTempFile::new().unwrap();
        base.write_all(SAMPLE_RAVEN_CONF.as_bytes()).unwrap();
        let result = load_raven_channels_with_override(
            base.path(),
            "/nonexistent/raven.conf.override",
        )
        .unwrap();
        let channels = result.unwrap();
        assert_eq!(channels.len(), 5);
    }

    #[test]
    fn test_load_with_override_replaces_channels() {
        let mut base = NamedTempFile::new().unwrap();
        base.write_all(SAMPLE_RAVEN_CONF.as_bytes()).unwrap();

        let mut over = NamedTempFile::new().unwrap();
        over.write_all(
            br#"{
                "channels": [
                    { "namekey": "AREDN og==" },
                    { "namekey": "LongFast AQ==" },
                    { "namekey": "MeshCore izOH6cXN6mrJ5e26oRXNcg==" },
                    { "namekey": "OK-WX Uw==" },
                    { "namekey": "PMesh DcYgnMhTKG64bXt+n8gLCzK57IpEhRnhYwpL4xowu9Y=" },
                    { "namekey": "OK-Wide oA==" }
                ]
            }"#,
        )
        .unwrap();

        let result =
            load_raven_channels_with_override(base.path(), over.path()).unwrap();
        let channels = result.unwrap();
        assert_eq!(channels.len(), 6);
        assert_eq!(channels[5].name, "OK-Wide");
        assert_eq!(channels[5].psk, vec![0xa0]);
    }

    #[test]
    fn test_load_with_override_empty_object() {
        let mut base = NamedTempFile::new().unwrap();
        base.write_all(SAMPLE_RAVEN_CONF.as_bytes()).unwrap();

        let mut over = NamedTempFile::new().unwrap();
        over.write_all(b"{}").unwrap();

        let result =
            load_raven_channels_with_override(base.path(), over.path()).unwrap();
        let channels = result.unwrap();
        assert_eq!(channels.len(), 5, "empty override should not change channels");
    }

    #[test]
    fn test_load_with_override_non_object_ignored() {
        // Raven treats non-object overrides (e.g. an array) as no-op.
        let mut base = NamedTempFile::new().unwrap();
        base.write_all(SAMPLE_RAVEN_CONF.as_bytes()).unwrap();

        let mut over = NamedTempFile::new().unwrap();
        over.write_all(b"[]").unwrap();

        let result =
            load_raven_channels_with_override(base.path(), over.path()).unwrap();
        let channels = result.unwrap();
        assert_eq!(channels.len(), 5, "array override should be ignored");
    }

    #[test]
    fn test_load_with_override_invalid_json() {
        let mut base = NamedTempFile::new().unwrap();
        base.write_all(SAMPLE_RAVEN_CONF.as_bytes()).unwrap();

        let mut over = NamedTempFile::new().unwrap();
        over.write_all(b"not json").unwrap();

        assert!(
            load_raven_channels_with_override(base.path(), over.path()).is_err()
        );
    }
}
