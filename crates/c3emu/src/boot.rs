//! The HLE loader: everything between "firmware file" and "jump to the OS entry" —
//! image placement, the machine state the boot loader leaves behind, and the HLE
//! handlers the OS needs. Shared by hle-boot (command line) and the web build.

use crate::bb5;
use crate::fpsx::Fpsx;
use crate::layout::*;
use crate::machine::{Machine, State};
use crate::uc::{RegisterARM, Uc};

pub struct BootOptions {
    /// Echo the OS's debug messages on stdout (they are always kept in State::oslog).
    pub oslog_echo: bool,
    /// Print every ISI message.
    pub isi: bool,
    pub storage: crate::storage::Backend,
}

/// Load the firmware (and the optional PPM language pack) into `m`, set up the
/// hand-over state and the HLE handlers. Returns the OS entry point and a log of what
/// was done.
pub fn setup(m: &mut Machine, firmware: &[u8], ppm: Option<&[u8]>, opt: BootOptions)
             -> Result<(u64, Vec<String>), String> {
    let mut log = Vec::new();
    let fw = Fpsx::parse(firmware).map_err(|e| format!("firmware: {e}"))?;
    let mut entry = None;
    for &(name, base) in OS_IMAGES {
        let img = fw.image(name).ok_or_else(|| format!("image {name} not in firmware"))?;
        m.load(&img.data, base)?;
        log.push(format!("{name:8} -> 0x{base:08x}..0x{:08x}", base + img.data.len() as u64));
        if entry.is_none() {
            let e = bb5::os_entry(&img.data, base as u32).map_err(|e| format!("{name}: {e}"))?;
            log.push(format!("vector table at hdr+0x1C = +0x{:x}; reset vector -> 0x{:08x} \
                              (hdr+0x148 says 0x{:08x})", e.vector_table, e.entry, e.header_hint));
            entry = Some(e.entry);
        }
    }
    let entry = entry.ok_or("no OS image")?;

    // PPM (language / UI resources) at its VMM window
    if let Some(buf) = ppm {
        let f = Fpsx::parse(buf).map_err(|e| format!("PPM: {e}"))?;
        let r = f.regions.iter().max_by_key(|r| r.data.len()).ok_or("empty PPM")?;
        if r.addr != 0x01E0_0000 {
            log.push(format!("PPM flash address 0x{:08x}, expected 0x01E00000", r.addr));
        }
        m.load(&r.data, PPM_BASE)?;
        log.push(format!("PPM      -> 0x{PPM_BASE:08x}..0x{:08x}", PPM_BASE + r.data.len() as u64));
    }

    // Machine state the loader would have left behind.
    let sctlr = m.sctlr();
    m.set_sctlr(sctlr & !((1 << 0) | (1 << 2) | (1 << 12) | (1 << 13))); // MMU, D$, I$, V off
    let banked = m.setup_stacks(MODE_SVC);
    log.push(format!("banked SPs: {}",
                     banked.iter().map(|(n, v)| format!("{n}=0x{v:08x}")).collect::<Vec<_>>().join(" ")));
    let placed = crate::bootinfo::install(m, &fw).map_err(|e| format!("bootinfo: {e}"))?;
    log.push(format!("boot info: power-on = power key; images: {}", placed.join(" ")));
    let stubs: Vec<u8> = (0..8).flat_map(|_| VECTOR_STUB.to_le_bytes()).collect();
    m.load(&stubs, HIVEC)?; // OS fills the pointer table at HIVEC+0x20 itself
    m.st.borrow_mut().deliver_exceptions = true;
    let _ = m.uc.reg_write(RegisterARM::R2, BOOTINFO); // reset code: r6 = r2
    let _ = m.uc.reg_write(RegisterARM::LR, 0);
    // On-chip ROM secure-service dispatcher: log the service, report success (r0 = 0).
    // ASSUMPTION: 0 = OK; the first call site (svc 0x1800 @0x805ea1d4) ignores r0.
    // a real instruction at the ROM entry (Thumb BKPT): if Unicorn ever skips the
    // address hook (seen in turbo mode), the breakpoint lands in the exception hook,
    // which runs the HLE instead of executing empty ROM
    m.load(&0xBE00u16.to_le_bytes(), ROM_SEC_SERVICE)?;
    m.hle_fn(ROM_SEC_SERVICE, "rom_sec_service", Box::new(|uc: &mut Uc<'_>, s: &mut State| {
        let r = |n| uc.reg_read(i32::from(RegisterARM::R0) + n).unwrap_or(0);
        let desc = uc.mem_read_as_vec(r(0), 16).unwrap_or_default();
        let svc = match desc.get(..12) {
            Some(id) if id == SEC_SERVICE_ID => format!("0x{:02x}", u16::from_le_bytes([desc[14], desc[15]])),
            _ => format!("?(desc {})", desc.iter().map(|b| format!("{b:02x}")).collect::<String>()),
        };
        let (lr, r1, r2, r3) = (uc.reg_read(RegisterARM::LR).unwrap_or(0), r(1), r(2), r(3));
        *s.hle_calls.entry(format!("rom_sec_service[{svc}]")).or_default() += 1;
        let sp = uc.reg_read(RegisterARM::SP).unwrap_or(0);
        let mut stk = [0u8; 12];
        crate::mmu::read_virt(uc, sp as u32, &mut stk); // task stacks are virtual
        let w = |b: &[u8], i: usize| u32::from_le_bytes(b[4 * i..4 * i + 4].try_into().unwrap());
        s.event(format!("ROM sec-service {svc} r1=0x{r1:x} r2=0x{r2:x} r3=0x{r3:x} stack {:x} {:x} {:x} from 0x{lr:08x}",
                        w(&stk, 0), w(&stk, 1), w(&stk, 2)));
        if svc == "0x12" && lr & !1 == crate::seccall::PA_CALL_RET {
            return Some(crate::seccall::pa_call(uc, s, w(&stk, 0), w(&stk, 1)));
        }
        Some(0)
    }));
    crate::patches::install(m);
    crate::analog::install(m);
    crate::i2c::power_on(&mut m.st.borrow_mut().i2c);
    crate::oslog::install(m, opt.oslog_echo);
    crate::isi::install(m, opt.isi);
    crate::storage::install(m, opt.storage);
    // OS fatal error: decode the assertion record and stop instead of rebooting.
    m.hle_fn(OS_FATAL_RESET, "os_fatal", Box::new(|uc: &mut Uc<'_>, s: &mut State| {
        let rec = uc.reg_read(RegisterARM::R0).unwrap_or(0);
        let word = |a: u64| {
            let mut b = [0u8; 4];
            let _ = uc.mem_read(a, &mut b);
            u32::from_le_bytes(b) as u64
        };
        let cstr = |a: u64| {
            let v = uc.mem_read_as_vec(a, 96).unwrap_or_default();
            v.iter().take_while(|&&c| c != 0).map(|&c| c as char).collect::<String>()
        };
        let (expr, file, line) = (cstr(word(rec)), cstr(word(rec + 4)), word(rec + 8));
        let lr = uc.reg_read(RegisterARM::LR).unwrap_or(0);
        let msg = format!("OS fatal error: {file}:{line}: {expr} (reset called from 0x{lr:08x})");
        s.event(msg.clone());
        s.stop_reason = Some(msg);
        let _ = uc.emu_stop();
        Some(0)
    }));
    Ok((entry as u64, log))
}
