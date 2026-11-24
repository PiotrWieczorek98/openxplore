//! Xbox 360 FATX (big-endian, "XTAF") filesystem.
use anyhow::{bail, ensure, Context, Result};
use std::collections::BTreeSet;
use std::fs::File;
use std::io::{Read, Write};
use std::os::unix::fs::FileExt;

const HEADER_SIZE: u64 = 0x1000;
const DIRENT_SIZE: usize = 64;
const NAME_MAX: usize = 42;
pub const ATTR_DIR: u8 = 0x10;
const NAME_DELETED: u8 = 0xE5;
const EOC: u32 = 0xFFFF_FFFF;

/// Retail Xbox 360 HDD layout (offset, length; 0 = to end of disk).
const RETAIL: &[(&str, u64, u64)] = &[
    ("syscache", 0x80000, 0x8000_0000),
    ("gamecache", 0x8008_0000, 0xA0E3_0000),
    ("sysext", 0x1_0C08_0000, 0xCE3_0000),
    ("sysext2", 0x1_18EB_0000, 0x800_0000),
    ("compat", 0x1_20EB_0000, 0x1000_0000),
    ("data", 0x1_30EB_0000, 0),
];

/// Partitions with a valid XTAF header: (name, offset, length).
/// A raw partition image (magic at offset 0) is reported as "image".
pub fn find_partitions(dev: &File) -> Result<Vec<(&'static str, u64, u64)>> {
    let disk = device_size(dev)?;
    let has_magic = |off: u64| {
        let mut m = [0u8; 4];
        dev.read_exact_at(&mut m, off).is_ok() && &m == b"XTAF"
    };
    if has_magic(0) {
        return Ok(vec![("image", 0, disk)]);
    }
    Ok(RETAIL
        .iter()
        .filter(|(_, off, _)| *off < disk && has_magic(*off))
        .map(|&(n, off, len)| (n, off, if len == 0 { disk - off } else { len.min(disk - off) }))
        .collect())
}

fn device_size(dev: &File) -> Result<u64> {
    use std::io::{Seek, SeekFrom};
    // seek works for both regular files and block devices (metadata().len() is 0 for the latter)
    Ok((&*dev).seek(SeekFrom::End(0))?)
}

#[derive(Clone, Debug)]
pub struct Entry {
    pub name: String,
    pub attr: u8,
    pub first: u32,
    pub size: u32,
    /// FAT-style packed timestamp (date << 16 | time), year base 1980.
    pub mtime: u32,
    /// Byte position of this dirent on the device (None for root).
    pos: Option<u64>,
}

impl Entry {
    pub fn is_dir(&self) -> bool {
        self.attr & ATTR_DIR != 0
    }
}

pub struct Volume {
    dev: File,
    pub cluster_size: u64,
    root: u32,
    fat32: bool,
    fat_off: u64,
    data_off: u64,
    /// Usable clusters are 1..nclusters.
    nclusters: u32,
    // shortcut: whole FAT in RAM (~4 bytes per 16K cluster, ~120MB for 500GB), page it if 2TB+ disks matter
    fat: Vec<u32>,
    dirty: BTreeSet<u32>,
}

impl Volume {
    pub fn open(dev: File, start: u64, len: u64) -> Result<Self> {
        let mut h = [0u8; 16];
        dev.read_exact_at(&mut h, start).context("reading FATX header")?;
        ensure!(&h[0..4] == b"XTAF", "no XTAF magic at offset {start:#x}");
        let spc = be32(&h[8..]);
        let root = be32(&h[12..]);
        ensure!(spc.is_power_of_two() && spc <= 1024, "bad sectors-per-cluster {spc}");
        let cluster_size = spc as u64 * 512;

        let entries = len / cluster_size;
        let fat32 = entries >= 0xFFF0;
        let esz = if fat32 { 4 } else { 2 };
        let fat_size = (entries * esz).next_multiple_of(0x1000);
        let fat_off = start + HEADER_SIZE;
        let data_off = fat_off + fat_size;
        ensure!(data_off < start + len, "partition too small");
        let nclusters = entries.min((start + len - data_off) / cluster_size + 1) as u32;

        let mut raw = vec![0u8; (entries * esz) as usize];
        dev.read_exact_at(&mut raw, fat_off).context("reading FAT")?;
        let fat = if fat32 {
            raw.chunks_exact(4).map(be32).collect()
        } else {
            // widen FAT16 markers (0xFFF7 bad, 0xFFF8+ end) to their 32-bit form
            raw.chunks_exact(2)
                .map(|c| match u16::from_be_bytes([c[0], c[1]]) as u32 {
                    v if v >= 0xFFF0 => v | 0xFFFF_0000,
                    v => v,
                })
                .collect()
        };
        let v = Volume { dev, cluster_size, root, fat32, fat_off, data_off, nclusters, fat, dirty: BTreeSet::new() };
        ensure!(v.valid(root), "root cluster {root} out of range");
        Ok(v)
    }

    pub fn root(&self) -> Entry {
        Entry { name: String::new(), attr: ATTR_DIR, first: self.root, size: 0, mtime: 0, pos: None }
    }

    /// (used clusters, total clusters)
    pub fn usage(&self) -> (u64, u64) {
        let used = self.fat[1..self.nclusters as usize].iter().filter(|&&v| v != 0).count();
        (used as u64, self.nclusters as u64 - 1)
    }

    fn valid(&self, c: u32) -> bool {
        c >= 1 && c < self.nclusters
    }

    fn cluster_pos(&self, c: u32) -> u64 {
        self.data_off + (c as u64 - 1) * self.cluster_size
    }

    fn chain(&self, first: u32) -> Result<Vec<u32>> {
        let mut out = Vec::new();
        let mut c = first;
        while c < 0xFFFF_FFF0 {
            ensure!(self.valid(c), "corrupt cluster chain: {c:#x}");
            ensure!(out.len() < self.nclusters as usize, "cluster chain loop at {first}");
            out.push(c);
            c = self.fat[c as usize];
        }
        ensure!(c >= 0xFFFF_FFF8, "corrupt chain terminator {c:#x}");
        Ok(out)
    }

    fn read_cluster(&self, c: u32) -> Result<Vec<u8>> {
        let mut buf = vec![0u8; self.cluster_size as usize];
        self.dev.read_exact_at(&mut buf, self.cluster_pos(c))?;
        Ok(buf)
    }

    pub fn read_dir(&self, dir: &Entry) -> Result<Vec<Entry>> {
        ensure!(dir.is_dir(), "{} is not a directory", dir.name);
        let mut out = Vec::new();
        for c in self.chain(dir.first)? {
            let buf = self.read_cluster(c)?;
            for (i, d) in buf.chunks_exact(DIRENT_SIZE).enumerate() {
                let n = d[0];
                if n == 0x00 || n == 0xFF {
                    return Ok(out);
                }
                if n == NAME_DELETED || n as usize > NAME_MAX {
                    continue;
                }
                out.push(Entry {
                    name: String::from_utf8_lossy(&d[2..2 + n as usize]).into_owned(),
                    attr: d[1],
                    first: be32(&d[44..]),
                    size: be32(&d[48..]),
                    mtime: be32(&d[56..]),
                    pos: Some(self.cluster_pos(c) + (i * DIRENT_SIZE) as u64),
                });
            }
        }
        Ok(out)
    }

    pub fn find(&self, dir: &Entry, name: &str) -> Result<Option<Entry>> {
        Ok(self.read_dir(dir)?.into_iter().find(|e| e.name.eq_ignore_ascii_case(name)))
    }

    /// Resolve a '/'-separated path from the root.
    pub fn lookup(&self, path: &str) -> Result<Entry> {
        let mut cur = self.root();
        for part in path.split('/').filter(|p| !p.is_empty()) {
            cur = self.find(&cur, part)?.with_context(|| format!("{path}: not found"))?;
        }
        Ok(cur)
    }

    /// Read `buf.len()` bytes of a file starting at `off` (clamped to file size). Returns bytes read.
    pub fn read_at(&self, e: &Entry, off: u64, buf: &mut [u8]) -> Result<usize> {
        let size = e.size as u64;
        if off >= size || e.first == 0 {
            return Ok(0);
        }
        let want = buf.len().min((size - off) as usize);
        let chain = self.chain(e.first)?;
        let mut done = 0;
        while done < want {
            let pos = off + done as u64;
            let c = *chain.get((pos / self.cluster_size) as usize).context("file shorter than its size")?;
            let in_c = pos % self.cluster_size;
            let n = (want - done).min((self.cluster_size - in_c) as usize);
            self.dev.read_exact_at(&mut buf[done..done + n], self.cluster_pos(c) + in_c)?;
            done += n;
        }
        Ok(want)
    }

    pub fn copy_out(&self, e: &Entry, out: &mut impl Write) -> Result<()> {
        let mut buf = vec![0u8; self.cluster_size as usize];
        let mut off = 0;
        loop {
            let n = self.read_at(e, off, &mut buf)?;
            if n == 0 {
                return Ok(());
            }
            out.write_all(&buf[..n])?;
            off += n as u64;
        }
    }

    // ---------- writing ----------

    fn set_fat(&mut self, c: u32, v: u32) {
        self.fat[c as usize] = v;
        self.dirty.insert(c);
    }

    fn alloc(&mut self, n: usize) -> Result<Vec<u32>> {
        let free: Vec<u32> = (1..self.nclusters).filter(|&c| self.fat[c as usize] == 0).take(n).collect();
        ensure!(free.len() == n, "volume full");
        for w in free.windows(2) {
            self.set_fat(w[0], w[1]);
        }
        if let Some(&last) = free.last() {
            self.set_fat(last, EOC);
        }
        Ok(free)
    }

    fn free_chain(&mut self, first: u32) -> Result<()> {
        if first != 0 {
            for c in self.chain(first)? {
                self.set_fat(c, 0);
            }
        }
        Ok(())
    }

    /// Write dirty FAT entries to disk. Call after every mutation.
    pub fn flush(&mut self) -> Result<()> {
        for c in std::mem::take(&mut self.dirty) {
            let v = self.fat[c as usize];
            if self.fat32 {
                self.dev.write_all_at(&v.to_be_bytes(), self.fat_off + c as u64 * 4)?;
            } else {
                self.dev.write_all_at(&(v as u16).to_be_bytes(), self.fat_off + c as u64 * 2)?;
            }
        }
        self.dev.sync_all()?;
        Ok(())
    }

    /// Find (or make) a free dirent slot in `dir`, returning its device position.
    fn free_slot(&mut self, dir: &Entry) -> Result<u64> {
        let chain = self.chain(dir.first)?;
        let per = self.cluster_size as usize / DIRENT_SIZE;
        for (ci, &c) in chain.iter().enumerate() {
            let buf = self.read_cluster(c)?;
            for i in 0..per {
                let n = buf[i * DIRENT_SIZE];
                if n == NAME_DELETED {
                    return Ok(self.cluster_pos(c) + (i * DIRENT_SIZE) as u64);
                }
                if n == 0x00 || n == 0xFF {
                    // taking the terminator slot: make sure the next slot terminates the listing
                    let next = if i + 1 < per {
                        Some(self.cluster_pos(c) + ((i + 1) * DIRENT_SIZE) as u64)
                    } else {
                        chain.get(ci + 1).map(|&nc| self.cluster_pos(nc))
                    };
                    if let Some(p) = next {
                        self.dev.write_all_at(&[0xFF], p)?;
                    }
                    return Ok(self.cluster_pos(c) + (i * DIRENT_SIZE) as u64);
                }
            }
        }
        // directory full: extend it by one cluster
        let nc = self.alloc(1)?[0];
        self.dev.write_all_at(&vec![0xFF; self.cluster_size as usize], self.cluster_pos(nc))?;
        self.set_fat(*chain.last().unwrap(), nc);
        Ok(self.cluster_pos(nc))
    }

    fn write_dirent(&self, pos: u64, name: &str, attr: u8, first: u32, size: u32) -> Result<()> {
        let mut d = [0xFFu8; DIRENT_SIZE];
        d[0] = name.len() as u8;
        d[1] = attr;
        d[2..2 + name.len()].copy_from_slice(name.as_bytes());
        d[44..48].copy_from_slice(&first.to_be_bytes());
        d[48..52].copy_from_slice(&size.to_be_bytes());
        let t = now_fat().to_be_bytes();
        for f in [52, 56, 60] {
            d[f..f + 4].copy_from_slice(&t);
        }
        self.dev.write_all_at(&d, pos)?;
        Ok(())
    }

    fn check_new_name(&self, dir: &Entry, name: &str) -> Result<()> {
        ensure!(!name.is_empty() && name.len() <= NAME_MAX, "name must be 1-{NAME_MAX} chars: {name}");
        ensure!(
            name.bytes().all(|b| b.is_ascii_graphic() || b == b' ') && !name.contains(['/', '\\', ':', '*', '?', '"', '<', '>', '|']),
            "invalid characters in name: {name}"
        );
        ensure!(self.find(dir, name)?.is_none(), "{name} already exists");
        Ok(())
    }

    pub fn mkdir(&mut self, dir: &Entry, name: &str) -> Result<Entry> {
        self.check_new_name(dir, name)?;
        let c = self.alloc(1)?[0];
        self.dev.write_all_at(&vec![0xFF; self.cluster_size as usize], self.cluster_pos(c))?;
        let pos = self.free_slot(dir)?;
        self.flush()?;
        self.write_dirent(pos, name, ATTR_DIR, c, 0)?;
        self.find(dir, name)?.context("mkdir vanished")
    }

    /// Create a file from `src` of length `len`; an existing file with that name is replaced.
    pub fn put(&mut self, dir: &Entry, name: &str, src: &mut impl Read, len: u64) -> Result<()> {
        ensure!(len <= u32::MAX as u64, "FATX files are limited to 4 GiB");
        if let Some(old) = self.find(dir, name)? {
            ensure!(!old.is_dir(), "{name} is a directory");
            self.remove(dir, name)?;
        }
        self.check_new_name(dir, name)?;
        let clusters = self.alloc(len.div_ceil(self.cluster_size) as usize)?;
        let mut buf = vec![0u8; self.cluster_size as usize];
        let mut left = len;
        for &c in &clusters {
            let n = left.min(self.cluster_size) as usize;
            src.read_exact(&mut buf[..n]).context("source shorter than expected")?;
            buf[n..].fill(0);
            self.dev.write_all_at(&buf, self.cluster_pos(c))?;
            left -= n as u64;
        }
        let pos = self.free_slot(dir)?;
        // data and FAT hit disk before the dirent, so a crash leaks clusters instead of corrupting files
        self.flush()?;
        self.write_dirent(pos, name, 0, clusters.first().copied().unwrap_or(0), len as u32)?;
        self.dev.sync_all()?;
        Ok(())
    }

    /// Delete a file or an empty directory.
    pub fn remove(&mut self, dir: &Entry, name: &str) -> Result<()> {
        let e = self.find(dir, name)?.with_context(|| format!("{name}: not found"))?;
        if e.is_dir() && !self.read_dir(&e)?.is_empty() {
            bail!("{name}: directory not empty");
        }
        self.dev.write_all_at(&[NAME_DELETED], e.pos.context("cannot remove root")?)?;
        self.free_chain(e.first)?;
        self.flush()
    }
}

fn be32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

/// Decode a FATX timestamp to Unix seconds (treated as UTC).
pub fn fat_to_unix(t: u32) -> i64 {
    let (date, time) = (t >> 16, t & 0xFFFF);
    let (y, m, d) = (1980 + (date >> 9) as i64, ((date >> 5) & 0xF).clamp(1, 12) as i64, (date & 0x1F).max(1) as i64);
    days_from_civil(y, m, d) * 86400 + ((time >> 11) * 3600 + ((time >> 5) & 0x3F) * 60 + (time & 0x1F) * 2) as i64
}

fn now_fat() -> u32 {
    let s = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    let (y, m, d) = civil_from_days(s.div_euclid(86400));
    let sod = s.rem_euclid(86400) as u32;
    let date = (((y - 1980).max(0) as u32) << 9) | ((m as u32) << 5) | d as u32;
    let time = ((sod / 3600) << 11) | (((sod / 60) % 60) << 5) | ((sod % 60) / 2);
    (date << 16) | time
}

// Howard Hinnant's date algorithms
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    era * 146097 + (yoe * 365 + yoe / 4 - yoe / 100 + doy) - 719468
}

pub fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { yoe + era * 400 + 1 } else { yoe + era * 400 }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a blank FATX image the way the 360 formats one: header, FAT with root = EOC, root cluster of 0xFF.
    fn blank_image(len: u64, spc: u32) -> File {
        let path = std::env::temp_dir().join(format!("fatx-test-{}-{len}", std::process::id()));
        let f = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(true).open(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        f.set_len(len).unwrap();
        let mut h = vec![0xFFu8; 0x1000];
        h[0..4].copy_from_slice(b"XTAF");
        h[4..8].copy_from_slice(&0x1234_5678u32.to_be_bytes());
        h[8..12].copy_from_slice(&spc.to_be_bytes());
        h[12..16].copy_from_slice(&1u32.to_be_bytes());
        f.write_all_at(&h, 0).unwrap();
        let cs = spc as u64 * 512;
        let entries = len / cs;
        let esz = if entries >= 0xFFF0 { 4 } else { 2 };
        let fat_size = (entries * esz).next_multiple_of(0x1000);
        let eoc = vec![0xFFu8; esz as usize];
        f.write_all_at(&eoc, 0x1000).unwrap(); // entry 0 (media)
        f.write_all_at(&eoc, 0x1000 + esz).unwrap(); // entry 1 (root)
        f.write_all_at(&vec![0xFF; cs as usize], 0x1000 + fat_size).unwrap();
        f
    }

    fn roundtrip(len: u64, spc: u32) {
        let f = blank_image(len, spc);
        let dev = f.try_clone().unwrap();
        assert_eq!(find_partitions(&dev).unwrap(), vec![("image", 0, len)]);
        let mut v = Volume::open(f, 0, len).unwrap();
        let root = v.root();
        let content = v.mkdir(&root, "Content").unwrap();
        // enough files to spill the directory into a second cluster
        let per = (v.cluster_size as usize / DIRENT_SIZE) + 3;
        for i in 0..per {
            let data: Vec<u8> = (0..i * 197).map(|b| (b % 251) as u8).collect();
            v.put(&content, &format!("file{i}.bin"), &mut &data[..], data.len() as u64).unwrap();
        }
        v.remove(&content, "file1.bin").unwrap();
        v.put(&content, "FILE2.bin", &mut &b"replaced"[..], 8).unwrap(); // case-insensitive replace

        let v = Volume::open(dev, 0, len).unwrap(); // reopen from disk
        let names: Vec<_> = v.read_dir(&v.lookup("/content").unwrap()).unwrap().into_iter().map(|e| e.name).collect();
        assert_eq!(names.len(), per - 1);
        assert!(!names.iter().any(|n| n == "file1.bin"));
        let mut out = Vec::new();
        v.copy_out(&v.lookup("Content/file200.bin").unwrap(), &mut out).unwrap();
        assert_eq!(out, (0..200 * 197).map(|b| (b % 251) as u8).collect::<Vec<_>>());
        out.clear();
        v.copy_out(&v.lookup("Content/FILE2.bin").unwrap(), &mut out).unwrap();
        assert_eq!(out, b"replaced");
        assert!(v.lookup("Content/file0.bin").unwrap().first == 0); // empty file has no clusters
        let mut v = v;
        let c = v.lookup("Content").unwrap();
        assert!(v.remove(&root, "Content").is_err()); // not empty
        for e in v.read_dir(&c).unwrap() {
            v.remove(&c, &e.name).unwrap();
        }
        v.remove(&root, "Content").unwrap();
        assert_eq!(v.usage().0, 1); // only the root cluster left
    }

    #[test]
    fn fat16_roundtrip() {
        roundtrip(64 << 20, 32); // 4096 clusters
    }

    #[test]
    fn fat32_roundtrip() {
        roundtrip(1100 << 20, 32); // 70400 clusters
    }

    #[test]
    fn timestamps() {
        let t = now_fat();
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
        assert!((fat_to_unix(t) - now).abs() <= 2);
    }
}
