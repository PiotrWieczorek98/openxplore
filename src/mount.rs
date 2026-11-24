//! Read-only FUSE view of a FATX volume.
use crate::fatx::{fat_to_unix, Entry, Volume};
use fuser::*;
use std::ffi::OsStr;
use std::sync::Mutex;
use std::time::{Duration, UNIX_EPOCH};

const TTL: Duration = Duration::from_secs(60);

pub struct Fs {
    vol: Volume,
    // inode n = nodes[n - 1]; read-only, so entries never go stale
    nodes: Mutex<Vec<Entry>>,
    uid: u32,
    gid: u32,
}

impl Fs {
    pub fn new(vol: Volume) -> Self {
        let root = vol.root();
        Fs { vol, nodes: Mutex::new(vec![root]), uid: unsafe { libc::getuid() }, gid: unsafe { libc::getgid() } }
    }

    fn node(&self, ino: INodeNo) -> Option<Entry> {
        self.nodes.lock().unwrap().get(u64::from(ino) as usize - 1).cloned()
    }

    fn ino_for(&self, e: &Entry) -> INodeNo {
        let mut nodes = self.nodes.lock().unwrap();
        // directories/files are identified by first cluster; empty files (cluster 0) by name+size
        let pos = nodes.iter().position(|n| n.first == e.first && (e.first != 0 || (n.name == e.name && n.size == e.size && n.mtime == e.mtime)));
        INodeNo(match pos {
            Some(i) => i as u64 + 1,
            None => {
                nodes.push(e.clone());
                nodes.len() as u64
            }
        })
    }

    fn attr(&self, ino: INodeNo, e: &Entry) -> FileAttr {
        let t = if e.mtime == 0 { UNIX_EPOCH } else { UNIX_EPOCH + Duration::from_secs(fat_to_unix(e.mtime).max(0) as u64) };
        FileAttr {
            ino,
            size: e.size as u64,
            blocks: (e.size as u64).div_ceil(512),
            atime: t,
            mtime: t,
            ctime: t,
            crtime: t,
            kind: if e.is_dir() { FileType::Directory } else { FileType::RegularFile },
            perm: if e.is_dir() { 0o555 } else { 0o444 },
            nlink: if e.is_dir() { 2 } else { 1 },
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            flags: 0,
            blksize: self.vol.cluster_size as u32,
        }
    }
}

impl Filesystem for Fs {
    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let found = self.node(parent).and_then(|p| self.vol.find(&p, &name.to_string_lossy()).ok().flatten());
        match found {
            Some(e) => reply.entry(&TTL, &self.attr(self.ino_for(&e), &e), Generation(0)),
            None => reply.error(Errno::ENOENT),
        }
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        match self.node(ino) {
            Some(e) => reply.attr(&TTL, &self.attr(ino, &e)),
            None => reply.error(Errno::ENOENT),
        }
    }

    fn read(&self, _req: &Request, ino: INodeNo, _fh: FileHandle, offset: u64, size: u32, _flags: OpenFlags, _lock: Option<LockOwner>, reply: ReplyData) {
        let Some(e) = self.node(ino) else { return reply.error(Errno::ENOENT) };
        let mut buf = vec![0u8; size as usize];
        match self.vol.read_at(&e, offset, &mut buf) {
            Ok(n) => reply.data(&buf[..n]),
            Err(err) => {
                eprintln!("read {}: {err:#}", e.name);
                reply.error(Errno::EIO)
            }
        }
    }

    fn readdir(&self, _req: &Request, ino: INodeNo, _fh: FileHandle, offset: u64, mut reply: ReplyDirectory) {
        let Some(dir) = self.node(ino) else { return reply.error(Errno::ENOENT) };
        let entries = match self.vol.read_dir(&dir) {
            Ok(v) => v,
            Err(err) => {
                eprintln!("readdir {}: {err:#}", dir.name);
                return reply.error(Errno::EIO);
            }
        };
        let list = [(ino, FileType::Directory, ".".to_string()), (ino, FileType::Directory, "..".to_string())]
            .into_iter()
            .chain(entries.iter().map(|e| (self.ino_for(e), if e.is_dir() { FileType::Directory } else { FileType::RegularFile }, e.name.clone())));
        for (i, (ino, kind, name)) in list.enumerate().skip(offset as usize) {
            if reply.add(ino, i as u64 + 1, kind, name) {
                break;
            }
        }
        reply.ok();
    }
}

pub fn run(vol: Volume, mountpoint: &std::path::Path) -> anyhow::Result<()> {
    let mut cfg = Config::default();
    cfg.mount_options.extend([MountOption::RO, MountOption::FSName("fatx".into())]);
    eprintln!("mounted at {} (read-only). Ctrl+C or `fusermount3 -u {0}` to unmount.", mountpoint.display());
    fuser::mount(Fs::new(vol), mountpoint, &cfg)?;
    Ok(())
}
