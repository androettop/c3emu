//! Address map of the emulated RM-614 (RAPIDO / RAP3G): the single place for HW and
//! image-placement constants.

/// Low 256 MB: IRAM/SRAM (OS early stack at 0x0870xxxx), OneNAND window, SoC regs.
pub const LOWRAM: u64 = 0x0000_0000;
pub const LOWRAM_SZ: u64 = 0x1000_0000;

/// SDRAM. 64 MB because MCUSW holds literals up to 0x83Axxxxx.
pub const SDRAM: u64 = 0x8000_0000;
pub const SDRAM_SZ: u64 = 0x0400_0000;

/// Peripheral bank (sub-blocks every 0x10000). Reads are served from the MMIO model.
pub const PERIPH: u64 = 0x9000_0000;
pub const PERIPH_SZ: u64 = 0x1000_0000;

/// SoC register block inside low memory (ADC 0x0883xxxx, boot-mode 0x0888xxxx, ...).
/// Behaves as RAM unless the MMIO model names the register; accesses are logged.
pub const SOC_IO_LO: u64 = 0x0880_0000;
pub const SOC_IO_HI: u64 = 0x08A0_0000;
/// Display controller (src/display_chipset_api.c, base literal 0x08030000).
pub const LCDC_LO: u64 = 0x0803_0000;
pub const LCDC_HI: u64 = 0x0804_0000;

/// On-chip boot ROM (not in the FPSX package). PRIMAPP holds Thumb fn pointers
/// 0x1000_3CA5 / 0x1000_521D / 0x1000_6CE5 into it. Mapped empty; entry points the OS
/// uses are served by HLE handlers.
pub const BOOTROM: u64 = 0x1000_0000;
pub const BOOTROM_SZ: u64 = 0x0010_0000;
/// ROM secure-service dispatcher (Thumb). Literal 0x10010809 in RAP3NAND, UPDAPP (x7)
/// and MCUSW. Call: r0 = &descriptor{ id[12] = SEC_SERVICE_ID, u32 service }, r1..r3 args.
pub const ROM_SEC_SERVICE: u64 = 0x1001_0808;
pub const SEC_SERVICE_ID: [u8; 12] =
    [0xAB, 0x0F, 0xBD, 0x96, 0xAF, 0x2A, 0xC2, 0x71, 0xD2, 0x62, 0x64, 0xAF];

/// High exception vectors (the OS sets SCTLR.V).
pub const HIVEC: u64 = 0xFFFF_0000;
pub const HIVEC_SZ: u64 = 0x1_0000;
/// The OS writes its handler-pointer table at HIVEC+0x20 (reset code @0x80130648) and
/// expects the 8 vector slots to hold `ldr pc,[pc,#0x18]` (provided by ROM/loader).
pub const VECTOR_STUB: u32 = 0xE59F_F018;

/// BB5 signed-image header magic (first word of every image).
pub const BB5_MAGIC: u32 = 0x8097_95A3;

/// MCUSW is placed whole (header included): its vector literals are 0x8013_04xx.
pub const MCUSW_BASE: u64 = 0x8013_0000;
/// MCUSW1: Thumb function-pointer literals land on `push {..,lr}` at this base
/// (22069 hits vs ~400 for neighbouring bases).
pub const MCUSW1_BASE: u64 = 0x8500_0000;
/// RAM window holding the two XIP images (MCUSW1 and PPM), 0x85000000-0x86400000.
pub const MCUSW1_WINDOW: u64 = 0x0140_0000;
/// PPM (language / UI resources, rm614__*.ppm_*): the OS VMM's second window,
/// VA 0x85B00000-0x862FFFFF <-> flash 0x01E00000-0x025FFFFF (table @0x80BE44DC); the
/// PPM package is flashed at 0x01E00000.
pub const PPM_BASE: u64 = 0x85B0_0000;
pub const PPM_VA_END: u64 = 0x8630_0000;
/// VMM windows mapped XIP on the first fault in either of them.
pub const XIP_WINDOWS: &[(u64, u64)] = &[(MCUSW1_BASE, MCUSW1_VA_END), (PPM_BASE, PPM_VA_END)];
/// The OS VMM window for MCUSW1 (table @0x80BE44C4: VA 0x85000000-0x85A0F538 <-> flash
/// 0x01300000-0x01DFFFFF). See machine.rs (xip_fixup).
pub const MCUSW1_VA_END: u64 = 0x85A0_F600;
/// L1 section attributes for that mapping: AP=11, domain 1 (as the kernel's own
/// entries), C+B, type section.
pub const XIP_SECTION_ATTR: u32 = (3 << 10) | (1 << 5) | 0xC | 0x2;

/// The loader's "HW configuration" block passed in r2 (see bootinfo.rs).
pub const BOOTINFO: u64 = 0x8012_0000;
pub const BOOTINFO_SZ: u64 = 0x1000;
/// Boot-info tag types (see BOOTINFO).
pub const TAG_END: u32 = 2;

/// Images loaded by the HLE loader: (FPSX image name, link base).
/// The first one provides the entry point.
pub const OS_IMAGES: &[(&str, u64)] = &[
    ("MCUSW", MCUSW_BASE),
    ("MCUSW1", MCUSW1_BASE),
];

/// ARM modes and the top of the banked stack we give each (below MCUSW, above BOOTINFO).
pub const MODE_STACKS: &[(&str, u32, u64)] = &[
    ("fiq", 0x11, 0x8012_8000),
    ("irq", 0x12, 0x8012_A000),
    ("abt", 0x17, 0x8012_C000),
    ("und", 0x1B, 0x8012_E000),
    ("sys", 0x1F, 0x8013_0000),
    ("svc", 0x13, 0x8012_6000),
];
pub const MODE_SVC: u32 = 0x13;
/// CPSR I+F bits (IRQ and FIQ masked).
pub const CPSR_IF: u32 = 0xC0;

/// MCU<->DSP ("pg5") shared memory: low 64 KB of SDRAM, region table at 0x80F67BA8.
/// Command ring (src/sharedmem.c): 128 x 8-byte entries at DSP_CMDQ, u16 indices
/// in/out at +0x400/+0x402 (7-bit). The MCU rings the DSP via IPC_DOORBELL |= 9.
pub const DSP_CMDQ: u64 = 0x8000_C000;
pub const DSP_CMDQ_IN: u64 = DSP_CMDQ + 0x400;
pub const DSP_CMDQ_OUT: u64 = DSP_CMDQ + 0x402;
pub const DSP_CMDQ_LEN: u16 = 128;
/// DSP -> MCU status ring right after the command ring: 32 x {status, arg0, arg1,
/// arg2} at +0x404, u16 in/out at +0x504/+0x506 (layout from the crash-snapshot code
/// @0x8085E486). Delivered to handlers as r0 = status|arg0<<16, r1 = arg1|arg2<<16.
pub const DSP_STATQ: u64 = DSP_CMDQ + 0x404;
pub const DSP_STATQ_IN: u64 = DSP_CMDQ + 0x504;
pub const DSP_STATQ_LEN: u16 = 32;
/// "Status pending" flag in the unpaged shared memory (0x80002000 + 0x59C): when set,
/// the DSP ISR (0x807F4920) activates the status HISR.
pub const DSP_STATUS_FLAG: u64 = 0x8000_259C;
/// Interrupt controller block (see INTC_*); +0x24 is written |= 9 to signal the DSP.
pub const IPC_BASE: u64 = 0x0881_0000;
pub const IPC_DOORBELL: u64 = IPC_BASE + 0x24;
/// DSP -> MCU interrupt line (handler registered by 0x807F48C8, ISR 0x807F4920).
/// On hardware the DSP raises it every GSM TDMA frame (4.615 ms) as well.
pub const DSP_IRQ: u32 = 1;
/// One TDMA frame (4.615 ms) in OS-timer ticks (5120 Hz).
pub const DSP_FRAME_TICKS: u64 = 24;

/// OS timer (IRQ line 0): free-running counter + one-shot compare. See soc.rs.
pub const TIMER_BASE: u64 = 0x0880_0000;
pub const TIMER_COMPARE: u64 = TIMER_BASE + 0x0C;
pub const TIMER_COUNTER: u64 = TIMER_BASE + 0x10;
pub const TIMER_IRQ: u32 = 0;
/// OS timer rate: the OS converts ms to ticks as ms*5*1024/1000 (0x803AE8B2), i.e.
/// 5120 Hz; consistent with the watchdog kick period (~6050 ticks ~= 1.2 s). At the
/// 208 MHz MCU clock ("MCU PLL set 208000 kHz") that is ~40625 instructions per tick.
pub const TIMER_INSNS_PER_TICK: u64 = 40_625;
/// Instructions after a compare write before it can match (see soc.rs).
pub const TIMER_SYNC_INSNS: u64 = 256;
/// Interrupt controller banks (lines 0-31, 32-63, 64-95). See soc.rs.
pub const INTC_BASE: u64 = IPC_BASE;
pub const INTC_BANKS: &[u64] = &[INTC_BASE, INTC_BASE + 0x100, INTC_BASE + 0x180];
pub const INTC_ACK: u64 = 0x08;
pub const INTC_PENDING: u64 = 0x0C;

/// OS fatal-error/reset routine: r0 = &record{ expr*, file*, line, ... }. It maps the
/// error to a power-on cause (SYS_SetPowerOnCause @0x8082B3BC), disables the MMU and
/// spins. hle-boot stops here and prints the decoded assertion.
pub const OS_FATAL_RESET: u64 = 0x809F_6104;
