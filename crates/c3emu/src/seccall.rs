//! Protected-application calls through the boot-ROM secure service (service 0x12,
//! r1 = 0x1F), made by the OS PA client 0x805EA8BE (e.g. "PA_CRYPT"). The real PAs run
//! signed code in the secure world with device keys; the emulator has neither, so the
//! call is answered at its interface: status OK and the data passed through unchanged.
//!
//! Request descriptor (physical address, built on the client's stack):
//!   +0x02 u8 0x0C, +0x04 operation, +0x08 parameter, +0x0C input (phys), +0x10 input
//!   length, +0x14 buffer size, +0x18 status (out, 1 = OK), +0x1C output (phys),
//!   +0x20 output length (out).
//! The client returns the ROM's r0, and its caller (0x803F9582) requires r0 == 1 and
//! status == 1, then copies `output length` bytes from the output buffer.

use crate::uc::Uc;

use crate::machine::State;

/// Return address of the PA client's ROM call (blx r4 @0x805EA9A0).
pub const PA_CALL_RET: u64 = 0x805E_A9A2;

pub fn pa_call(uc: &mut Uc<'_>, s: &mut State, desc: u32, _status: u32) -> u32 {
    let mut d = [0u8; 0x24];
    if uc.mem_read(desc as u64, &mut d).is_err() {
        return 0;
    }
    let w = |o: usize| u32::from_le_bytes(d[o..o + 4].try_into().unwrap());
    let (op, param, input, len, out) = (w(0x04), w(0x08), w(0x0C), w(0x10), w(0x1C));
    let mut data = vec![0u8; len.min(0x10000) as usize];
    let _ = uc.mem_read(input as u64, &mut data);
    let _ = uc.mem_write(out as u64, &data);
    let _ = uc.mem_write(desc as u64 + 0x18, &1u32.to_le_bytes());
    let _ = uc.mem_write(desc as u64 + 0x20, &len.to_le_bytes());
    *s.hle_calls.entry(format!("pa_call op 0x{op:x}")).or_default() += 1;
    s.event(format!("PA call op=0x{op:x} param=0x{param:x} len={len}: passed through"));
    1
}
