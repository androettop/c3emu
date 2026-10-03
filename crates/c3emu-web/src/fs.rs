//! The phone's C: drive (XSR device 0, a FAT16 "superfloppy") seen from the host: list,
//! read, write and delete files for the file panel of the web page.

use std::io::{self, Read, Seek, SeekFrom, Write};

use c3emu::storage::SparseDisk;

/// The disk as a byte stream for fatfs.
pub struct DiskIo<'d> {
    disk: &'d mut SparseDisk,
    pos: u64,
}

impl Read for DiskIo<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = (self.disk.size.saturating_sub(self.pos) as usize).min(buf.len());
        self.disk.read_at(self.pos, &mut buf[..n]);
        self.pos += n as u64;
        Ok(n)
    }
}

impl Write for DiskIo<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = (self.disk.size.saturating_sub(self.pos) as usize).min(buf.len());
        if n == 0 && !buf.is_empty() {
            return Err(io::Error::new(io::ErrorKind::WriteZero, "end of disk"));
        }
        self.disk.write_at(self.pos, &buf[..n]);
        self.pos += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Seek for DiskIo<'_> {
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        let p = match to {
            SeekFrom::Start(p) => p as i64,
            SeekFrom::End(d) => self.disk.size as i64 + d,
            SeekFrom::Current(d) => self.pos as i64 + d,
        };
        if p < 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "seek before start"));
        }
        self.pos = p as u64;
        Ok(self.pos)
    }
}

type Fs<'d> = fatfs::FileSystem<DiskIo<'d>>;

fn open(disk: &mut SparseDisk) -> Result<Fs<'_>, String> {
    fatfs::FileSystem::new(DiskIo { disk, pos: 0 }, fatfs::FsOptions::new())
        .map_err(|e| format!("no FAT file system on the phone memory yet ({e}): start the phone once"))
}

fn parts(path: &str) -> Vec<&str> {
    path.split('/').filter(|p| !p.is_empty()).collect()
}

fn dir<'a, 'd>(fs: &'a Fs<'d>, path: &str) -> Result<fatfs::Dir<'a, DiskIo<'d>>, String> {
    let p = parts(path).join("/");
    if p.is_empty() { Ok(fs.root_dir()) } else { fs.root_dir().open_dir(&p).map_err(|e| format!("{path}: {e}")) }
}

pub struct Entry {
    pub name: String,
    pub dir: bool,
    pub size: u64,
}

pub fn list(disk: &mut SparseDisk, path: &str) -> Result<(Vec<Entry>, u64, u64), String> {
    let fs = open(disk)?;
    let mut out = Vec::new();
    for e in dir(&fs, path)?.iter() {
        let e = e.map_err(|e| e.to_string())?;
        let name = e.file_name();
        if name == "." || name == ".." {
            continue;
        }
        out.push(Entry { name, dir: e.is_dir(), size: e.len() });
    }
    out.sort_by(|a, b| b.dir.cmp(&a.dir).then(a.name.to_lowercase().cmp(&b.name.to_lowercase())));
    let st = fs.stats().map_err(|e| e.to_string())?;
    let cl = st.cluster_size() as u64;
    Ok((out, st.total_clusters() as u64 * cl, st.free_clusters() as u64 * cl))
}

pub fn read(disk: &mut SparseDisk, path: &str) -> Result<Vec<u8>, String> {
    let fs = open(disk)?;
    let p = parts(path).join("/");
    let mut f = fs.root_dir().open_file(&p).map_err(|e| format!("{path}: {e}"))?;
    let mut v = Vec::new();
    f.read_to_end(&mut v).map_err(|e| e.to_string())?;
    Ok(v)
}

/// Create (or replace) a file, creating the folders on the way.
pub fn write(disk: &mut SparseDisk, path: &str, data: &[u8]) -> Result<(), String> {
    let fs = open(disk)?;
    let ps = parts(path);
    let (name, dirs) = ps.split_last().ok_or("empty path")?;
    let mut d = fs.root_dir();
    for p in dirs {
        d = match d.open_dir(p) {
            Ok(x) => x,
            Err(_) => d.create_dir(p).map_err(|e| format!("{p}: {e}"))?,
        };
    }
    let mut f = d.create_file(name).map_err(|e| format!("{name}: {e}"))?;
    f.truncate().map_err(|e| e.to_string())?;
    f.write_all(data).map_err(|e| format!("{name}: {e}"))?;
    f.flush().map_err(|e| e.to_string())?;
    Ok(())
}

pub fn mkdir(disk: &mut SparseDisk, path: &str) -> Result<(), String> {
    let fs = open(disk)?;
    let mut d = fs.root_dir();
    for p in parts(path) {
        d = match d.open_dir(p) {
            Ok(x) => x,
            Err(_) => d.create_dir(p).map_err(|e| format!("{p}: {e}"))?,
        };
    }
    Ok(())
}

/// Delete a file or a folder with everything in it.
pub fn remove(disk: &mut SparseDisk, path: &str) -> Result<(), String> {
    let fs = open(disk)?;
    fn rm_tree(d: &fatfs::Dir<'_, DiskIo<'_>>, name: &str) -> Result<(), String> {
        let sub = d.open_dir(name).map_err(|e| e.to_string())?;
        let names: Vec<(String, bool)> = sub.iter().filter_map(|e| e.ok())
            .map(|e| (e.file_name(), e.is_dir()))
            .filter(|(n, _)| n != "." && n != "..").collect();
        for (n, is_dir) in names {
            if is_dir {
                rm_tree(&sub, &n)?;
            } else {
                sub.remove(&n).map_err(|e| e.to_string())?;
            }
        }
        d.remove(name).map_err(|e| e.to_string())
    }
    let ps = parts(path);
    let (name, dirs) = ps.split_last().ok_or("empty path")?;
    let d = dir(&fs, &dirs.join("/"))?;
    let is_dir = d.iter().filter_map(|e| e.ok()).any(|e| e.file_name().eq_ignore_ascii_case(name) && e.is_dir());
    if is_dir { rm_tree(&d, name) } else { d.remove(name).map_err(|e| format!("{path}: {e}")) }
}
