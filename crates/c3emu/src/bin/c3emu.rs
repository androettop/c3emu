//! c3emu — Nokia C3-00 emulator (desktop).
//!
//! Boots the phone's Series 40 OS from your own firmware files: the `*.mcusw` image,
//! optionally a language pack (`*.ppm_*`) and the factory content image (`*.image_*`,
//! the C: drive). Files named like the firmware next to it are found automatically.
//!
//! Usage: c3emu <firmware.mcusw> [--ppm FILE] [--content FILE] [--storage DIR]
//!        (opens the phone in a window; --headless runs without one)
//! Debug options:
//!                 [--insns N] (stop after N instructions; default: no limit)
//!                 [--trace N] [--trace-from N] (disassemble N instructions from instruction M)
//!                 [--mmio model.json] (peripheral model; default: built in)
//!                 [--hle-sym NAME=ADDR]... [--watch ADDR]... [--dump ADDR:LEN]...
//!                 [--dis ADDR:N]... (odd ADDR = Thumb; disassembles emulated memory at exit)
//!                 [--track ADDR:LEN]... (first read of each address in the range)
//!                 [--tasks] (Nucleus task list + what each task is blocked on, at exit)
//!                 [--events-from N] (record events / arm --hle-sym stops from instruction N on)
//!                 [--storage DIR] (flash device images, default ./c3emu-data)
//!                 [--va ADDR]... (MMU translation of ADDR at exit)
//!                 [--iolog ADDR:LEN]... (log every access in the range as events)
//!                 [--screen OUT.png] (save the screen / window at exit)
//!                 [--frames DIR] (save every changed display frame as a PNG)
//!                 [--headless] (no window)
//!                 [--turbo] (fastest mode: exact interpreter / Unicorn turbo; default with the window)
//!                 [--realtime] (per-block hook, wall-clock time; --no-realtime: instruction-count time)
//!                 [--paced] (headless runs paced to real time)
//!                 [--hot] (count block executions for the hottest-blocks report)
//!                 [--oslog] (print the OS's own debug messages as they happen)
//!                 [--probe NAME=ADDR]... (log r0-r3 each time ADDR executes, keep running)
//!                 [--isi] (print every Nokia ISI message sent between servers)
//!                 [--key MINSNS:KEY[:HOLD_MS]]... (scripted key presses) [--wav OUT.wav]

use std::process::exit;

use c3emu::fpsx::Fpsx;
use c3emu::layout::*;
use c3emu::machine::{parse_mmio_model, parse_u64, Machine, State};
use c3emu::uc::{RegisterARM, Uc};

struct Args {
    firmware: String,
    insns: u64,
    trace: u64,
    mmio: Option<String>,
    hle_syms: Vec<(String, u64)>,
    watches: Vec<u64>,
    dumps: Vec<(u64, usize)>,
    diss: Vec<(u64, usize)>,
    tracks: Vec<(u64, usize)>,
    tasks: bool,
    events_from: u64,
    storage: String,
    vas: Vec<u64>,
    trace_from: u64,
    iologs: Vec<(u64, usize)>,
    screen: Option<String>,
    /// Save every changed display frame as a PNG in this directory.
    frames: Option<String>,
    window: bool,
    /// Scripted key presses: (icount, key index, hold ticks).
    keys: Vec<(u64, u8, u32)>,
    /// Real-time clock (wall-clock time instead of instruction count). Default: on with --window.
    turbo: Option<bool>,
    /// Turbo: Unicorn without the per-block hook (implies the real-time clock). Default:
    /// on with --window unless --realtime / --no-realtime is given.
    turbo_free: Option<bool>,
    /// Pace headless runs to real time (as the live window does).
    paced: bool,
    /// Record the phone's audio output to this WAV file.
    wav: Option<String>,
    /// Debug DSP status injections.
    dsp_status: Vec<(u64, [u16; 4])>,
    /// Block coverage: (from, to, output file).
    cover: Option<(u64, u64, String)>,
    hot: bool,
    oslog: bool,
    ppm: Option<String>,
    /// Content image (factory C: drive); default: the *.image* next to the firmware.
    content: Option<String>,
    probes: Vec<(String, u64)>,
    isi: bool,
}

fn usage() -> ! {
    eprintln!("usage: c3emu <firmware.mcusw> [--ppm FILE] [--content FILE] [--storage DIR] [--headless]\n       (debug options: see the top of src/bin/c3emu.rs)");
    exit(2)
}

fn parse_args() -> Args {
    let mut it = std::env::args().skip(1);
    let mut a = Args { firmware: String::new(), insns: u64::MAX, trace: 0, mmio: None, hle_syms: vec![], watches: vec![], dumps: vec![], diss: vec![], tracks: vec![], tasks: false, events_from: 0, storage: "c3emu-data".into(), vas: vec![], trace_from: 0, iologs: vec![], screen: None, frames: None, window: true, hot: false, oslog: false, ppm: None, probes: vec![], isi: false, keys: vec![], cover: None, dsp_status: vec![], wav: None, content: None, turbo: None, turbo_free: None, paced: false };
    let num = |v: Option<String>| v.as_deref().and_then(parse_u64).unwrap_or_else(|| usage());
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--insns" => a.insns = num(it.next()),
            "--trace" => a.trace = num(it.next()),
            "--mmio" => a.mmio = Some(it.next().unwrap_or_else(|| usage())),
            "--hle-sym" => {
                let s = it.next().unwrap_or_else(|| usage());
                let (n, addr) = s.split_once('=').unwrap_or_else(|| usage());
                a.hle_syms.push((n.into(), parse_u64(addr).unwrap_or_else(|| usage()) & !1));
            }
            "--watch" => a.watches.push(num(it.next())),
            "--tasks" => a.tasks = true,
            "--va" => a.vas.push(num(it.next())),
            "--trace-from" => a.trace_from = num(it.next()),
            "--screen" => a.screen = Some(it.next().unwrap_or_else(|| usage())),
            "--frames" => a.frames = Some(it.next().unwrap_or_else(|| usage())),
            "--window" => a.window = true,
            "--headless" => a.window = false,
            "--realtime" => a.turbo = Some(true),
            "--turbo" => a.turbo_free = Some(true),
            "--paced" => a.paced = true,
            "--no-realtime" => a.turbo = Some(false),
            "--key" => {
                // MINSNS:KEY[:HOLD_MS] (KEY 255 = red power/end key)
                let v = it.next().unwrap_or_default();
                let p: Vec<&str> = v.split(':').collect();
                if p.len() < 2 { usage() }
                let at = p[0].parse::<u64>().unwrap_or_else(|_| usage()) * 1_000_000;
                let k = p[1].parse::<u8>().unwrap_or_else(|_| usage());
                let ticks = p.get(2).map_or(c3emu::keypad::HOLD_TICKS, |ms| {
                    (ms.parse::<u64>().unwrap_or_else(|_| usage()) * c3emu::soc::TIMER_HZ / 1000) as u32
                });
                a.keys.push((at, k, ticks));
            }
            "--hot" => a.hot = true,
            "--dsp-status" => {
                // MINSNS:CODE[:A0[:A1[:A2]]] (hex) — push a DSP status entry
                let v = it.next().unwrap_or_default();
                let p: Vec<&str> = v.split(':').collect();
                let at = p[0].parse::<u64>().unwrap_or_else(|_| usage()) * 1_000_000;
                let mut e = [0u16; 4];
                for (i, x) in p[1..].iter().take(4).enumerate() {
                    e[i] = u16::from_str_radix(x.trim_start_matches("0x"), 16).unwrap_or_else(|_| usage());
                }
                a.dsp_status.push((at, e));
            }
            "--wav" => a.wav = Some(it.next().unwrap_or_else(|| usage())),
            "--cover" => {
                // FROM_M:TO_M:FILE — blocks first run in [FROM, TO) million insns
                let v = it.next().unwrap_or_default();
                let p: Vec<&str> = v.splitn(3, ':').collect();
                if p.len() != 3 { usage() }
                let m = |x: &str| x.parse::<u64>().unwrap_or_else(|_| usage()) * 1_000_000;
                a.cover = Some((m(p[0]), m(p[1]), p[2].to_string()));
            }
            "--oslog" => a.oslog = true,
            "--isi" => a.isi = true,
            "--ppm" => a.ppm = Some(it.next().unwrap_or_else(|| usage())),
            "--content" => a.content = Some(it.next().unwrap_or_else(|| usage())),
            "--probe" => {
                let s = it.next().unwrap_or_else(|| usage());
                let (n, addr) = s.split_once('=').unwrap_or_else(|| usage());
                a.probes.push((n.into(), parse_u64(addr).unwrap_or_else(|| usage()) & !1));
            }
            "--iolog" => {
                let s = it.next().unwrap_or_else(|| usage());
                let (addr, n) = s.split_once(':').unwrap_or_else(|| usage());
                a.iologs.push((parse_u64(addr).unwrap_or_else(|| usage()),
                               parse_u64(n).unwrap_or_else(|| usage()) as usize));
            }
            "--storage" => a.storage = it.next().unwrap_or_else(|| usage()),
            "--events-from" => a.events_from = num(it.next()),
            "--track" => {
                let s = it.next().unwrap_or_else(|| usage());
                let (addr, n) = s.split_once(':').unwrap_or_else(|| usage());
                a.tracks.push((parse_u64(addr).unwrap_or_else(|| usage()),
                               parse_u64(n).unwrap_or_else(|| usage()) as usize));
            }
            "--dis" => {
                let s = it.next().unwrap_or_else(|| usage());
                let (addr, n) = s.split_once(':').unwrap_or_else(|| usage());
                a.diss.push((parse_u64(addr).unwrap_or_else(|| usage()),
                             parse_u64(n).unwrap_or_else(|| usage()) as usize));
            }
            "--dump" => {
                let s = it.next().unwrap_or_else(|| usage());
                let (addr, len) = s.split_once(':').unwrap_or_else(|| usage());
                a.dumps.push((parse_u64(addr).unwrap_or_else(|| usage()),
                              parse_u64(len).unwrap_or_else(|| usage()) as usize));
            }
            "-h" | "--help" => usage(),
            _ if a.firmware.is_empty() && !arg.starts_with('-') => a.firmware = arg,
            _ => usage(),
        }
    }
    if a.firmware.is_empty() {
        usage()
    }
    a
}

fn main() {
    let args = parse_args();
    let model = match &args.mmio {
        Some(p) => std::fs::read_to_string(p)
            .map_err(|e| e.to_string())
            .and_then(|s| parse_mmio_model(&s))
            .unwrap_or_else(|e| { eprintln!("mmio model: {e}"); exit(1) }),
        None => parse_mmio_model(c3emu::machine::DEFAULT_MMIO_MODEL).expect("built-in model"),
    };
    let mut m = Machine::new(args.trace, model).unwrap_or_else(|e| { eprintln!("{e}"); exit(1) });

    let buf = std::fs::read(&args.firmware).unwrap_or_else(|e| { eprintln!("{}: {e}", args.firmware); exit(1) });
    // PPM (language / UI resources) at its VMM window
    let ppm = args.ppm.clone().or_else(|| {
        let dir = std::path::Path::new(&args.firmware).parent()?;
        let stem = std::path::Path::new(&args.firmware).file_stem()?.to_string_lossy().into_owned();
        std::fs::read_dir(dir).ok()?.filter_map(|e| e.ok()).map(|e| e.path())
            .filter(|p| p.to_string_lossy().contains(".ppm"))
            .find(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with(&stem)))
            .map(|p| p.to_string_lossy().into_owned())
    });
    // content image (factory C: drive) -> storage dev0, once
    let content = args.content.clone().or_else(|| {
        let dir = std::path::Path::new(&args.firmware).parent()?;
        let stem = std::path::Path::new(&args.firmware).file_stem()?.to_string_lossy().into_owned();
        std::fs::read_dir(dir).ok()?.filter_map(|e| e.ok()).map(|e| e.path())
            .filter(|p| p.to_string_lossy().contains(".image"))
            .find(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with(&stem)))
            .map(|p| p.to_string_lossy().into_owned())
    });
    if let Some(path) = &content {
        let min = c3emu::storage::DEFAULT_DEV_SECTORS as u64 * c3emu::storage::SECTOR;
        match c3emu::content::install(std::path::Path::new(path), std::path::Path::new(&args.storage), min) {
            Ok(Some((n, bytes))) => println!("[hle] content  -> {}/dev0.img: {n} blocks, {:.1} MB volume ({path})",
                                             args.storage, bytes as f64 / 1048576.0),
            Ok(None) => println!("[hle] content  : {}/dev0.img already present (delete it to reinstall)", args.storage),
            Err(e) => eprintln!("{path}: {e}"),
        }
    }
    let ppm_data = ppm.as_ref().map(|path| {
        std::fs::read(path).unwrap_or_else(|e| { eprintln!("{path}: {e}"); exit(1) })
    });
    let fw = Fpsx::parse(&buf).unwrap_or_else(|e| { eprintln!("{}: {e}", args.firmware); exit(1) });
    let opt = c3emu::boot::BootOptions {
        oslog_echo: args.oslog,
        isi: args.isi,
        storage: c3emu::storage::Backend::Dir(args.storage.clone().into()),
    };
    let (entry, log) = c3emu::boot::setup(&mut m, &buf, ppm_data.as_deref(), opt)
        .unwrap_or_else(|e| { eprintln!("{}: {e}", args.firmware); exit(1) });
    for l in log {
        println!("[hle] {l}");
    }
    if let Some(path) = &ppm {
        println!("[hle] PPM file: {path}");
    }
    for &(a, n) in &args.iologs {
        m.iolog(a, n as u64).unwrap_or_else(|e| { eprintln!("{e}"); exit(1) });
    }
    for &(a, n) in &args.tracks {
        m.track(a, n as u64).unwrap_or_else(|e| { eprintln!("{e}"); exit(1) });
    }
    m.st.borrow_mut().events_from = args.events_from;
    m.st.borrow_mut().collect_hits = args.hot;
    m.st.borrow_mut().cover = args.cover.as_ref().map(|c| (c.0, c.1));
    if let Some(dir) = &args.frames {
        std::fs::create_dir_all(dir).unwrap_or_else(|e| { eprintln!("{dir}: {e}"); exit(1) });
        m.st.borrow_mut().panel.dump_dir = Some(dir.clone());
    }
    if args.trace > 0 {
        m.enable_trace(args.trace_from, args.trace);
    }
    for &w in &args.watches {
        m.watch(w).unwrap_or_else(|e| { eprintln!("{e}"); exit(1) });
    }
    for (n, a) in &args.probes {
        let name = n.clone();
        m.hle_fn(*a, n, Box::new(move |uc: &mut Uc<'_>, s: &mut State| {
            let r: Vec<String> = (0..8).map(|i| format!("{:x}", uc.reg_read(i32::from(RegisterARM::R0) + i).unwrap_or(0))).collect();
            let lr = uc.reg_read(RegisterARM::LR).unwrap_or(0);
            let sp = uc.reg_read(RegisterARM::SP).unwrap_or(0);
            *s.hle_calls.entry(format!("probe {name}")).or_default() += 1;
            s.event(format!("probe {name}: r0..r7={} lr={lr:x} sp={sp:x}", r.join(",")));
            None
        }));
    }
    {
        let mut keys = args.keys.clone();
        keys.sort();
        m.st.borrow_mut().keypad.script = keys.into();
        let mut d = args.dsp_status.clone();
        d.sort();
        m.st.borrow_mut().dsp_inject = d.into();
        if args.wav.is_some() {
            m.st.borrow_mut().audio.record = Some(Vec::new());
        }
    }
    for (n, a) in &args.hle_syms {
        m.hle_stop(*a, n);
    }

    if args.paced {
        let mut st = m.st.borrow_mut();
        let tick = c3emu::soc::now(&st);
        st.soc.realtime = Some((std::time::Instant::now(), tick));
    }
    let t0 = std::time::Instant::now();
    let turbo = args.turbo.unwrap_or(args.window);
    let free = args.turbo_free.unwrap_or(args.window && args.turbo.is_none());
    let why = if args.window {
        live_window(&mut m, entry, args.insns, args.screen.as_deref(), turbo, free)
    } else if free {
        m.run_fast(entry, args.insns, |_| true)
    } else if turbo {
        m.run_realtime(entry, args.insns, |_| true)
    } else {
        m.run(entry, args.insns)
    };
    println!("\n[hle] stopped: {why} ({:.1}s)", t0.elapsed().as_secs_f64());
    if let Some(path) = &args.wav {
        let st = m.st.borrow();
        let rec = st.audio.record.as_deref().unwrap_or(&[]);
        match c3emu::audio::write_wav(path, rec) {
            Ok(()) => println!("[hle] audio: {:.1} s written to {path} (FIFO words played {}, max FIFO index 0x{:x}, underruns {}, clock catch-up {:.2} s)",
                               rec.len() as f64 / 2.0 / c3emu::audio::OUT_RATE as f64, st.audio.fifo_words_played, st.audio.max_in_index,
                               st.audio.fifo_underruns, st.soc.catch_up as f64 / c3emu::soc::TIMER_HZ as f64),
            Err(e) => eprintln!("wav: {e}"),
        }
    }
    if let Some((_, _, path)) = &args.cover {
        let st = m.st.borrow();
        let mut v: Vec<_> = st.covered.iter().map(|(a, t)| (*t, *a)).collect();
        v.sort();
        let txt: String = v.iter().map(|(t, a)| format!("{a:08x} {t}\n")).collect();
        std::fs::write(path, txt).unwrap_or_else(|e| eprintln!("cover: {e}"));
    }
    m.report();
    {
        let st = m.st.borrow();
        let p = &st.panel;
        println!("[hle] display: {} memory-write commands ({} changed frames), {} pixels written", p.frames, p.frames_changed, p.pixels_written);
        if let Some(path) = args.screen.as_ref().filter(|_| !args.window) {
            p.save_png(path).unwrap_or_else(|e| eprintln!("screen: {e}"));
            println!("[hle] display saved to {path}");
        }
    }
    for &va in &args.vas {
        let mut ttbr = c3emu::uc::RegisterARMCP { cp: 15, crn: 2, ..Default::default() };
        let _ = m.uc.reg_read_arm_coproc(&mut ttbr);
        let l1a = (ttbr.val as u32 & 0xFFFF_C000) | ((va as u32 >> 20) << 2);
        let l1 = c3emu::mmu::phys_word(&m.uc, l1a);
        println!("[hle] VA 0x{va:08x}: TTBR=0x{:08x} L1[0x{l1a:08x}]=0x{l1:08x} -> {:?}", ttbr.val,
                 c3emu::mmu::virt_to_phys(&m.uc, va as u32).map(|p| format!("0x{p:08x}")));
    }
    if args.tasks {
        let code = |w: u32| {
            let a = (w & !1) as u64;
            OS_IMAGES.iter().any(|&(n, b)| fw.image(n).is_some_and(|r| a >= b + 0x400 && a < b + r.data.len() as u64))
        };
        let ts = c3emu::nucleus::tasks(&m.uc, code);
        println!("[hle] Nucleus tasks ({}):", ts.len());
        for t in &ts {
            let tr: Vec<String> = t.trace.iter().map(|w| format!("{w:08x}")).collect();
            println!("    {:8} {:10} pri={:3} tcb=0x{:08x} sp=0x{:08x}  {}", t.name,
                     c3emu::nucleus::status_name(t.status), t.priority, t.tcb, t.sp, tr.join(" "));
        }
        let names: std::collections::HashMap<u32, &str> = ts.iter().map(|t| (t.tcb as u32, t.name.as_str())).collect();
        for sm in c3emu::nucleus::semaphores(&m.uc) {
            // waiters are virtual TCB addresses (the scan reports the TCB 4 bytes in)
            let w: Vec<String> = sm.waiters.iter().map(|&t| {
                c3emu::mmu::virt_to_phys(&m.uc, t).and_then(|p| names.get(&((p + 4) as u32)))
                    .map_or(format!("0x{t:08x}"), |n| n.to_string())
            }).collect();
            println!("    sem {:8} scb=0x{:08x} count={} waiting={}: {}", sm.name, sm.scb, sm.count, sm.waiting, w.join(" "));
        }
    }
    for &(addr, n) in &args.diss {
        let thumb = addr & 1 == 1;
        let mut a = addr & !1;
        println!("[hle] disasm 0x{a:08x} ({}):", if thumb { "Thumb" } else { "ARM" });
        for _ in 0..n {
            let mut b = m.uc.mem_read_as_vec(a, 4).unwrap_or_default();
            let len = if thumb && u16::from_le_bytes([b[0], b[1]]) >> 11 < 0b11101 { 2 } else { 4 };
            b.truncate(len);
            println!("    {a:08x}: {}", m.dis.one(&b, a, thumb));
            a += len as u64;
        }
    }
    for &(addr, len) in &args.dumps {
        let mem = m.uc.mem_read_as_vec(addr, len).unwrap_or_default();
        println!("[hle] dump 0x{addr:08x}:");
        for (i, row) in mem.chunks(16).enumerate() {
            let words: Vec<String> = row.chunks(4)
                .map(|w| format!("{:08x}", u32::from_le_bytes(w.try_into().unwrap_or([0; 4])))).collect();
            let text: String = row.iter().map(|&b| if (0x20..0x7f).contains(&b) { b as char } else { '.' }).collect();
            println!("    {:08x}: {}  {text}", addr + 16 * i as u64, words.join(" "));
        }
    }
}

/// PC key -> keypad matrix index (row * 8 + column; see keypad.rs). Identified by
/// pressing every index on the first-boot date editor (digits land on r t y / f g h /
/// v b n / m, as printed on the C3-00 QWERTY) and on the no-SIM query. The keyboard
/// server reports index -> key code: q..p = 0x80..0x89, a..l = 0x90..0x98,
/// z..m = 0xA0..0xA6, D-pad up/down/left/right = 0xE007/E008/E00D/E00E,
/// centre = 0xE00C, soft left/right = 0xE003/E004.
const KEYMAP: &[(minifb::Key, u8)] = {
    use minifb::Key::*;
    &[
        // letters
        (Q, 0), (W, 1), (E, 2), (R, 3), (T, 4), (Y, 5), (U, 6), (I, 24), (O, 25), (P, 26),
        (A, 8), (S, 9), (D, 10), (F, 11), (G, 12), (H, 13), (J, 14), (K, 32), (L, 33),
        (Z, 16), (X, 17), (C, 18), (V, 19), (B, 20), (N, 21), (M, 22),
        (Backspace, 34),   // 0x99, right of L
        (NumPadEnter, 42), // 0xA9, QWERTY Enter
        (Space, 49),       // 0xB3
        (LeftShift, 35), (RightShift, 35), // 0xB1
        (LeftCtrl, 27), (RightCtrl, 27),   // 0xB9
        (Period, 28),      // 0xB7
        (Comma, 45),       // 0xB8
        (Minus, 40), (Equal, 41), // 0xA7 / 0xA8: not identified yet
        // D-pad
        (Up, 56), (Down, 57), (Right, 59), (Left, 58), (Enter, 60),
        // soft keys and the rest of the keypad (identified on the phone UI)
        (F1, 29),  // left soft key, 0xE003
        (F2, 43),  // right soft key, 0xE004
        (F3, 30),  // call key, 0xE005
        (F4, 38),  // contacts key, 0xE07C
        (F5, 52),  // messaging key, 0xE07B
        (F10, 48), // Sym, 0xB2
        (F12, 50), // Fn, 0xB0 (keyguard unlock with the left soft key)
    ]
};

/// Run with a live window showing the phone (shell + screen, as device simulators do),
/// refreshed ~30 times per second. Resizable; the drawn keys are clickable and the PC
/// keyboard works too (KEYMAP). Speed and frame counts are in the title.
fn live_window(m: &mut Machine, entry: u64, insns: u64, save: Option<&str>, turbo: bool, free: bool) -> String {
    use minifb::{MouseButton, MouseMode, ScaleMode, Window, WindowOptions};
    let mut skin = match c3emu::skin::Skin::new() {
        Ok(s) => s,
        Err(e) => return format!("shell image: {e}"),
    };
    let (w0, h0) = (442, 883); // 55 % of the shell; the window can be resized
    let opts = WindowOptions { resize: true, scale_mode: ScaleMode::Stretch, ..WindowOptions::default() };
    let mut win = match Window::new("Nokia C3-00 — c3emu", w0, h0, opts) {
        Ok(w) => w,
        Err(e) => return format!("cannot open window: {e}"),
    };
    let mut buf: Vec<u32> = Vec::new();
    let mut size = (w0, h0);
    let t0 = std::time::Instant::now();
    // the phone's audio through the host sound card
    let sink: c3emu::audio::Sink = Default::default();
    m.st.borrow_mut().audio.sink = Some(sink.clone());
    let _stream = c3emu::sound::start(sink);
    if _stream.is_none() {
        eprintln!("[hle] no sound output device: running silent");
    }
    {
        // the phone's clock follows the wall clock while it idles
        let mut st = m.st.borrow_mut();
        let tick = c3emu::soc::now(&st);
        st.soc.realtime = Some((t0, tick));
        // the phone's clock starts at the host's local time
        if let Some(e) = host_epoch() {
            st.rtc.epoch = e;
        }
    }
    let mut last = t0;
    let (mut prev_n, mut prev_t) = (0u64, t0);
    let mut mips = 0.0f64;
    let mut clicked: Option<u8> = None;
    let frame = |m: &mut Machine| -> bool {
        if last.elapsed().as_millis() < 33 {
            return true;
        }
        let now = std::time::Instant::now();
        last = now;
        // keyboard and mouse -> keypad: a press shorter than keypad::TAP_MS is one tap,
        // a longer one a long press (keypad::host_down / host_up). End = red key.
        {
            let held = win.get_keys();
            let mut st = m.st.borrow_mut();
            let idx_of = |k: minifb::Key| if k == minifb::Key::End {
                Some(c3emu::keypad::POWER_KEY)
            } else {
                KEYMAP.iter().find(|e| e.0 == k).map(|e| e.1)
            };
            for k in win.get_keys_pressed(minifb::KeyRepeat::No) {
                if let Some(idx) = idx_of(k) {
                    c3emu::keypad::host_down(&mut st, idx);
                }
            }
            let mouse = win.get_mouse_down(MouseButton::Left);
            if mouse && clicked.is_none() {
                if let Some(idx) = win.get_mouse_pos(MouseMode::Discard).and_then(|(x, y)| skin.hit(x, y)) {
                    clicked = Some(idx);
                    c3emu::keypad::host_down(&mut st, idx);
                }
            }
            let mut down_now: Vec<u8> = held.iter().filter_map(|&k| idx_of(k)).collect();
            if !mouse {
                clicked = None;
            }
            down_now.extend(clicked);
            for idx in c3emu::keypad::host_held(&st) {
                if !down_now.contains(&idx) {
                    c3emu::keypad::host_up(&mut st, idx);
                }
            }
        }
        let (w, h) = win.get_size();
        if w > 0 && h > 0 {
            size = (w, h);
        }
        let st = m.st.borrow();
        skin.render(&st.panel.gram, size.0, size.1, &mut buf, clicked);
        // speed over the last ~0.5 s
        let n = st.icount;
        let dt = now.duration_since(prev_t).as_secs_f64();
        if dt >= 0.5 {
            mips = (n - prev_n) as f64 / dt / 1e6;
            prev_n = n;
            prev_t = now;
        }
        win.set_title(&format!("Nokia C3-00 — c3emu   {:.0} MIPS (x{:.2})   frames {} ({} changed)",
                               mips, mips / 208.0, st.panel.frames, st.panel.frames_changed));
        win.update_with_buffer(&buf, size.0, size.1).is_ok() && win.is_open()
    };
    let mut frame = frame;
    let why = if free {
        m.run_fast(entry, insns, |m| frame(m))
    } else if turbo {
        m.run_realtime(entry, insns, |m| frame(m))
    } else {
        m.run_with(entry, insns, 2_000_000, |m| frame(m))
    };
    // Dropping the minifb window on Wayland prints proxy warnings; the process is about
    // to exit anyway, so let the OS reclaim it.
    std::mem::forget(win);
    if let Some(path) = save {
        // the window as last shown
        let rgb: Vec<u8> = buf.iter().flat_map(|p| [(p >> 16) as u8, (p >> 8) as u8, *p as u8]).collect();
        let f = std::fs::File::create(path).map(std::io::BufWriter::new);
        if let Ok(f) = f {
            let mut enc = png::Encoder::new(f, size.0 as u32, size.1 as u32);
            enc.set_color(png::ColorType::Rgb);
            if let Ok(mut w) = enc.write_header() {
                let _ = w.write_image_data(&rgb);
            }
        }
    }
    why
}

/// The host's local time as seconds since 2000-01-01 (for the phone's real-time clock).
fn host_epoch() -> Option<u64> {
    let unix = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).ok()?.as_secs() as i64;
    // UTC offset as +hhmm / -hhmm (std has no time zones)
    #[cfg(windows)]
    let out = std::process::Command::new("powershell")
        .args(["-NoProfile", "-Command", "(Get-Date).ToString('zzz').Replace(':','')"]).output().ok()?;
    #[cfg(not(windows))]
    let out = std::process::Command::new("date").arg("+%z").output().ok()?;
    let txt = String::from_utf8(out.stdout).ok()?;
    let z = txt.trim();
    let sign = if z.starts_with('-') { -1 } else { 1 };
    let (h, m): (i64, i64) = (z.get(1..3)?.parse().ok()?, z.get(3..5)?.parse().ok()?);
    Some(c3emu::rtc::epoch_from_unix(unix, sign * (h * 3600 + m * 60)))
}
