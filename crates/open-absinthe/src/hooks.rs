//! In-process implementations of the libc / CoreFoundation / IOKit / DiskArb
//! symbols that `IMDAppleServices` imports while generating validation data.
//! Direct port of the hook table in the sidecar's `nac.py`.
//!
//! The emulated binary never sees real Apple frameworks; instead each imported
//! symbol is bound (in `jelly.rs`) to a hook address, and `dispatch` below
//! services the call: it reads the SysV argument registers, runs the handler,
//! and writes the result to RAX. CoreFoundation objects are modelled by a
//! side table (`cf`) indexed 1-based, exactly like `nac.py`'s `CF_OBJECTS`
//! (index 0 == NULL).

use std::collections::HashMap;

use unicorn_engine::{RegisterX86, Unicorn};

use crate::nac::HardwareConfig;

/// Apple's fixed EFI/NVRAM vendor GUID, used to namespace the ROM/MLB IOKit
/// keys. Constant across machines (not the host's GPU UUID).
const NVRAM_GUID: &str = "4D1EDE05-38C7-4A6A-9CC6-4BCCA8B38C14";

/// A modelled CoreFoundation object.
#[derive(Clone, Debug)]
pub(crate) enum Cf {
    Data(Vec<u8>),
    Str(String),
    Dict(HashMap<String, Cf>),
}

pub(crate) struct NacState {
    pub heap_use: u64,
    /// 1-based CF object table (see module docs).
    cf: Vec<Cf>,
    /// One-shot iterator hack mirroring `ETH_ITERATOR_HACK` in nac.py.
    eth_hack: bool,
    /// IOKit property key -> value, built from the HardwareConfig.
    iokit: HashMap<String, Cf>,
    /// Volume UUID returned by DADiskCopyDescription.
    root_disk_uuid: String,
    /// Hook page address -> symbol name (filled by jelly.rs).
    pub hook_addr_to_name: HashMap<u64, &'static str>,
    /// Tiny LCG so `arc4random` has no external dependency.
    rng: u64,
}

impl NacState {
    pub fn from_hw(hw: &HardwareConfig) -> Self {
        // product-name / board-id are stored in IOKit as CFData holding a
        // NUL-terminated C string. VERIFY: exact framing (trailing NUL,
        // padding) may need adjustment once the emulator runs end to end.
        let cstr_data = |s: &str| {
            let mut v = s.as_bytes().to_vec();
            v.push(0);
            Cf::Data(v)
        };

        let mut iokit = HashMap::new();
        iokit.insert("product-name".into(), cstr_data(&hw.product_name));
        iokit.insert("IOMACAddress".into(), Cf::Data(hw.io_mac_address.to_vec()));
        iokit.insert(
            "IOPlatformSerialNumber".into(),
            Cf::Str(hw.platform_serial_number.clone()),
        );
        iokit.insert("IOPlatformUUID".into(), Cf::Str(hw.platform_uuid.clone()));
        iokit.insert("board-id".into(), cstr_data(&hw.board_id));
        // Obfuscated keys (comments map to nac.rs field names).
        iokit.insert("Gq3489ugfi".into(), Cf::Data(hw.platform_serial_number_enc.clone()));
        iokit.insert("Fyp98tpgj".into(), Cf::Data(hw.platform_uuid_enc.clone()));
        iokit.insert("kbjfrfpoJU".into(), Cf::Data(hw.root_disk_uuid_enc.clone()));
        iokit.insert("oycqAZloTNDm".into(), Cf::Data(hw.rom_enc.clone()));
        iokit.insert("abKPld1EcMni".into(), Cf::Data(hw.mlb_enc.clone()));
        // ROM/MLB by NVRAM GUID: plaintext ROM (6 bytes) and MLB string bytes.
        iokit.insert(format!("{NVRAM_GUID}:ROM"), Cf::Data(hw.rom.clone()));
        iokit.insert(format!("{NVRAM_GUID}:MLB"), Cf::Data(hw.mlb.as_bytes().to_vec()));

        // Diagnostic: dump what HardwareConfig actually arrived with. If these
        // sizes/values are empty or wrong, the app decoded the blob wrong
        // (open-absinthe just reports what it was handed).
        log::debug!(
            "seeded hw: product_name={:?} serial={:?} uuid={:?} board_id={:?} mac={}B root_disk_uuid={:?} | enc sizes ser={} uuid={} rootdisk={} rom_plain={} rom_enc={} mlb={:?} mlb_enc={}",
            hw.product_name,
            hw.platform_serial_number,
            hw.platform_uuid,
            hw.board_id,
            hw.io_mac_address.len(),
            hw.root_disk_uuid,
            hw.platform_serial_number_enc.len(),
            hw.platform_uuid_enc.len(),
            hw.root_disk_uuid_enc.len(),
            hw.rom.len(),
            hw.rom_enc.len(),
            hw.mlb,
            hw.mlb_enc.len(),
        );

        NacState {
            heap_use: 0,
            cf: Vec::new(),
            eth_hack: false,
            iokit,
            root_disk_uuid: hw.root_disk_uuid.clone(),
            hook_addr_to_name: HashMap::new(),
            rng: 0x9E37_79B9_7F4A_7C15,
        }
    }

    fn push(&mut self, obj: Cf) -> u64 {
        self.cf.push(obj);
        self.cf.len() as u64 // 1-based handle
    }

    fn get(&self, handle: u64) -> Option<&Cf> {
        if handle == 0 {
            return None;
        }
        self.cf.get((handle - 1) as usize)
    }

    fn next_rand(&mut self) -> u32 {
        // xorshift64*
        let mut x = self.rng;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.rng = x;
        ((x.wrapping_mul(0x2545_F491_4F6C_DD1D)) >> 32) as u32
    }
}

/// The complete set of symbols we bind. Order defines the hook-page address
/// assignment; do not reorder casually (only matters internally, but keep it
/// stable for debugging).
pub(crate) const HOOKS: &[&str] = &[
    "_malloc",
    "___stack_chk_guard",
    "___memset_chk",
    "_sysctlbyname",
    "_memcpy",
    "_kIOMasterPortDefault",
    "_IORegistryEntryFromPath",
    "_kCFAllocatorDefault",
    "_IORegistryEntryCreateCFProperty",
    "_CFGetTypeID",
    "_CFStringGetTypeID",
    "_CFDataGetTypeID",
    "_CFDataGetLength",
    "_CFDataGetBytes",
    "_CFRelease",
    "_IOObjectRelease",
    "_statfs$INODE64",
    "_DASessionCreate",
    "_DADiskCreateFromBSDName",
    "_kDADiskDescriptionVolumeUUIDKey",
    "_DADiskCopyDescription",
    "_CFDictionaryGetValue",
    "_CFUUIDCreateString",
    "_CFStringGetLength",
    "_CFStringGetMaximumSizeForEncoding",
    "_CFStringGetCString",
    "_free",
    "_IOServiceMatching",
    "_IOServiceGetMatchingService",
    "_CFDictionaryCreateMutable",
    "_kCFBooleanTrue",
    "_CFDictionarySetValue",
    "_IOServiceGetMatchingServices",
    "_IOIteratorNext",
    "___bzero",
    "_IORegistryEntryGetParentEntry",
    "_arc4random",
];

// ---- register / memory helpers -------------------------------------------

fn arg(uc: &mut Unicorn<()>, i: usize) -> u64 {
    const R: [RegisterX86; 6] = [
        RegisterX86::RDI,
        RegisterX86::RSI,
        RegisterX86::RDX,
        RegisterX86::RCX,
        RegisterX86::R8,
        RegisterX86::R9,
    ];
    uc.reg_read(R[i]).unwrap_or(0)
}

fn ret(uc: &mut Unicorn<()>, v: u64) {
    let _ = uc.reg_write(RegisterX86::RAX, v);
}

fn rd(uc: &mut Unicorn<()>, addr: u64, len: usize) -> Vec<u8> {
    uc.mem_read_as_vec(addr, len).unwrap_or_default()
}

fn wr(uc: &mut Unicorn<()>, addr: u64, data: &[u8]) {
    let _ = uc.mem_write(addr, data);
}

/// Read a `__builtin_CFString` (isa, flags, char* str, long len) and return the
/// UTF-8 contents.
fn parse_cfstr(uc: &mut Unicorn<()>, ptr: u64) -> String {
    let hdr = rd(uc, ptr, 32);
    if hdr.len() < 32 {
        return String::new();
    }
    let str_ptr = u64::from_le_bytes(hdr[16..24].try_into().unwrap());
    let len = u64::from_le_bytes(hdr[24..32].try_into().unwrap()) as usize;
    String::from_utf8_lossy(&rd(uc, str_ptr, len)).into_owned()
}

/// Read a NUL-terminated C string (lazy 256-byte window, like nac.py).
fn parse_cstr(uc: &mut Unicorn<()>, ptr: u64) -> String {
    let data = rd(uc, ptr, 256);
    let end = data.iter().position(|&b| b == 0).unwrap_or(data.len());
    String::from_utf8_lossy(&data[..end]).into_owned()
}

// ---- dispatch -------------------------------------------------------------

/// Service a hooked symbol. Reads args from registers, writes RAX. The 0xC3 at
/// the hook address performs the actual `ret` once we return.
pub(crate) fn dispatch(uc: &mut Unicorn<()>, st: &mut NacState, name: &str) {
    match name {
        // ---- libc ----
        "_malloc" => {
            let len = arg(uc, 0);
            // mirror jelly.malloc, but heap_use lives in state here
            let addr = 0x40_0000 + st.heap_use;
            st.heap_use += len;
            ret(uc, addr);
        }
        "___memset_chk" => {
            let (dest, c, len) = (arg(uc, 0), arg(uc, 1) as u8, arg(uc, 2) as usize);
            wr(uc, dest, &vec![c; len]);
            ret(uc, 0);
        }
        "___bzero" => {
            let (ptr, len) = (arg(uc, 0), arg(uc, 1) as usize);
            wr(uc, ptr, &vec![0u8; len]);
            ret(uc, 0);
        }
        "_memcpy" => {
            let (dest, src, len) = (arg(uc, 0), arg(uc, 1), arg(uc, 2) as usize);
            let data = rd(uc, src, len);
            wr(uc, dest, &data);
            ret(uc, 0); // parity with nac.py (callers don't use the return)
        }
        "_sysctlbyname" | "_statfs$INODE64" | "_CFRelease" | "_IOObjectRelease" | "_free" => {
            ret(uc, 0);
        }
        "_arc4random" => {
            let r = st.next_rand() as u64;
            ret(uc, r);
        }

        // ---- IOKit ----
        "_IORegistryEntryFromPath" => ret(uc, 1),
        "_IORegistryEntryCreateCFProperty" => {
            let key_ptr = arg(uc, 1);
            let key = parse_cfstr(uc, key_ptr);
            match st.iokit.get(&key).cloned() {
                Some(obj) => {
                    let n = match &obj {
                        Cf::Data(d) => d.len(),
                        Cf::Str(s) => s.len(),
                        _ => 0,
                    };
                    log::debug!("IOKit {key} -> hit ({n})");
                    let h = st.push(obj);
                    ret(uc, h);
                }
                None => {
                    log::warn!("IOKit {key} -> MISS (returning NULL)");
                    ret(uc, 0);
                }
            }
        }
        "_IOServiceMatching" => {
            let name_ptr = arg(uc, 0);
            let name = parse_cstr(uc, name_ptr);
            let name_h = st.push(Cf::Str(name.clone()));
            let mut d = HashMap::new();
            d.insert("IOProviderClass".to_string(), Cf::Str(format!("@{name_h}")));
            let h = st.push(Cf::Dict(d));
            ret(uc, h);
        }
        "_IOServiceGetMatchingService" => ret(uc, 92),
        "_IOServiceGetMatchingServices" => {
            // existing iterator written to 3rd arg; arm the one-shot.
            let existing = arg(uc, 2);
            wr(uc, existing, &[93]);
            st.eth_hack = true;
            ret(uc, 0);
        }
        "_IOIteratorNext" => {
            if st.eth_hack {
                st.eth_hack = false;
                ret(uc, 94);
            } else {
                ret(uc, 0);
            }
        }
        "_IORegistryEntryGetParentEntry" => {
            let entry = arg(uc, 0);
            let parent = arg(uc, 2);
            wr(uc, parent, &[(entry + 100) as u8]);
            ret(uc, 0);
        }

        // ---- CoreFoundation: type IDs ----
        "_CFGetTypeID" => {
            let h = arg(uc, 0);
            let id = match st.get(h) {
                Some(Cf::Data(_)) => 1,
                Some(Cf::Str(_)) => 2,
                _ => 0,
            };
            ret(uc, id);
        }
        "_CFStringGetTypeID" => ret(uc, 2),
        "_CFDataGetTypeID" => ret(uc, 1),

        // ---- CoreFoundation: data ----
        "_CFDataGetLength" => {
            let h = arg(uc, 0);
            let len = match st.get(h) {
                Some(Cf::Data(d)) => d.len() as u64,
                _ => 0,
            };
            ret(uc, len);
        }
        "_CFDataGetBytes" => {
            let (h, start, end, buf) =
                (arg(uc, 0), arg(uc, 1) as usize, arg(uc, 2) as usize, arg(uc, 3));
            let slice = match st.get(h) {
                Some(Cf::Data(d)) => d.get(start..end.min(d.len())).map(|s| s.to_vec()),
                _ => None,
            };
            if let Some(s) = slice {
                wr(uc, buf, &s);
                ret(uc, s.len() as u64);
            } else {
                ret(uc, 0);
            }
        }

        // ---- CoreFoundation: strings ----
        "_CFStringGetLength" => {
            let h = arg(uc, 0);
            let len = match st.get(h) {
                Some(Cf::Str(s)) => s.chars().count() as u64,
                _ => 0,
            };
            ret(uc, len);
        }
        "_CFStringGetMaximumSizeForEncoding" => {
            // CFStringGetMaximumSizeForEncoding(length, encoding): return length (index 0).
            let v = arg(uc, 0);
            ret(uc, v);
        }
        "_CFStringGetCString" => {
            let (h, buf) = (arg(uc, 0), arg(uc, 1));
            let bytes = match st.get(h) {
                Some(Cf::Str(s)) => Some(s.as_bytes().to_vec()),
                _ => None,
            };
            if let Some(b) = bytes {
                wr(uc, buf, &b);
                ret(uc, b.len() as u64);
            } else {
                ret(uc, 0);
            }
        }
        "_CFUUIDCreateString" => {
            // CFUUIDCreateString(alloc, uuid): uuid is the 2nd arg (index 1).
            let v = arg(uc, 1);
            ret(uc, v);
        }

        // ---- CoreFoundation: dictionaries ----
        "_CFDictionaryCreateMutable" => {
            let h = st.push(Cf::Dict(HashMap::new()));
            ret(uc, h);
        }
        "_CFDictionaryGetValue" => {
            let (d, key) = (arg(uc, 0), arg(uc, 1));
            // 0xc3c3… is the value read through a `ret`-filled data symbol slot
            // (_kDADiskDescriptionVolumeUUIDKey); nac.py special-cases it.
            let key_str = if key == 0xC3C3_C3C3_C3C3_C3C3 {
                "DADiskDescriptionVolumeUUIDKey".to_string()
            } else {
                resolve_key(uc, st, key)
            };
            let val = match st.get(d) {
                Some(Cf::Dict(m)) => m.get(&key_str).cloned(),
                _ => None,
            };
            match val {
                Some(v) => {
                    let h = st.push(v);
                    ret(uc, h);
                }
                None => ret(uc, 0),
            }
        }
        "_CFDictionarySetValue" => {
            let (d, key, val) = (arg(uc, 0), arg(uc, 1), arg(uc, 2));
            let key_str = resolve_key(uc, st, key);
            let v = resolve_val(uc, st, val);
            if let Some(Cf::Dict(m)) = st.cf.get_mut((d.wrapping_sub(1)) as usize) {
                m.insert(key_str, v);
            }
            ret(uc, 0);
        }

        // ---- DiskArbitration ----
        "_DASessionCreate" => ret(uc, 201),
        "_DADiskCreateFromBSDName" => ret(uc, 202),
        "_DADiskCopyDescription" => {
            let mut m = HashMap::new();
            m.insert(
                "DADiskDescriptionVolumeUUIDKey".to_string(),
                Cf::Str(st.root_disk_uuid.clone()),
            );
            let h = st.push(Cf::Dict(m));
            ret(uc, h);
        }

        // ---- imported *data* symbols (never executed; bound only so their
        // GOT slots hold a mapped pointer). If somehow called, return 0. ----
        "___stack_chk_guard"
        | "_kIOMasterPortDefault"
        | "_kCFAllocatorDefault"
        | "_kDADiskDescriptionVolumeUUIDKey"
        | "_kCFBooleanTrue" => ret(uc, 0),

        other => {
            log::warn!("unhandled hook: {other}");
            ret(uc, 0);
        }
    }
}

/// Resolve a CF key argument to a Rust string. Keys arrive either as a tracked
/// CF Str handle, our internal `@<handle>` marker, or a raw CFString pointer.
fn resolve_key(uc: &mut Unicorn<()>, st: &NacState, key: u64) -> String {
    if let Some(Cf::Str(s)) = st.get(key) {
        return s.clone();
    }
    // Out-of-range handle: treat as a CFString constant pointer in the binary.
    if key > st.cf.len() as u64 {
        return parse_cfstr(uc, key);
    }
    String::new()
}

fn resolve_val(uc: &mut Unicorn<()>, st: &NacState, val: u64) -> Cf {
    if let Some(v) = st.get(val) {
        return v.clone();
    }
    // Best-effort: a CFString constant pointer, else opaque.
    if val > st.cf.len() as u64 {
        return Cf::Str(parse_cfstr(uc, val));
    }
    Cf::Str(format!("@{val}"))
}
