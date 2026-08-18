use std::convert::{TryFrom, TryInto};
use std::os::unix::io::AsRawFd;
use std::{mem, str};

use event::{EventFlags, RawEventQueue};
use libredox::flag;
use libredox::protocol::FsCall;
use libredox::Fd;
use redox_path::RedoxPath;
use redox_rings::sync::{BlockingConsumer, BlockingProducer};
use scheme_utils::{FpathWriter, HandleMap};
use syscall::dirent::{DirEntry, DirentBuf, DirentKind};
use syscall::error::{
    EACCES, EBADF, EBADFD, EEXIST, EINVAL, EIO, EISDIR, ENOENT, ENOMEM, ENOSYS, ENOTDIR, ENOTEMPTY,
    EOPNOTSUPP, EOVERFLOW, EPERM, ERANGE,
};
use syscall::flag::{
    StdFsCallKind, O_ACCMODE, O_CREAT, O_DIRECTORY, O_EXCL, O_RDONLY, O_RDWR, O_STAT, O_TRUNC,
    O_WRONLY,
};
use syscall::schemev2::NewFdFlags;
use syscall::{
    Error, FmoveFdFlags, FobtainFdFlags, Result, Stat, StatVfs, StdFsCallMeta, TimeSpec,
};
use syscall::{MODE_DIR, MODE_FILE, MODE_PERM, MODE_TYPE};

use indexmap::IndexMap;

use redox_scheme::scheme::SchemeSync;
use redox_scheme::{CallerCtx, OpenResult, SendFdRequest, Socket};
use zerocopy::TryFromBytes;

use crate::filesystem::{self, File, FileData, Filesystem, Inode};

use redox_rings::op::{
    FsOpCqe, FsOpKind, FsOpSqe, RingCallVerb, RingSetupFlags, RingSetupParams, RING_MAX_CQ_ENTRIES,
    RING_MAX_SQ_ENTRIES,
};

pub struct Shm {
    ptr: *mut u8,
    size: usize,
}

impl Shm {
    fn new(size: u32) -> Self {
        use libredox::call::{mmap, MmapArgs};
        use libredox::flag::{MAP_SHARED, PROT_READ, PROT_WRITE};

        let size = size as usize;

        assert_ne!(size, 0);
        let ptr = unsafe {
            mmap(MmapArgs {
                addr: core::ptr::null_mut(),
                length: size.next_multiple_of(syscall::PAGE_SIZE),
                prot: PROT_READ | PROT_WRITE,
                flags: MAP_SHARED,
                fd: !0,
                offset: 0,
            })
            .unwrap()
        }
        .cast::<u8>();

        Self { ptr, size }
    }

    unsafe fn get(&mut self, offset: usize, size: usize) -> Option<&mut [u8]> {
        if offset + size > self.size {
            return None;
        }

        unsafe { Some(core::slice::from_raw_parts_mut(self.ptr.add(offset), size)) }
    }
}

impl Drop for Shm {
    fn drop(&mut self) {
        use libredox::call::munmap;
        unsafe {
            munmap(
                self.ptr.cast::<()>(),
                self.size.next_multiple_of(syscall::PAGE_SIZE),
            )
            .unwrap();
        }
    }
}

pub enum RingState {
    Inactive,
    Active {
        sq: BlockingConsumer<FsOpSqe>,
        cq: BlockingProducer<FsOpCqe>,
        fixed_ftbl: Vec<Inode>,
        pipe_fd: Fd,
        shm: Shm,
    },
}

pub enum Handle {
    Inode(usize),
    Ring(RingState),
}

impl Handle {
    fn try_as_inode(&self) -> Result<Inode> {
        match self {
            &Self::Inode(inode) => Ok(Inode(inode)),
            _ => Err(Error::new(EBADFD)),
        }
    }

    fn as_inode(&self) -> Result<usize> {
        match self {
            &Self::Inode(inode) => Ok(inode),
            _ => Err(Error::new(EBADFD)),
        }
    }
}

pub fn handle_ring_req(
    shm: &mut Shm,
    fixed_fdtbl: &mut [Inode],
    fs: &mut Filesystem,
    sqe: FsOpSqe,
) -> Result<usize> {
    let kind = FsOpKind::try_from_raw(sqe.opcode).ok_or(Error::new(EINVAL))?;

    if sqe.buf_offset as usize + sqe.buf_len as usize > shm.size {
        return Err(Error::new(EACCES));
    }

    let buf = unsafe { shm.get(sqe.buf_offset as usize, sqe.buf_len as usize) }
        .ok_or(Error::new(EINVAL))?;

    let inode = fixed_fdtbl
        .get(sqe.file_idx as usize)
        .ok_or(Error::new(EBADFD))?;

    let file = fs
        .files
        .get_mut(&inode.0)
        .ok_or(Error::new(EBADFD))
        .unwrap();

    match kind {
        FsOpKind::Read => file.read(sqe.off as usize, buf),
        FsOpKind::Write => file.write(sqe.off as usize, buf),
    }
}

pub struct Scheme<'a> {
    scheme_name: String,
    socket: &'a Socket,
    pub filesystem: Filesystem,
    pub handles: HandleMap<Handle>,
    proc_creds_capability: Fd,

    queue: &'a RawEventQueue,
    shm_dir: Fd,
    pipe_root: Fd,
}

impl<'a> Scheme<'a> {
    /// Create the scheme, with the name being used for `fpath`.
    pub fn new(
        socket: &'a Socket,
        scheme_name: String,
        queue: &'a RawEventQueue,
        root_mode: u16,
    ) -> Result<Self> {
        let shm_dir_name = format!("/scheme/shm/{scheme_name}");
        Ok(Self {
            scheme_name,
            socket,
            filesystem: Filesystem::new(root_mode)?,
            handles: HandleMap::new(),
            proc_creds_capability: {
                Fd::open(
                    "/scheme/proc/proc-creds-capability",
                    libredox::flag::O_RDONLY,
                    0,
                )?
            },

            queue,
            shm_dir: Fd::open(&shm_dir_name, flag::O_DIRECTORY | flag::O_CLOEXEC, 0)?,
            pipe_root: Fd::open("/scheme/pipe/scheme-root", flag::O_CLOEXEC, 0)?,
        })
    }

    /// Remove a directory entry, where the entry can be both a file or a directory. Used by `unlinkat`.
    fn remove_dentry(&mut self, path: &str, uid: u32, gid: u32, directory: bool) -> Result<()> {
        let (parent_dir_inode, name_to_delete) =
            self.filesystem.resolve_except_last(path, uid, gid)?;
        let name_to_delete = name_to_delete.ok_or(Error::new(EINVAL))?; // can't remove root

        let removed_inode = {
            let parent = self
                .filesystem
                .files
                .get(&parent_dir_inode)
                .ok_or(Error::new(EIO))?;

            check_permissions(O_WRONLY, current_perm(parent, uid, gid))?;

            let FileData::Directory(ref dentries) = parent.data else {
                return Err(Error::new(ENOTDIR));
            };
            let Inode(entry_inode) = dentries
                .get(name_to_delete.as_ref())
                .copied()
                .ok_or(Error::new(ENOENT))?;

            let entry = self
                .filesystem
                .files
                .get(&entry_inode)
                .ok_or(Error::new(EIO))?;

            Self::check_sticky_bit(parent, entry, uid)?;

            let is_dir = entry.mode & MODE_TYPE == MODE_DIR;

            if directory && !is_dir {
                return Err(Error::new(ENOTDIR));
            }
            if !directory && is_dir {
                return Err(Error::new(EISDIR));
            }
            if is_dir {
                let FileData::Directory(ref dentries) = entry.data else {
                    return Err(Error::new(EIO));
                };
                if !dentries.is_empty() {
                    return Err(Error::new(ENOTEMPTY));
                }
            }

            entry_inode
        };

        {
            let parent = self.filesystem.files.get_mut(&parent_dir_inode).unwrap();
            let FileData::Directory(ref mut dentries) = parent.data else {
                unreachable!() // checked above
            };
            assert_eq!(
                dentries.shift_remove(name_to_delete.as_ref()).unwrap().0,
                removed_inode
            );
            if directory {
                parent.nlink -= 1; // '..' of subdirectory
            }
        }

        let removed_inode_info = self.filesystem.files.get_mut(&removed_inode).unwrap();
        if directory {
            removed_inode_info.nlink -= 2; // both the parent entry and '.'
        } else {
            removed_inode_info.nlink -= 1;
        }

        if removed_inode_info.nlink == 0 && removed_inode_info.open_handles == 0 {
            self.filesystem.files.remove(&removed_inode);
        }

        Ok(())
    }

    fn open_existing(&mut self, path: &str, flags: usize, uid: u32, gid: u32) -> Result<Inode> {
        let inode = self.filesystem.resolve(path, uid, gid)?;
        let file = self
            .filesystem
            .files
            .get_mut(&inode)
            .ok_or(Error::new(EIO))?;

        if flags & O_STAT == 0 && flags & O_DIRECTORY != 0 && file.mode & MODE_TYPE != MODE_DIR {
            return Err(Error::new(ENOTDIR));
        }

        // Unlike on Linux, which allows directories to be opened without O_DIRECTORY, Redox has no
        // getdents(2) syscall, and thus it adds the additional restriction that directories have
        // to be opened with O_DIRECTORY, if they aren't opened with O_STAT to check whether it's a
        // directory.
        if flags & O_STAT == 0 && flags & O_DIRECTORY == 0 && file.mode & MODE_TYPE == MODE_DIR {
            return Err(Error::new(EISDIR));
        }

        let current_perm = current_perm(file, uid, gid);
        check_permissions(flags, current_perm)?;

        let opened_as_write = flags & O_ACCMODE == O_WRONLY || flags & O_ACCMODE == O_RDWR;

        if flags & O_TRUNC == O_TRUNC && opened_as_write {
            match file.data {
                // file.data and file.mode should match
                FileData::Directory(_) => return Err(Error::new(EBADFD)),

                // If we opened an existing file with O_CREAT and O_TRUNC
                FileData::File(ref mut data) => data.clear(),

                FileData::Socket(_) => unreachable!(),
            }
        }

        file.open_handles += 1;

        Ok(Inode(inode))
    }

    fn handle_connect(&mut self, id: usize, payload: &mut [u8]) -> Result<usize> {
        let inode = self.handles.get(id)?.as_inode()?;
        let file = self
            .filesystem
            .files
            .get_mut(&inode)
            .ok_or(Error::new(EBADFD))?;
        let FileData::Socket(ref socket) = file.data else {
            return Err(Error::new(EACCES));
        };
        let len = libredox::call::get_socket_token(socket.raw(), payload)?;
        return Ok(len);
    }

    /// Validates new link, used by `flink` and `frename`.
    /// Returns (old_parent_inode, new_parent_inode, target_inode_opt, new_name)
    fn validate_new_link(
        &self,
        inode: usize,
        new_path_str: &str,
        uid: u32,
        gid: u32,
    ) -> Result<(usize, usize, Option<usize>, String)> {
        if inode == Filesystem::ROOT_INODE {
            return Err(Error::new(EINVAL));
        }
        let (new_parent_inode, Some(new_name)) =
            self.filesystem
                .resolve_except_last(new_path_str, uid, gid)?
        else {
            return Err(Error::new(EINVAL));
        };

        let old_parent_inode = self
            .filesystem
            .files
            .get(&inode)
            .ok_or(Error::new(EBADFD))?
            .parent
            .0;

        let target_inode_opt = {
            let new_parent = self
                .filesystem
                .files
                .get(&new_parent_inode)
                .ok_or(Error::new(EIO))?;

            check_permissions(O_WRONLY, current_perm(new_parent, uid, gid))?;

            let FileData::Directory(ref dentries) = new_parent.data else {
                return Err(Error::new(ENOTDIR));
            };

            dentries.get(new_name.as_ref()).copied().map(|i| i.0)
        };

        Ok((
            old_parent_inode,
            new_parent_inode,
            target_inode_opt,
            new_name.to_string(),
        ))
    }

    fn check_sticky_bit(
        parent: &crate::filesystem::File,
        child: &crate::filesystem::File,
        uid: u32,
    ) -> Result<()> {
        if parent.mode & 0o1000 != 0 {
            if uid != 0 && uid != parent.uid && uid != child.uid {
                return Err(Error::new(EACCES));
            }
        }
        Ok(())
    }
}

impl SchemeSync for Scheme<'_> {
    fn scheme_root(&mut self) -> Result<usize> {
        Ok(self.handles.insert(Handle::Inode(Filesystem::ROOT_INODE)))
    }

    fn openat(
        &mut self,
        dirfd: usize,
        path: &str,
        flags: usize,
        fcntl_flags: u32,
        ctx: &CallerCtx,
    ) -> Result<OpenResult> {
        if self.handles.get(dirfd)?.as_inode()? != Filesystem::ROOT_INODE {
            return Err(Error::new(EACCES));
        }

        let exists = self.filesystem.resolve(path, 0, 0).is_ok();
        if flags & O_CREAT != 0 && flags & O_EXCL != 0 && exists {
            return Err(Error::new(EEXIST));
        }

        let inode = if flags & O_CREAT != 0 && exists {
            self.open_existing(path, flags, ctx.uid, ctx.gid)?.0
        } else if flags & O_CREAT != 0 {
            if flags & O_STAT != 0 {
                return Err(Error::new(EINVAL));
            }

            let (parent_dir_inode, new_name) = self
                .filesystem
                .resolve_except_last(path, ctx.uid, ctx.gid)?;
            let new_name = new_name.ok_or(Error::new(EINVAL))?; // cannot mkdir /

            {
                let parent_file = self
                    .filesystem
                    .files
                    .get(&parent_dir_inode)
                    .ok_or(Error::new(EIO))?;

                check_permissions(O_WRONLY, current_perm(parent_file, ctx.uid, ctx.gid))?
            }

            let current_time = filesystem::current_time();

            let new_inode_number = self.filesystem.next_inode_number()?;

            let mut mode = (flags & 0xFFFF) as u16;

            let new_inode = if flags & O_DIRECTORY != 0 {
                if mode & MODE_TYPE == 0 {
                    mode |= MODE_DIR
                }
                if mode & MODE_TYPE != MODE_DIR {
                    return Err(Error::new(EINVAL));
                }

                File {
                    atime: current_time,
                    ctime: current_time,
                    mtime: current_time,
                    gid: ctx.gid,
                    uid: ctx.uid,
                    mode,
                    nlink: 2, // parent entry, "."
                    data: FileData::Directory(IndexMap::new()),
                    open_handles: 1,
                    parent: Inode(parent_dir_inode),
                }
            } else {
                if mode & MODE_TYPE == 0 {
                    mode |= MODE_FILE
                }
                if mode & MODE_TYPE == MODE_DIR {
                    return Err(Error::new(EINVAL));
                }

                File {
                    atime: current_time,
                    ctime: current_time,
                    mtime: current_time,
                    gid: ctx.gid,
                    uid: ctx.uid,
                    mode,
                    nlink: 1,
                    data: FileData::File(Vec::new()),
                    open_handles: 1,
                    parent: Inode(parent_dir_inode),
                }
            };
            let current_perm = current_perm(&new_inode, ctx.uid, ctx.gid);
            check_permissions(flags, current_perm)?;

            self.filesystem.files.insert(new_inode_number, new_inode);

            let parent_file = self
                .filesystem
                .files
                .get_mut(&parent_dir_inode)
                .ok_or(Error::new(EIO))?;
            match parent_file.data {
                FileData::File(_) | FileData::Socket(_) => return Err(Error::new(EIO)),
                FileData::Directory(ref mut entries) => {
                    entries.insert(new_name.to_string(), Inode(new_inode_number));
                    if flags & O_DIRECTORY != 0 {
                        parent_file.nlink += 1; // for '..' backlink
                    }
                }
            }

            new_inode_number
        } else {
            self.open_existing(path, flags | fcntl_flags as usize, ctx.uid, ctx.gid)?
                .0
        };
        let new_id = self.handles.insert(Handle::Inode(inode));
        Ok(OpenResult::ThisScheme {
            number: new_id,
            flags: NewFdFlags::POSITIONED,
        })
    }

    fn call_multiple_ids(
        &mut self,
        ids: &[usize],
        _payload: &mut [u8],
        metadata: &[u64],
        _ctx: &CallerCtx, // Only pid and id are correct here, uid/gid are not used
    ) -> Result<usize> {
        let verb = RingCallVerb::try_from_raw(metadata[0] as u8).ok_or(Error::new(EINVAL))?;

        match verb {
            RingCallVerb::SetFileTable => {
                let (&ring_fd, ids) = ids.split_first().ok_or(Error::new(EINVAL))?;
                if ids.is_empty() {
                    log::error!("RingCallVerb::SetFileTable: got an empty file table");
                    return Err(Error::new(EINVAL));
                }

                let files = ids
                    .iter()
                    .map(|&id| {
                        self.handles
                            .get(id)
                            .and_then(|handle| handle.try_as_inode())
                    })
                    .collect::<Result<Vec<_>, _>>()?;

                let Handle::Ring(RingState::Active { fixed_ftbl, .. }) =
                    self.handles.get_mut(ring_fd)?
                else {
                    return Err(Error::new(EINVAL));
                };

                *fixed_ftbl = files;
                Ok(ids.len())
            }

            RingCallVerb::Setup => Err(Error::new(EINVAL)),
        }
    }

    fn call(
        &mut self,
        id: usize,
        payload: &mut [u8],
        metadata: &[u64],
        _ctx: &CallerCtx, // Only pid and id are correct here, uid/gid are not used
    ) -> Result<usize> {
        match self.handles.get_mut(id)? {
            Handle::Inode(_) => {
                let Some(verb) = FsCall::try_from_raw(metadata[0] as usize) else {
                    return Err(Error::new(EINVAL));
                };

                match verb {
                    FsCall::Connect => self.handle_connect(id, payload),
                    _ => Err(Error::new(EOPNOTSUPP)),
                }
            }

            Handle::Ring(ref mut state) => {
                let verb =
                    RingCallVerb::try_from_raw(metadata[0] as u8).ok_or(Error::new(EINVAL))?;

                match verb {
                    RingCallVerb::Setup => {
                        if !matches!(state, RingState::Inactive) {
                            return Err(Error::new(EIO));
                        }

                        let params = RingSetupParams::try_mut_from_bytes(payload)
                            .map_err(|_| Error::new(EINVAL))?;

                        let flags = params.flags().ok_or(Error::new(EINVAL))?;

                        if params.pool_size == 0 {
                            return Err(Error::new(EINVAL));
                        }

                        if params.nr_sq_entries == 0 || params.nr_sq_entries > RING_MAX_SQ_ENTRIES {
                            return Err(Error::new(EINVAL));
                        }

                        let nr_sq_entries = params.nr_sq_entries.next_power_of_two();
                        let nr_cq_entries = if flags.contains(RingSetupFlags::CQSIZE) {
                            if params.nr_cq_entries == 0
                                || params.nr_cq_entries > RING_MAX_CQ_ENTRIES
                            {
                                return Err(Error::new(EINVAL));
                            }

                            let nr_cq_entries = nr_sq_entries.next_power_of_two();
                            if nr_cq_entries < nr_sq_entries {
                                return Err(Error::new(EINVAL));
                            }

                            nr_cq_entries
                        } else {
                            nr_sq_entries * 2
                        };

                        params.nr_sq_entries = nr_sq_entries;
                        params.nr_cq_entries = nr_cq_entries;

                        let sq_name = format!("{id}.sq");
                        let cq_name = format!("{id}.cq");

                        let queue_flags = flag::O_CREAT | flag::O_RDWR | flag::O_CLOEXEC;

                        let sq_fd = self.shm_dir.openat(&sq_name, queue_flags, 0)?;
                        let cq_fd = self.shm_dir.openat(&cq_name, queue_flags, 0)?;
                        let pipe_fd = self.pipe_root.openat("", flag::O_CLOEXEC, 0)?;

                        let sq =
                            BlockingConsumer::<FsOpSqe>::from_fd(sq_fd, true, Some(nr_sq_entries))?;
                        let cq =
                            BlockingProducer::<FsOpCqe>::from_fd(cq_fd, true, Some(nr_cq_entries))?;

                        // TODO: When can [`RawEventQueue::subscribe`] return an error?
                        self.queue.subscribe(pipe_fd.raw(), id, EventFlags::READ)?;

                        *state = RingState::Active {
                            sq,
                            cq,
                            fixed_ftbl: Vec::new(),
                            pipe_fd,
                            shm: Shm::new(params.pool_size),
                        };

                        Ok(0)
                    }

                    RingCallVerb::SetFileTable => Err(Error::new(EINVAL)),
                }
            }
        }
    }

    fn dup(&mut self, old_id: usize, buf: &[u8], _ctx: &CallerCtx) -> Result<OpenResult> {
        let handle = self.handles.get(old_id)?;

        if buf == b"uring" {
            if handle.as_inode()? != Filesystem::ROOT_INODE {
                return Err(Error::new(EOPNOTSUPP));
            }

            let new_id = self.handles.insert(Handle::Ring(RingState::Inactive));

            Ok(OpenResult::ThisScheme {
                number: new_id,
                flags: NewFdFlags::empty(),
            })
        } else {
            Err(Error::new(EOPNOTSUPP))
        }
    }

    fn on_recvfd(&mut self, recvfd_request: &redox_scheme::RecvFdRequest) -> Result<OpenResult> {
        let id = recvfd_request.id();
        let handle = self.handles.get(id)?;

        match handle {
            Handle::Ring(RingState::Active {
                sq, cq, pipe_fd, ..
            }) => {
                let ring_fds = [sq.fd().raw(), cq.fd().raw(), pipe_fd.raw()];
                recvfd_request.move_fd(self.socket, FmoveFdFlags::CLONE, ring_fds.as_slice())?;

                Ok(OpenResult::OtherSchemeMultiple {
                    // TODO: This field seems to be unused in the kernel. Can it be removed?
                    num_fds: recvfd_request.num_fds(),
                })
            }

            _ => Err(Error::new(EINVAL)),
        }
    }

    fn mmap_prep(
        &mut self,
        id: usize,
        offset: u64,
        size: usize,
        _flags: syscall::MapFlags,
        _ctx: &CallerCtx,
    ) -> Result<usize> {
        match self.handles.get(id)? {
            Handle::Ring(RingState::Active { shm, .. }) => {
                let offset = offset as usize;
                if offset + size > shm.size {
                    return Err(Error::new(EINVAL));
                }
                Ok((shm.ptr as usize) + offset)
            }
            // TODO
            Handle::Inode(_) | Handle::Ring(_) => Err(Error::new(ENOSYS)),
        }
    }

    fn unlinkat(&mut self, dirfd: usize, path: &str, flags: usize, ctx: &CallerCtx) -> Result<()> {
        if self.handles.get(dirfd)?.as_inode()? != Filesystem::ROOT_INODE {
            return Err(Error::new(EACCES));
        }
        self.remove_dentry(
            path,
            ctx.uid,
            ctx.gid,
            flags & syscall::AT_REMOVEDIR == syscall::AT_REMOVEDIR,
        )
    }

    fn read(
        &mut self,
        fd: usize,
        buf: &mut [u8],
        offset: u64,
        fcntl_flags: u32,
        _ctx: &CallerCtx,
    ) -> Result<usize> {
        let Ok(offset) = usize::try_from(offset) else {
            return Err(Error::new(EOVERFLOW));
        };
        let inode = self.handles.get(fd)?.as_inode()?;
        let file = self
            .filesystem
            .files
            .get_mut(&inode)
            .ok_or(Error::new(EBADFD))?;

        if !matches!((fcntl_flags as usize) & O_ACCMODE, O_RDONLY | O_RDWR) {
            return Err(Error::new(EBADF));
        }

        file.read(offset, buf)
    }

    fn write(
        &mut self,
        fd: usize,
        buf: &[u8],
        offset: u64,
        _fcntl_flags: u32,
        _ctx: &CallerCtx,
    ) -> Result<usize> {
        let Ok(offset) = usize::try_from(offset) else {
            return Err(Error::new(EOVERFLOW));
        };
        let inode = self.handles.get(fd)?.as_inode()?;
        let file = self
            .filesystem
            .files
            .get_mut(&inode)
            .ok_or(Error::new(EBADFD))?;

        file.write(offset, buf)
    }

    fn getdents<'buf>(
        &mut self,
        fd: usize,
        mut buf: DirentBuf<&'buf mut [u8]>,
        opaque_offset: u64,
    ) -> Result<DirentBuf<&'buf mut [u8]>> {
        let Ok(offset) = usize::try_from(opaque_offset) else {
            return Ok(buf);
        };
        let inode = self.handles.get(fd)?.as_inode()?;
        let file = self
            .filesystem
            .files
            .get_mut(&inode)
            .ok_or(Error::new(EBADFD))?;

        let FileData::Directory(ref dir) = file.data else {
            return Err(Error::new(ENOTDIR));
        };

        for (i, (dent_name, Inode(dent_inode))) in dir.iter().enumerate().skip(offset) {
            buf.entry(DirEntry {
                inode: *dent_inode as u64,
                name: dent_name,
                kind: DirentKind::Unspecified,
                next_opaque_id: i as u64 + 1,
            })?;
        }
        Ok(buf)
    }
    fn fchmod(&mut self, fd: usize, mode: u16, _ctx: &CallerCtx) -> Result<()> {
        let inode = self.handles.get(fd)?.as_inode()?;
        let file = self
            .filesystem
            .files
            .get_mut(&inode)
            .ok_or(Error::new(EBADFD))?;

        let cur_type = file.mode & MODE_TYPE;

        /*
        // TODO: validate ctx
        if ctx.uid != 0 && ctx.uid != file.uid {
            return Err(Error::new(EPERM));
        }
        */

        /*
        if mode & MODE_TYPE != 0 {
            return Err(Error::new(EINVAL));
        }
        */

        file.mode = (mode & 0o7777) | cur_type;

        Ok(())
    }
    fn fchown(&mut self, fd: usize, uid: u32, gid: u32, _ctx: &CallerCtx) -> Result<()> {
        let inode = self.handles.get(fd)?.as_inode()?;
        let file = self
            .filesystem
            .files
            .get_mut(&inode)
            .ok_or(Error::new(EBADFD))?;

        // TODO: validate ctx

        file.uid = uid;
        file.gid = gid;

        Ok(())
    }
    fn fcntl(
        &mut self,
        _inode: usize,
        _cmd: usize,
        _arg: usize,
        _ctx: &CallerCtx,
    ) -> Result<usize> {
        Ok(0)
    }
    fn fevent(
        &mut self,
        _inode: usize,
        _flags: syscall::EventFlags,
        _ctx: &CallerCtx,
    ) -> Result<syscall::EventFlags> {
        // TODO?
        Err(Error::new(ENOSYS))
    }
    fn fpath(&mut self, fd: usize, buf: &mut [u8], _ctx: &CallerCtx) -> Result<usize> {
        FpathWriter::with(buf, &self.scheme_name, |w| {
            let mut current_inode = self.handles.get(fd)?.as_inode()?;
            let mut chain = Vec::new();

            let mut current_info = self
                .filesystem
                .files
                .get(&current_inode)
                .ok_or(Error::new(EBADFD))?;

            while current_inode != Filesystem::ROOT_INODE {
                let parent_info = self
                    .filesystem
                    .files
                    .get(&current_info.parent.0)
                    .ok_or(Error::new(EBADFD))?;

                let FileData::Directory(ref dir) = parent_info.data else {
                    return Err(Error::new(EBADFD));
                };
                // TODO: error handling?
                let (name, _) = dir
                    .iter()
                    .find(|(_name, inode)| inode.0 == current_inode)
                    .ok_or(Error::new(ENOENT))?;
                chain.push(&**name);

                current_inode = current_info.parent.0;
                current_info = parent_info;
            }

            for (i, component) in chain.iter().copied().rev().enumerate() {
                if i != 0 {
                    w.push_str("/");
                }
                w.push_str(component);
            }
            Ok(())
        })
    }
    fn flink(&mut self, fd: usize, path: &str, ctx: &CallerCtx) -> Result<usize> {
        let inode = self.handles.get(fd)?.as_inode()?;
        let uid = ctx.uid;
        let gid = ctx.gid;

        let (_, new_parent_inode, target_inode_opt, new_name) =
            self.validate_new_link(inode, path, uid, gid)?;

        if target_inode_opt.is_some() {
            return Err(Error::new(EEXIST));
        }

        if {
            let file = self.filesystem.files.get(&inode).ok_or(Error::new(EIO))?;
            file.mode & MODE_TYPE == MODE_DIR
        } {
            // Prevent hard link directories
            return Err(Error::new(EPERM));
        }

        {
            let file = self
                .filesystem
                .files
                .get_mut(&inode)
                .ok_or(Error::new(EIO))?;
            file.nlink = file.nlink.checked_add(1).ok_or(Error::new(EOVERFLOW))?;
            file.ctime = filesystem::current_time();
        }

        {
            let new_parent = self
                .filesystem
                .files
                .get_mut(&new_parent_inode)
                .ok_or(Error::new(EIO))?;

            let FileData::Directory(ref mut dentries) = new_parent.data else {
                return Err(Error::new(EIO));
            };

            dentries.insert(new_name, Inode(inode));

            let cur_time = filesystem::current_time();
            new_parent.mtime = cur_time;
            new_parent.ctime = cur_time;
        }

        Ok(0)
    }

    fn frename(&mut self, fd: usize, path: &str, ctx: &CallerCtx) -> Result<usize> {
        let inode = self.handles.get(fd)?.as_inode()?;
        let uid = ctx.uid;
        let gid = ctx.gid;

        let (old_parent_inode, new_parent_inode, target_inode_opt, new_name) =
            self.validate_new_link(inode, path, uid, gid)?;

        {
            let old_parent = self
                .filesystem
                .files
                .get(&old_parent_inode)
                .ok_or(Error::new(EIO))?;

            let file = self.filesystem.files.get(&inode).ok_or(Error::new(EIO))?;

            check_permissions(O_WRONLY, current_perm(old_parent, uid, gid))?;

            Self::check_sticky_bit(old_parent, file, uid)?;
        }

        let is_dir = {
            let file = self.filesystem.files.get(&inode).ok_or(Error::new(EIO))?;
            file.mode & MODE_TYPE == MODE_DIR
        };

        if let Some(target_inode) = target_inode_opt {
            if target_inode == inode {
                return Ok(0); //  no-op
            }

            let target_file = self
                .filesystem
                .files
                .get(&target_inode)
                .ok_or(Error::new(EIO))?;
            let target_is_dir = target_file.mode & MODE_TYPE == MODE_DIR;

            if is_dir && !target_is_dir {
                return Err(Error::new(ENOTDIR));
            }
            if !is_dir && target_is_dir {
                return Err(Error::new(EISDIR));
            }
            if target_is_dir {
                let FileData::Directory(ref dentries) = target_file.data else {
                    return Err(Error::new(EIO));
                };
                if !dentries.is_empty() {
                    return Err(Error::new(ENOTEMPTY));
                }
            }
        }

        if is_dir {
            let mut curr = new_parent_inode;
            while curr != Filesystem::ROOT_INODE {
                if curr == inode {
                    // Prevent moving this to subdir of itself
                    return Err(Error::new(EINVAL));
                }
                curr = self
                    .filesystem
                    .files
                    .get(&curr)
                    .ok_or(Error::new(EIO))?
                    .parent
                    .0;
            }
        }

        {
            let old_parent = self
                .filesystem
                .files
                .get_mut(&old_parent_inode)
                .ok_or(Error::new(EIO))?;
            let FileData::Directory(ref mut dentries) = old_parent.data else {
                return Err(Error::new(EIO));
            };

            let mut found = None;
            for (k, v) in dentries.iter() {
                if v.0 == inode {
                    found = Some(k.clone());
                    break;
                }
            }
            let name = found.ok_or(Error::new(ENOENT))?;
            dentries.shift_remove(&name);
        }

        {
            let new_parent = self
                .filesystem
                .files
                .get_mut(&new_parent_inode)
                .ok_or(Error::new(EIO))?;
            let FileData::Directory(ref mut dentries) = new_parent.data else {
                return Err(Error::new(EIO));
            };
            dentries.insert(new_name, Inode(inode));
        }

        {
            let file = self
                .filesystem
                .files
                .get_mut(&inode)
                .ok_or(Error::new(EIO))?;
            file.parent = Inode(new_parent_inode);
            file.ctime = filesystem::current_time();
        }

        let cur_time = filesystem::current_time();
        if old_parent_inode == new_parent_inode {
            let parent = self
                .filesystem
                .files
                .get_mut(&old_parent_inode)
                .ok_or(Error::new(EIO))?;
            parent.mtime = cur_time;
            parent.ctime = cur_time;
        } else {
            {
                let old_parent = self
                    .filesystem
                    .files
                    .get_mut(&old_parent_inode)
                    .ok_or(Error::new(EIO))?;
                old_parent.mtime = cur_time;
                old_parent.ctime = cur_time;
                if is_dir {
                    old_parent.nlink -= 1;
                }
            }
            {
                let new_parent = self
                    .filesystem
                    .files
                    .get_mut(&new_parent_inode)
                    .ok_or(Error::new(EIO))?;
                new_parent.mtime = cur_time;
                new_parent.ctime = cur_time;
                if is_dir {
                    new_parent.nlink += 1;
                }
            }
        }

        if let Some(target_inode) = target_inode_opt {
            if target_inode != inode {
                // TODO: call remove_dentry instead?
                let is_target_dir = {
                    let target_file = self
                        .filesystem
                        .files
                        .get_mut(&target_inode)
                        .ok_or(Error::new(EIO))?;
                    if target_file.mode & MODE_TYPE == MODE_DIR {
                        target_file.nlink -= 2; // '.' and the parent entry
                        true
                    } else {
                        target_file.nlink -= 1;
                        false
                    }
                };

                if is_target_dir {
                    let new_parent = self
                        .filesystem
                        .files
                        .get_mut(&new_parent_inode)
                        .ok_or(Error::new(EIO))?;
                    new_parent.nlink -= 1; // for '..' backlink
                }

                let remove = {
                    let target_file = self
                        .filesystem
                        .files
                        .get(&target_inode)
                        .ok_or(Error::new(EIO))?;
                    target_file.nlink == 0 && target_file.open_handles == 0
                };

                if remove {
                    self.filesystem.files.remove(&target_inode);
                }
            }
        }

        Ok(0)
    }
    fn fstat(&mut self, fd: usize, stat: &mut Stat, _ctx: &CallerCtx) -> Result<()> {
        let inode = self.handles.get(fd)?.as_inode()?;

        let block_size = self.filesystem.block_size();
        let file = self
            .filesystem
            .files
            .get_mut(&inode)
            .ok_or(Error::new(EBADFD))?;

        let size = file.data.size().try_into().or(Err(Error::new(EOVERFLOW)))?;

        *stat = Stat {
            st_mode: file.mode,
            st_uid: file.uid,
            st_gid: file.gid,
            st_ino: inode.try_into().map_err(|_| Error::new(EOVERFLOW))?,
            st_nlink: file.nlink.try_into().or(Err(Error::new(EOVERFLOW)))?,
            st_dev: 0,

            st_size: size,
            st_blksize: block_size,
            st_blocks: size.next_multiple_of(u64::from(block_size)),

            st_atime: file
                .atime
                .tv_sec
                .try_into()
                .or(Err(Error::new(EOVERFLOW)))?,
            st_atime_nsec: file
                .atime
                .tv_nsec
                .try_into()
                .or(Err(Error::new(EOVERFLOW)))?,

            st_ctime: file
                .ctime
                .tv_sec
                .try_into()
                .or(Err(Error::new(EOVERFLOW)))?,
            st_ctime_nsec: file
                .ctime
                .tv_nsec
                .try_into()
                .or(Err(Error::new(EOVERFLOW)))?,

            st_mtime: file
                .mtime
                .tv_sec
                .try_into()
                .or(Err(Error::new(EOVERFLOW)))?,
            st_mtime_nsec: file
                .mtime
                .tv_nsec
                .try_into()
                .or(Err(Error::new(EOVERFLOW)))?,
        };

        Ok(())
    }
    fn fstatvfs(&mut self, _inode: usize, stat: &mut StatVfs, _ctx: &CallerCtx) -> Result<()> {
        let abi_stat = libredox::call::fstatvfs(self.filesystem.memory_file.as_raw_fd() as usize)?;
        // TODO: From impl
        *stat = StatVfs {
            f_bavail: abi_stat.f_bavail as u64,
            f_bfree: abi_stat.f_bfree as u64,
            f_blocks: abi_stat.f_blocks as u64,
            f_bsize: abi_stat.f_bsize as u32,
        };

        Ok(())
    }
    fn fsync(&mut self, _inode: usize, _ctx: &CallerCtx) -> Result<()> {
        Ok(())
    }
    fn ftruncate(&mut self, fd: usize, size: u64, _ctx: &CallerCtx) -> Result<()> {
        let inode = self.handles.get(fd)?.as_inode()?;
        let file = self
            .filesystem
            .files
            .get_mut(&inode)
            .ok_or(Error::new(EBADFD))?;

        if file.mode & MODE_TYPE == MODE_DIR {
            return Err(Error::new(EISDIR));
        }
        let size = usize::try_from(size).map_err(|_| Error::new(EOVERFLOW))?;
        match &mut file.data {
            &mut FileData::File(ref mut bytes) => {
                if size > bytes.len() {
                    let additional = size - bytes.len();
                    bytes.try_reserve(additional).or(Err(Error::new(ENOMEM)))?;
                    bytes.resize(size, 0u8)
                } else {
                    bytes.resize(size, 0u8)
                }
            }
            &mut FileData::Directory(_) | &mut FileData::Socket(_) => {
                return Err(Error::new(EBADFD))
            }
        }
        Ok(())
    }
    fn futimens(&mut self, fd: usize, times: &[TimeSpec], _ctx: &CallerCtx) -> Result<()> {
        let inode = self.handles.get(fd)?.as_inode()?;
        let file = self
            .filesystem
            .files
            .get_mut(&inode)
            .ok_or(Error::new(EBADFD))?;

        let new_atime = *times.get(0).ok_or(Error::new(EINVAL))?;
        let new_mtime = *times.get(1).ok_or(Error::new(EINVAL))?;

        file.atime = new_atime;
        file.mtime = new_mtime;

        Ok(())
    }

    fn relpathat(
        &mut self,
        dir_id: usize,
        id: usize,
        path: &mut [u8],
        _ctx: &CallerCtx,
    ) -> Result<usize> {
        let dir_inode = self.handles.get(dir_id)?.as_inode()?;
        let mut current_inode = self.handles.get(id)?.as_inode()?;

        let mut chain = Vec::new();

        let mut current_info = self
            .filesystem
            .files
            .get(&current_inode)
            .ok_or(Error::new(EBADFD))?;

        while current_inode != dir_inode && current_inode != Filesystem::ROOT_INODE {
            let parent_info = self
                .filesystem
                .files
                .get(&current_info.parent.0)
                .ok_or(Error::new(EBADFD))?;

            let FileData::Directory(ref dir) = parent_info.data else {
                return Err(Error::new(EBADFD));
            };

            let (name, _) = dir
                .iter()
                .find(|(_name, inode)| inode.0 == current_inode)
                .ok_or(Error::new(ENOENT))?;

            chain.push(name);

            current_inode = current_info.parent.0;
            current_info = parent_info;
        }

        let mut offset = 0;
        for (i, component) in chain.iter().rev().enumerate() {
            if i != 0 {
                if offset >= path.len() {
                    return Err(Error::new(ERANGE));
                }
                path[offset] = b'/';
                offset += 1;
            }

            let bytes: &[u8] = component.as_ref();
            if offset + bytes.len() > path.len() {
                return Err(Error::new(ERANGE));
            }

            path[offset..offset + bytes.len()].copy_from_slice(bytes);
            offset += bytes.len();
        }

        Ok(offset)
    }

    fn on_sendfd(&mut self, sendfd_request: &SendFdRequest) -> Result<usize> {
        let ctx = sendfd_request.caller();
        let uid = ctx.uid;
        let gid = ctx.gid;

        let parent_inode = self.handles.get(sendfd_request.id())?.as_inode()?;
        let parent_file = self
            .filesystem
            .files
            .get_mut(&parent_inode)
            .ok_or(Error::new(EBADFD))?;
        let FileData::Directory(_) = parent_file.data else {
            return Err(Error::new(ENOTDIR));
        };

        check_permissions(O_WRONLY, current_perm(parent_file, uid, gid))?;

        let mut new_fd = usize::MAX;
        if let Err(e) = sendfd_request.obtain_fd(
            &self.socket,
            FobtainFdFlags::empty(),
            std::slice::from_mut(&mut new_fd),
        ) {
            return Err(e);
        }
        let other_scheme_fd = Fd::new(new_fd);

        let mut url_buf = [0u8; redox_path::PATH_MAX];
        let url_len = other_scheme_fd.fpath(&mut url_buf)?;
        let redox_path =
            RedoxPath::from_absolute_buf(&url_buf, url_len).ok_or(Error::new(EINVAL))?;
        let (_, path) = redox_path.as_parts().ok_or(Error::new(EINVAL))?;
        let mut last_part = String::new();

        if path.dirname_split().1.is_none() {
            return Err(Error::new(EINVAL));
        }

        let (parent_dir_inode, new_name) =
            self.filesystem
                .resolve_except_last(path.as_ref(), ctx.uid, ctx.gid)?;
        let new_name = new_name.ok_or(Error::new(EINVAL))?; // cannot mkdir /

        let current_time = filesystem::current_time();

        let new_inode_number = self.filesystem.next_inode_number()?;

        let stat = other_scheme_fd.stat()?;
        let mode_type = stat.st_mode as u16 & MODE_TYPE;

        let flags = 0o777;

        let new_inode = File {
            atime: current_time,
            ctime: current_time,
            mtime: current_time,
            gid: ctx.gid,
            uid: ctx.uid,
            mode: mode_type | (flags as u16 & MODE_PERM),
            nlink: 1,
            data: FileData::Socket(other_scheme_fd),
            open_handles: 1,
            parent: Inode(parent_dir_inode),
        };
        check_permissions(flags, current_perm(&new_inode, ctx.uid, ctx.gid))?;

        self.filesystem.files.insert(new_inode_number, new_inode);

        let parent_file = self
            .filesystem
            .files
            .get_mut(&parent_dir_inode)
            .ok_or(Error::new(EIO))?;
        match parent_file.data {
            FileData::File(_) | FileData::Socket(_) => return Err(Error::new(EIO)),
            FileData::Directory(ref mut entries) => {
                entries.insert(new_name.to_string(), Inode(new_inode_number));
            }
        }

        Ok(self.handles.insert(Handle::Inode(new_inode_number)))
    }

    fn std_fs_call(
        &mut self,
        id: usize,
        kind: StdFsCallKind,
        _payload: &mut [u8],
        metadata: StdFsCallMeta,
        ctx: &CallerCtx,
    ) -> Result<usize> {
        match kind {
            StdFsCallKind::Fchown => {
                let (new_uid, new_gid) = (metadata.arg1 as u32, metadata.arg1 >> 32 as u32);
                let (_pid, uid, gid) = get_uid_gid_from_pid(&self.proc_creds_capability, ctx.pid)?;
                if uid != 0 && (uid != ctx.uid || gid != ctx.gid) {
                    return Err(Error::new(EPERM));
                }
                self.fchown(id, new_uid, new_gid as u32, ctx).map(|_| 0)
            }
            /* TODO: Support Unlinkat using std_fs_call
            Unlinkat => {
                let path = unsafe { str::from_utf8_unchecked(payload) };
                let flags = metadata.arg1;                {
                    if !matches!(
                        self.handles.get(&id).ok_or(Error::new(EBADF))?,
                        Handle::SchemeRoot
                    ) {
                        return Err(Error::new(EACCES));
                    }
                }
                let (_pid, uid, gid) = get_uid_gid_from_pid(&self.proc_creds_capability, ctx.pid)?;
                self.remove_dentry(
                    path,
                    uid,
                    gid,
                    *flags as usize & syscall::AT_REMOVEDIR == syscall::AT_REMOVEDIR,
                )
                .map(|_| 0)
            }
            */
            _ => Err(Error::new(EOPNOTSUPP)),
        }
    }

    fn on_close(&mut self, fd: usize) {
        let Some(handle) = self.handles.remove(fd) else {
            return;
        };

        match handle {
            Handle::Inode(inode) => {
                let Some(inode_info) = self.filesystem.files.get_mut(&inode) else {
                    return;
                };

                inode_info.open_handles -= 1;

                if inode_info.nlink == 0 && inode_info.open_handles == 0 {
                    self.filesystem.files.remove(&inode);
                }
            }

            Handle::Ring(RingState::Inactive) => {}
            Handle::Ring(RingState::Active { pipe_fd, .. }) => {
                self.queue.unsubscribe(pipe_fd.into_raw()).unwrap();
            }
        }
    }
}
pub fn current_perm(file: &crate::filesystem::File, uid: u32, gid: u32) -> u8 {
    let perm = file.mode & MODE_PERM;

    if uid == 0 {
        // root doesn't have to be checked
        0o7
    } else if uid == file.uid {
        ((perm & 0o700) >> 6) as u8
    } else if gid == file.gid {
        ((perm & 0o70) >> 3) as u8
    } else {
        (perm & 0o7) as u8
    }
}
fn check_permissions(flags: usize, single_mode: u8) -> Result<()> {
    if flags & O_ACCMODE == O_RDONLY && single_mode & 0o4 == 0 {
        return Err(Error::new(EACCES));
    } else if flags & O_ACCMODE == O_WRONLY && single_mode & 0o2 == 0 {
        return Err(Error::new(EACCES));
    } else if flags & O_ACCMODE == O_RDWR && single_mode & 0o6 != 0o6 {
        return Err(Error::new(EACCES));
    }
    Ok(())
}

fn get_uid_gid_from_pid(cap_fd: &libredox::Fd, target_pid: usize) -> Result<(u32, u32, u32)> {
    let mut buffer = [0u8; mem::size_of::<libredox::protocol::ProcMeta>()];
    let _ = libredox::call::get_proc_credentials(cap_fd.raw(), target_pid, &mut buffer).map_err(
        |e| {
            eprintln!(
                "Failed to get process credentials for pid {}: {:?}",
                target_pid, e
            );
            Error::new(EINVAL)
        },
    )?;
    let mut cursor = 0;
    let pid = read_u32(&buffer, cursor)?;
    cursor += mem::size_of::<u32>() * 3;
    let uid = read_u32(&buffer, cursor)?;
    cursor += mem::size_of::<u32>() * 3;
    let gid = read_u32(&buffer, cursor)?;
    Ok((pid, uid, gid))
}

fn read_u32(buffer: &[u8], offset: usize) -> Result<u32> {
    let bytes = buffer
        .get(offset..offset + 4)
        .and_then(|slice| slice.try_into().ok())
        .ok_or_else(|| Error::new(EINVAL))?;

    Ok(u32::from_le_bytes(bytes))
}
