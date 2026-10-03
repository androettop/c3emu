//! c3emu in the browser. The emulator (interpreter backend) behind a small C ABI that
//! the page's worker (www/worker.js) drives: hand over the firmware files, boot, run
//! for a time budget, fetch the screen / audio, send keys, and reach the phone's C:
//! drive (file panel) and its flash images (saved in IndexedDB).
//!
//! Buffers: the page allocates with c3_alloc, passes (ptr, len); results that are not
//! plain numbers (JSON, file contents, error text) are left in a result buffer read
//! with c3_result_ptr / c3_result_len.

mod fs;

use std::cell::RefCell;

use c3emu::clock::Instant;
use c3emu::machine::{parse_mmio_model, Machine};
use c3emu::storage::{MemDisks, SparseDisk, DEFAULT_DEV_SECTORS, SECTOR};

const MODEL: &str = c3emu::machine::DEFAULT_MMIO_MODEL;
/// File kinds handed over with c3_set_file.
const FIRMWARE: usize = 0;
const PPM: usize = 1;
const CONTENT: usize = 2;
/// The C: drive (XSR device 0).
const DRIVE_C: u32 = 0;

#[derive(Default)]
struct Emu {
    files: [Option<Vec<u8>>; 3],
    disks: MemDisks,
    machine: Option<Machine<'static>>,
    sink: c3emu::audio::Sink,
    frame: Vec<u8>,
    audio: Vec<i16>,
    result: Vec<u8>,
    stopped: Option<String>,
    /// Time of day for the phone's clock at boot (seconds since 2000).
    epoch: Option<u64>,
}

thread_local! {
    static EMU: RefCell<Emu> = RefCell::new(Emu::default());
}

fn with<T>(f: impl FnOnce(&mut Emu) -> T) -> T {
    EMU.with(|e| f(&mut e.borrow_mut()))
}

fn set_result(e: &mut Emu, data: Vec<u8>) {
    e.result = data;
}

fn json_str(s: &str) -> String {
    let mut o = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

/// # Safety
/// (ptr, len) must come from c3_alloc.
unsafe fn take_vec(ptr: *mut u8, len: usize) -> Vec<u8> {
    unsafe { Vec::from_raw_parts(ptr, len, len) }
}

/// # Safety
/// (ptr, len) must be a live buffer from c3_alloc.
unsafe fn borrow_str<'a>(ptr: *const u8, len: usize) -> &'a str {
    unsafe { std::str::from_utf8(std::slice::from_raw_parts(ptr, len)).unwrap_or("") }
}

#[unsafe(no_mangle)]
pub extern "C" fn c3_alloc(len: usize) -> *mut u8 {
    let mut v = vec![0u8; len];
    let p = v.as_mut_ptr();
    std::mem::forget(v);
    p
}

/// # Safety
/// (ptr, len) must come from c3_alloc and not be used afterwards.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn c3_free(ptr: *mut u8, len: usize) {
    drop(unsafe { take_vec(ptr, len) });
}

#[unsafe(no_mangle)]
pub extern "C" fn c3_result_ptr() -> *const u8 {
    with(|e| e.result.as_ptr())
}

#[unsafe(no_mangle)]
pub extern "C" fn c3_result_len() -> usize {
    with(|e| e.result.len())
}

/// Hand over a firmware file (kind 0 = MCUSW, 1 = PPM, 2 = content image). Takes
/// ownership of the buffer.
///
/// # Safety
/// (ptr, len) must come from c3_alloc.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn c3_set_file(kind: usize, ptr: *mut u8, len: usize) {
    let v = unsafe { take_vec(ptr, len) };
    with(|e| {
        if kind < 3 {
            e.files[kind] = Some(v);
        }
    });
}

/// Restore one saved 64 KB chunk of a flash image (before c3_boot).
///
/// # Safety
/// (ptr, len) must be a live buffer from c3_alloc.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn c3_disk_load_chunk(dev: u32, off: f64, ptr: *const u8, len: usize) {
    let data = unsafe { std::slice::from_raw_parts(ptr, len) };
    with(|e| {
        let mut d = e.disks.borrow_mut();
        d.entry(dev).or_insert_with(|| SparseDisk::new(0)).load_chunk(off as u64, data);
    });
}

/// Size of a restored flash image (after its chunks).
#[unsafe(no_mangle)]
pub extern "C" fn c3_disk_set_size(dev: u32, size: f64) {
    with(|e| {
        e.disks.borrow_mut().entry(dev).or_insert_with(|| SparseDisk::new(0)).size = size as u64;
    });
}

/// Forget all flash images (factory reset; takes effect at the next boot).
#[unsafe(no_mangle)]
pub extern "C" fn c3_disks_clear() {
    with(|e| e.disks.borrow_mut().clear());
}

/// Chunks of the flash images written since the last call, for saving: records of
/// u32 dev, f64 offset, u32 flags (1 = data follows, 0 = chunk is all zeros), u64 disk
/// size, then 64 KB of data. Returns the record count.
#[unsafe(no_mangle)]
pub extern "C" fn c3_disks_dirty() -> u32 {
    with(|e| {
        let mut out = Vec::new();
        let mut n = 0;
        for (&dev, d) in e.disks.borrow_mut().iter_mut() {
            for off in d.take_dirty() {
                out.extend_from_slice(&dev.to_le_bytes());
                out.extend_from_slice(&(off as f64).to_le_bytes());
                match d.chunk(off) {
                    Some(c) => {
                        out.extend_from_slice(&1u32.to_le_bytes());
                        out.extend_from_slice(&d.size.to_le_bytes());
                        out.extend_from_slice(c);
                    }
                    None => {
                        out.extend_from_slice(&0u32.to_le_bytes());
                        out.extend_from_slice(&d.size.to_le_bytes());
                    }
                }
                n += 1;
            }
        }
        e.result = out;
        n
    })
}

fn boot(e: &mut Emu) -> Result<(), String> {
    e.machine = None;
    e.stopped = None;
    let fw = e.files[FIRMWARE].as_ref().ok_or("no firmware (.mcusw) loaded")?;
    let model = parse_mmio_model(MODEL)?;
    let mut m = Machine::new(0, model)?;
    // factory C: drive from the content image, unless the phone memory already exists
    if !e.disks.borrow().contains_key(&DRIVE_C) {
        if let Some(c) = &e.files[CONTENT] {
            let d = c3emu::content::to_disk(c, DEFAULT_DEV_SECTORS as u64 * SECTOR)?;
            e.disks.borrow_mut().insert(DRIVE_C, d);
        }
    }
    let opt = c3emu::boot::BootOptions {
        oslog_echo: false,
        isi: false,
        storage: c3emu::storage::Backend::Memory(e.disks.clone()),
    };
    let (entry, _log) = c3emu::boot::setup(&mut m, fw, e.files[PPM].as_deref(), opt)?;
    if let Ok(mut q) = e.sink.lock() {
        q.clear();
    }
    m.st.borrow_mut().audio.sink = Some(e.sink.clone());
    {
        // paced to real time (the phone sleeps at WFI while ahead of the wall clock)
        let mut st = m.st.borrow_mut();
        let tick = c3emu::soc::now(&st);
        st.soc.realtime = Some((Instant::now(), tick));
        if let Some(ep) = e.epoch {
            st.rtc.epoch = ep;
        }
    }
    m.fast_start(entry);
    e.machine = Some(m);
    Ok(())
}

/// The host's local time (Unix seconds and UTC offset in seconds) for the phone's
/// real-time clock at the next boot.
#[unsafe(no_mangle)]
pub extern "C" fn c3_set_time(unix: f64, utc_offset: f64) {
    with(|e| e.epoch = Some(c3emu::rtc::epoch_from_unix(unix as i64, utc_offset as i64)));
}

/// Boot the phone (again) from the files handed over. 0 = ok, else the error text is
/// in the result buffer.
#[unsafe(no_mangle)]
pub extern "C" fn c3_boot() -> i32 {
    with(|e| match boot(e) {
        Ok(()) => 0,
        Err(msg) => {
            set_result(e, msg.into_bytes());
            1
        }
    })
}

/// Run for about `budget_ms`. Returns how long the page should wait before the next
/// call (the phone is idle until then; 0 = call again soon), or -1 when the emulator
/// stopped (reason in the result buffer).
#[unsafe(no_mangle)]
pub extern "C" fn c3_run(budget_ms: f64) -> f64 {
    with(|e| {
        let Some(m) = e.machine.as_mut() else { return -1.0 };
        if e.stopped.is_some() {
            return -1.0;
        }
        let t0 = Instant::now();
        loop {
            if let Err(why) = m.fast_slice(u64::MAX) {
                e.result = why.clone().into_bytes();
                e.stopped = Some(why);
                return -1.0;
            }
            let slept = c3emu::clock::take_sleep();
            if slept > 0.0 {
                return slept;
            }
            if t0.elapsed().as_secs_f64() * 1000.0 >= budget_ms {
                return 0.0;
            }
        }
    })
}

/// The screen as 320 x 240 RGBA (refreshed by this call).
#[unsafe(no_mangle)]
pub extern "C" fn c3_frame() -> *const u8 {
    with(|e| {
        let (w, h) = (c3emu::lcd::WIDTH, c3emu::lcd::HEIGHT);
        e.frame.resize(w * h * 4, 255);
        if let Some(m) = &e.machine {
            let st = m.st.borrow();
            for (px, rgb) in e.frame.chunks_exact_mut(4).zip(st.panel.gram.chunks_exact(3)) {
                px[..3].copy_from_slice(rgb);
                px[3] = 255;
            }
        }
        e.frame.as_ptr()
    })
}

/// Changes whenever the screen may have changed.
#[unsafe(no_mangle)]
pub extern "C" fn c3_frame_serial() -> u32 {
    with(|e| e.machine.as_ref().map_or(0, |m| m.st.borrow().panel.pixels_written as u32))
}

/// Take the audio produced so far (interleaved stereo i16 at 44.1 kHz): returns the
/// sample count; the samples are at c3_audio_ptr.
#[unsafe(no_mangle)]
pub extern "C" fn c3_audio() -> usize {
    with(|e| {
        e.audio.clear();
        if let Ok(mut q) = e.sink.lock() {
            e.audio.extend(q.drain(..));
        }
        e.audio.len()
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn c3_audio_ptr() -> *const i16 {
    with(|e| e.audio.as_ptr())
}

/// A key changed: matrix index (row * 8 + column) or 255 for the red key. Short presses
/// are single taps, long ones long presses (keypad::host_down / host_up).
#[unsafe(no_mangle)]
pub extern "C" fn c3_key(index: u32, down: u32) {
    with(|e| {
        let Some(m) = &e.machine else { return };
        let mut st = m.st.borrow_mut();
        if down != 0 {
            c3emu::keypad::host_down(&mut st, index as u8);
        } else {
            c3emu::keypad::host_up(&mut st, index as u8);
        }
    })
}

/// Status as JSON: {"running", "stopped", "frames", "changed", "idle", "flash_writes"}.
#[unsafe(no_mangle)]
pub extern "C" fn c3_stats() {
    with(|e| {
        let s = match &e.machine {
            None => "{\"running\":false}".to_string(),
            Some(m) => {
                let st = m.st.borrow();
                format!("{{\"running\":{},\"stopped\":{},\"frames\":{},\"changed\":{},\"idle\":{},\"flash_writes\":{}}}",
                        e.stopped.is_none(),
                        e.stopped.as_deref().map(json_str).unwrap_or("null".into()),
                        st.panel.frames, st.panel.frames_changed, st.soc.wfi_count,
                        st.hle_calls.get("storage:write").copied().unwrap_or(0))
            }
        };
        e.result = s.into_bytes();
    })
}

/// Stop the phone (before changing its files; boot again afterwards).
#[unsafe(no_mangle)]
pub extern "C" fn c3_stop() {
    with(|e| e.machine = None);
}

fn fs_result(e: &mut Emu, r: Result<Vec<u8>, String>) -> i32 {
    match r {
        Ok(v) => {
            e.result = v;
            0
        }
        Err(msg) => {
            e.result = msg.into_bytes();
            1
        }
    }
}

fn drive_c<T>(e: &mut Emu, f: impl FnOnce(&mut SparseDisk) -> Result<T, String>) -> Result<T, String> {
    let disks: MemDisks = e.disks.clone();
    let mut d = disks.borrow_mut();
    let disk = d.get_mut(&DRIVE_C).ok_or("the phone memory (C:) does not exist yet: start the phone once")?;
    f(disk)
}

/// List a folder of C: as JSON {"entries":[{"name","dir","size"}], "total", "free"}.
///
/// # Safety
/// (ptr, len) must be a live buffer from c3_alloc.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn c3_fs_list(ptr: *const u8, len: usize) -> i32 {
    let path = unsafe { borrow_str(ptr, len) }.to_string();
    with(|e| {
        let r = drive_c(e, |d| fs::list(d, &path)).map(|(entries, total, free)| {
            let items: Vec<String> = entries.iter().map(|x| {
                format!("{{\"name\":{},\"dir\":{},\"size\":{}}}", json_str(&x.name), x.dir, x.size)
            }).collect();
            format!("{{\"entries\":[{}],\"total\":{total},\"free\":{free}}}", items.join(",")).into_bytes()
        });
        fs_result(e, r)
    })
}

/// Read a file of C: into the result buffer.
///
/// # Safety
/// (ptr, len) must be a live buffer from c3_alloc.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn c3_fs_read(ptr: *const u8, len: usize) -> i32 {
    let path = unsafe { borrow_str(ptr, len) }.to_string();
    with(|e| {
        let r = drive_c(e, |d| fs::read(d, &path));
        fs_result(e, r)
    })
}

/// Write a file to C: (folders created as needed).
///
/// # Safety
/// Both buffers must be live buffers from c3_alloc.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn c3_fs_write(p: *const u8, pl: usize, d: *const u8, dl: usize) -> i32 {
    let path = unsafe { borrow_str(p, pl) }.to_string();
    let data = unsafe { std::slice::from_raw_parts(d, dl) }.to_vec();
    with(|e| {
        let r = drive_c(e, |disk| fs::write(disk, &path, &data)).map(|_| Vec::new());
        fs_result(e, r)
    })
}

/// Create a folder on C:.
///
/// # Safety
/// (ptr, len) must be a live buffer from c3_alloc.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn c3_fs_mkdir(ptr: *const u8, len: usize) -> i32 {
    let path = unsafe { borrow_str(ptr, len) }.to_string();
    with(|e| {
        let r = drive_c(e, |d| fs::mkdir(d, &path)).map(|_| Vec::new());
        fs_result(e, r)
    })
}

/// Delete a file or folder (recursively) on C:.
///
/// # Safety
/// (ptr, len) must be a live buffer from c3_alloc.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn c3_fs_remove(ptr: *const u8, len: usize) -> i32 {
    let path = unsafe { borrow_str(ptr, len) }.to_string();
    with(|e| {
        let r = drive_c(e, |d| fs::remove(d, &path)).map(|_| Vec::new());
        fs_result(e, r)
    })
}

/// Check of the file panel against a content image (C3EMU_CONTENT=FILE cargo test).
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_drive_lists_and_writes() {
        let Some(img) = std::env::var_os("C3EMU_CONTENT").and_then(|p| std::fs::read(p).ok()) else { return };
        let mut d = c3emu::content::to_disk(&img, DEFAULT_DEV_SECTORS as u64 * SECTOR).unwrap();
        let (root, total, free) = fs::list(&mut d, "/").unwrap();
        for x in &root {
            println!("{} {} {}", if x.dir { "D" } else { "F" }, x.name, x.size);
        }
        println!("total {total} free {free}");
        fs::write(&mut d, "/Prueba/hola.txt", b"hola mundo").unwrap();
        assert_eq!(fs::read(&mut d, "/Prueba/hola.txt").unwrap(), b"hola mundo");
        fs::remove(&mut d, "/Prueba").unwrap();
        assert!(fs::list(&mut d, "/").unwrap().0.iter().all(|x| x.name != "Prueba"));
    }
}
