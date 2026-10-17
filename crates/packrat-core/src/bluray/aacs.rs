//! Packrat-owned AACS VUK path.
//!
//! This covers the common BD-ROM case: a `KEYDB.cfg` row with the disc's
//! Volume Unique Key. The VUK unwraps each CPS Unit key from
//! `AACS/Unit_Key_RO.inf`; content Aligned Units are AES-128-CBC encrypted
//! under a per-unit block key derived from that CPS key and the 16-byte seed
//! at the start of the unit.

use std::fmt;
use std::path::{Path, PathBuf};

use aes::cipher::{BlockDecrypt, BlockEncrypt, KeyInit};
use aes::Aes128;

const UNIT_LEN: usize = 6144;
const SEED_LEN: usize = 16;
const IV0: [u8; 16] = [
    0x0B, 0xA0, 0xF8, 0xDD, 0xFE, 0xA6, 0x1F, 0xB3, 0xD8, 0xDF, 0x9F, 0x56, 0x6A, 0x05, 0x0F, 0x78,
];

#[derive(Debug)]
pub enum AacsError {
    Io(std::io::Error),
    NoKey(String),
    Invalid(String),
}

impl fmt::Display for AacsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "AACS I/O: {error}"),
            Self::NoKey(message) | Self::Invalid(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for AacsError {}

impl From<std::io::Error> for AacsError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

type Result<T> = std::result::Result<T, AacsError>;

pub(super) trait UnitDecryptor: Send {
    fn decrypt_units(&mut self, buf: &mut [u8], clip_offset: u64) -> Result<()>;
}

pub(super) struct AacsDecryptor {
    cps_keys: Vec<[u8; 16]>,
    selected: Option<[u8; 16]>,
}

/// Resolve a VUK and return a decryptor only after trial-decrypting the first
/// clip. `None` means the disc has no AACS directory or it is already clear.
pub(super) fn open(
    disc_root: &Path,
    keydb_path: Option<&Path>,
) -> Result<Option<Box<dyn UnitDecryptor>>> {
    if !disc_root.join("AACS").is_dir() {
        return Ok(None);
    }
    let unit_key_path = first_existing(&[
        disc_root.join("AACS").join("Unit_Key_RO.inf"),
        disc_root
            .join("AACS")
            .join("DUPLICATE")
            .join("Unit_Key_RO.inf"),
    ])
    .ok_or_else(|| AacsError::NoKey("AACS/Unit_Key_RO.inf is missing".into()))?;
    let unit_keys = std::fs::read(unit_key_path)?;
    let disc_id = sha1(&unit_keys);
    let disc_id_hex = hex(&disc_id);

    let keydb = keydb_path
        .map(Path::to_path_buf)
        .or_else(find_keydb_path)
        .ok_or_else(|| AacsError::NoKey("no KEYDB.cfg found".into()))?;
    let vuk = find_vuk(&keydb, &disc_id_hex)?;
    let encrypted_keys = parse_unit_keys(&unit_keys)?;
    let first_clip = first_clip(disc_root)
        .ok_or_else(|| AacsError::NoKey("no BDMV/STREAM/*.m2ts clip".into()))?;
    let mut sample = vec![0u8; UNIT_LEN];
    {
        use std::io::Read;
        let mut file = std::fs::File::open(&first_clip)?;
        file.read_exact(&mut sample)?;
    }

    let mut cps_keys = Vec::new();
    for encrypted in encrypted_keys {
        let cps_key = aes_decrypt_block(&vuk, &encrypted);
        let mut trial = sample.clone();
        decrypt_unit(&cps_key, &mut trial)?;
        if looks_like_ts(&trial) {
            cps_keys.push(cps_key);
        }
    }
    if cps_keys.is_empty() {
        return Err(AacsError::NoKey(format!(
            "the KEYDB VUK for {disc_id_hex} did not decrypt any CPS unit"
        )));
    }
    Ok(Some(Box::new(AacsDecryptor {
        cps_keys,
        selected: None,
    })))
}

impl UnitDecryptor for AacsDecryptor {
    fn decrypt_units(&mut self, buf: &mut [u8], clip_offset: u64) -> Result<()> {
        if buf.len() % UNIT_LEN != 0 {
            return Err(AacsError::Invalid(format!(
                "AACS buffer length {} is not a multiple of {UNIT_LEN}",
                buf.len()
            )));
        }
        if clip_offset == 0 {
            // A new clip may belong to a different CPS unit (episodes, angles,
            // or a separate title sharing the disc key). Pick the key that
            // makes this clip's first aligned unit look like BDAV TS.
            let probe = &buf[..UNIT_LEN];
            let mut found = None;
            for key in &self.cps_keys {
                let mut trial = probe.to_vec();
                decrypt_unit(key, &mut trial)?;
                if looks_like_ts(&trial) {
                    found = Some(*key);
                    break;
                }
            }
            self.selected = Some(found.ok_or_else(|| {
                AacsError::NoKey("no CPS Unit key decrypts the first aligned unit".into())
            })?);
        }
        let key = self
            .selected
            .ok_or_else(|| AacsError::Invalid("AACS key not selected".into()))?;
        for unit in buf.chunks_exact_mut(UNIT_LEN) {
            decrypt_unit(&key, unit)?;
        }
        Ok(())
    }
}

fn decrypt_unit(cps_key: &[u8; 16], unit: &mut [u8]) -> Result<()> {
    if unit.len() != UNIT_LEN {
        return Err(AacsError::Invalid("short AACS aligned unit".into()));
    }
    let mut seed = [0u8; SEED_LEN];
    seed.copy_from_slice(&unit[..SEED_LEN]);
    let mut block_key = aes_encrypt_block(cps_key, &seed);
    for (byte, seed_byte) in block_key.iter_mut().zip(seed) {
        *byte ^= seed_byte;
    }
    cbc_decrypt(&block_key, &mut unit[SEED_LEN..]);
    unit[..SEED_LEN].copy_from_slice(&seed);
    Ok(())
}

fn aes_encrypt_block(key: &[u8; 16], block: &[u8; 16]) -> [u8; 16] {
    let cipher = Aes128::new_from_slice(key).expect("AES-128 key length");
    let mut out = *block;
    cipher.encrypt_block((&mut out).into());
    out
}

fn aes_decrypt_block(key: &[u8; 16], block: &[u8; 16]) -> [u8; 16] {
    let cipher = Aes128::new_from_slice(key).expect("AES-128 key length");
    let mut out = *block;
    cipher.decrypt_block((&mut out).into());
    out
}

fn cbc_decrypt(key: &[u8; 16], data: &mut [u8]) {
    let cipher = Aes128::new_from_slice(key).expect("AES-128 key length");
    let mut previous = IV0;
    for block in data.chunks_exact_mut(16) {
        let ciphertext = {
            let mut copy = [0u8; 16];
            copy.copy_from_slice(block);
            copy
        };
        let mut plain = ciphertext;
        cipher.decrypt_block((&mut plain).into());
        for (byte, prev) in plain.iter_mut().zip(previous) {
            *byte ^= prev;
        }
        block.copy_from_slice(&plain);
        previous = ciphertext;
    }
}

fn parse_unit_keys(bytes: &[u8]) -> Result<Vec<[u8; 16]>> {
    if bytes.len() < 20 {
        return Err(AacsError::Invalid("Unit_Key_RO.inf is truncated".into()));
    }
    let block_start = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
    if block_start + 16 > bytes.len() {
        return Err(AacsError::Invalid(
            "Unit_Key_RO.inf CPS block is outside the file".into(),
        ));
    }
    let block = &bytes[block_start..];
    let count = u16::from_be_bytes([block[0], block[1]]) as usize;
    let needed = 16 + count * 48;
    if needed > block.len() {
        return Err(AacsError::Invalid(
            "Unit_Key_RO.inf CPS records are truncated".into(),
        ));
    }
    let mut keys = Vec::with_capacity(count);
    for index in 0..count {
        let start = 16 + index * 48 + 32;
        let mut key = [0u8; 16];
        key.copy_from_slice(&block[start..start + 16]);
        keys.push(key);
    }
    if keys.is_empty() {
        return Err(AacsError::NoKey(
            "Unit_Key_RO.inf lists no CPS units".into(),
        ));
    }
    Ok(keys)
}

fn find_vuk(path: &Path, disc_id_hex: &str) -> Result<[u8; 16]> {
    let text = std::fs::read_to_string(path)?;
    let needle = disc_id_hex.to_ascii_lowercase();
    for line in text.lines() {
        let lower = line.to_ascii_lowercase();
        if !lower.contains(&needle) {
            continue;
        }
        // Legacy: 0x<id> = V 0x<vuk> / V <vuk>
        if let Some((_, rhs)) = line.split_once('=') {
            let fields: Vec<&str> = rhs.split_whitespace().collect();
            if fields.first().is_some_and(|field| *field == "V") {
                if let Some(vuk) = fields.get(1).and_then(|raw| parse_hex16(raw)) {
                    return Ok(vuk);
                }
            }
        }
        // Extended: ... | V | 0x<vuk> | ...
        let fields: Vec<&str> = line.split('|').map(str::trim).collect();
        for pair in fields.windows(2) {
            if pair[0].eq_ignore_ascii_case("V") || pair[0].eq_ignore_ascii_case("VUK") {
                if let Some(vuk) = parse_hex16(pair[1]) {
                    return Ok(vuk);
                }
            }
        }
    }
    Err(AacsError::NoKey(format!(
        "no KEYDB.cfg VUK entry for disc {disc_id_hex}"
    )))
}

fn parse_hex16(raw: &str) -> Option<[u8; 16]> {
    let trimmed = raw.trim().trim_start_matches("0x").trim_start_matches("0X");
    if trimmed.len() != 32 || !trimmed.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let mut out = [0u8; 16];
    for index in 0..16 {
        out[index] = u8::from_str_radix(&trimmed[index * 2..index * 2 + 2], 16).ok()?;
    }
    Some(out)
}

fn sha1(bytes: &[u8]) -> [u8; 20] {
    use sha1::{Digest, Sha1};
    let mut hasher = Sha1::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut out = [0u8; 20];
    out.copy_from_slice(&digest);
    out
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

fn looks_like_ts(unit: &[u8]) -> bool {
    unit.len() >= 6144 && (0..4).all(|index| unit[index * 192 + 4] == 0x47)
}

fn first_clip(root: &Path) -> Option<PathBuf> {
    let dir = root.join("BDMV").join("STREAM");
    let mut clips: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("m2ts"))
        .collect();
    clips.sort();
    clips.into_iter().next()
}

fn first_existing(paths: &[PathBuf]) -> Option<PathBuf> {
    paths.iter().find(|path| path.is_file()).cloned()
}

fn find_keydb_path() -> Option<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        candidates.push(home.join(".config").join("aacs").join("KEYDB.cfg"));
        candidates.push(home.join(".config").join("packrat").join("KEYDB.cfg"));
    }
    if let Some(config) = std::env::var_os("XDG_CONFIG_HOME") {
        candidates.push(PathBuf::from(config).join("aacs").join("KEYDB.cfg"));
    }
    first_existing(&candidates)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_parser_accepts_0x_and_plain() {
        let expected = [0x11u8; 16];
        assert_eq!(
            parse_hex16(&format!("0x{}", "11".repeat(16))),
            Some(expected)
        );
        assert_eq!(parse_hex16(&"11".repeat(16)), Some(expected));
        assert_eq!(parse_hex16("not-hex"), None);
    }

    #[test]
    fn unit_key_parser_reads_cps_records() {
        let mut bytes = vec![0u8; 0x60];
        bytes[..4].copy_from_slice(&0x20u32.to_be_bytes());
        bytes[0x20..0x22].copy_from_slice(&1u16.to_be_bytes());
        bytes[0x20 + 16 + 32..0x20 + 16 + 48].copy_from_slice(&[0x42; 16]);
        assert_eq!(parse_unit_keys(&bytes).unwrap(), vec![[0x42; 16]]);
    }

    #[test]
    fn aligned_unit_round_trips() {
        let key = [0x33u8; 16];
        let mut plain = vec![0u8; UNIT_LEN];
        for (index, byte) in plain.iter_mut().enumerate() {
            *byte = (index % 251) as u8;
        }
        let seed = plain[..16].to_vec();
        let expected = plain.clone();
        // Encrypt the body by reversing the decrypt path.
        let mut block_key = aes_encrypt_block(&key, &seed[..16].try_into().unwrap());
        for (byte, seed_byte) in block_key.iter_mut().zip(&seed) {
            *byte ^= seed_byte;
        }
        cbc_encrypt(&block_key, &mut plain[16..]);
        let mut unit = plain;
        decrypt_unit(&key, &mut unit).unwrap();
        assert_eq!(&unit[..], &expected[..]);
    }

    fn cbc_encrypt(key: &[u8; 16], data: &mut [u8]) {
        let cipher = Aes128::new_from_slice(key).unwrap();
        let mut previous = IV0;
        for block in data.chunks_exact_mut(16) {
            for (byte, prev) in block.iter_mut().zip(previous) {
                *byte ^= prev;
            }
            cipher.encrypt_block((&mut *block).into());
            previous.copy_from_slice(block);
        }
    }
}
