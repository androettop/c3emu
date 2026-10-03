//! The loader's hand-over to the OS ("HW configuration written to SDRAM"), rebuilt by
//! the HLE loader. r2 at the OS entry points to a tag list (stored at 0x80F5AF18) that
//! 0x8095F5CC(type, n) walks: {u32 type, u32 size, payload...}; type 2 = end,
//! type 3 = link (next block at +8), otherwise advance by `size`.
//!
//! Tags the OS reads (all callers of 0x8095F5CC):
//!   0x0D  power-on reason, word at +8. Bit 0 = power key (0x80696A4C: "MTC_Prod_Init:
//!         PWR key startup: Show BootLogo"); bits 16-19 are read by rtc_chipset_api.
//!   0x0A  pointer (+8) to the table of images the loader placed in RAM (stored by
//!         0x806FD0FE, looked up by name by 0x806FD0B4 / 0x806FD0F8).
//!   0x1F  optional (callers check for NULL); not provided.
//!
//! Image table: 0x20-byte entries {+0 offset of the image from the table itself, +4 size,
//! +0x14 name[12]}, terminated by name[0] == 0xFF. (get_addr @0x806FD0E6 returns
//! entry[0] + table base.) Names the OS looks up: MCUSW, GENIO_INIT, PAPUBKEYS, PASUBTOC (in the
//! FPSX package) and CCC, HWC, NPC (per-device data from the phone's protected area —
//! not available, left out). Images are placed whole, BB5 header included: callers
//! read e.g. +0x148 (the header's entry offset).

use crate::fpsx::Fpsx;
use crate::layout::*;
use crate::machine::Machine;

pub const TAG_LINK: u32 = 3;
pub const TAG_IMAGES: u32 = 0x0A;
pub const TAG_POWER_ON: u32 = 0x0D;
/// Power-on reason bit: power key.
pub const POWER_ON_KEY: u32 = 1;

/// Images copied into RAM for the OS (name, destination). MCUSW is already at its link
/// base; the others go just below the boot-info page, in 0x80071000-0x80120000 which the
/// OS never touched over 1.5G insns. (0x80010000 was overwritten by the audio driver —
/// CHIPSET_ copies DSP data there — which wiped PASUBTOC and made PA_CRYPT disappear.)
const PLACED_IMAGES: &[(&str, u64)] = &[
    ("PASUBTOC", 0x8010_0000),
    ("PAPUBKEYS", 0x8010_F000),
    ("GENIO_INIT", 0x8010_F800),
];
/// Image table location (inside the boot-info page).
const IMAGE_TABLE: u64 = BOOTINFO + 0x800;

fn tag(out: &mut Vec<u8>, ty: u32, payload: &[u32]) {
    let size = 8 + 4 * payload.len() as u32;
    out.extend(ty.to_le_bytes());
    out.extend(size.to_le_bytes());
    for w in payload {
        out.extend(w.to_le_bytes());
    }
}

/// Place the images, build the tag list and image table. Returns a summary per image.
pub fn install(m: &mut Machine, fw: &Fpsx) -> Result<Vec<String>, String> {
    let mut table = Vec::new();
    let mut summary = Vec::new();
    let mut entry = |name: &str, addr: u64, size: usize| {
        let mut e = [0u8; 0x20];
        e[0..4].copy_from_slice(&(addr as u32).wrapping_sub(IMAGE_TABLE as u32).to_le_bytes());
        e[4..8].copy_from_slice(&(size as u32).to_le_bytes());
        e[0x14..0x14 + name.len().min(12)].copy_from_slice(&name.as_bytes()[..name.len().min(12)]);
        table.extend_from_slice(&e);
        summary.push(format!("{name}@0x{addr:08x}+0x{size:x}"));
    };
    let mcusw = fw.image("MCUSW").ok_or("no MCUSW")?;
    entry("MCUSW", MCUSW_BASE, mcusw.data.len());
    for &(name, addr) in PLACED_IMAGES {
        let img = fw.image(name).ok_or_else(|| format!("image {name} not in firmware"))?;
        m.load(&img.data, addr)?;
        entry(name, addr, img.data.len());
    }
    let mut end = [0u8; 0x20];
    end[0x14] = 0xFF;
    table.extend_from_slice(&end);
    m.load(&table, IMAGE_TABLE)?;

    let mut tags = Vec::new();
    tag(&mut tags, TAG_POWER_ON, &[POWER_ON_KEY]);
    tag(&mut tags, TAG_IMAGES, &[IMAGE_TABLE as u32]);
    tag(&mut tags, TAG_END, &[]);
    let mut page = vec![0u8; 0x800];
    page[..tags.len()].copy_from_slice(&tags);
    m.load(&page, BOOTINFO)?;
    Ok(summary)
}
