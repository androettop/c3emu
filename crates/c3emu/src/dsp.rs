//! HLE of the DSP ("pg5") side of the MCU<->DSP shared-memory protocol.
//!
//! The DSP firmware is not executed (different core). On each doorbell this model
//! consumes the MCU's command ring, logs each command, and answers the commands listed
//! in REPLIES through the status ring.

use crate::uc::Uc;

use crate::layout::*;
use crate::machine::State;

/// Command -> status reply {status, arg0, arg1, arg2}. Evidence per entry.
const REPLIES: &[(u16, [u16; 4])] = &[
    // audvoc_codec.c: MSC sends cmd 0x6D and polls [0x80F6CFF8], which the audio status
    // handler 0x8071EBC4 sets on status 0x25 with arg0 == 0xAAAA, arg1 == 0xBBBB
    // (check @0x8071F0B2): a DSP alive handshake.
    (0x6D, [0x25, 0xAAAA, 0xBBBB, 0]),
];

/// Command 0x0B {addr, tag, 0}: read a DSP data-memory word (sender 0x807F32D4, used by
/// hal_audio_sublayer_common.c 0x80778208 to read DSP_AUDIO_AMCR at 0xE540, tag 0xAA).
/// The DSP answers with status 3 {addr, tag, value}: the MCU status dispatcher
/// 0x807F3AE6 (case 3 -> 0x807F3DE6) checks addr/tag, stores the value at 0x80F65E94 and
/// releases the waiter's semaphore (0x80F65EA0).
const CMD_READ_MEM: u16 = 0x0B;
const STATUS_READ_MEM: u16 = 3;

/// "Write data memory": cmd 0x52 {addr, mask, value} = read-modify-write of a DSP word
/// (e.g. 0052 e7f8 c000 4000, then 0052 e7f8 c000 0000).
const CMD_WRITE_MEM: u16 = 0x52;
/// cmd 0x15 {1 | 0}: audio block on / off.
const CMD_AUDIO_ENABLE: u16 = 0x15;
const AMCR: u16 = 0xE540;
const AMCR_ENABLED: u16 = 0x0020;

/// DSP data memory as the MCU observes it: what the MCU wrote (cmd 0x52), else the
/// reset value. AMCR (0xE540): "Enable AMCR" (0x80773198) polls until bit 5 is set
/// (enabled); it comes up enabled (provisional).
fn dsp_mem_read(s: &State, addr: u16) -> u16 {
    if let Some(&v) = s.dsp_mem.get(&addr) {
        return v;
    }
    match addr {
        0xE540 => 0x0020,
        _ => 0,
    }
}

fn rd16(uc: &Uc<'_>, a: u64) -> u16 {
    let mut b = [0u8; 2];
    let _ = uc.mem_read(a, &mut b);
    u16::from_le_bytes(b)
}

/// Post a status entry to the MCU and raise the DSP interrupt.
pub fn push_status(uc: &mut Uc<'_>, s: &mut State, st: [u16; 4]) {
    let i = rd16(uc, DSP_STATQ_IN) % DSP_STATQ_LEN;
    let bytes: Vec<u8> = st.iter().flat_map(|h| h.to_le_bytes()).collect();
    let _ = uc.mem_write(DSP_STATQ + 8 * i as u64, &bytes);
    let _ = uc.mem_write(DSP_STATQ_IN, &((i + 1) % DSP_STATQ_LEN).to_le_bytes());
    let _ = uc.mem_write(DSP_STATUS_FLAG, &1u16.to_le_bytes());
    s.soc.intc_raw[0] |= 1 << DSP_IRQ;
    *s.hle_calls.entry(format!("dsp_status 0x{:x}", st[0])).or_default() += 1;
}

/// Bus write hook: the MCU rings the DSP (IPC_DOORBELL |= 9) after posting commands
/// (0x808185E4, called from the sender 0x8080927C). Consume every entry between the
/// ring's out and in indices, answer the known ones, and signal the MCU.
pub fn write(uc: &mut Uc<'_>, s: &mut State, addr: u64) {
    if addr != IPC_DOORBELL {
        return;
    }
    *s.hle_calls.entry("dsp_doorbell".into()).or_default() += 1;
    drain_fast_queue(uc, s);
    let new_in = rd16(uc, DSP_CMDQ_IN) % DSP_CMDQ_LEN;
    let mut out = rd16(uc, DSP_CMDQ_OUT) % DSP_CMDQ_LEN;
    if out == new_in {
        return;
    }
    while out != new_in {
        let mut cmd = [0u8; 8];
        let _ = uc.mem_read(DSP_CMDQ + 8 * out as u64, &mut cmd);
        *s.hle_calls.entry("dsp_cmd".into()).or_default() += 1;
        let id = u16::from_le_bytes([cmd[0], cmd[1]]);
        *s.dsp_cmds.entry(id).or_default() += 1;
        let arg = |i: usize| u16::from_le_bytes([cmd[2 * i], cmd[2 * i + 1]]);
        crate::audio::command(uc, s, id, [arg(1), arg(2), arg(3)]);
        if let Some(&(_, reply)) = REPLIES.iter().find(|(c, _)| *c == id) {
            push_status(uc, s, reply);
        } else if id == CMD_READ_MEM {
            let (addr, tag) = (arg(1), arg(2));
            let v = dsp_mem_read(s, addr);
            push_status(uc, s, [STATUS_READ_MEM, addr, tag, v]);
        } else if id == CMD_AUDIO_ENABLE {
            // the DSP reflects the audio block state in AMCR bit 5 (CHIPSET_ polls it
            // with cmd 0x0B after sending 0x15 1)
            let old = dsp_mem_read(s, AMCR);
            let new = if arg(1) != 0 { old | AMCR_ENABLED } else { old & !AMCR_ENABLED };
            s.dsp_mem.insert(AMCR, new);
        } else if id == CMD_WRITE_MEM {
            let (addr, mask, val) = (arg(1), arg(2), arg(3));
            let old = dsp_mem_read(s, addr);
            s.dsp_mem.insert(addr, (old & !mask) | (val & mask));
        }
        if s.hle_calls["dsp_cmd"] <= 32 || (id != 0x0C && s.icount >= s.events_from && s.events_from > 0) {
            let words: Vec<String> = cmd.chunks(2).map(|h| format!("{:04x}", u16::from_le_bytes([h[0], h[1]]))).collect();
            s.event(format!("DSP cmd[{out}] {}", words.join(" ")));
        }
        out = (out + 1) % DSP_CMDQ_LEN;
    }
    let _ = uc.mem_write(DSP_CMDQ_OUT, &out.to_le_bytes());
    s.soc.intc_raw[0] |= 1 << DSP_IRQ; // DSP signals the MCU
}

/// INT1 fast command queue (shared_fast_cmdq, after the status queue): 16 entries of
/// 8 bytes at 0x8000C508, in/out indices at 0x8000C588/0x8000C58A. The MCU waits at most
/// 10 OS ticks for room (sharedmem.c:1263 'FALSE' assert otherwise), so it must be
/// drained like the main queue. FastCommand_t: 0 READ, 1 WRITE, 2 BITWISE_WRITE, ...
const FAST_CMDQ: u64 = 0x8000_C508;
const FAST_CMDQ_IN: u64 = 0x8000_C588;
const FAST_CMDQ_OUT: u64 = 0x8000_C58A;
const FAST_CMDQ_LEN: u16 = 16;

fn drain_fast_queue(uc: &mut Uc<'_>, s: &mut State) {
    let new_in = rd16(uc, FAST_CMDQ_IN) % FAST_CMDQ_LEN;
    let mut out = rd16(uc, FAST_CMDQ_OUT) % FAST_CMDQ_LEN;
    while out != new_in {
        let mut cmd = [0u8; 8];
        let _ = uc.mem_read(FAST_CMDQ + 8 * out as u64, &mut cmd);
        let arg = |i: usize| u16::from_le_bytes([cmd[2 * i], cmd[2 * i + 1]]);
        *s.hle_calls.entry(format!("dsp_fast_cmd 0x{:x}", arg(0))).or_default() += 1;
        match arg(0) {
            0 => {
                let v = dsp_mem_read(s, arg(1));
                push_status(uc, s, [STATUS_READ_MEM, arg(1), arg(2), v]);
            }
            1 => {
                s.dsp_mem.insert(arg(1), arg(2));
            }
            2 => {
                let (addr, mask, val) = (arg(1), arg(2), arg(3));
                let old = dsp_mem_read(s, addr);
                s.dsp_mem.insert(addr, (old & !mask) | (val & mask));
            }
            _ => {}
        }
        out = (out + 1) % FAST_CMDQ_LEN;
    }
    let _ = uc.mem_write(FAST_CMDQ_OUT, &out.to_le_bytes());
}
