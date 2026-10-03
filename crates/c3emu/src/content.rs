//! The variant's content image (`*.image_*`, FPSX): the factory contents of the C:
//! drive (predefined Java apps, themes, Flash Lite apps, gallery, tones). After the TLV
//! header it is a series of data blocks, each
//!   0x54, 01 30 15 00 03 00, u16 checksum, u32, u32, u32 BE length, u32 BE address,
//!   u8 header checksum, then `length` bytes for byte `address` of the volume.
//! The volume is a FAT16 "superfloppy" (boot sector at address 0, no MBR; the boot
//! sector gives its size, 150656 sectors for RM-614). Only used areas are stored.
//! It is written into the storage image of the C: device (XSR device 0, which the OS
//! otherwise formats itself as an empty "Mass Memory") before the first boot.

use std::io::{Seek, SeekFrom, Write};
use std::path::Path;

const BLOCK_MAGIC: [u8; 7] = [0x54, 0x01, 0x30, 0x15, 0x00, 0x03, 0x00];
const HEADER_LEN: usize = 26;

pub struct Content {
    pub blocks: Vec<(u64, Vec<u8>)>,
    /// Volume size in bytes (from the FAT boot sector).
    pub volume_bytes: u64,
}

pub fn parse(data: &[u8]) -> Result<Content, String> {
    let start = data.windows(BLOCK_MAGIC.len()).position(|w| w == BLOCK_MAGIC)
        .ok_or("no data blocks (not a content image?)")?;
    let mut blocks = Vec::new();
    let mut i = start;
    while i + HEADER_LEN <= data.len() {
        if data[i..i + BLOCK_MAGIC.len()] != BLOCK_MAGIC {
            break;
        }
        let be = |o: usize| u32::from_be_bytes(data[i + o..i + o + 4].try_into().unwrap()) as usize;
        let (len, addr) = (be(17), be(21));
        let body = i + HEADER_LEN;
        if body + len > data.len() {
            return Err(format!("block @0x{i:x} runs past the end"));
        }
        blocks.push((addr as u64, data[body..body + len].to_vec()));
        i = body + len;
    }
    let boot = blocks.iter().find(|(a, _)| *a == 0).map(|(_, d)| d).ok_or("no boot sector")?;
    if boot.len() < 512 || boot[510] != 0x55 || boot[511] != 0xAA {
        return Err("block 0 is not a FAT boot sector".into());
    }
    let bps = u16::from_le_bytes([boot[11], boot[12]]) as u64;
    let tot16 = u16::from_le_bytes([boot[19], boot[20]]) as u64;
    let tot32 = u32::from_le_bytes(boot[32..36].try_into().unwrap()) as u64;
    let volume_bytes = bps * if tot16 != 0 { tot16 } else { tot32 };
    Ok(Content { blocks, volume_bytes })
}

/// Create `<dir>/dev0.img` from the content image unless it already exists (the flash
/// persists: the factory contents are installed once, like flashing the phone).
pub fn install(image: &Path, dir: &Path, min_bytes: u64) -> Result<Option<(usize, u64)>, String> {
    let target = dir.join("dev0.img");
    if target.exists() {
        return Ok(None);
    }
    let data = std::fs::read(image).map_err(|e| format!("{}: {e}", image.display()))?;
    let c = parse(&data)?;
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let mut f = std::fs::File::create(&target).map_err(|e| e.to_string())?;
    f.set_len(c.volume_bytes.max(min_bytes)).map_err(|e| e.to_string())?;
    for (addr, d) in &c.blocks {
        f.seek(SeekFrom::Start(*addr)).map_err(|e| e.to_string())?;
        f.write_all(d).map_err(|e| e.to_string())?;
    }
    Ok(Some((c.blocks.len(), c.volume_bytes)))
}

/// Build a fresh in-memory C: drive from the content image (the web build).
pub fn to_disk(data: &[u8], min_bytes: u64) -> Result<crate::storage::SparseDisk, String> {
    let c = parse(data)?;
    let mut d = crate::storage::SparseDisk::new(c.volume_bytes.max(min_bytes));
    for (addr, b) in &c.blocks {
        d.write_at(*addr, b);
    }
    Ok(d)
}
