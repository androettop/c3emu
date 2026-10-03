//! Call-site patches for steps that cannot succeed without hardware we don't emulate.
//! Each entry: the instruction is skipped and r0 gets the given value. Keep the
//! evidence next to every patch; prefer modeling the device when it becomes feasible.

use crate::machine::Machine;

pub struct Patch {
    pub addr: u64,
    pub len: u64,
    pub r0: u32,
    pub name: &'static str,
}

pub const PATCHES: &[Patch] = &[
    // phyframe.c:1073 — L1 init (0x807CDBFC) sends DSP cmd 0x1C "get version ID"
    // (0x807F31A8) and waits on a semaphore for the DSP's reply:
    //   bl 0x80829044(sem, 0x400 ticks, file, line) -> 1 on success.
    // No DSP firmware runs, so the reply never comes. Report success.
    Patch { addr: 0x807C_DC3A, len: 4, r0: 1, name: "patch:dsp_version_wait" },
];

/// Busy-wait delays calibrated for the real CPU; skipped (pure time waste here).
pub const DELAYS: &[Patch] = &[
    // i2c.c: after each transfer, two loops of 100 empty iterations (0x8077CC00..0x8077CC0F);
    // r0/r1 are dead afterwards (r0 reloaded with #5 at 0x8077CC10).
    Patch { addr: 0x8077_CC00, len: 0x10, r0: 0, name: "skip:i2c_delay" },
];
/// Delay *functions* returned from immediately: delay(n) @0x80819C14 (subs/bne/bx lr),
/// used around WFI in the idle routine.
pub const DELAY_FNS: &[u64] = &[0x8081_9C14];

pub fn install(m: &mut Machine) {
    for p in PATCHES.iter().chain(DELAYS) {
        m.hle_skip(p.addr, p.len, p.r0, p.name);
    }
    for &f in DELAY_FNS {
        m.hle_fn(f, "skip:delay_fn", Box::new(|_uc: &mut crate::uc::Uc<'_>,
                                             _s: &mut crate::machine::State| Some(0)));
    }
}
