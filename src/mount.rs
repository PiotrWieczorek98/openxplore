//! Read-write FUSE view of a FATX volume, so any file manager can use the drive.
//!
//! FATX has no inode numbers, so an entry is identified by where its 64-byte directory
//! entry sits on the device. That position survives everything except a move to another
//! directory, where `rename` re-points the inode number at the new slot. All work happens
//! under one lock: the FUSE session is single-threaded anyway, and the volume caches state.
use crate::fatx::{fat_to_unix, Entry, Volume};
use fuser::*;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::sync::Mutex;
use std::time::{Duration, UNIX_EPOCH};

const TTL: Duration = Duration::from_secs(1);

struct State {
    vol: Volume,
    /// inode -> directory entry position (None = root, inode 1), and the reverse.
    inos: HashMap<u64, Option<u64>>,
    by_pos: HashMap<Option<u64>, u64>,
    /// Next inode number to hand out. Never reused: once an entry is deleted its slot can
    /// hold a new file, which must not inherit the old inode the kernel may still cache.
    next_ino: u64,
    /// File data was written since the last sync (deletes, renames and mkdir sync themselves).
    unsynced: bool,
}

impl State {
    fn ino(&mut self, pos: Option<u64>) -> INodeNo {
        let ino = *self.by_pos.entry(pos).or_insert_with(|| {
            self.next_ino += 1;
            self.next_ino
        });
        self.inos.insert(ino, pos);
        INodeNo(ino)
    }

    fn forget_pos(&mut self, pos: Option<u64>) {
        if let Some(ino) = self.by_pos.remove(&pos) {
            self.inos.remove(&ino);
        }
    }

    /// Fresh entry for an inode, re-read from disk so sizes and names are never stale.
    fn entry(&self, ino: INodeNo) -> anyhow::Result<Entry> {
        let pos = *self.inos.get(&u64::from(ino)).ok_or_else(|| anyhow::anyhow!("not found"))?;
        self.vol.entry_at(pos)
    }

    fn child(&self, parent: INodeNo, name: &OsStr) -> anyhow::Result<(Entry, Entry)> {
        let p = self.entry(parent)?;
        let e = self.vol.find(&p, &name.to_string_lossy())?.ok_or_else(|| anyhow::anyhow!("not found"))?;
        Ok((p, e))
    }
}

pub struct Fs {
    st: Mutex<State>,
    uid: u32,
    gid: u32,
    cluster_size: u32,
}

/// Map library errors onto errno values the file manager can show.
// shortcut: matches on error message text, so renaming a message in fatx.rs silently turns
// it into EIO; switch fatx.rs to a typed error enum if more cases appear.
fn errno(e: &anyhow::Error) -> Errno {
    let m = format!("{e:#}");
    let has = |s| m.contains(s);
    if has("not found") {
        Errno::ENOENT
    } else if has("already exists") {
        Errno::EEXIST
    } else if has("not empty") {
        Errno::ENOTEMPTY
    } else if has("volume full") {
        Errno::ENOSPC
    } else if has("name too long") {
        Errno::ENAMETOOLONG
    } else if has("invalid characters") {
        Errno::EINVAL
    } else if has("4 GiB") {
        Errno::EFBIG
    } else if has("is a directory") {
        Errno::EISDIR
    } else {
        eprintln!("error: {m}");
        Errno::EIO
    }
}

impl Fs {
    fn with<T>(&self, f: impl FnOnce(&mut State) -> anyhow::Result<T>) -> Result<T, Errno> {
        f(&mut self.st.lock().unwrap()).map_err(|e| errno(&e))
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
            perm: if e.is_dir() { 0o755 } else { 0o644 },
            nlink: if e.is_dir() { 2 } else { 1 },
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            flags: 0,
            blksize: self.cluster_size,
        }
    }

    /// Reply with an entry looked up / created by `f`.
    fn entry_reply(&self, reply: ReplyEntry, f: impl FnOnce(&mut State) -> anyhow::Result<Entry>) {
        match self.with(|st| f(st).map(|e| (st.ino(e.pos), e))) {
            Ok((ino, e)) => reply.entry(&TTL, &self.attr(ino, &e), Generation(0)),
            Err(e) => reply.error(e),
        }
    }

    fn empty_reply(&self, reply: ReplyEmpty, f: impl FnOnce(&mut State) -> anyhow::Result<()>) {
        match self.with(f) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }
}

impl Filesystem for Fs {
    fn destroy(&mut self) {
        if let Err(e) = self.st.lock().unwrap().vol.sync() {
            eprintln!("sync on unmount failed: {e:#}");
        }
    }

    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        self.entry_reply(reply, |st| Ok(st.child(parent, name)?.1));
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        match self.with(|st| st.entry(ino)) {
            Ok(e) => reply.attr(&TTL, &self.attr(ino, &e)),
            Err(e) => reply.error(e),
        }
    }

    // Only size changes are stored; FATX has no owners/permissions, and mtime is set on every write.
    fn setattr(
        &self, _req: &Request, ino: INodeNo, _mode: Option<u32>, _uid: Option<u32>, _gid: Option<u32>, size: Option<u64>,
        _atime: Option<TimeOrNow>, _mtime: Option<TimeOrNow>, _ctime: Option<std::time::SystemTime>, _fh: Option<FileHandle>,
        _crtime: Option<std::time::SystemTime>, _chgtime: Option<std::time::SystemTime>, _bkuptime: Option<std::time::SystemTime>,
        _flags: Option<BsdFileFlags>, _kill_suid_gid: bool, reply: ReplyAttr,
    ) {
        let r = self.with(|st| {
            let e = st.entry(ino)?;
            match size {
                Some(s) => {
                    st.unsynced = true;
                    st.vol.truncate(&e, s)
                }
                None => Ok(e),
            }
        });
        match r {
            Ok(e) => reply.attr(&TTL, &self.attr(ino, &e)),
            Err(e) => reply.error(e),
        }
    }

    fn read(&self, _req: &Request, ino: INodeNo, _fh: FileHandle, offset: u64, size: u32, _flags: OpenFlags, _lock: Option<LockOwner>, reply: ReplyData) {
        let r = self.with(|st| {
            let e = st.entry(ino)?;
            let mut buf = vec![0u8; size as usize];
            let n = st.vol.read_at(&e, offset, &mut buf)?;
            buf.truncate(n);
            Ok(buf)
        });
        match r {
            Ok(buf) => reply.data(&buf),
            Err(e) => reply.error(e),
        }
    }

    fn write(
        &self, _req: &Request, ino: INodeNo, _fh: FileHandle, offset: u64, data: &[u8], _wf: WriteFlags, _flags: OpenFlags,
        _lock: Option<LockOwner>, reply: ReplyWrite,
    ) {
        match self.with(|st| {
            let e = st.entry(ino)?;
            st.unsynced = true;
            st.vol.write_at(&e, offset, data)
        }) {
            Ok(_) => reply.written(data.len() as u32),
            Err(e) => reply.error(e),
        }
    }

    fn create(&self, _req: &Request, parent: INodeNo, name: &OsStr, _mode: u32, _umask: u32, _flags: i32, _k: bool, _o: Owner, reply: ReplyCreate) {
        match self.with(|st| {
            let p = st.entry(parent)?;
            let e = st.vol.create(&p, &name.to_string_lossy())?;
            Ok((st.ino(e.pos), e))
        }) {
            Ok((ino, e)) => reply.created(&TTL, &self.attr(ino, &e), Generation(0), FileHandle(0), FopenFlags::empty()),
            Err(e) => reply.error(e),
        }
    }

    fn mkdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, _mode: u32, _umask: u32, _o: Owner, reply: ReplyEntry) {
        self.entry_reply(reply, |st| {
            let p = st.entry(parent)?;
            st.vol.mkdir(&p, &name.to_string_lossy())
        });
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        self.empty_reply(reply, |st| {
            let (p, e) = st.child(parent, name)?;
            anyhow::ensure!(!e.is_dir(), "{} is a directory", e.name);
            st.vol.remove(&p, &e.name)?;
            st.forget_pos(e.pos);
            Ok(())
        });
    }

    fn rmdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        self.empty_reply(reply, |st| {
            let (p, e) = st.child(parent, name)?;
            st.vol.remove(&p, &e.name)?;
            st.forget_pos(e.pos);
            Ok(())
        });
    }

    fn rename(&self, _req: &Request, parent: INodeNo, name: &OsStr, newparent: INodeNo, newname: &OsStr, flags: RenameFlags, _o: Option<Owner>, reply: ReplyEmpty) {
        if flags.contains(RenameFlags::RENAME_EXCHANGE) {
            return reply.error(Errno::EINVAL);
        }
        self.empty_reply(reply, |st| {
            let (p, e) = st.child(parent, name)?;
            let dst = st.entry(newparent)?;
            if let Some(t) = st.vol.find(&dst, &newname.to_string_lossy())?
                && t.pos != e.pos
            {
                anyhow::ensure!(!flags.contains(RenameFlags::RENAME_NOREPLACE), "already exists");
                st.forget_pos(t.pos);
            }
            let moved = st.vol.rename(&p, &e.name, &dst, &newname.to_string_lossy())?;
            // keep the inode number, now pointing at the new slot
            if let Some(ino) = st.by_pos.remove(&e.pos) {
                st.by_pos.insert(moved.pos, ino);
                st.inos.insert(ino, moved.pos);
            }
            Ok(())
        });
    }

    fn readdir(&self, _req: &Request, ino: INodeNo, _fh: FileHandle, offset: u64, mut reply: ReplyDirectory) {
        let r = self.with(|st| {
            let dir = st.entry(ino)?;
            Ok(st.vol.read_dir(&dir)?.into_iter().map(|e| (st.ino(e.pos), e)).collect::<Vec<_>>())
        });
        let entries = match r {
            Ok(v) => v,
            Err(e) => return reply.error(e),
        };
        let list = [(ino, FileType::Directory, ".".to_string()), (ino, FileType::Directory, "..".to_string())]
            .into_iter()
            .chain(entries.into_iter().map(|(i, e)| (i, if e.is_dir() { FileType::Directory } else { FileType::RegularFile }, e.name)));
        for (i, (ino, kind, name)) in list.enumerate().skip(offset as usize) {
            if reply.add(ino, i as u64 + 1, kind, name) {
                break;
            }
        }
        reply.ok();
    }

    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
        let (used, total) = self.st.lock().unwrap().vol.usage();
        let cs = self.cluster_size;
        reply.statfs(total, total - used, total - used, 0, 0, cs, 42, cs);
    }

    fn fsync(&self, _req: &Request, _ino: INodeNo, _fh: FileHandle, _datasync: bool, reply: ReplyEmpty) {
        self.empty_reply(reply, |st| st.vol.sync());
    }

    // Sync when a written file is closed: a USB drive is likely to be unplugged soon after
    // copying. Closing a file that was only read skips it, since each sync flushes the
    // disk's cache and file managers open many files just to preview them.
    fn release(&self, _req: &Request, _ino: INodeNo, _fh: FileHandle, _flags: OpenFlags, _lock: Option<LockOwner>, _flush: bool, reply: ReplyEmpty) {
        self.empty_reply(reply, |st| {
            if std::mem::take(&mut st.unsynced) {
                st.vol.sync()?;
            }
            Ok(())
        });
    }
}

pub fn run(vol: Volume, mountpoint: &std::path::Path, control: bool) -> anyhow::Result<()> {
    // Run as root (sudo/pkexec): show files as owned by the user who asked, and allow other
    // users in (allow_other), since the user's file manager is not root. Run as a user
    // (drive opened via the udev rule): the defaults already fit.
    let env_id = |k| std::env::var(k).ok().and_then(|v| v.parse().ok());
    let root = unsafe { libc::getuid() } == 0;
    let uid = env_id("SUDO_UID").or(env_id("PKEXEC_UID")).unwrap_or(unsafe { libc::getuid() });
    let pw = unsafe { libc::getpwuid(uid) };
    let gid = if pw.is_null() { unsafe { libc::getgid() } } else { unsafe { (*pw).pw_gid } };
    let mut cfg = Config::default();
    cfg.mount_options.extend([MountOption::FSName("fatx".into()), MountOption::DefaultPermissions]);
    if root {
        cfg.acl = SessionACL::All;
    }
    let cluster_size = vol.cluster_size as u32;
    let st = State { vol, inos: HashMap::from([(1, None)]), by_pos: HashMap::from([(None, 1)]), next_ino: 1, unsynced: false };
    // Ctrl+C / SIGTERM: unmount cleanly (which syncs) instead of leaving a dead mount behind.
    // Blocked before spawning, so the FUSE threads inherit the mask and only sigwait sees them.
    let sigs = unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGINT);
        libc::sigaddset(&mut set, libc::SIGTERM);
        libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
        set
    };
    let session = fuser::spawn_mount(Fs { st: Mutex::new(st), uid, gid, cluster_size }, mountpoint, &cfg)?;
    let umount = {
        let mp = mountpoint.to_owned();
        move |lazy: bool| {
            let mut cmd = std::process::Command::new("fusermount3");
            cmd.arg(if lazy { "-uz" } else { "-u" }).arg(&mp).stderr(std::process::Stdio::null());
            cmd.status().is_ok_and(|s| s.success())
        }
    };
    let u = umount.clone();
    std::thread::spawn(move || loop {
        let mut sig = 0;
        unsafe { libc::sigwait(&sigs, &mut sig) };
        if u(false) {
            break;
        }
        eprintln!("still in use: close programs (e.g. Dolphin tabs) showing it, then press Ctrl+C again");
    });
    if control {
        // Line protocol with the tray front end (see gui.rs): it sends "unmount", we answer
        // "busy" or exit after printing "unmounted". EOF means the front end died, so detach
        // lazily rather than leave the drive mounted with nobody to unmount it.
        println!("mounted");
        std::thread::spawn(move || {
            for line in std::io::stdin().lines() {
                if line.is_err() {
                    break;
                }
                if umount(false) {
                    return;
                }
                println!("busy");
            }
            umount(true);
        });
    }
    eprintln!("mounted at {}. Press Ctrl+C to unmount; wait for this to exit before unplugging the drive.", mountpoint.display());
    session.join()?;
    eprintln!("unmounted, all data written.");
    if control {
        println!("unmounted");
    }
    Ok(())
}
