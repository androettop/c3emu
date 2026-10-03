//! BB5 signed-image header (0x400 bytes, magic 0x809795a3): the OS entry point.

use crate::layout::BB5_MAGIC;

fn word(img: &[u8], off: usize) -> Option<u32> {
    img.get(off..off + 4).map(|b| u32::from_le_bytes(b.try_into().unwrap()))
}

#[derive(Debug, Clone, Copy)]
pub struct OsEntry {
    /// Cold-start entry (reset vector target).
    pub entry: u32,
    /// File offset of the ARM vector table (header word +0x1C).
    pub vector_table: u32,
    /// base + header word +0x148, an independent copy of the entry offset.
    pub header_hint: u32,
}

/// Decode the cold-start entry of a BB5 OS image placed at `base`.
///
/// For OS-type images header +0x1C is the file offset of an ARM vector table made of
/// `ldr pc,[pc,#imm]`; vector 0 (reset) points at the cold start.
pub fn os_entry(img: &[u8], base: u32) -> Result<OsEntry, String> {
    if word(img, 0) != Some(BB5_MAGIC) {
        return Err("not a BB5 signed image".into());
    }
    let vt = word(img, 0x1C).ok_or("short header")?;
    let insn = word(img, vt as usize).ok_or("vector table out of range")?;
    if insn & 0xFFFF_F000 != 0xE59F_F000 {
        return Err(format!("vector 0 at +0x{vt:x} is not `ldr pc,[pc,#imm]` (0x{insn:08x})"));
    }
    let lit = vt as usize + 8 + (insn & 0xFFF) as usize;
    let entry = word(img, lit).ok_or("reset literal out of range")?;
    let hint = word(img, 0x148).ok_or("short header")?;
    Ok(OsEntry { entry, vector_table: vt, header_hint: base.wrapping_add(hint) })
}
