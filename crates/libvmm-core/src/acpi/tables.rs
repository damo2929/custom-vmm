//! ACPI system description table primitives.

/// Every ACPI table except the RSDP starts with this 36-byte header.
pub const HEADER_LEN: usize = 36;

/// Offset of the single-byte checksum field within an SDT header.
pub const CHECKSUM_OFFSET: usize = 9;

/// A description-table header, laid out exactly as ACPI defines it.
#[derive(Debug, Clone)]
pub struct SdtHeader {
    pub signature: [u8; 4],
    pub length: u32,
    pub revision: u8,
    pub oem_id: [u8; 6],
    pub oem_table_id: [u8; 8],
    pub oem_revision: u32,
    pub creator_id: [u8; 4],
    pub creator_revision: u32,
}

impl SdtHeader {
    pub fn new(signature: &[u8; 4], revision: u8, oem_table_id: &[u8; 8]) -> Self {
        SdtHeader {
            signature: *signature,
            // Patched once the body length is known.
            length: HEADER_LEN as u32,
            revision,
            oem_id: *b"RUSTVM",
            oem_table_id: *oem_table_id,
            oem_revision: 1,
            creator_id: *b"RVMM",
            creator_revision: 1,
        }
    }

    /// Emit the header with a zero checksum; the caller patches length and
    /// checksum with [`finalize`] once the body is appended.
    pub fn write_into(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.signature);
        out.extend_from_slice(&self.length.to_le_bytes());
        out.push(self.revision);
        out.push(0); // checksum, patched by finalize()
        out.extend_from_slice(&self.oem_id);
        out.extend_from_slice(&self.oem_table_id);
        out.extend_from_slice(&self.oem_revision.to_le_bytes());
        out.extend_from_slice(&self.creator_id);
        out.extend_from_slice(&self.creator_revision.to_le_bytes());
        debug_assert_eq!(out.len() % HEADER_LEN, 0);
    }
}

/// Patch a completed table's `length` field and compute its checksum so the
/// whole table sums to zero mod 256.
pub fn finalize(table: &mut [u8]) {
    let len = table.len() as u32;
    table[4..8].copy_from_slice(&len.to_le_bytes());
    table[CHECKSUM_OFFSET] = 0;
    table[CHECKSUM_OFFSET] = checksum8(table);
}

/// The value that makes `bytes` sum to zero mod 256.
pub fn checksum8(bytes: &[u8]) -> u8 {
    let sum = bytes.iter().fold(0u8, |a, b| a.wrapping_add(*b));
    (0u8).wrapping_sub(sum)
}

/// §3.3: every table MUST pass an 8-bit sum-to-zero check before linkage.
pub fn verify_checksum(bytes: &[u8]) -> bool {
    !bytes.is_empty() && bytes.iter().fold(0u8, |a, b| a.wrapping_add(*b)) == 0
}

/// Signature of a table blob, for logs and error messages.
pub fn signature_of(bytes: &[u8]) -> String {
    if bytes.len() < 4 {
        return "????".to_string();
    }
    String::from_utf8_lossy(&bytes[0..4]).into_owned()
}

/// The table's self-declared length, if it has a full header.
pub fn declared_length(bytes: &[u8]) -> Option<u32> {
    (bytes.len() >= HEADER_LEN)
        .then(|| u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]))
}
