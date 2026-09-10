//! SMBIOS Type 1 — §3.2.
//!
//! Invariant: the `serial_number` string equals `vm.name` byte-for-byte —
//! no hash, no prefix, no UUID substitution. Windows OEM activation reads
//! this field, so any transformation breaks it.

use libvmm_config::Vm;

pub const SMBIOS_TYPE_SYSTEM: u8 = 1;

/// String-table indices used by the Type 1 structure (1-based; 0 = "none").
const STR_MANUFACTURER: u8 = 1;
const STR_PRODUCT_NAME: u8 = 2;
const STR_VERSION: u8 = 3;
const STR_SERIAL_NUMBER: u8 = 4;
const STR_SKU: u8 = 5;
const STR_FAMILY: u8 = 6;

/// Formatted area of SMBIOS Type 1, before the string table.
///
/// ```text
/// struct smbios_type1 { u8 type=1; u8 length; u16 handle;
///    u8 manufacturer; u8 product_name; u8 version;
///    u8 serial_number; u8 uuid[16];
///    u8 wakeup_type; u8 sku; u8 family; }
/// ```
const TYPE1_FORMATTED_LEN: u8 = 27;

/// `Power Switch` — the standard wake-up type for a virtual machine.
const WAKEUP_POWER_SWITCH: u8 = 0x06;

pub struct SmbiosType1 {
    pub manufacturer: String,
    pub product_name: String,
    pub version: String,
    /// MUST equal `vm.name` verbatim.
    pub serial_number: String,
    /// MUST equal `vm.id`.
    pub uuid: [u8; 16],
    pub sku: String,
    pub family: String,
}

impl SmbiosType1 {
    /// Build the Type 1 structure for a machine.
    ///
    /// `vm.id` has already been validated as a UUID by the config layer; a
    /// parse failure here falls back to the nil UUID rather than panicking,
    /// because this runs on the boot path.
    pub fn from_config(vm: &Vm) -> Self {
        let uuid = uuid::Uuid::parse_str(&vm.id)
            .map(|u| *u.as_bytes())
            .unwrap_or([0u8; 16]);
        SmbiosType1 {
            manufacturer: "RUST".to_string(),
            product_name: "Legacy-Free KVM Machine".to_string(),
            version: "1.0".to_string(),
            // §3.2 invariant — verbatim, no transformation.
            serial_number: vm.name.clone(),
            uuid,
            sku: "vmm".to_string(),
            family: "Virtual".to_string(),
        }
    }

    /// Serialise the formatted area followed by the double-NUL-terminated
    /// string table.
    pub fn to_bytes(&self, handle: u16) -> Vec<u8> {
        let mut out = Vec::with_capacity(64);
        out.push(SMBIOS_TYPE_SYSTEM);
        out.push(TYPE1_FORMATTED_LEN);
        out.extend_from_slice(&handle.to_le_bytes());
        out.push(STR_MANUFACTURER);
        out.push(STR_PRODUCT_NAME);
        out.push(STR_VERSION);
        out.push(STR_SERIAL_NUMBER);
        // SMBIOS stores the UUID with the first three fields little-endian.
        out.extend_from_slice(&smbios_uuid_bytes(&self.uuid));
        out.push(WAKEUP_POWER_SWITCH);
        out.push(STR_SKU);
        out.push(STR_FAMILY);
        debug_assert_eq!(out.len(), TYPE1_FORMATTED_LEN as usize);

        for s in [
            &self.manufacturer,
            &self.product_name,
            &self.version,
            &self.serial_number,
            &self.sku,
            &self.family,
        ] {
            out.extend_from_slice(s.as_bytes());
            out.push(0);
        }
        // Terminate the string table.
        out.push(0);
        out
    }

    /// Read the serial back out of a serialised structure. Used by the test
    /// that pins the §3.2 verbatim invariant.
    pub fn serial_from_bytes(bytes: &[u8]) -> Option<String> {
        let formatted_len = *bytes.get(1)? as usize;
        // type(1) length(1) handle(2) manufacturer(1) product(1) version(1)
        // puts serial_number at offset 7.
        let serial_index = *bytes.get(7)? as usize;
        if serial_index == 0 {
            return None;
        }
        let table = bytes.get(formatted_len..)?;
        table
            .split(|b| *b == 0)
            .nth(serial_index - 1)
            .map(|s| String::from_utf8_lossy(s).into_owned())
    }
}

/// SMBIOS/EFI store the UUID's first three fields little-endian; RFC 4122
/// byte order is big-endian. Convert so a guest reports the configured UUID.
fn smbios_uuid_bytes(rfc: &[u8; 16]) -> [u8; 16] {
    let mut o = *rfc;
    o[0..4].copy_from_slice(&[rfc[3], rfc[2], rfc[1], rfc[0]]);
    o[4..6].copy_from_slice(&[rfc[5], rfc[4]]);
    o[6..8].copy_from_slice(&[rfc[7], rfc[6]]);
    o
}
