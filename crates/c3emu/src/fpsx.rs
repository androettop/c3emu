//! Nokia BB5 FPSX container (.mcusw, .ppm_*, .image_*): header TLVs + data blocks.
//!
//! All multi-byte fields are big-endian. Header: `B2 | hlen[4] | count[4] | TLVs`.
//! Body: a stream of blocks, `0xFF` used as padding.
//! - `0x54` raw block:   `54 01 17 0E | flags[3] crc16[2] unk[1] | len[4] addr[4] | cksum[1] | data`
//! - `0x5D` named block: `5D 01 27 | sha1[20] 'q' name[12] | flags[3] crc16[2] | len[4] addr[4] | cksum[1] | data`
//!
//! `addr` is the flash target address. A `0x5D` block starts a named image; following
//! `0x54` blocks at contiguous addresses continue it.

fn be32(b: &[u8], off: usize) -> Result<u32, String> {
    b.get(off..off + 4)
        .map(|s| u32::from_be_bytes(s.try_into().unwrap()))
        .ok_or_else(|| format!("truncated at 0x{off:x}"))
}

pub struct Tlv {
    pub tag: u8,
    pub value: Vec<u8>,
}

/// One contiguous image (a named block plus its continuation blocks).
pub struct Region {
    pub name: Option<String>,
    pub addr: u32,
    pub flags: [u8; 3],
    pub data: Vec<u8>,
    pub blocks: usize,
}

impl Region {
    /// File name used by the old extractor: `<addr:08x>_<name|raw>.bin`.
    pub fn file_name(&self) -> String {
        format!("{:08x}_{}.bin", self.addr, self.name.as_deref().unwrap_or("raw"))
    }
}

pub struct Fpsx {
    pub tlvs: Vec<Tlv>,
    pub regions: Vec<Region>,
}

impl Fpsx {
    pub fn parse(buf: &[u8]) -> Result<Fpsx, String> {
        if buf.first() != Some(&0xB2) {
            return Err("not an FPSX (B2) file".into());
        }
        let hlen = be32(buf, 1)? as usize;
        let count = be32(buf, 5)? as usize;
        let end = 5 + hlen;
        let mut tlvs = Vec::new();
        let mut p = 9;
        while p < end && tlvs.len() < count {
            let l = *buf.get(p + 1).ok_or("truncated TLV")? as usize;
            let value = buf.get(p + 2..p + 2 + l).ok_or("truncated TLV")?.to_vec();
            tlvs.push(Tlv { tag: buf[p], value });
            p += 2 + l;
        }

        let mut regions: Vec<Region> = Vec::new();
        let mut off = end;
        let mut name: Option<String> = None;
        while off < buf.len() {
            let t = buf[off];
            let h = match t {
                0xFF => {
                    off += 1;
                    continue;
                }
                0x54 => off + 4,
                0x5D => {
                    let raw = buf.get(off + 24..off + 36).ok_or("truncated name")?;
                    let n = raw.split(|&c| c == 0).next().unwrap_or(&[]);
                    name = Some(n.iter().map(|&c| c as char).collect());
                    off + 36
                }
                _ => return Err(format!("unknown block type 0x{t:02x} at 0x{off:x}")),
            };
            let flags: [u8; 3] = buf.get(h..h + 3).ok_or("truncated block")?.try_into().unwrap();
            let lo = if t == 0x54 { h + 6 } else { h + 5 };
            let len = be32(buf, lo)? as usize;
            let addr = be32(buf, lo + 4)?;
            let dstart = lo + 9;
            let data = buf.get(dstart..dstart + len).ok_or("truncated block data")?;
            let cont = t == 0x54
                && regions.last().is_some_and(|r| r.addr as usize + r.data.len() == addr as usize);
            if cont {
                let r = regions.last_mut().unwrap();
                r.data.extend_from_slice(data);
                r.blocks += 1;
            } else {
                regions.push(Region {
                    name: if t == 0x5D { name.clone() } else { None },
                    addr,
                    flags,
                    data: data.to_vec(),
                    blocks: 1,
                });
            }
            off = dstart + len;
        }
        Ok(Fpsx { tlvs, regions })
    }

    pub fn image(&self, name: &str) -> Option<&Region> {
        self.regions.iter().find(|r| r.name.as_deref() == Some(name))
    }
}
