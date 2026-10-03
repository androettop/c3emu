//! HLE of the flash storage at the OS's XSR device layer (no NAND/BML emulation).
//!
//! The OS reaches XSR through a per-device ops table (filled by 0x801ED738):
//!   open   0x801ED738(dev, ops*)            -> 0 ok   (fills ops[0..11], records size)
//!   read   0x801ED6E8(dev, lsn, count, buf) -> STL_Read(0, dev+8, lsn, count, buf)
//!   write  0x801ED6B2(dev, lsn, count, buf) -> STL_Write(...)
//!   delete 0x801ED682(dev, lsn, count)      -> STL_Delete(...)
//!   ops[7] total sectors = [0x80F94F58 + 0xC + 4*dev], ops[8] sector size = 512.
//! Devices (static table @0x80AF7E0E): ids 0, 4, 2, 3. Each is an image of 512-byte
//! sectors: a sparse host file `<dir>/dev<N>.img` persisted across runs like real flash
//! (Backend::Dir), or a sparse in-memory disk (Backend::Memory, the web build).

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::rc::Rc;
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};

use crate::uc::{RegisterARM, Uc};

use crate::machine::{Machine, State};
use crate::mmu::{read_virt, write_virt};

pub const SECTOR: u64 = 512;
const DEV_OPEN: u64 = 0x801E_D738;
const DEV_READ: u64 = 0x801E_D6E8;
const DEV_WRITE: u64 = 0x801E_D6B2;
const DEV_DELETE: u64 = 0x801E_D682;
/// The 11 ops the OS installs (copied from the literal pool @0x801ED8FC).
const OPS: [u32; 11] = [
    0x801E_D6E9, 0x801E_D6B3, 0x801E_D683, 0x801E_D62B, 0x801E_D603, 0x801E_D557,
    0x801E_D4F1, 0x801E_D4E7, 0x801E_D4E1, 0x801E_D4D9, 0x801E_D4C9,
];
/// Per-device total-sector array used by ops[7].
const DEV_SECTORS_TABLE: u64 = 0x80F9_4F58 + 0xC;
/// Size given to every device (sectors). Real partition sizes are unknown.
pub const DEFAULT_DEV_SECTORS: u32 = 64 * 1024; // 32 MB

/// Sparse in-memory disk image: 64 KB chunks, absent chunks read as zeros.
pub struct SparseDisk {
    pub size: u64,
    chunks: HashMap<u64, Box<[u8]>>,
    /// Chunks written since the last take_dirty (for saving incrementally).
    dirty: std::collections::BTreeSet<u64>,
}

const CHUNK: u64 = 64 * 1024;

impl SparseDisk {
    pub fn new(size: u64) -> Self {
        SparseDisk { size, chunks: HashMap::new(), dirty: Default::default() }
    }

    pub fn read_at(&self, off: u64, buf: &mut [u8]) {
        let mut done = 0usize;
        while done < buf.len() {
            let a = off + done as u64;
            let (c, o) = (a / CHUNK, (a % CHUNK) as usize);
            let n = (CHUNK as usize - o).min(buf.len() - done);
            match self.chunks.get(&c) {
                Some(d) => buf[done..done + n].copy_from_slice(&d[o..o + n]),
                None => buf[done..done + n].fill(0),
            }
            done += n;
        }
    }

    pub fn write_at(&mut self, off: u64, data: &[u8]) {
        let mut done = 0usize;
        while done < data.len() {
            let a = off + done as u64;
            let (c, o) = (a / CHUNK, (a % CHUNK) as usize);
            let n = (CHUNK as usize - o).min(data.len() - done);
            let src = &data[done..done + n];
            self.dirty.insert(c);
            if let Some(d) = self.chunks.get_mut(&c) {
                d[o..o + n].copy_from_slice(src);
            } else if src.iter().any(|&b| b != 0) {
                let mut d = vec![0u8; CHUNK as usize].into_boxed_slice();
                d[o..o + n].copy_from_slice(src);
                self.chunks.insert(c, d);
            }
            done += n;
        }
        self.size = self.size.max(off + data.len() as u64);
    }

    /// The chunks holding data: (byte offset, 64 KB), for saving the image.
    pub fn chunks(&self) -> impl Iterator<Item = (u64, &[u8])> {
        self.chunks.iter().map(|(c, d)| (c * CHUNK, &d[..]))
    }

    pub fn chunk_size() -> u64 {
        CHUNK
    }

    /// Offsets of the chunks written since the last call.
    pub fn take_dirty(&mut self) -> Vec<u64> {
        std::mem::take(&mut self.dirty).into_iter().map(|c| c * CHUNK).collect()
    }

    /// The chunk at byte offset `off` (None: all zeros).
    pub fn chunk(&self, off: u64) -> Option<&[u8]> {
        self.chunks.get(&(off / CHUNK)).map(|d| &d[..])
    }

    /// Restore a saved chunk (not marked dirty).
    pub fn load_chunk(&mut self, off: u64, data: &[u8]) {
        let mut d = vec![0u8; CHUNK as usize].into_boxed_slice();
        let n = data.len().min(CHUNK as usize);
        d[..n].copy_from_slice(&data[..n]);
        self.chunks.insert(off / CHUNK, d);
        self.size = self.size.max(off + CHUNK);
    }
}

/// Storage devices kept in memory, shared with the host (the web page's file panel).
pub type MemDisks = Rc<RefCell<BTreeMap<u32, SparseDisk>>>;

pub enum Backend {
    /// `<dir>/dev<N>.img` files.
    Dir(PathBuf),
    Memory(MemDisks),
}

pub struct Storage {
    backend: Backend,
    files: BTreeMap<u32, File>,
    pub stats: BTreeMap<String, u64>,
}

impl Storage {
    fn file(&mut self, dev: u32) -> std::io::Result<&mut File> {
        let Backend::Dir(dir) = &self.backend else { unreachable!() };
        if !self.files.contains_key(&dev) {
            std::fs::create_dir_all(dir)?;
            let f = OpenOptions::new().read(true).write(true).create(true).truncate(false)
                .open(dir.join(format!("dev{dev}.img")))?;
            if f.metadata()?.len() < DEFAULT_DEV_SECTORS as u64 * SECTOR {
                f.set_len(DEFAULT_DEV_SECTORS as u64 * SECTOR)?; // sparse, reads as 0
            }
            self.files.insert(dev, f);
        }
        Ok(self.files.get_mut(&dev).unwrap())
    }

    /// Device size in bytes (creating the device).
    fn size(&mut self, dev: u32) -> std::io::Result<u64> {
        if let Backend::Memory(m) = &self.backend {
            let mut m = m.borrow_mut();
            let d = m.entry(dev).or_insert_with(|| SparseDisk::new(DEFAULT_DEV_SECTORS as u64 * SECTOR));
            return Ok(d.size);
        }
        self.file(dev)?.metadata().map(|m| m.len())
    }

    fn read(&mut self, dev: u32, off: u64, buf: &mut [u8]) -> std::io::Result<()> {
        if let Backend::Memory(m) = &self.backend {
            match m.borrow().get(&dev) {
                Some(d) => d.read_at(off, buf),
                None => buf.fill(0),
            }
            return Ok(());
        }
        let f = self.file(dev)?;
        f.seek(SeekFrom::Start(off))?;
        f.read_exact(buf)
    }

    fn write(&mut self, dev: u32, off: u64, data: &[u8]) -> std::io::Result<()> {
        if let Backend::Memory(m) = &self.backend {
            let mut m = m.borrow_mut();
            m.entry(dev).or_insert_with(|| SparseDisk::new(DEFAULT_DEV_SECTORS as u64 * SECTOR)).write_at(off, data);
            return Ok(());
        }
        let f = self.file(dev)?;
        f.seek(SeekFrom::Start(off))?;
        f.write_all(data)
    }
}

fn arg(uc: &Uc<'_>, n: i32) -> u32 {
    uc.reg_read(i32::from(RegisterARM::R0) + n).unwrap_or(0) as u32
}

/// 0x801ED834(dev): erase-unit size in bytes, from the XSR volume info (BML_GetVolInfo
/// 0x802D2DDC: pages per block x sectors per page x 512), also stored at
/// [0x80F94F58 + 8]. Without the BML it returned 0, and the file system later divided
/// by it (clib_stubs.c:119 "divided by zero!" in UI_TASK). Answered with a 128 KB
/// block (64 pages x 4 sectors), the usual OneNAND geometry.
const DEV_BLOCK_SIZE_FN: u64 = 0x801E_D834;
const DEV_BLOCK_SIZE_VAR: u64 = 0x80F9_4F58 + 8;
pub const BLOCK_BYTES: u32 = 64 * 4 * SECTOR as u32;

/// 0x801ED86A(dev): BML_Init + BML_Open of the volume (1 = failure). The device layer
/// above it is HLE'd, so the volume is always there.
const DEV_BML_OPEN: u64 = 0x801E_D86A;

pub fn install(m: &mut Machine, backend: Backend) {
    m.hle_fn(DEV_BML_OPEN, "storage:bml_open", Box::new(|_uc: &mut Uc<'_>, _s: &mut State| Some(0)));
    m.hle_fn(DEV_BLOCK_SIZE_FN, "storage:block_size", Box::new(|uc: &mut Uc<'_>, _s: &mut State| {
        let _ = uc.mem_write(DEV_BLOCK_SIZE_VAR, &BLOCK_BYTES.to_le_bytes());
        Some(BLOCK_BYTES)
    }));
    let sto = Rc::new(RefCell::new(Storage { backend, files: BTreeMap::new(), stats: BTreeMap::new() }));

    let s2 = sto.clone();
    m.hle_fn(DEV_OPEN, "storage:open", Box::new(move |uc: &mut Uc<'_>, s: &mut State| {
        let (dev, ops) = (arg(uc, 0), arg(uc, 1));
        let words: Vec<u8> = OPS.iter().flat_map(|w| w.to_le_bytes()).collect();
        write_virt(uc, ops, &words);
        // the device is as large as its image (at least DEFAULT_DEV_SECTORS; the C:
        // image made from the content file is larger)
        let sectors = s2.borrow_mut().size(dev).map(|n| (n / SECTOR) as u32);
        let ok = sectors.is_ok();
        let sectors = sectors.unwrap_or(DEFAULT_DEV_SECTORS);
        write_virt(uc, (DEV_SECTORS_TABLE + 4 * dev as u64) as u32, &sectors.to_le_bytes());
        s.event(format!("storage: open dev{dev} ({sectors} sectors){}",
                        if ok { "" } else { " FAILED to create image" }));
        Some(if ok { 0 } else { 1 })
    }));

    let s2 = sto.clone();
    m.hle_fn(DEV_READ, "storage:read", Box::new(move |uc: &mut Uc<'_>, s: &mut State| {
        let (dev, lsn, n, buf) = (arg(uc, 0), arg(uc, 1), arg(uc, 2), arg(uc, 3));
        let mut data = vec![0u8; (n as u64 * SECTOR) as usize];
        let mut st = s2.borrow_mut();
        *st.stats.entry(format!("read dev{dev}")).or_default() += n as u64;
        let ok = st.read(dev, lsn as u64 * SECTOR, &mut data).is_ok() && write_virt(uc, buf, &data);
        if s.hle_calls.get("storage:read").copied().unwrap_or(0) <= 8 {
            s.event(format!("storage: read dev{dev} lsn={lsn} n={n} -> 0x{buf:08x}{}", if ok { "" } else { " FAILED" }));
        }
        Some(if ok { 0 } else { 1 })
    }));

    let s2 = sto.clone();
    m.hle_fn(DEV_WRITE, "storage:write", Box::new(move |uc: &mut Uc<'_>, s: &mut State| {
        let (dev, lsn, n, buf) = (arg(uc, 0), arg(uc, 1), arg(uc, 2), arg(uc, 3));
        let mut data = vec![0u8; (n as u64 * SECTOR) as usize];
        let ok = read_virt(uc, buf, &mut data) && {
            let mut st = s2.borrow_mut();
            *st.stats.entry(format!("write dev{dev}")).or_default() += n as u64;
            st.write(dev, lsn as u64 * SECTOR, &data).is_ok()
        };
        if s.hle_calls.get("storage:write").copied().unwrap_or(0) <= 8 {
            s.event(format!("storage: write dev{dev} lsn={lsn} n={n}{}", if ok { "" } else { " FAILED" }));
        }
        Some(if ok { 0 } else { 1 })
    }));

    m.hle_fn(DEV_DELETE, "storage:delete", Box::new(move |_uc: &mut Uc<'_>, _s: &mut State| {
        Some(0) // trim hint: nothing to do for a host file
    }));
}
