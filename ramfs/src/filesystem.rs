use std::collections::BTreeMap;
use std::convert::TryInto;
use std::os::unix::io::AsRawFd;
use std::{fs, iter, time};

use indexmap::IndexMap;
use libredox::Fd;
use redox_path::RedoxReference;
use syscall::error::{EACCES, EBADFD, EIO, EISDIR, ENFILE, ENOENT, ENOMEM, ENXIO, EOVERFLOW};
use syscall::{Error, Result, TimeSpec, EINVAL, MODE_DIR, MODE_TYPE};

use super::scheme::current_perm;

#[derive(Debug)]
pub struct File {
    pub mode: u16,
    pub uid: u32,
    pub gid: u32,
    pub nlink: usize,
    pub parent: Inode,

    pub open_handles: usize,

    pub atime: TimeSpec,
    pub ctime: TimeSpec,
    pub mtime: TimeSpec,

    pub data: FileData,
}

impl File {
    pub fn read(&self, offset: usize, buf: &mut [u8]) -> Result<usize> {
        match self.data {
            FileData::File(ref bytes) => {
                if self.mode & MODE_TYPE == MODE_DIR {
                    return Err(Error::new(EBADFD));
                }

                let src_bytes = bytes.get(offset..).unwrap_or(&[]);
                let bytes_to_read = src_bytes.len().min(buf.len());
                buf[..bytes_to_read].copy_from_slice(&src_bytes[..bytes_to_read]);
                Ok(bytes_to_read)
            }

            FileData::Directory(_) => Err(Error::new(EISDIR)),
            FileData::Socket(_) => Err(Error::new(ENXIO)),
        }
    }

    pub fn write(&mut self, offset: usize, buf: &[u8]) -> Result<usize> {
        match self.data {
            FileData::File(ref mut bytes) => {
                if self.mode & MODE_TYPE == MODE_DIR {
                    return Err(Error::new(EBADFD));
                }

                // if there's a seek hole, fill it with 0 and continue writing.
                let end_off = offset.checked_add(buf.len()).ok_or(Error::new(EOVERFLOW))?;
                if end_off > bytes.len() {
                    let additional = end_off - bytes.len();
                    bytes.try_reserve(additional).or(Err(Error::new(ENOMEM)))?;
                    bytes.resize(end_off, 0u8);
                }
                bytes[offset..][..buf.len()].copy_from_slice(buf);

                Ok(buf.len())
            }

            FileData::Directory(_) => Err(Error::new(EISDIR)),
            FileData::Socket(_) => Err(Error::new(ENXIO)),
        }
    }
}

#[derive(Copy, Clone, Debug)]
pub struct Inode(pub usize);

pub enum FileData {
    File(Vec<u8>),
    Directory(IndexMap<String, Inode>),
    Socket(Fd),
}

impl FileData {
    pub fn size(&self) -> usize {
        match self {
            &Self::File(ref data) => data.len(),
            &Self::Directory(_) | &Self::Socket(_) => 0,
        }
    }
}

impl std::fmt::Debug for FileData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::File(data) => f.debug_tuple("File").field(data).finish(),
            Self::Directory(files) => f.debug_tuple("Directory").field(files).finish(),
            Self::Socket(fd) => f.debug_tuple("Socket").field(&fd.raw()).finish(),
        }
    }
}
pub struct Filesystem {
    pub files: BTreeMap<usize, File>,
    pub memory_file: fs::File,
    pub last_inode_number: usize,
}
impl Filesystem {
    pub const DEFAULT_BLOCK_SIZE: u32 = 4096;
    pub const ROOT_INODE: usize = 1;

    pub fn new(root_mode: u16) -> Result<Self> {
        Ok(Self {
            files: iter::once((Self::ROOT_INODE, Self::create_root_inode(root_mode))).collect(),
            memory_file: fs::File::open("/scheme/memory").or(Err(Error::new(EIO)))?,
            last_inode_number: Self::ROOT_INODE,
        })
    }
    fn create_root_inode(mode: u16) -> File {
        let cur_time = current_time();
        File {
            atime: cur_time,
            ctime: cur_time,
            mtime: cur_time,

            mode: MODE_DIR | (mode & 0o7777),
            nlink: 1,
            open_handles: 0,

            uid: 0,
            gid: 0,

            data: FileData::Directory(IndexMap::new()),
            parent: Inode(Self::ROOT_INODE),
        }
    }
    pub fn get_block_size(&self) -> Result<u32> {
        Ok(libredox::call::fstatvfs(self.memory_file.as_raw_fd() as usize)?.f_bsize as u32)
    }
    pub fn block_size(&self) -> u32 {
        self.get_block_size().unwrap_or(Self::DEFAULT_BLOCK_SIZE)
    }
    pub fn next_inode_number(&mut self) -> Result<usize> {
        let next = self
            .last_inode_number
            .checked_add(1)
            .ok_or(Error::new(ENFILE))?;
        self.last_inode_number = next;
        Ok(next)
    }
    fn resolve_generic(&self, parts: RedoxReference<'_>, uid: u32, gid: u32) -> Result<usize> {
        let mut current_file = self
            .files
            .get(&Self::ROOT_INODE)
            .ok_or(Error::new(ENOENT))?;
        let mut current_inode = Self::ROOT_INODE;

        if parts.as_ref().is_empty() {
            return Ok(current_inode);
        }

        let mut parts = parts.as_ref().split('/');

        loop {
            let Some(part) = parts.next() else {
                break;
            };
            let dentries = match current_file.data {
                FileData::Directory(ref dentries) => dentries,
                FileData::File(_) | FileData::Socket(_) => return Err(Error::new(ENOENT)),
            };
            let perm = current_perm(&current_file, uid, gid);
            if perm & 0o1 == 0 {
                return Err(Error::new(EACCES));
            }

            current_inode = dentries.get(part).ok_or(Error::new(ENOENT))?.0;
            current_file = self.files.get(&current_inode).ok_or(Error::new(EIO))?;
        }
        Ok(current_inode)
    }
    pub fn resolve_except_last<'a>(
        &self,
        path: &'a str,
        uid: u32,
        gid: u32,
    ) -> Result<(usize, Option<RedoxReference<'a>>)> {
        let path = RedoxReference::new(path)
            .ok_or(Error::new(EINVAL))?
            .canonical();

        let (dir, name) = path.dirname_split();
        Ok((
            self.resolve_generic(dir, uid, gid)?,
            name.map(|s| s.into_owned()),
        ))
    }
    pub fn resolve(&self, path: &str, uid: u32, gid: u32) -> Result<usize> {
        let path = RedoxReference::new(path).ok_or(Error::new(EINVAL))?;

        self.resolve_generic(path.canonical(), uid, gid)
    }
}

pub fn current_time() -> TimeSpec {
    let sys_time = time::SystemTime::now();

    let duration = match sys_time.duration_since(time::SystemTime::UNIX_EPOCH) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("The time is apparently now before the Unix epoch...");

            let negative_duration = e.duration();

            return TimeSpec {
                tv_sec: negative_duration
                    .as_secs()
                    .try_into()
                    .unwrap_or(i64::min_value()),
                tv_nsec: negative_duration
                    .subsec_nanos()
                    .try_into()
                    .unwrap_or(i32::min_value()),
            };
        }
    };

    TimeSpec {
        tv_sec: duration.as_secs().try_into().unwrap_or(i64::max_value()),
        tv_nsec: duration
            .subsec_nanos()
            .try_into()
            .unwrap_or(i32::max_value()),
    }
}
