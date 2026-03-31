use aes::cipher::{BlockEncrypt, KeyInit, generic_array::GenericArray};
use aes::{Aes128, Aes256};

/// The well-known default Meshtastic PSK (for 1-byte shorthand value 1).
const DEFAULT_PSK: [u8; 16] = [
    0xd4, 0xf1, 0xbb, 0x3a, 0x20, 0x29, 0x07, 0x59, 0xf0, 0xbc, 0xff, 0xab, 0xcf, 0x4e, 0x69,
    0x01,
];

/// A channel's crypto material, derived from the serial handshake.
#[derive(Debug, Clone)]
pub struct ChannelKey {
    /// The local channel index (0 = primary, 1+ = secondary).
    pub index: u32,
    /// Channel name (empty string for unnamed/default channels).
    pub name: String,
    /// Expanded AES key (16 or 32 bytes). Empty if no encryption.
    pub key: Vec<u8>,
    /// The 8-bit channel hash used in the MeshPacket `channel` field for encrypted OTA packets.
    pub hash: u32,
}

/// Expand a PSK from the proto representation to a full key.
/// - 0 bytes → no crypto (empty key)
/// - 1 byte → default key with last byte replaced  
/// - 16 or 32 bytes → used as-is
pub fn expand_psk(psk: &[u8]) -> Vec<u8> {
    match psk.len() {
        0 => vec![],
        1 => {
            let mut key = DEFAULT_PSK;
            key[15] = psk[0];
            key.to_vec()
        }
        16 | 32 => psk.to_vec(),
        _ => {
            log::warn!("unexpected PSK length {}, using as-is", psk.len());
            psk.to_vec()
        }
    }
}

/// Compute the Meshtastic channel hash: XOR all bytes of name, then XOR all bytes of expanded key.
pub fn channel_hash(name: &str, key: &[u8]) -> u32 {
    let mut hash: u8 = 0;
    for b in name.as_bytes() {
        hash ^= b;
    }
    for b in key {
        hash ^= b;
    }
    hash as u32
}

/// Encrypt plaintext using Meshtastic AES-CTR.
/// Nonce/counter: [packet_id(LE32), 0(LE32), from_node(LE32), 0(LE32)]
/// Uses AES-128 for 16-byte keys, AES-256 for 32-byte keys.
pub fn encrypt_aes_ctr(key: &[u8], from: u32, packet_id: u32, plaintext: &[u8]) -> Vec<u8> {
    aes_ctr_transform(key, from, packet_id, plaintext)
}

/// Decrypt ciphertext using Meshtastic AES-CTR (symmetric — same as encrypt).
pub fn decrypt_aes_ctr(key: &[u8], from: u32, packet_id: u32, ciphertext: &[u8]) -> Vec<u8> {
    aes_ctr_transform(key, from, packet_id, ciphertext)
}

/// Encrypt a single AES block using the appropriate key size.
fn encrypt_block(key: &[u8], block: &mut GenericArray<u8, aes::cipher::typenum::U16>) {
    if key.len() >= 32 {
        let cipher = Aes256::new(GenericArray::from_slice(&key[..32]));
        cipher.encrypt_block(block);
    } else {
        let cipher = Aes128::new(GenericArray::from_slice(&key[..16]));
        cipher.encrypt_block(block);
    }
}

fn aes_ctr_transform(key: &[u8], from: u32, packet_id: u32, data: &[u8]) -> Vec<u8> {
    if key.len() < 16 {
        return data.to_vec();
    }

    // Initial counter: [id(LE), 0, from(LE), 0]
    let mut counter = [0u8; 16];
    counter[0..4].copy_from_slice(&packet_id.to_le_bytes());
    // bytes 4..8 = 0
    counter[8..12].copy_from_slice(&from.to_le_bytes());
    // bytes 12..16 = 0

    let mut output = Vec::with_capacity(data.len());
    let mut keystream_pos = 16; // force generation on first byte
    let mut keystream_block = [0u8; 16];

    for &byte in data {
        if keystream_pos == 16 {
            let mut block = GenericArray::clone_from_slice(&counter);
            encrypt_block(key, &mut block);
            keystream_block = block.into();
            keystream_pos = 0;
            // Increment counter (big-endian style, matching Meshtastic firmware)
            for j in (0..16).rev() {
                counter[j] = counter[j].wrapping_add(1);
                if counter[j] != 0 {
                    break;
                }
            }
        }
        output.push(byte ^ keystream_block[keystream_pos]);
        keystream_pos += 1;
    }

    output
}

/// Build a `ChannelKey` from a protobuf `Channel` message.
pub fn channel_key_from_proto(
    index: u32,
    name: &str,
    psk: &[u8],
) -> ChannelKey {
    let key = expand_psk(psk);
    // For hash computation, use the channel name (or the preset name for default channels).
    // If name is empty, the firmware uses the modem preset name, but for hash purposes
    // an empty string is fine — it just means only the key bytes contribute to the hash.
    let hash = channel_hash(name, &key);
    ChannelKey {
        index,
        name: name.to_string(),
        key,
        hash,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_expand_psk_empty() {
        assert!(expand_psk(&[]).is_empty());
    }

    #[test]
    fn test_expand_psk_one_byte_default() {
        let key = expand_psk(&[1]);
        assert_eq!(key.len(), 16);
        assert_eq!(key[..15], DEFAULT_PSK[..15]);
        assert_eq!(key[15], 1);
    }

    #[test]
    fn test_expand_psk_one_byte_variant() {
        let key = expand_psk(&[5]);
        assert_eq!(key.len(), 16);
        assert_eq!(key[15], 5);
    }

    #[test]
    fn test_expand_psk_16_bytes() {
        let psk = [0u8; 16];
        assert_eq!(expand_psk(&psk), psk);
    }

    #[test]
    fn test_expand_psk_32_bytes() {
        let psk = [0xAB; 32];
        assert_eq!(expand_psk(&psk), psk);
    }

    #[test]
    fn test_channel_hash_default() {
        // "LongFast" with default key (psk=[1] → expanded)
        let key = expand_psk(&[1]);
        let hash = channel_hash("LongFast", &key);
        // XOR of "LongFast" bytes: 'L'^'o'^'n'^'g'^'F'^'a'^'s'^'t'
        let name_xor: u8 = b"LongFast".iter().fold(0u8, |a, b| a ^ b);
        let key_xor: u8 = key.iter().fold(0u8, |a, b| a ^ b);
        assert_eq!(hash, (name_xor ^ key_xor) as u32);
    }

    #[test]
    fn test_channel_hash_empty_name() {
        let key = expand_psk(&[1]);
        let hash = channel_hash("", &key);
        let key_xor: u8 = key.iter().fold(0u8, |a, b| a ^ b);
        assert_eq!(hash, key_xor as u32);
    }

    #[test]
    fn test_encrypt_decrypt_roundtrip() {
        let key = expand_psk(&[1]);
        let plaintext = b"Hello Meshtastic!";
        let from = 0x12345678;
        let id = 0xDEADBEEF;

        let ciphertext = encrypt_aes_ctr(&key, from, id, plaintext);
        assert_ne!(ciphertext, plaintext);
        assert_eq!(ciphertext.len(), plaintext.len());

        let decrypted = decrypt_aes_ctr(&key, from, id, &ciphertext);
        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn test_encrypt_empty_key_passthrough() {
        let plaintext = b"no crypto";
        let result = encrypt_aes_ctr(&[], 1, 2, plaintext);
        assert_eq!(result, plaintext);
    }

    #[test]
    fn test_encrypt_deterministic() {
        let key = expand_psk(&[1]);
        let plaintext = b"test";
        let c1 = encrypt_aes_ctr(&key, 1, 2, plaintext);
        let c2 = encrypt_aes_ctr(&key, 1, 2, plaintext);
        assert_eq!(c1, c2);
    }

    #[test]
    fn test_different_from_produces_different_ciphertext() {
        let key = expand_psk(&[1]);
        let plaintext = b"test";
        let c1 = encrypt_aes_ctr(&key, 1, 100, plaintext);
        let c2 = encrypt_aes_ctr(&key, 2, 100, plaintext);
        assert_ne!(c1, c2);
    }

    #[test]
    fn test_different_id_produces_different_ciphertext() {
        let key = expand_psk(&[1]);
        let plaintext = b"test";
        let c1 = encrypt_aes_ctr(&key, 1, 100, plaintext);
        let c2 = encrypt_aes_ctr(&key, 1, 200, plaintext);
        assert_ne!(c1, c2);
    }

    #[test]
    fn test_channel_key_from_proto() {
        let ck = channel_key_from_proto(0, "LongFast", &[1]);
        assert_eq!(ck.index, 0);
        assert_eq!(ck.name, "LongFast");
        assert_eq!(ck.key.len(), 16);
        assert_eq!(ck.hash, channel_hash("LongFast", &ck.key));
    }
}
