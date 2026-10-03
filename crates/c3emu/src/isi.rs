//! Trace of Nokia ISI (PhoNet) messages between the OS servers.
//!
//! 0x8037C304(msg) sends an ISI message (found in MTC @0x803E6EE8, which builds
//! [3] = 0x15 PN_MTC, [4..6] = length, [6]/[7] = objects, [9] = message id). Header:
//!   +0 media, +1 receiver device, +2 sender device, +3 resource, +4 u16 length (big
//!   endian on the wire; stored little endian here), +6 receiver object, +7 sender
//!   object, +8 transaction id, +9 message id, +10.. payload.

use crate::uc::{RegisterARM, Uc};

use crate::machine::{Machine, State};
use crate::mmu::read_virt;

pub const ISI_SEND: u64 = 0x8037_C304;
/// Return of the OS message receive (0x8054D41C): r0 = the received message. Sees
/// local (same-CPU) messages too.
pub const ISI_RECV_RET: u64 = 0x8054_D4E8;

pub fn install(m: &mut Machine, echo: bool) {
    m.hle_fn(ISI_SEND, "isi_send", Box::new(move |uc: &mut Uc<'_>, s: &mut State| {
        let p = uc.reg_read(RegisterARM::R0).unwrap_or(0) as u32;
        let mut h = [0u8; 72];
        if read_virt(uc, p, &mut h) {
            let len = u16::from_le_bytes([h[4], h[5]]);
            let payload: Vec<String> = h[10..(6 + len as usize).clamp(10, 72)].iter().map(|b| format!("{b:02x}")).collect();
            let lr = uc.reg_read(RegisterARM::LR).unwrap_or(0);
            let line = format!("ISI res=0x{:02x} id=0x{:02x} {:02x}:{:02x} -> {:02x}:{:02x} tr={:02x} len={len} [{}] from 0x{lr:08x}",
                               h[3], h[9], h[2], h[7], h[1], h[6], h[8], payload.join(" "));
            *s.isi_counts.entry((h[3], h[9])).or_default() += 1;
            if echo {
                println!("[isi {:>10}] {line}", s.icount);
            }
            if s.oslog.len() == crate::oslog::MAX_LINES {
                s.oslog.pop_front();
            }
            s.oslog.push_back((s.icount, line));
        }
        None
    }));
    m.hle_fn(ISI_RECV_RET, "isi_recv", Box::new(move |uc: &mut Uc<'_>, s: &mut State| {
        let p = uc.reg_read(RegisterARM::R0).unwrap_or(0) as u32;
        let mut h = [0u8; 72];
        if p >= 0x8000_0000 && read_virt(uc, p, &mut h) {
            let len = u16::from_le_bytes([h[4], h[5]]);
            let pl: Vec<String> = h[10..(6 + len as usize).clamp(10, 72)].iter().map(|b| format!("{b:02x}")).collect();
            let mut line = format!("RECV res=0x{:02x} id=0x{:02x} {:02x}:{:02x} -> {:02x}:{:02x} tr={:02x} len={len} [{}]",
                                   h[3], h[9], h[2], h[7], h[1], h[6], h[8], pl.join(" "));
            // text payloads (trace / error strings): show them
            let mut t = [0u8; 160];
            if h[10..14].iter().all(|b| b.is_ascii_graphic() || *b == b' ') && read_virt(uc, p + 10, &mut t) {
                let end = t.iter().position(|&b| b == 0 || !(b.is_ascii_graphic() || b == b' ')).unwrap_or(t.len());
                line += &format!(" \"{}\"", String::from_utf8_lossy(&t[..end]));
            }
            if echo {
                println!("[isi {:>10}] {line}", s.icount);
            }
        }
        None
    }));
}
