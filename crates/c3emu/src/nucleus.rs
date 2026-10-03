//! Nucleus PLUS 1.7 introspection ("Copyright (c) 1993-1998 ATI - Nucleus PLUS - THUMB
//! ARM Version 1.7.G1.3" in MCUSW). Finds task control blocks in RAM and reports what
//! each task is doing — the main tool for finding what a blocked OS is waiting for.
//!
//! TC_TCB layout (Nucleus PLUS): +0 created-list node (prev,next), +8 tc_id = 'TASK'
//! (0x5441534B), +12 name[8], +20 status (u8), +22 priority (u8), +32 stack start,
//! +36 stack end, +40 saved stack pointer.

use crate::uc::Uc;

use crate::mmu::virt_word;

use crate::layout::{SDRAM, SDRAM_SZ};

/// Physical ranges scanned for TCBs: SDRAM and the low RAM used for IRAM/task stacks.
const SCAN: &[(u64, u64)] = &[(SDRAM, SDRAM_SZ), (0x0800_0000, 0x0300_0000)];

pub const TC_TASK_ID: u32 = 0x5441_534B;

pub struct Task {
    pub tcb: u64,
    pub name: String,
    pub status: u8,
    pub priority: u8,
    pub sp: u32,
    pub stack: (u32, u32),
    /// Code-looking words found on the saved stack (heuristic backtrace).
    pub trace: Vec<u32>,
}

pub fn status_name(s: u8) -> &'static str {
    match s {
        0 => "READY",
        1 => "PURE_SUSPEND",
        2 => "SLEEP",
        3 => "MAILBOX",
        4 => "QUEUE",
        5 => "PIPE",
        6 => "SEMAPHORE",
        7 => "EVENT",
        8 => "PARTITION",
        9 => "MEMORY",
        10 => "DRIVER",
        11 => "FINISHED",
        12 => "TERMINATED",
        _ => "?",
    }
}


/// `is_code` decides whether a stack word looks like a return address.
pub fn tasks(uc: &Uc<'_>, is_code: impl Fn(u32) -> bool) -> Vec<Task> {
    let mut out = Vec::new();
    for &(base, size) in SCAN {
        scan(uc, base, size, &is_code, &mut out);
    }
    out
}

fn scan(uc: &Uc<'_>, base: u64, size: u64, is_code: &impl Fn(u32) -> bool, out: &mut Vec<Task>) {
    let mem = uc.mem_read_as_vec(base, size as usize).unwrap_or_default();
    for off in (8..mem.len().saturating_sub(48)).step_by(4) {
        if u32::from_le_bytes(mem[off..off + 4].try_into().unwrap()) != TC_TASK_ID {
            continue;
        }
        let tcb = base + off as u64 - 8;
        let rd = |o: usize| u32::from_le_bytes(mem[off - 8 + o..off - 8 + o + 4].try_into().unwrap());
        let (start, end, sp) = (rd(32), rd(36), rd(40));
        // sanity: saved SP inside its own (sanely sized) stack, printable name, valid status
        let raw_name: Vec<u8> = mem[off + 4..off + 12].iter().copied().take_while(|&c| c != 0).collect();
        if !(start <= sp && sp <= end && end > start && end - start <= 0x40000)
            || mem[off + 12] > 12
            || raw_name.is_empty()
            || !raw_name.iter().all(|c| (0x20..0x7f).contains(c))
        {
            continue;
        }
        let name: String = raw_name.iter().map(|&c| c as char).collect();
        let mut trace = Vec::new();
        let mut a = sp as u64;
        while a + 4 <= end as u64 && trace.len() < 8 && a < sp as u64 + 0x200 {
            let Some(w) = virt_word(uc, a as u32) else { break };
            if is_code(w) {
                trace.push(w);
            }
            a += 4;
        }
        out.push(Task { tcb, name, status: mem[off + 12], priority: mem[off + 14], sp,
                        stack: (start, end), trace });
    }
}

pub const SM_SEMA_ID: u32 = 0x5345_4D41;

/// A semaphore with suspended tasks. SM_SCB: +0 created node (12 bytes), +12 'SEMA',
/// +16 name[8], +24 count, +28 FIFO flag, +32 tasks waiting, +36 suspension list ->
/// SM_SUSPEND (on the waiting task's stack) {node (12), +12 scb, +16 task}.
pub struct Semaphore {
    pub scb: u64,
    pub name: String,
    pub count: u32,
    pub waiting: u32,
    /// TCBs of the suspended tasks (first few, in list order).
    pub waiters: Vec<u32>,
}

/// Semaphores that have at least one task waiting.
pub fn semaphores(uc: &Uc<'_>) -> Vec<Semaphore> {
    let mut out = Vec::new();
    for &(base, size) in SCAN {
        let mem = uc.mem_read_as_vec(base, size as usize).unwrap_or_default();
        for off in (8..mem.len().saturating_sub(40)).step_by(4) {
            let rd = |o: usize| u32::from_le_bytes(mem[o..o + 4].try_into().unwrap());
            if rd(off) != SM_SEMA_ID {
                continue;
            }
            let (count, waiting, list) = (rd(off + 12), rd(off + 20), rd(off + 24));
            if waiting == 0 || waiting > 256 || list == 0 {
                continue;
            }
            let name: String = mem[off + 4..off + 12].iter().take_while(|&&c| c != 0)
                .map(|&c| if (0x20..0x7f).contains(&c) { c as char } else { '.' }).collect();
            let mut waiters = Vec::new();
            let mut node = list;
            while waiters.len() < (waiting as usize).min(8) {
                let Some(t) = virt_word(uc, node.wrapping_add(16)) else { break };
                waiters.push(t);
                match virt_word(uc, node.wrapping_add(4)) {
                    Some(n) if n != list && n != 0 => node = n,
                    _ => break,
                }
            }
            out.push(Semaphore { scb: base + off as u64 - 12, name, count, waiting, waiters });
        }
    }
    out
}
