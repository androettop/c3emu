//! Guest virtual memory access. Unicorn's mem_read/mem_write take physical addresses;
//! once the OS enables the MMU, the pointers it hands us are virtual.

use crate::uc::{RegisterARMCP, Uc};

/// ARMv5 (ARM926) MMU walk: virtual -> physical using TTBR0. Returns None if unmapped.
/// Unicorn's mem_read takes physical addresses, the guest's pointers are virtual.
pub fn virt_to_phys(uc: &Uc<'_>, va: u32) -> Option<u32> {
    let mut sctlr = RegisterARMCP { cp: 15, crn: 1, ..Default::default() };
    let _ = uc.reg_read_arm_coproc(&mut sctlr);
    if sctlr.val & 1 == 0 {
        return Some(va);
    }
    let mut ttbr = RegisterARMCP { cp: 15, crn: 2, ..Default::default() };
    let _ = uc.reg_read_arm_coproc(&mut ttbr);
    let l1 = phys_word(uc, (ttbr.val as u32 & 0xFFFF_C000) | ((va >> 20) << 2));
    match l1 & 3 {
        2 => Some((l1 & 0xFFF0_0000) | (va & 0x000F_FFFF)), // section
        1 => {
            // coarse table: 256 entries
            let l2 = phys_word(uc, (l1 & 0xFFFF_FC00) | (((va >> 12) & 0xFF) << 2));
            page(l2, va)
        }
        3 => {
            // fine table: 1024 entries
            let l2 = phys_word(uc, (l1 & 0xFFFF_F000) | (((va >> 10) & 0x3FF) << 2));
            page(l2, va)
        }
        _ => None,
    }
}

fn page(l2: u32, va: u32) -> Option<u32> {
    match l2 & 3 {
        1 => Some((l2 & 0xFFFF_0000) | (va & 0xFFFF)), // large 64K
        2 => Some((l2 & 0xFFFF_F000) | (va & 0xFFF)),  // small 4K
        3 => Some((l2 & 0xFFFF_FC00) | (va & 0x3FF)),  // tiny 1K
        _ => None,
    }
}

pub fn phys_word(uc: &Uc<'_>, pa: u32) -> u32 {
    let mut b = [0u8; 4];
    let _ = uc.mem_read(pa as u64, &mut b);
    u32::from_le_bytes(b)
}

/// Read a 32-bit word at a guest virtual address.
pub fn virt_word(uc: &Uc<'_>, va: u32) -> Option<u32> {
    virt_to_phys(uc, va).map(|pa| phys_word(uc, pa))
}


/// Copy guest virtual memory into `out` (page by page). False if any page is unmapped.
pub fn read_virt(uc: &Uc<'_>, va: u32, out: &mut [u8]) -> bool {
    let mut done = 0usize;
    while done < out.len() {
        let a = va.wrapping_add(done as u32);
        let Some(pa) = virt_to_phys(uc, a) else { return false };
        let n = (0x400 - (a as usize & 0x3FF)).min(out.len() - done); // 1 KB: smallest page
        if uc.mem_read(pa as u64, &mut out[done..done + n]).is_err() {
            return false;
        }
        done += n;
    }
    true
}

/// Copy `data` into guest virtual memory (page by page). False if any page is unmapped.
pub fn write_virt(uc: &mut Uc<'_>, va: u32, data: &[u8]) -> bool {
    let mut done = 0usize;
    while done < data.len() {
        let a = va.wrapping_add(done as u32);
        let Some(pa) = virt_to_phys(uc, a) else { return false };
        let n = (0x400 - (a as usize & 0x3FF)).min(data.len() - done);
        if uc.mem_write(pa as u64, &data[done..done + n]).is_err() {
            return false;
        }
        done += n;
    }
    true
}
