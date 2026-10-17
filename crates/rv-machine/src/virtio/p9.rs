//! virtio-9p (§5.13 of earlier virtio drafts; Linux `9pnet_virtio`): a host directory shared
//! with the guest over 9P2000.L.
//!
//! The guest mounts it with `mount -t 9p -o trans=virtio,version=9p2000.L TAG /mnt`.
//! Files appear owned by a configurable uid/gid (the host's own ownership means nothing in the
//! guest); host permissions still apply to what the emulator can do. Paths never leave the
//! shared directory: `..` is resolved by the walk and host symlinks are not followed (the guest
//! resolves symlinks itself, through READLINK).

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use super::{Activation, Chain, VirtioDevice, read_config_bytes};

const DEVICE_TYPE: u16 = 9;
const FEATURE_MOUNT_TAG: u64 = 1;
const MSIZE: u32 = 512 << 10;

mod msg {
    pub const TLERROR: u8 = 6;
    pub const TSTATFS: u8 = 8;
    pub const TLOPEN: u8 = 12;
    pub const TLCREATE: u8 = 14;
    pub const TSYMLINK: u8 = 16;
    pub const TMKNOD: u8 = 18;
    pub const TRENAME: u8 = 20;
    pub const TREADLINK: u8 = 22;
    pub const TGETATTR: u8 = 24;
    pub const TSETATTR: u8 = 26;
    pub const TXATTRWALK: u8 = 30;
    pub const TREADDIR: u8 = 40;
    pub const TFSYNC: u8 = 50;
    pub const TLOCK: u8 = 52;
    pub const TGETLOCK: u8 = 54;
    pub const TLINK: u8 = 70;
    pub const TMKDIR: u8 = 72;
    pub const TRENAMEAT: u8 = 74;
    pub const TUNLINKAT: u8 = 76;
    pub const TVERSION: u8 = 100;
    pub const TATTACH: u8 = 104;
    pub const TFLUSH: u8 = 108;
    pub const TWALK: u8 = 110;
    pub const TREAD: u8 = 116;
    pub const TWRITE: u8 = 118;
    pub const TCLUNK: u8 = 120;
}

mod errno {
    pub const ENOENT: u32 = 2;
    pub const EIO: u32 = 5;
    pub const EBADF: u32 = 9;
    pub const EACCES: u32 = 13;
    pub const EEXIST: u32 = 17;
    pub const ENOTDIR: u32 = 20;
    pub const EISDIR: u32 = 21;
    pub const EINVAL: u32 = 22;
    pub const EROFS: u32 = 30;
    pub const ENAMETOOLONG: u32 = 36;
    pub const ENOTEMPTY: u32 = 39;
    pub const EOPNOTSUPP: u32 = 95;
}

const QT_DIR: u8 = 0x80;
const QT_SYMLINK: u8 = 0x02;
/// Linux open flags the server looks at.
const O_ACCMODE: u32 = 3;
const O_WRONLY: u32 = 1;
const O_RDWR: u32 = 2;
const O_CREAT: u32 = 0o100;
const O_EXCL: u32 = 0o200;
const O_TRUNC: u32 = 0o1000;
const O_APPEND: u32 = 0o2000;
const AT_REMOVEDIR: u32 = 0x200;

/// A failed operation: the Linux errno to report.
struct Error(u32);

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        use io::ErrorKind::*;
        Error(match e.kind() {
            NotFound => errno::ENOENT,
            PermissionDenied => errno::EACCES,
            AlreadyExists => errno::EEXIST,
            NotADirectory => errno::ENOTDIR,
            IsADirectory => errno::EISDIR,
            DirectoryNotEmpty => errno::ENOTEMPTY,
            InvalidInput | InvalidFilename => errno::EINVAL,
            ReadOnlyFilesystem => errno::EROFS,
            Unsupported => errno::EOPNOTSUPP,
            _ => e.raw_os_error().filter(|_| cfg!(unix)).map_or(errno::EIO, |n| n as u32),
        })
    }
}

type Result<T> = std::result::Result<T, Error>;

/// A message being decoded.
struct Reader<'a> {
    data: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn bytes(&mut self, n: usize) -> Result<&'a [u8]> {
        let b = self.data.get(self.at..self.at + n).ok_or(Error(errno::EINVAL))?;
        self.at += n;
        Ok(b)
    }
    fn u8(&mut self) -> Result<u8> {
        Ok(self.bytes(1)?[0])
    }
    fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.bytes(2)?.try_into().unwrap()))
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.bytes(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.bytes(8)?.try_into().unwrap()))
    }
    fn string(&mut self) -> Result<String> {
        let n = self.u16()? as usize;
        String::from_utf8(self.bytes(n)?.to_vec()).map_err(|_| Error(errno::EINVAL))
    }
}

/// A reply being encoded.
#[derive(Default)]
struct Writer(Vec<u8>);

impl Writer {
    fn u8(&mut self, v: u8) -> &mut Self {
        self.0.push(v);
        self
    }
    fn u16(&mut self, v: u16) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn u32(&mut self, v: u32) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn u64(&mut self, v: u64) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn string(&mut self, s: &str) -> &mut Self {
        self.u16(s.len() as u16);
        self.0.extend_from_slice(s.as_bytes());
        self
    }
    fn qid(&mut self, q: Qid) -> &mut Self {
        self.u8(q.kind).u32(q.version).u64(q.path)
    }
}

#[derive(Clone, Copy)]
struct Qid {
    kind: u8,
    version: u32,
    path: u64,
}

struct Fid {
    /// Path relative to the shared root.
    path: PathBuf,
    file: Option<File>,
    /// Directory listing taken at the first READDIR at offset 0.
    listing: Option<Vec<(String, Qid, u8)>>,
}

pub struct P9 {
    tag: String,
    root: PathBuf,
    read_only: bool,
    uid: u32,
    gid: u32,
    fids: HashMap<u32, Fid>,
    active: Option<Activation>,
}

impl P9 {
    /// Shares `root` under mount tag `tag`.
    pub fn new(root: PathBuf, tag: &str, read_only: bool, uid: u32, gid: u32) -> io::Result<Self> {
        let root = root.canonicalize()?;
        if !root.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::NotADirectory,
                format!("{} is not a directory", root.display()),
            ));
        }
        Ok(P9 { tag: tag.into(), root, read_only, uid, gid, fids: HashMap::new(), active: None })
    }

    fn host(&self, rel: &Path) -> PathBuf {
        self.root.join(rel)
    }

    fn fid(&self, fid: u32) -> Result<&Fid> {
        self.fids.get(&fid).ok_or(Error(errno::EBADF))
    }

    fn writable(&self) -> Result<()> {
        if self.read_only { Err(Error(errno::EROFS)) } else { Ok(()) }
    }

    /// `name` as a single path component inside a directory.
    fn child(dir: &Path, name: &str) -> Result<PathBuf> {
        if name.is_empty() || name.len() > 255 {
            return Err(Error(errno::ENAMETOOLONG));
        }
        let mut c = Path::new(name).components();
        match (c.next(), c.next()) {
            (Some(Component::Normal(n)), None) => Ok(dir.join(n)),
            _ => Err(Error(errno::EINVAL)),
        }
    }

    fn metadata(&self, rel: &Path) -> Result<fs::Metadata> {
        Ok(fs::symlink_metadata(self.host(rel))?)
    }

    fn qid(meta: &fs::Metadata, host: &Path) -> Qid {
        let kind = if meta.is_dir() {
            QT_DIR
        } else if meta.file_type().is_symlink() {
            QT_SYMLINK
        } else {
            0
        };
        let mtime =
            meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs());
        Qid { kind, version: mtime as u32, path: inode(meta, host) }
    }

    fn handle(&mut self, kind: u8, r: &mut Reader) -> Result<Writer> {
        let mut w = Writer::default();
        match kind {
            msg::TVERSION => {
                let msize = r.u32()?.min(MSIZE);
                let version = r.string()?;
                self.fids.clear();
                w.u32(msize).string(if version.starts_with("9P2000.L") { "9P2000.L" } else { "unknown" });
            }
            msg::TATTACH => {
                let fid = r.u32()?;
                let meta = self.metadata(Path::new(""))?;
                self.fids.insert(fid, Fid { path: PathBuf::new(), file: None, listing: None });
                w.qid(Self::qid(&meta, &self.root));
            }
            msg::TWALK => {
                let (fid, newfid, n) = (r.u32()?, r.u32()?, r.u16()?);
                let mut path = self.fid(fid)?.path.clone();
                let mut qids = Vec::new();
                for i in 0..n {
                    let name = r.string()?;
                    let next = if name == ".." {
                        path.parent().map(Path::to_path_buf).unwrap_or_default()
                    } else {
                        // Walking through a host symlink would leave the share: refuse.
                        if !path.as_os_str().is_empty() && self.metadata(&path)?.file_type().is_symlink() {
                            return Err(Error(errno::ENOTDIR));
                        }
                        Self::child(&path, &name)?
                    };
                    match self.metadata(&next) {
                        Ok(meta) => {
                            qids.push(Self::qid(&meta, &self.host(&next)));
                            path = next;
                        }
                        // The first element must exist; later failures return a partial walk.
                        Err(e) if i == 0 => return Err(e),
                        Err(_) => break,
                    }
                }
                if qids.len() == n as usize {
                    self.fids.insert(newfid, Fid { path, file: None, listing: None });
                }
                w.u16(qids.len() as u16);
                for q in qids {
                    w.qid(q);
                }
            }
            msg::TLOPEN => {
                let (fid, flags) = (r.u32()?, r.u32()?);
                let path = self.fid(fid)?.path.clone();
                let host = self.host(&path);
                let meta = self.metadata(&path)?;
                let file = if meta.is_dir() {
                    None
                } else {
                    if flags & O_ACCMODE != 0 || flags & O_TRUNC != 0 {
                        self.writable()?;
                    }
                    Some(open(&host, flags, false)?)
                };
                let f = self.fids.get_mut(&fid).unwrap();
                f.file = file;
                f.listing = None;
                w.qid(Self::qid(&meta, &host)).u32(0);
            }
            msg::TLCREATE => {
                let (fid, name, flags, mode, _gid) = (r.u32()?, r.string()?, r.u32()?, r.u32()?, r.u32()?);
                self.writable()?;
                let path = Self::child(&self.fid(fid)?.path, &name)?;
                let host = self.host(&path);
                let file = open(&host, flags | O_CREAT, true)?;
                set_mode(&host, mode);
                let meta = self.metadata(&path)?;
                let f = self.fids.get_mut(&fid).unwrap();
                f.path = path;
                f.file = Some(file);
                w.qid(Self::qid(&meta, &host)).u32(0);
            }
            msg::TREAD => {
                let (fid, offset, count) = (r.u32()?, r.u64()?, r.u32()?);
                let file = self.fid(fid)?.file.as_ref().ok_or(Error(errno::EBADF))?;
                let mut buf = vec![0u8; count.min(MSIZE - 64) as usize];
                let n = read_at(file, &mut buf, offset)?;
                w.u32(n as u32);
                w.0.extend_from_slice(&buf[..n]);
            }
            msg::TWRITE => {
                let (fid, offset, count) = (r.u32()?, r.u64()?, r.u32()?);
                let data = r.bytes(count as usize)?;
                self.writable()?;
                let file = self.fid(fid)?.file.as_ref().ok_or(Error(errno::EBADF))?;
                write_at(file, data, offset)?;
                w.u32(count);
            }
            msg::TCLUNK => {
                let fid = r.u32()?;
                self.fids.remove(&fid).ok_or(Error(errno::EBADF))?;
            }
            msg::TGETATTR => {
                let (fid, _mask) = (r.u32()?, r.u64()?);
                let path = self.fid(fid)?.path.clone();
                let host = self.host(&path);
                let meta = self.metadata(&path)?;
                let time = |t: io::Result<SystemTime>| {
                    t.ok()
                        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                        .map_or((0, 0), |d| (d.as_secs(), d.subsec_nanos() as u64))
                };
                let (at, mt) = (time(meta.accessed()), time(meta.modified()));
                // valid: the basic fields (P9_GETATTR_BASIC).
                w.u64(0x7ff).qid(Self::qid(&meta, &host));
                w.u32(mode(&meta)).u32(self.uid).u32(self.gid).u64(nlink(&meta)).u64(0);
                w.u64(meta.len()).u64(4096).u64(meta.len().div_ceil(512));
                w.u64(at.0).u64(at.1).u64(mt.0).u64(mt.1).u64(mt.0).u64(mt.1);
                w.u64(0).u64(0).u64(0).u64(0); // btime, gen, data_version
            }
            msg::TSETATTR => {
                let (fid, valid, mode, _uid, _gid, size) =
                    (r.u32()?, r.u32()?, r.u32()?, r.u32()?, r.u32()?, r.u64()?);
                let path = self.fid(fid)?.path.clone();
                let host = self.host(&path);
                if valid & (1 | 8 | 0x20 | 0x40) != 0 {
                    self.writable()?;
                }
                if valid & 1 != 0 {
                    set_mode(&host, mode);
                }
                if valid & 8 != 0 {
                    OpenOptions::new().write(true).open(&host)?.set_len(size)?;
                }
                // Ownership changes are accepted and ignored; times are left to the host.
            }
            msg::TREADDIR => {
                let (fid, offset, count) = (r.u32()?, r.u64()?, r.u32()?);
                let path = self.fid(fid)?.path.clone();
                if offset == 0 || self.fid(fid)?.listing.is_none() {
                    let listing = self.list(&path)?;
                    self.fids.get_mut(&fid).unwrap().listing = Some(listing);
                }
                let listing = self.fid(fid)?.listing.as_ref().unwrap();
                let mut out = Writer::default();
                for (i, (name, qid, dtype)) in listing.iter().enumerate().skip(offset as usize) {
                    let mut e = Writer::default();
                    e.qid(*qid).u64(i as u64 + 1).u8(*dtype).string(name);
                    if out.0.len() + e.0.len() > count as usize {
                        break;
                    }
                    out.0.extend(e.0);
                }
                w.u32(out.0.len() as u32);
                w.0.extend(out.0);
            }
            msg::TSTATFS => {
                let _fid = r.u32()?;
                // Plausible numbers; the host's are not portably available.
                w.u32(0x0102_1997).u32(4096).u64(1 << 28).u64(1 << 27).u64(1 << 27).u64(1 << 20).u64(1 << 19);
                w.u64(0).u32(255);
            }
            msg::TMKDIR => {
                let (dfid, name, mode, _gid) = (r.u32()?, r.string()?, r.u32()?, r.u32()?);
                self.writable()?;
                let path = Self::child(&self.fid(dfid)?.path, &name)?;
                let host = self.host(&path);
                fs::create_dir(&host)?;
                set_mode(&host, mode);
                let meta = self.metadata(&path)?;
                w.qid(Self::qid(&meta, &host));
            }
            msg::TSYMLINK => {
                let (fid, name, target, _gid) = (r.u32()?, r.string()?, r.string()?, r.u32()?);
                self.writable()?;
                let path = Self::child(&self.fid(fid)?.path, &name)?;
                let host = self.host(&path);
                symlink(&target, &host)?;
                let meta = self.metadata(&path)?;
                w.qid(Self::qid(&meta, &host));
            }
            msg::TREADLINK => {
                let fid = r.u32()?;
                let target = fs::read_link(self.host(&self.fid(fid)?.path))?;
                w.string(&target.to_string_lossy());
            }
            msg::TLINK => {
                let (dfid, fid, name) = (r.u32()?, r.u32()?, r.string()?);
                self.writable()?;
                let path = Self::child(&self.fid(dfid)?.path, &name)?;
                fs::hard_link(self.host(&self.fid(fid)?.path), self.host(&path))?;
            }
            msg::TRENAMEAT => {
                let (olddir, oldname, newdir, newname) = (r.u32()?, r.string()?, r.u32()?, r.string()?);
                self.writable()?;
                let from = Self::child(&self.fid(olddir)?.path, &oldname)?;
                let to = Self::child(&self.fid(newdir)?.path, &newname)?;
                fs::rename(self.host(&from), self.host(&to))?;
            }
            msg::TRENAME => {
                let (fid, dfid, name) = (r.u32()?, r.u32()?, r.string()?);
                self.writable()?;
                let from = self.fid(fid)?.path.clone();
                let to = Self::child(&self.fid(dfid)?.path, &name)?;
                fs::rename(self.host(&from), self.host(&to))?;
                self.fids.get_mut(&fid).unwrap().path = to;
            }
            msg::TUNLINKAT => {
                let (dfid, name, flags) = (r.u32()?, r.string()?, r.u32()?);
                self.writable()?;
                let path = Self::child(&self.fid(dfid)?.path, &name)?;
                let host = self.host(&path);
                if flags & AT_REMOVEDIR != 0 { fs::remove_dir(host)? } else { fs::remove_file(host)? }
            }
            msg::TFSYNC => {
                let fid = r.u32()?;
                if let Some(file) = &self.fid(fid)?.file {
                    file.sync_data()?;
                }
            }
            msg::TLOCK => {
                // Advisory locks are granted (single guest, no host-side locking).
                w.u8(0);
            }
            msg::TGETLOCK => {
                let (_fid, _kind, start, length, proc_id) = (r.u32()?, r.u8()?, r.u64()?, r.u64()?, r.u32()?);
                // F_UNLCK: nothing holds a conflicting lock.
                w.u8(2).u64(start).u64(length).u32(proc_id).string("");
            }
            msg::TFLUSH => {}
            msg::TXATTRWALK | msg::TMKNOD => return Err(Error(errno::EOPNOTSUPP)),
            _ => return Err(Error(errno::EOPNOTSUPP)),
        }
        Ok(w)
    }

    fn list(&self, rel: &Path) -> Result<Vec<(String, Qid, u8)>> {
        let host = self.host(rel);
        let mut out = Vec::new();
        for (name, path) in
            [(".", rel.to_path_buf()), ("..", rel.parent().map(Path::to_path_buf).unwrap_or_default())]
        {
            let meta = self.metadata(&path)?;
            out.push((name.to_string(), Self::qid(&meta, &self.host(&path)), 4));
        }
        for entry in fs::read_dir(&host)? {
            let entry = entry?;
            let Ok(meta) = fs::symlink_metadata(entry.path()) else { continue };
            let qid = Self::qid(&meta, &entry.path());
            // DT_DIR, DT_LNK, DT_REG.
            let dtype = if meta.is_dir() {
                4
            } else if meta.file_type().is_symlink() {
                10
            } else {
                8
            };
            out.push((entry.file_name().to_string_lossy().into_owned(), qid, dtype));
        }
        Ok(out)
    }

    /// Serves one request chain; returns the bytes written.
    fn request(&mut self, a: &Activation, chain: &Chain) -> u32 {
        let mut req = vec![0u8; chain.readable_len().min(MSIZE as u64 + 64) as usize];
        let n = chain.reader(&a.mem).read(&mut req).unwrap_or(0);
        req.truncate(n);
        if req.len() < 7 {
            return 0;
        }
        let (kind, tag) = (req[4], u16::from_le_bytes([req[5], req[6]]));
        let mut r = Reader { data: &req, at: 7 };
        let (reply_kind, body) = match self.handle(kind, &mut r) {
            Ok(w) => (kind + 1, w.0),
            Err(Error(code)) => {
                let mut w = Writer::default();
                w.u32(code);
                (msg::TLERROR + 1, w.0)
            }
        };
        let mut reply = Writer::default();
        reply.u32(7 + body.len() as u32).u8(reply_kind).u16(tag);
        reply.0.extend(body);
        chain.writer(&a.mem).write(&reply.0).unwrap_or(0) as u32
    }
}

fn open(path: &Path, flags: u32, create: bool) -> io::Result<File> {
    let mut o = OpenOptions::new();
    match flags & O_ACCMODE {
        O_WRONLY => o.write(true),
        O_RDWR => o.read(true).write(true),
        _ => o.read(true),
    };
    if flags & O_APPEND != 0 {
        o.append(true);
    }
    if flags & O_TRUNC != 0 {
        o.truncate(true);
    }
    if create {
        if flags & O_ACCMODE == 0 {
            o.write(true);
        }
        if flags & O_EXCL != 0 {
            o.create_new(true)
        } else {
            o.create(true)
        };
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // Never follow a symlink out of the share.
        o.custom_flags(libc_o_nofollow());
    }
    o.open(path)
}

#[cfg(unix)]
fn libc_o_nofollow() -> i32 {
    #[cfg(target_os = "linux")]
    {
        0o400000
    }
    #[cfg(not(target_os = "linux"))]
    {
        0x100
    }
}

fn read_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    #[cfg(unix)]
    {
        std::os::unix::fs::FileExt::read_at(file, buf, offset)
    }
    #[cfg(windows)]
    {
        std::os::windows::fs::FileExt::seek_read(file, buf, offset)
    }
}

fn write_at(file: &File, data: &[u8], offset: u64) -> io::Result<()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::FileExt::write_all_at(file, data, offset)
    }
    #[cfg(windows)]
    {
        let mut done = 0;
        while done < data.len() {
            done += std::os::windows::fs::FileExt::seek_write(file, &data[done..], offset + done as u64)?;
        }
        Ok(())
    }
}

fn mode(meta: &fs::Metadata) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        meta.mode()
    }
    #[cfg(not(unix))]
    {
        let kind = if meta.is_dir() {
            0o040000
        } else if meta.file_type().is_symlink() {
            0o120000
        } else {
            0o100000
        };
        let perm = if meta.permissions().readonly() { 0o555 } else { 0o755 };
        kind | if meta.is_dir() { perm } else { perm & 0o666 }
    }
}

fn nlink(meta: &fs::Metadata) -> u64 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        meta.nlink()
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        1
    }
}

fn inode(meta: &fs::Metadata, host: &Path) -> u64 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let _ = host;
        meta.ino()
    }
    #[cfg(not(unix))]
    {
        use std::hash::{Hash, Hasher};
        let _ = meta;
        let mut h = std::collections::hash_map::DefaultHasher::new();
        host.hash(&mut h);
        h.finish()
    }
}

fn set_mode(path: &Path, mode: u32) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(mode & 0o7777));
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
    }
}

fn symlink(target: &str, link: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link)
    }
    #[cfg(windows)]
    {
        std::os::windows::fs::symlink_file(target, link)
    }
}

impl VirtioDevice for P9 {
    fn device_type(&self) -> u16 {
        DEVICE_TYPE
    }

    fn features(&self) -> u64 {
        FEATURE_MOUNT_TAG
    }

    fn queue_sizes(&self) -> Vec<u16> {
        vec![128]
    }

    fn read_config(&self, offset: usize, data: &mut [u8]) {
        let mut config = (self.tag.len() as u16).to_le_bytes().to_vec();
        config.extend_from_slice(self.tag.as_bytes());
        read_config_bytes(&config, offset, data);
    }

    fn activate(&mut self, activation: Activation) {
        self.active = Some(activation);
    }

    fn notify(&mut self, queue: usize) {
        let Some(a) = self.active.clone() else { return };
        a.drain(queue, |chain| self.request(&a, chain));
    }

    fn reset(&mut self) {
        self.active = None;
        self.fids.clear();
    }

    fn pci_class(&self) -> u32 {
        0x00_02_00
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(p: &mut P9, kind: u8, body: &[u8]) -> (u8, Vec<u8>) {
        let mut r = Reader { data: body, at: 0 };
        match p.handle(kind, &mut r) {
            Ok(w) => (kind + 1, w.0),
            Err(Error(e)) => (msg::TLERROR + 1, e.to_le_bytes().to_vec()),
        }
    }

    fn walk_body(fid: u32, newfid: u32, names: &[&str]) -> Vec<u8> {
        let mut w = Writer::default();
        w.u32(fid).u32(newfid).u16(names.len() as u16);
        for n in names {
            w.string(n);
        }
        w.0
    }

    #[test]
    fn walk_open_read_and_stay_inside() {
        let dir = std::env::temp_dir().join(format!("rust-v-9p-{}", std::process::id()));
        fs::create_dir_all(dir.join("sub")).unwrap();
        fs::write(dir.join("sub/hello.txt"), b"hello 9p").unwrap();
        let mut p = P9::new(dir.clone(), "share", false, 1000, 1000).unwrap();
        let mut w = Writer::default();
        w.u32(1).u32(!0).string("root").string("").u32(0);
        assert_eq!(call(&mut p, msg::TATTACH, &w.0).0, msg::TATTACH + 1);

        let (kind, reply) = call(&mut p, msg::TWALK, &walk_body(1, 2, &["sub", "hello.txt"]));
        assert_eq!(kind, msg::TWALK + 1);
        assert_eq!(u16::from_le_bytes([reply[0], reply[1]]), 2);
        assert_eq!(call(&mut p, msg::TLOPEN, &[2, 0, 0, 0, 0, 0, 0, 0]).0, msg::TLOPEN + 1);
        let mut w = Writer::default();
        w.u32(2).u64(6).u32(100);
        let (_, reply) = call(&mut p, msg::TREAD, &w.0);
        assert_eq!(&reply[4..], b"9p");

        // `..` at the root stays at the root.
        let (_, reply) = call(&mut p, msg::TWALK, &walk_body(1, 3, &["..", "..", "sub"]));
        assert_eq!(u16::from_le_bytes([reply[0], reply[1]]), 3);
        // Names with separators are refused.
        let (kind, _) = call(&mut p, msg::TWALK, &walk_body(1, 4, &["sub/hello.txt"]));
        assert_eq!(kind, msg::TLERROR + 1);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn read_only_refuses_writes() {
        let dir = std::env::temp_dir().join(format!("rust-v-9p-ro-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let mut p = P9::new(dir.clone(), "share", true, 0, 0).unwrap();
        let mut w = Writer::default();
        w.u32(1).u32(!0).string("root").string("").u32(0);
        call(&mut p, msg::TATTACH, &w.0);
        let mut w = Writer::default();
        w.u32(1).string("new").u32(0).u32(0o755).u32(0);
        let (kind, reply) = call(&mut p, msg::TMKDIR, &w.0);
        assert_eq!(
            (kind, u32::from_le_bytes(reply[..4].try_into().unwrap())),
            (msg::TLERROR + 1, errno::EROFS)
        );
        fs::remove_dir_all(dir).unwrap();
    }
}
