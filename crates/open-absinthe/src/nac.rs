use serde::{Deserialize, Serialize};
use serde::{Deserializer, Serializer, de};

use std::io::Read;
use std::path::PathBuf;

use sha1::{Digest, Sha1};

use crate::AbsintheError;
use crate::jelly::Jelly;

// Offsets of the three nac entrypoints within the x86-64 slice (from nac.py;
// the binary is mapped at base 0 so these are also virtual addresses).
const NAC_INIT: u64 = 0xB1DB0;
const NAC_KEY_ESTABLISHMENT: u64 = 0xB1DD0;
const NAC_SIGN: u64 = 0xB1DF0;

/// Expected sha1 of the fat `IMDAppleServices` (informational; see load_binary).
pub const BINARY_SHA1: &str = "e1181ccad82e6629d52c6a006645ad87ee59bd13";

pub fn bin_serialize<S>(x: &[u8], s: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    s.serialize_bytes(x)
}

pub fn bin_deserialize_mac<'de, D>(d: D) -> Result<[u8; 6], D::Error>
where
    D: Deserializer<'de>,
{
    bin_deserialize(d).map(|i| i.try_into().unwrap())
}

pub fn bin_deserialize<'de, D>(d: D) -> Result<Vec<u8>, D::Error>
where
    D: Deserializer<'de>,
{
    use core::fmt;

    struct DataVisitor;

    impl<'de> de::Visitor<'de> for DataVisitor {
        type Value = Vec<u8>;

        fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
            formatter.write_str("a byte array")
        }

        fn visit_bytes<E>(self, v: &[u8]) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            self.visit_byte_buf(v.to_owned())
        }

        fn visit_byte_buf<E>(self, v: Vec<u8>) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            Ok(v.into())
        }
    }

    d.deserialize_byte_buf(DataVisitor)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HardwareConfig {
    pub product_name: String,
    #[serde(serialize_with = "bin_serialize", deserialize_with = "bin_deserialize_mac")]
    pub io_mac_address: [u8; 6],
    pub platform_serial_number: String,
    pub platform_uuid: String,
    pub root_disk_uuid: String,
    pub board_id: String,
    pub os_build_num: String,
    #[serde(serialize_with = "bin_serialize", deserialize_with = "bin_deserialize")]
    pub platform_serial_number_enc: Vec<u8>, // Gq3489ugfi
    #[serde(serialize_with = "bin_serialize", deserialize_with = "bin_deserialize")]
    pub platform_uuid_enc: Vec<u8>, // Fyp98tpgj
    #[serde(serialize_with = "bin_serialize", deserialize_with = "bin_deserialize")]
    pub root_disk_uuid_enc: Vec<u8>, // kbjfrfpoJU
    #[serde(serialize_with = "bin_serialize", deserialize_with = "bin_deserialize")]
    pub rom: Vec<u8>,
    #[serde(serialize_with = "bin_serialize", deserialize_with = "bin_deserialize")]
    pub rom_enc: Vec<u8>, // oycqAZloTNDm
    pub mlb: String,
    #[serde(serialize_with = "bin_serialize", deserialize_with = "bin_deserialize")]
    pub mlb_enc: Vec<u8>, // abKPld1EcMni
}

impl HardwareConfig {
    /// Reverse path (parse a Mac's own validation data into a HardwareConfig).
    /// Not needed for generation and not implemented in the emulated build.
    pub fn from_validation_data(_data: &[u8]) -> Result<HardwareConfig, AbsintheError> {
        Err(AbsintheError::new(-100))
    }
}

/// Community mirror of the (non-redistributable) Apple binary — the same source
/// the Python sidecar used. We ship none of Apple's code; it's fetched as data.
const IMD_URL: &str = "https://github.com/JJTech0130/nacserver/raw/main/IMDAppleServices";

/// Where the downloaded binary is cached between runs.
fn cache_path() -> PathBuf {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("open-absinthe").join("IMDAppleServices")
}

fn sha1_hex(bytes: &[u8]) -> String {
    Sha1::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

/// Reject anything that isn't the exact pinned build — the function offsets and
/// hooks only match this one binary.
fn verify(bytes: Vec<u8>, source: &str) -> Result<Vec<u8>, AbsintheError> {
    let got = sha1_hex(&bytes);
    if got != BINARY_SHA1 {
        log::error!("IMDAppleServices from {source}: sha1 {got}, expected {BINARY_SHA1}");
        return Err(AbsintheError::new(-104));
    }
    Ok(bytes)
}

fn download_imd() -> Result<Vec<u8>, AbsintheError> {
    log::info!("downloading IMDAppleServices from {IMD_URL}");
    let resp = ureq::get(IMD_URL).call().map_err(|e| {
        log::error!("IMDAppleServices download failed: {e}");
        AbsintheError::new(-102)
    })?;
    let mut buf = Vec::new();
    resp.into_reader().read_to_end(&mut buf).map_err(|e| {
        log::error!("IMDAppleServices read failed: {e}");
        AbsintheError::new(-103)
    })?;
    Ok(buf)
}

/// Locate the fat `IMDAppleServices` the emulator runs. Resolution order:
///   1. `OPEN_ABSINTHE_IMD` env var (explicit path override), else
///   2. the cached copy under `$XDG_CACHE_HOME/open-absinthe` (or `~/.cache/...`), else
///   3. download from the community mirror, verify, and cache it.
/// Every path is sha1-checked against `BINARY_SHA1`.
fn load_binary() -> Result<Vec<u8>, AbsintheError> {
    if let Ok(path) = std::env::var("OPEN_ABSINTHE_IMD") {
        let bytes = std::fs::read(&path).map_err(|e| {
            log::error!("OPEN_ABSINTHE_IMD={path} unreadable: {e}");
            AbsintheError::new(-101)
        })?;
        return verify(bytes, &path);
    }

    let cache = cache_path();
    if let Ok(bytes) = std::fs::read(&cache) {
        match verify(bytes, "cache") {
            Ok(b) => return Ok(b),
            Err(_) => log::warn!("cached IMDAppleServices failed verification; re-downloading"),
        }
    }

    let bytes = verify(download_imd()?, "download")?;
    if let Some(parent) = cache.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match std::fs::write(&cache, &bytes) {
        Ok(()) => log::info!("cached IMDAppleServices to {}", cache.display()),
        Err(e) => log::warn!("could not cache IMDAppleServices to {}: {e}", cache.display()),
    }
    Ok(bytes)
}

/// A live validation context. Owns the emulator so the in-emulator
/// `validation_ctx` pointer stays valid across the three calls.
pub struct ValidationCtx {
    jelly: Jelly,
    ctx_addr: u64,
}

unsafe impl Send for ValidationCtx {}

impl ValidationCtx {
    /// nacInit: feed the Apple cert chain, capture the request bytes (written
    /// into `out_request_bytes`) that the caller POSTs to id-initialize-validation.
    pub fn new(
        cert_chain: &[u8],
        out_request_bytes: &mut Vec<u8>,
        hw_config: &HardwareConfig,
    ) -> Result<ValidationCtx, AbsintheError> {
        let full = load_binary()?;
        let slice = Jelly::extract_x86_64(&full)?;
        let mut jelly = Jelly::new(&slice, hw_config)?;

        let cert_addr = jelly.malloc(cert_chain.len());
        jelly.write(cert_addr, cert_chain)?;

        let out_ctx = jelly.malloc(8);
        let out_req = jelly.malloc(8);
        let out_len = jelly.malloc(8);

        let ret = jelly.call(
            NAC_INIT,
            &[cert_addr, cert_chain.len() as u64, out_ctx, out_req, out_len],
        )?;
        log::debug!("nac_init: cert={}B -> ret={}", cert_chain.len(), sign_extend(ret));
        if ret != 0 {
            log::error!("nac_init returned {}", sign_extend(ret));
            return Err(AbsintheError::new(sign_extend(ret)));
        }

        let req_addr = jelly.read_u64(out_req)?;
        let req_len = jelly.read_u64(out_len)? as usize;
        *out_request_bytes = jelly.read(req_addr, req_len)?;

        let ctx_addr = jelly.read_u64(out_ctx)?;
        log::debug!("nac_init ok: ctx={ctx_addr:#x} request_len={req_len}");

        Ok(ValidationCtx { jelly, ctx_addr })
    }

    /// nacKeyEstablishment: hand back Apple's session-info response.
    pub fn key_establishment(&mut self, response: &[u8]) -> Result<(), AbsintheError> {
        let resp_addr = self.jelly.malloc(response.len());
        self.jelly.write(resp_addr, response)?;

        let ret = self.jelly.call(
            NAC_KEY_ESTABLISHMENT,
            &[self.ctx_addr, resp_addr, response.len() as u64],
        )?;
        if ret != 0 {
            log::error!("nac_key_establishment returned {}", sign_extend(ret));
            return Err(AbsintheError::new(sign_extend(ret)));
        }
        Ok(())
    }

    /// nacSign: produce the final validation data.
    pub fn sign(&mut self) -> Result<Vec<u8>, AbsintheError> {
        let out_data = self.jelly.malloc(8);
        let out_data_len = self.jelly.malloc(8);

        let ret = self.jelly.call(
            NAC_SIGN,
            &[self.ctx_addr, 0, 0, out_data, out_data_len],
        )?;
        if ret != 0 {
            log::error!("nac_sign returned {}", sign_extend(ret));
            return Err(AbsintheError::new(sign_extend(ret)));
        }

        let data_addr = self.jelly.read_u64(out_data)?;
        let data_len = self.jelly.read_u64(out_data_len)? as usize;
        let data = self.jelly.read(data_addr, data_len)?;
        log::info!("generated validation data ({} bytes)", data.len());
        Ok(data)
    }
}

/// Interpret the low 32 bits of a return value as a signed error (nac.py does
/// the same when reporting failures).
fn sign_extend(ret: u64) -> i32 {
    (ret & 0xffff_ffff) as u32 as i32
}
