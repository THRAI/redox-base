use std::cmp;
use std::future::{Future, IntoFuture};
use std::io::{self, Read, Seek, SeekFrom};
use std::slice;

use std::collections::BTreeMap;
use std::convert::TryFrom;
use std::fmt::Write;
use std::str;
use std::sync::Mutex;
use std::task::Poll;

use event::EventFlags;
use executor::LocalExecutor;
use libredox::{flag, Fd};
use partitionlib::{LogicalBlockSize, PartitionTable};
use redox_rings::{
    raw::RingPushError,
    sync::{FutexWaitResult, WaitNotifyAsync},
};
use redox_scheme::scheme::{register_scheme_inner, SchemeAsync, SchemeState, SchemeSync};
use redox_scheme::{
    CallerCtx, OpenResult, RecvFdRequest, RequestKind, Response, SignalBehavior, Socket,
};
use scheme_utils::{FpathWriter, HandleMap};
use syscall::dirent::DirentBuf;
use syscall::schemev2::NewFdFlags;
use syscall::{
    Error, FmoveFdFlags, Result, Stat, TimeSpec, EACCES, EAGAIN, EBADF, EINTR, EINVAL, EISDIR,
    ENOENT, ENOLCK, EOPNOTSUPP, EOVERFLOW, EPROTO, EWOULDBLOCK, MODE_DIR, MODE_FILE, O_DIRECTORY,
    O_STAT,
};

/// Split the read operation into a series of block reads.
/// `read_fn` will be called with a block number to be read, and a buffer to be filled.
/// `read_fn` must return a full block of data.
/// Result will be the number of bytes read.
fn block_read(
    offset: u64,
    blksize: u32,
    buf: &mut [u8],
    mut read_fn: impl FnMut(u64, &mut [u8]) -> io::Result<()>,
) -> io::Result<usize> {
    // TODO: Yield sometimes, perhaps after a few blocks or something.

    if buf.len() == 0 {
        return Ok(0);
    }
    let to_copy = usize::try_from(
        offset.saturating_add(u64::try_from(buf.len()).expect("buf.len() larger than u64"))
            - offset,
    )
    .expect("bytes to copy larger than usize");
    let mut curr_buf = &mut buf[..to_copy];
    let mut curr_offset = offset;
    let blk_size = usize::try_from(blksize).expect("blksize larger than usize");
    let mut total_read = 0;

    let mut block_bytes = [0u8; 4096];
    let block_bytes = &mut block_bytes[..blk_size];

    while curr_buf.len() > 0 {
        // TODO: Async/await? I mean, shouldn't AHCI be async?

        let blk_offset =
            usize::try_from(curr_offset % u64::from(blksize)).expect("usize smaller than blksize");
        let to_copy = cmp::min(curr_buf.len(), blk_size - blk_offset);
        assert!(blk_offset + to_copy <= blk_size);

        read_fn(curr_offset / u64::from(blksize), block_bytes)?;

        let src_buf = &block_bytes[blk_offset..];

        curr_buf[..to_copy].copy_from_slice(&src_buf[..to_copy]);
        curr_buf = &mut curr_buf[to_copy..];
        curr_offset += u64::try_from(to_copy).expect("bytes to copy larger than u64");
        total_read += to_copy;
    }
    Ok(total_read)
}

pub trait Disk {
    fn block_size(&self) -> u32;
    fn size(&self) -> u64;

    // These operate on a whole multiple of the block size
    // FIXME maybe only operate on a single block worth of data?
    async fn read(&mut self, block: u64, buffer: &mut [u8]) -> syscall::Result<usize>;
    async fn write(&mut self, block: u64, buffer: &[u8]) -> syscall::Result<usize>;
}

impl<T: Disk + ?Sized> Disk for Box<T> {
    fn block_size(&self) -> u32 {
        (**self).block_size()
    }

    fn size(&self) -> u64 {
        (**self).size()
    }

    async fn read(&mut self, block: u64, buffer: &mut [u8]) -> syscall::Result<usize> {
        (**self).read(block, buffer).await
    }

    async fn write(&mut self, block: u64, buffer: &[u8]) -> syscall::Result<usize> {
        (**self).write(block, buffer).await
    }
}

pub struct DiskWrapper<T> {
    pub disk: T,
    pub pt: Option<PartitionTable>,
}

impl<T: Disk> DiskWrapper<T> {
    pub fn pt(disk: &mut T, executor: &impl ExecutorTrait) -> Option<PartitionTable> {
        let bs = match disk.block_size() {
            512 => LogicalBlockSize::Lb512,
            4096 => LogicalBlockSize::Lb4096,
            _ => return None,
        };
        struct Device<'a, D: Disk, E: ExecutorTrait> {
            disk: &'a mut D,
            executor: &'a E,
            offset: u64,
        }

        impl<'a, D: Disk, E: ExecutorTrait> Seek for Device<'a, D, E> {
            fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
                let size = i64::try_from(self.disk.size()).or(Err(io::Error::new(
                    io::ErrorKind::Other,
                    "Disk larger than 2^63 - 1 bytes",
                )))?;

                self.offset = match from {
                    SeekFrom::Start(new_pos) => cmp::min(self.disk.size(), new_pos),
                    SeekFrom::Current(new_pos) => {
                        cmp::max(0, cmp::min(size, self.offset as i64 + new_pos)) as u64
                    }
                    SeekFrom::End(new_pos) => cmp::max(0, cmp::min(size + new_pos, size)) as u64,
                };

                Ok(self.offset)
            }
        }
        // TODO: Perhaps this impl should be used in the rest of the scheme.
        impl<'a, D: Disk, E: ExecutorTrait> Read for Device<'a, D, E> {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                let blksize = self.disk.block_size();
                let size_in_blocks = self.disk.size() / u64::from(blksize);

                let disk = &mut self.disk;

                let read_block = |block: u64, block_bytes: &mut [u8]| {
                    if block >= size_in_blocks {
                        return Err(io::Error::from_raw_os_error(syscall::EOVERFLOW));
                    }

                    let bytes = self.executor.block_on(disk.read(block, block_bytes))?;
                    assert_eq!(bytes, block_bytes.len());
                    Ok(())
                };
                let bytes_read = block_read(self.offset, blksize, buf, read_block)?;

                self.offset += bytes_read as u64;
                Ok(bytes_read)
            }
        }

        partitionlib::get_partitions(
            &mut Device {
                disk,
                offset: 0,
                executor,
            },
            bs,
        )
        .ok()
        .flatten()
    }

    pub fn new(mut disk: T, executor: &impl ExecutorTrait) -> Self {
        Self {
            pt: Self::pt(&mut disk, executor),
            disk,
        }
    }

    pub fn disk(&self) -> &T {
        &self.disk
    }

    pub fn disk_mut(&mut self) -> &mut T {
        &mut self.disk
    }

    pub fn block_size(&self) -> u32 {
        self.disk.block_size()
    }

    pub fn size(&self) -> u64 {
        self.disk.size()
    }

    pub async fn read(
        &mut self,
        part_num: Option<usize>,
        block: u64,
        buf: &mut [u8],
    ) -> syscall::Result<usize> {
        if buf.len() as u64 % u64::from(self.disk.block_size()) != 0 {
            return Err(Error::new(EINVAL));
        }

        if let Some(part_num) = part_num {
            let part = self
                .pt
                .as_ref()
                .ok_or(syscall::Error::new(EBADF))?
                .partitions
                .get(part_num)
                .ok_or(syscall::Error::new(EBADF))?;

            if block >= part.size {
                return Err(syscall::Error::new(EOVERFLOW));
            }

            let abs_block = part.start_lba + block;

            self.disk.read(abs_block, buf).await
        } else {
            self.disk.read(block, buf).await
        }
    }

    pub async fn write(
        &mut self,
        part_num: Option<usize>,
        block: u64,
        buf: &[u8],
    ) -> syscall::Result<usize> {
        if buf.len() as u64 % u64::from(self.disk.block_size()) != 0 {
            return Err(Error::new(EINVAL));
        }

        if let Some(part_num) = part_num {
            let part = self
                .pt
                .as_ref()
                .ok_or(syscall::Error::new(EBADF))?
                .partitions
                .get(part_num)
                .ok_or(syscall::Error::new(EBADF))?;

            if block >= part.size {
                return Err(syscall::Error::new(EOVERFLOW));
            }

            let abs_block = part.start_lba + block;

            self.disk.write(abs_block, buf).await
        } else {
            self.disk.write(block, buf).await
        }
    }
}

pub struct DiskScheme<T> {
    inner: DiskSchemeInner<T>,
    state: SchemeState,
    socket: Socket,
}

impl<T: Disk> DiskScheme<T> {
    pub fn new(
        daemon: Option<daemon::Daemon>,
        scheme_name: String,
        disks: BTreeMap<u32, T>,
        executor: &impl ExecutorTrait,
    ) -> Self {
        assert!(scheme_name.starts_with("disk"));
        let socket = Socket::nonblock().expect("failed to create disk scheme");

        let mut inner = DiskSchemeInner {
            scheme_name: scheme_name,
            disks: disks
                .into_iter()
                .map(|(k, disk)| (k, DiskWrapper::new(disk, executor)))
                .collect(),
            handles: HandleMap::new(),
        };

        let cap_id = inner.scheme_root().expect("failed to get this scheme root");
        register_scheme_inner(&socket, &inner.scheme_name, cap_id)
            .expect("failed to register disk scheme root");

        if let Some(daemon) = daemon {
            daemon.ready();
        }

        Self {
            inner,
            state: SchemeState::new(),
            socket,
        }
    }

    pub fn event_handle(&self) -> &Fd {
        self.socket.inner()
    }

    /// Process pending and new requests.
    ///
    /// This needs to be called each time there is a new event on the scheme.
    pub async fn tick(&mut self) -> io::Result<()> {
        // Handle new scheme requests
        loop {
            let request = match self.socket.next_request(SignalBehavior::Interrupt) {
                Ok(Some(request)) => request,
                Ok(None) => {
                    // Scheme likely got unmounted
                    // TODO: return this to caller instead
                    std::process::exit(0);
                }
                Err(error) if error.errno == EWOULDBLOCK || error.errno == EAGAIN => break,
                Err(err) if err.errno == EINTR => continue,
                Err(err) => return Err(err.into()),
            };

            let response = match request.kind() {
                RequestKind::Call(call_request) => {
                    // TODO: Spawn a separate task for each scheme call. This would however require the
                    // use of a smarter buffer pool (or direct IO, or a buffer per fd) in order to do
                    // parallel IO. It might also require async-aware locks so that a close() is
                    // correctly ordered wrt IO on the same fd.
                    call_request
                        .handle_async(&mut self.inner, &mut self.state)
                        .await
                }
                RequestKind::SendFd(request) => Response::err(EOPNOTSUPP, request),
                RequestKind::RecvFd(request) => Response::err(EOPNOTSUPP, request),
                RequestKind::Cancellation(_cancellation_request) => {
                    // FIXME implement cancellation
                    continue;
                }
                RequestKind::MsyncMsg | RequestKind::MunmapMsg | RequestKind::MmapMsg => {
                    unreachable!()
                }
                RequestKind::OnClose { id } => {
                    self.inner.on_close(id);
                    continue;
                }
                RequestKind::OnDetach { .. } => continue,
            };
            self.socket
                .write_response(response, SignalBehavior::Restart)?;
        }

        Ok(())
    }
}

enum Handle {
    List(Vec<u8>),       // entries
    Disk(u32),           // disk num
    Partition(u32, u32), // disk num, part num
    SchemeRoot,
}

struct DiskSchemeInner<T> {
    scheme_name: String,
    disks: BTreeMap<u32, DiskWrapper<T>>,
    handles: HandleMap<Handle>,
}

#[derive(Clone)]
pub struct RingDiskWrapper<T> {
    pub disk: T,
    pub pt: Option<PartitionTable>,
}

impl<T: Disk> RingDiskWrapper<T> {
    pub fn pt(disk: &mut T, executor: &impl ExecutorTrait) -> Option<PartitionTable> {
        let bs = match disk.block_size() {
            512 => LogicalBlockSize::Lb512,
            4096 => LogicalBlockSize::Lb4096,
            _ => return None,
        };
        struct Device<'a, D: Disk, E: ExecutorTrait> {
            disk: &'a mut D,
            executor: &'a E,
            offset: u64,
        }

        impl<'a, D: Disk, E: ExecutorTrait> Seek for Device<'a, D, E> {
            fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
                let size = i64::try_from(self.disk.size()).or(Err(io::Error::new(
                    io::ErrorKind::Other,
                    "Disk larger than 2^63 - 1 bytes",
                )))?;

                self.offset = match from {
                    SeekFrom::Start(new_pos) => cmp::min(self.disk.size(), new_pos),
                    SeekFrom::Current(new_pos) => {
                        cmp::max(0, cmp::min(size, self.offset as i64 + new_pos)) as u64
                    }
                    SeekFrom::End(new_pos) => cmp::max(0, cmp::min(size + new_pos, size)) as u64,
                };

                Ok(self.offset)
            }
        }
        // TODO: Perhaps this impl should be used in the rest of the scheme.
        impl<'a, D: Disk, E: ExecutorTrait> Read for Device<'a, D, E> {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                let blksize = self.disk.block_size();
                let size_in_blocks = self.disk.size() / u64::from(blksize);

                let disk = &mut self.disk;

                let read_block = |block: u64, block_bytes: &mut [u8]| {
                    if block >= size_in_blocks {
                        return Err(io::Error::from_raw_os_error(syscall::EOVERFLOW));
                    }

                    let bytes = self.executor.block_on(disk.read(block, block_bytes))?;
                    assert_eq!(bytes, block_bytes.len());
                    Ok(())
                };
                let bytes_read = block_read(self.offset, blksize, buf, read_block)?;

                self.offset += bytes_read as u64;
                Ok(bytes_read)
            }
        }

        partitionlib::get_partitions(
            &mut Device {
                disk,
                offset: 0,
                executor,
            },
            bs,
        )
        .ok()
        .flatten()
    }

    pub fn new(mut disk: T, executor: &impl ExecutorTrait) -> Self {
        Self {
            pt: Self::pt(&mut disk, executor),
            disk,
        }
    }

    pub fn disk(&self) -> &T {
        &self.disk
    }

    pub fn disk_mut(&mut self) -> &mut T {
        &mut self.disk
    }

    pub fn block_size(&self) -> u32 {
        self.disk.block_size()
    }

    pub fn size(&self) -> u64 {
        self.disk.size()
    }

    pub async fn read(
        &mut self,
        part_num: Option<usize>,
        block: u64,
        buf: &mut [u8],
    ) -> syscall::Result<usize> {
        if buf.len() as u64 % u64::from(self.disk.block_size()) != 0 {
            return Err(Error::new(EINVAL));
        }

        if let Some(part_num) = part_num {
            let part = self
                .pt
                .as_ref()
                .ok_or(syscall::Error::new(EBADF))?
                .partitions
                .get(part_num)
                .ok_or(syscall::Error::new(EBADF))?;

            if block >= part.size {
                return Err(syscall::Error::new(EOVERFLOW));
            }

            let abs_block = part.start_lba + block;

            self.disk.read(abs_block, buf).await
        } else {
            self.disk.read(block, buf).await
        }
    }

    pub async fn write(
        &mut self,
        part_num: Option<usize>,
        block: u64,
        buf: &[u8],
    ) -> syscall::Result<usize> {
        if buf.len() as u64 % u64::from(self.disk.block_size()) != 0 {
            return Err(Error::new(EINVAL));
        }

        if let Some(part_num) = part_num {
            let part = self
                .pt
                .as_ref()
                .ok_or(syscall::Error::new(EBADF))?
                .partitions
                .get(part_num)
                .ok_or(syscall::Error::new(EBADF))?;

            if block >= part.size {
                return Err(syscall::Error::new(EOVERFLOW));
            }

            let abs_block = part.start_lba + block;

            self.disk.write(abs_block, buf).await
        } else {
            self.disk.write(block, buf).await
        }
    }
}
pub trait EventSource {
    async fn next(&mut self);
}

impl<Hw: executor::Hardware + 'static> EventSource for executor::ExternalEventSource<Hw> {
    async fn next(&mut self) {
        let _ = std::pin::Pin::new(self).next().await;
    }
}

struct RingEventSource<Ev: EventSource>(Mutex<Ev>);

impl<Ev: EventSource> WaitNotifyAsync for RingEventSource<Ev> {
    async fn wait_on_tail(
        &self,
        _expected_tail: u32,
        _deadline_opt: Option<&TimeSpec>,
    ) -> FutexWaitResult {
        self.0.lock().unwrap().next().await;
        FutexWaitResult::Waited
    }

    fn notify_on_tail(&self) {
        unimplemented!("notify_on_tail is not implemented for RingEventSource")
    }

    async fn wait_on_head(
        &self,
        _expected_head: u32,
        _deadline_opt: Option<&TimeSpec>,
    ) -> FutexWaitResult {
        unimplemented!("wait_on_head is not implemented for RingEventSource")
    }

    fn notify_on_head(&self) {
        unimplemented!("notify_on_head is not implemented for RingEventSource")
    }
}

pub trait ExecutorTrait {
    fn block_on<'a, O: 'a>(&self, fut: impl IntoFuture<Output = O> + 'a) -> O;
    fn spawn(&self, fut: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + 'static>>);
    type Event: EventSource;
    fn register_external_event(&self, fd: usize, flags: EventFlags) -> Self::Event;
}

impl<Hw: executor::Hardware + 'static> ExecutorTrait for std::rc::Rc<executor::LocalExecutor<Hw>> {
    fn block_on<'a, O: 'a>(&self, fut: impl IntoFuture<Output = O> + 'a) -> O {
        executor::LocalExecutor::block_on(self, fut)
    }
    fn spawn(&self, fut: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + 'static>>) {
        executor::LocalExecutor::spawn(self, fut)
    }
    type Event = executor::ExternalEventSource<Hw>;
    fn register_external_event(&self, fd: usize, flags: EventFlags) -> Self::Event {
        executor::LocalExecutor::register_external_event(self, fd, flags)
    }
}

impl<T: Disk> DiskSchemeInner<T> {
    // Checks if any conflicting handles already exist
    fn check_locks(&self, disk_i: u32, part_i_opt: Option<u32>) -> Result<()> {
        for (_, handle) in self.handles.iter() {
            match handle {
                Handle::Disk(i) => {
                    if disk_i == *i {
                        return Err(Error::new(ENOLCK));
                    }
                }
                Handle::Partition(i, p) => {
                    if disk_i == *i {
                        match part_i_opt {
                            Some(part_i) => {
                                if part_i == *p {
                                    return Err(Error::new(ENOLCK));
                                }
                            }
                            None => {
                                return Err(Error::new(ENOLCK));
                            }
                        }
                    }
                }
                _ => (),
            }
        }
        Ok(())
    }
}

impl<T: Disk> SchemeAsync for DiskSchemeInner<T> {
    fn scheme_root(&mut self) -> Result<usize> {
        Ok(self.handles.insert(Handle::SchemeRoot))
    }
    async fn openat(
        &mut self,
        dirfd: usize,
        path_str: &str,
        flags: usize,
        _fcntl_flags: u32,
        ctx: &CallerCtx,
    ) -> Result<OpenResult> {
        if !matches!(self.handles.get(dirfd)?, Handle::SchemeRoot) {
            return Err(Error::new(EACCES));
        }

        if ctx.uid != 0 {
            return Err(Error::new(EACCES));
        }
        let path_str = path_str.trim_matches('/');

        let handle = if path_str.is_empty() {
            if flags & O_DIRECTORY == O_DIRECTORY || flags & O_STAT == O_STAT {
                let mut list = String::new();

                for (nsid, disk) in self.disks.iter() {
                    write!(list, "{}\n", nsid).unwrap();

                    if disk.pt.is_none() {
                        continue;
                    }
                    for part_num in 0..disk.pt.as_ref().unwrap().partitions.len() {
                        write!(list, "{}p{}\n", nsid, part_num).unwrap();
                    }
                }

                Handle::List(list.into_bytes())
            } else {
                return Err(Error::new(EISDIR));
            }
        } else if let Some(p_pos) = path_str.chars().position(|c| c == 'p') {
            let nsid_str = &path_str[..p_pos];

            if p_pos + 1 >= path_str.len() {
                return Err(Error::new(ENOENT));
            }
            let part_num_str = &path_str[p_pos + 1..];

            let nsid = nsid_str.parse::<u32>().or(Err(Error::new(ENOENT)))?;
            let part_num = part_num_str.parse::<u32>().or(Err(Error::new(ENOENT)))?;

            if let Some(disk) = self.disks.get(&nsid) {
                if disk
                    .pt
                    .as_ref()
                    .ok_or(Error::new(ENOENT))?
                    .partitions
                    .get(part_num as usize)
                    .is_some()
                {
                    self.check_locks(nsid, Some(part_num))?;

                    Handle::Partition(nsid, part_num)
                } else {
                    return Err(Error::new(ENOENT));
                }
            } else {
                return Err(Error::new(ENOENT));
            }
        } else {
            let nsid = path_str.parse::<u32>().or(Err(Error::new(ENOENT)))?;

            if self.disks.contains_key(&nsid) {
                self.check_locks(nsid, None)?;
                Handle::Disk(nsid)
            } else {
                return Err(Error::new(ENOENT));
            }
        };
        let id = self.handles.insert(handle);
        Ok(OpenResult::ThisScheme {
            number: id,
            flags: NewFdFlags::POSITIONED,
        })
    }
    async fn getdents<'buf>(
        &mut self,
        _id: usize,
        _buf: DirentBuf<&'buf mut [u8]>,
        _opaque_offset: u64,
    ) -> Result<DirentBuf<&'buf mut [u8]>> {
        // TODO
        Err(Error::new(EOPNOTSUPP))
    }

    async fn fstat(&mut self, id: usize, stat: &mut Stat, _ctx: &CallerCtx) -> Result<()> {
        match *self.handles.get(id)? {
            Handle::List(ref data) => {
                stat.st_mode = MODE_DIR;
                stat.st_size = data.len() as u64;
                Ok(())
            }
            Handle::Disk(number) => {
                let disk = self.disks.get_mut(&number).ok_or(Error::new(EBADF))?;
                stat.st_mode = MODE_FILE;
                stat.st_blocks = disk.disk().size() / u64::from(disk.block_size());
                stat.st_blksize = disk.block_size();
                stat.st_size = disk.size();
                Ok(())
            }
            Handle::Partition(disk_num, part_num) => {
                let disk = self.disks.get_mut(&disk_num).ok_or(Error::new(EBADF))?;
                let part = disk
                    .pt
                    .as_ref()
                    .ok_or(Error::new(EBADF))?
                    .partitions
                    .get(part_num as usize)
                    .ok_or(Error::new(EBADF))?;
                stat.st_mode = MODE_FILE;
                stat.st_size = part.size * u64::from(disk.block_size());
                stat.st_blocks = part.size;
                stat.st_blksize = disk.block_size();
                Ok(())
            }
            Handle::SchemeRoot => Err(Error::new(EBADF)),
        }
    }

    async fn fpath(&mut self, id: usize, buf: &mut [u8], _ctx: &CallerCtx) -> Result<usize> {
        FpathWriter::with(buf, &self.scheme_name, |w| {
            match *self.handles.get(id)? {
                Handle::List(_) => (),
                Handle::Disk(number) => {
                    write!(w, "{number}").unwrap();
                }
                Handle::Partition(disk_num, part_num) => {
                    write!(w, "{disk_num}p{part_num}").unwrap();
                }
                Handle::SchemeRoot => return Err(Error::new(EBADF)),
            }
            Ok(())
        })
    }

    async fn read(
        &mut self,
        id: usize,
        buf: &mut [u8],
        offset: u64,
        _fcntl_flags: u32,
        _ctx: &CallerCtx,
    ) -> Result<usize> {
        match *self.handles.get_mut(id)? {
            Handle::List(ref handle) => {
                let src = usize::try_from(offset)
                    .ok()
                    .and_then(|o| handle.get(o..))
                    .unwrap_or(&[]);
                let count = core::cmp::min(src.len(), buf.len());
                buf[..count].copy_from_slice(&src[..count]);
                Ok(count)
            }
            Handle::Disk(number) => {
                let disk = self.disks.get_mut(&number).ok_or(Error::new(EBADF))?;
                let block = offset / u64::from(disk.block_size());
                disk.read(None, block, buf).await
            }
            Handle::Partition(disk_num, part_num) => {
                let disk = self.disks.get_mut(&disk_num).ok_or(Error::new(EBADF))?;
                let block = offset / u64::from(disk.block_size());
                disk.read(Some(part_num as usize), block, buf).await
            }
            Handle::SchemeRoot => Err(Error::new(EBADF)),
        }
    }

    async fn write(
        &mut self,
        id: usize,
        buf: &[u8],
        offset: u64,
        _fcntl_flags: u32,
        _ctx: &CallerCtx,
    ) -> Result<usize> {
        match *self.handles.get_mut(id)? {
            Handle::List(_) => Err(Error::new(EBADF)),
            Handle::Disk(number) => {
                let disk = self.disks.get_mut(&number).ok_or(Error::new(EBADF))?;
                let block = offset / u64::from(disk.block_size());
                disk.write(None, block, buf).await
            }
            Handle::Partition(disk_num, part_num) => {
                let disk = self.disks.get_mut(&disk_num).ok_or(Error::new(EBADF))?;
                let block = offset / u64::from(disk.block_size());
                disk.write(Some(part_num as usize), block, buf).await
            }
            Handle::SchemeRoot => Err(Error::new(EBADF)),
        }
    }

    async fn fsize(&mut self, id: usize, _ctx: &CallerCtx) -> Result<u64> {
        Ok(match *self.handles.get_mut(id)? {
            Handle::List(ref handle) => handle.len() as u64,
            Handle::Disk(number) => {
                let disk = self.disks.get_mut(&number).ok_or(Error::new(EBADF))?;
                disk.size()
            }
            Handle::Partition(disk_num, part_num) => {
                let disk = self.disks.get_mut(&disk_num).ok_or(Error::new(EBADF))?;
                let part = disk
                    .pt
                    .as_ref()
                    .ok_or(Error::new(EBADF))?
                    .partitions
                    .get(part_num as usize)
                    .ok_or(Error::new(EBADF))?;

                part.size * u64::from(disk.block_size())
            }
            Handle::SchemeRoot => return Err(Error::new(EBADF)),
        })
    }
}

impl<D: Disk> DiskSchemeInner<D> {
    pub fn on_close(&mut self, id: usize) {
        let _ = self.handles.remove(id);
    }
}

pub struct RingDiskScheme<T, E> {
    inner: RingDiskSchemeInner<T, E>,
    state: SchemeState,
}

impl<T: Disk + Clone + 'static, E: ExecutorTrait + Clone + 'static> RingDiskScheme<T, E> {
    pub fn new(
        daemon: Option<daemon::Daemon>,
        scheme_name: String,
        disks: BTreeMap<u32, T>,
        executor: E,
    ) -> Self {
        assert!(scheme_name.starts_with("disk"));
        let socket = Socket::nonblock().expect("failed to create disk scheme");
        let shm_dir_name = format!("/scheme/shm/{}", scheme_name);

        let mut inner = RingDiskSchemeInner {
            scheme_name,
            socket,
            disks: BTreeMap::from_iter(
                disks
                    .into_iter()
                    .map(|(k, disk)| (k, RingDiskWrapper::new(disk, &executor))),
            ),
            next_id: 0,
            handles: BTreeMap::new(),
            executor,
            shm_dir: libredox::Fd::open(&shm_dir_name, flag::O_DIRECTORY | flag::O_CLOEXEC, 0)
                .expect("failed to open shm direcotry"),
            pipe_root: libredox::Fd::open("/scheme/pipe/scheme-root", flag::O_CLOEXEC, 0)
                .expect("failed to open pipe root"),
        };

        let cap_id = inner.scheme_root().expect("failed to get this scheme root");
        register_scheme_inner(&inner.socket, &inner.scheme_name, cap_id)
            .expect("failed to register disk scheme root");

        if let Some(daemon) = daemon {
            daemon.ready();
        }

        Self {
            inner,
            state: SchemeState::new(),
        }
    }

    pub fn event_handle(&self) -> &Fd {
        self.inner.socket.inner()
    }
    /// Process pending and new requests.
    ///
    /// This needs to be called each time there is a new event on the scheme.
    pub fn tick(&mut self) -> io::Result<()> {
        // Handle new scheme requests
        loop {
            let request = match self.inner.socket.next_request(SignalBehavior::Interrupt) {
                Ok(Some(request)) => request,
                Ok(None) => {
                    // Scheme likely got unmounted
                    // TODO: return this to caller instead
                    std::process::exit(0);
                }
                Err(error) if error.errno == EWOULDBLOCK || error.errno == EAGAIN => break,
                Err(err) if err.errno == EINTR => continue,
                Err(err) => return Err(err.into()),
            };

            let response = match request.kind() {
                RequestKind::Call(call_request) => {
                    // TODO: Spawn a separate task for each scheme call. This would however require the
                    // use of a smarter buffer pool (or direct IO, or a buffer per fd) in order to do
                    // parallel IO. It might also require async-aware locks so that a close() is
                    // correctly ordered wrt IO on the same fd.
                    call_request.handle_sync(&mut self.inner, &mut self.state)
                }
                RequestKind::SendFd(request) => Response::err(EOPNOTSUPP, request),
                RequestKind::RecvFd(request) => {
                    Response::open_dup_like(self.inner.on_recvfd(&request), request)
                }
                RequestKind::Cancellation(_cancellation_request) => {
                    // FIXME implement cancellation
                    continue;
                }
                RequestKind::MsyncMsg | RequestKind::MunmapMsg | RequestKind::MmapMsg => {
                    unreachable!()
                }
                RequestKind::OnClose { id } => {
                    self.inner.on_close(id);
                    continue;
                }
                RequestKind::OnDetach { .. } => continue,
            };
            self.inner
                .socket
                .write_response(response, SignalBehavior::Restart)?;
        }

        Ok(())
    }
}

enum RingHandle {
    List(Vec<u8>), // entries
    Disk {
        num: u32,
        pt: Option<usize>,
        ring_fds: [usize; 4],
    },
    SchemeRoot,
}

struct RingDiskSchemeInner<T, E> {
    scheme_name: String,
    socket: Socket,
    disks: BTreeMap<u32, RingDiskWrapper<T>>,
    handles: HandleMap<Handle>,
    executor: E,
    shm_dir: Fd,
    pipe_root: Fd,
}

impl<T: Disk + Clone + 'static, E: ExecutorTrait + Clone + 'static> RingDiskSchemeInner<T, E> {
    // Checks if any conflicting handles already exist
    fn check_locks(&self, disk_i: u32, part_i_opt: Option<usize>) -> Result<()> {
        for (_, handle) in self.handles.iter() {
            match handle {
                RingHandle::Disk { num, pt, .. } => {
                    let i = *num;
                    if let Some(p) = *pt {
                        if disk_i == i {
                            match part_i_opt {
                                Some(part_i) => {
                                    if part_i == p {
                                        return Err(Error::new(ENOLCK));
                                    }
                                }
                                None => {
                                    return Err(Error::new(ENOLCK));
                                }
                            }
                        }
                    } else {
                        if disk_i == i {
                            return Err(Error::new(ENOLCK));
                        }
                    }
                }
                _ => (),
            }
        }
        Ok(())
    }
    fn on_close(&mut self, id: usize) {
        let _ = self.handles.remove(&id);
    }

    fn setup_worker(
        &mut self,
        num: u32,
        pt: Option<usize>,
        disk: RingDiskWrapper<T>,
    ) -> Result<[usize; 4]> {
        let number_str = if let Some(part_num) = pt {
            format!("{}p{}", num, part_num)
        } else {
            format!("{}", num)
        };
        let pool_path = format!("{}.pool", number_str);
        let sq_path = format!("{}.sq", number_str);
        let cq_path = format!("{}.cq", number_str);

        let shm_fd = self.shm_dir.openat(
            &pool_path,
            flag::O_CREAT | flag::O_RDWR | flag::O_CLOEXEC,
            0,
        )?;

        syscall::ftruncate(shm_fd.raw(), POOL_SIZE)
            .map_err(|e| format!("Failed to resize shm pool: {:?}", e))
            .unwrap();

        let shm_ptr = unsafe {
            libredox::call::mmap(libredox::call::MmapArgs {
                fd: shm_fd.raw(),
                offset: 0,
                length: POOL_SIZE,
                prot: flag::PROT_READ | flag::PROT_WRITE,
                flags: flag::MAP_SHARED,
                addr: std::ptr::null_mut(),
            })? as *mut u8
        };

        let pipe = self.pipe_root.openat("", flag::O_CLOEXEC, 0)?;

        let sq_fd =
            self.shm_dir
                .openat(&sq_path, flag::O_CREAT | flag::O_RDWR | flag::O_CLOEXEC, 0)?;

        let cq_fd =
            self.shm_dir
                .openat(&cq_path, flag::O_CREAT | flag::O_RDWR | flag::O_CLOEXEC, 0)?;

        let ring_fds = [shm_fd.raw(), sq_fd.raw(), cq_fd.raw(), pipe.raw()];

        let sq = BlockingConsumer::<DiskOpSqe>::from_fd(sq_fd, true, Some(RING_SIZE))?;
        let cq = BlockingProducer::<DiskOpCqe>::from_fd(cq_fd, true, Some(RING_SIZE))?;

        const BATCH_LIMIT: usize = 128;
        let mut ring_worker = DiskWorker {
            sq,
            cq,
            disk,
            pt,
            shm_base: shm_ptr,
            shm_fd,
            pipe,
        };

        let exec_for_task = self.executor.clone();

        self.executor.spawn(Box::pin(async move {
            let source = RingEventSource(Mutex::new(
                exec_for_task.register_external_event(ring_worker.pipe.raw(), EventFlags::READ),
            ));
            let mut queue: Vec<DiskOpSqe> = Vec::with_capacity(BATCH_LIMIT);

            loop {
                let mut spun = false;

                for _ in 0..10_000 {
                    match ring_worker.sq.try_pop() {
                        Ok(req) => {
                            queue.push(req);
                            while queue.len() < BATCH_LIMIT {
                                match ring_worker.sq.try_pop() {
                                    Ok(req) => queue.push(req),
                                    Err(_) => break,
                                }
                            }
                            spun = true;
                            break;
                        }
                        Err(redox_rings::raw::RingPopError::Empty) => {
                            std::hint::spin_loop();
                        }
                        Err(e) => {
                            log::error!("Failed to pop Sqe with error: {:?}", e);
                            spun = true;
                            break;
                        }
                    }
                }

                for req in queue.drain(..) {
                    let _ = ring_worker.handle_request(req).await;
                }

                if spun {
                    continue;
                }

                if let Ok(req) = ring_worker.sq.inner.inner.pop_async(&source, None).await {
                    queue.push(req);
                }
            }
        }));

        Ok(ring_fds)
    }

    fn on_recvfd(&mut self, recvfd_request: &RecvFdRequest) -> Result<OpenResult> {
        let id = recvfd_request.id();
        let handle = self.handles.get(&id).ok_or(Error::new(EBADF))?;
        match handle {
            RingHandle::Disk { ring_fds, .. } => {
                if let Err(e) = recvfd_request.move_fd(&self.socket, FmoveFdFlags::CLONE, ring_fds)
                {
                    log::error!("recvfd_inner: move_fd failed with error: {:?}", e);
                    return Err(Error::new(EPROTO));
                }

                Ok(OpenResult::OtherSchemeMultiple {
                    num_fds: recvfd_request.num_fds(),
                })
            }
            RingHandle::SchemeRoot | RingHandle::List(_) => Err(Error::new(EBADF)),
        }
    }
}

impl<T: Disk + Clone + 'static, E: ExecutorTrait + Clone + 'static> SchemeSync
    for RingDiskSchemeInner<T, E>
{
    fn scheme_root(&mut self) -> Result<usize> {
        Ok(self.handles.insert(RingHandle::SchemeRoot))
    }

    fn openat(
        &mut self,
        dirfd: usize,
        path_str: &str,
        flags: usize,
        _fcntl_flags: u32,
        ctx: &CallerCtx,
    ) -> Result<OpenResult> {
        if !matches!(
            self.handles.get(&dirfd).ok_or(Error::new(EBADF))?,
            RingHandle::SchemeRoot
        ) {
            return Err(Error::new(EACCES));
        }

        if ctx.uid != 0 {
            return Err(Error::new(EACCES));
        }
        let path_str = path_str.trim_matches('/');

        let handle = if path_str.is_empty() {
            if flags & O_DIRECTORY == O_DIRECTORY || flags & O_STAT == O_STAT {
                let mut list = String::new();

                for (nsid, disk) in self.disks.iter() {
                    write!(list, "{}\n", nsid).unwrap();

                    if disk.pt.is_none() {
                        continue;
                    }
                    for part_num in 0..disk.pt.as_ref().unwrap().partitions.len() {
                        write!(list, "{}p{}\n", nsid, part_num).unwrap();
                    }
                }

                RingHandle::List(list.into_bytes())
            } else {
                return Err(Error::new(EISDIR));
            }
        } else {
            let (nsid, part_num_opt) = if let Some(p_pos) = path_str.chars().position(|c| c == 'p')
            {
                let nsid_str = &path_str[..p_pos];

                if p_pos + 1 >= path_str.len() {
                    return Err(Error::new(ENOENT));
                }
                let part_num_str = &path_str[p_pos + 1..];

                let nsid = nsid_str.parse::<u32>().or(Err(Error::new(ENOENT)))?;
                let part_num = part_num_str.parse::<usize>().or(Err(Error::new(ENOENT)))?;

                let disk = self.disks.get(&nsid).ok_or(Error::new(ENOENT))?;

                if disk
                    .pt
                    .as_ref()
                    .ok_or(Error::new(ENOENT))?
                    .partitions
                    .get(part_num)
                    .is_some()
                {
                    self.check_locks(nsid, Some(part_num))?;
                }
                (nsid, Some(part_num))
            } else {
                let nsid = path_str.parse::<u32>().or(Err(Error::new(ENOENT)))?;
                self.check_locks(nsid, None)?;

                if !self.disks.contains_key(&nsid) {
                    return Err(Error::new(ENOENT));
                }

                (nsid, None)
            };
            let disk_wrapper = self.disks.get(&nsid).unwrap().clone();
            let ring_fds = self.setup_worker(nsid, part_num_opt, disk_wrapper)?;
            RingHandle::Disk {
                num: nsid,
                pt: part_num_opt,
                ring_fds,
            }
        };
        let id = self.handles.insert(handle);
        Ok(OpenResult::ThisScheme {
            number: id,
            flags: NewFdFlags::POSITIONED,
        })
    }

    fn getdents<'buf>(
        &mut self,
        _id: usize,
        _buf: DirentBuf<&'buf mut [u8]>,
        _opaque_offset: u64,
    ) -> Result<DirentBuf<&'buf mut [u8]>> {
        // TODO
        Err(Error::new(EOPNOTSUPP))
    }

    fn fstat(&mut self, id: usize, stat: &mut Stat, _ctx: &CallerCtx) -> Result<()> {
        match *self.handles.get(&id).ok_or(Error::new(EBADF))? {
            RingHandle::List(ref data) => {
                stat.st_mode = MODE_DIR;
                stat.st_size = data.len() as u64;
                Ok(())
            }
            RingHandle::Disk { num, pt, .. } => {
                let disk = self.disks.get(&num).ok_or(Error::new(EBADF))?;
                if let Some(part_num) = pt {
                    let block_size = disk.block_size();
                    let part = disk
                        .pt
                        .as_ref()
                        .ok_or(Error::new(EBADF))?
                        .partitions
                        .get(part_num as usize)
                        .ok_or(Error::new(EBADF))?;
                    stat.st_mode = MODE_FILE;
                    stat.st_size = part.size * u64::from(block_size);
                    stat.st_blocks = part.size;
                    stat.st_blksize = block_size;
                    Ok(())
                } else {
                    let size = disk.size();
                    let block_size = disk.block_size();
                    stat.st_mode = MODE_FILE;
                    stat.st_blocks = size / u64::from(block_size);
                    stat.st_blksize = block_size as u32;
                    stat.st_size = size;
                    Ok(())
                }
            }
            RingHandle::SchemeRoot => Err(Error::new(EBADF)),
        }
    }

    fn fpath(&mut self, id: usize, buf: &mut [u8], _ctx: &CallerCtx) -> Result<usize> {
        let handle = self.handles.get(&id).ok_or(Error::new(EBADF))?;

        let mut i = 0;

        let scheme_name = self.scheme_name.as_bytes();
        let mut j = 0;
        // TODO: copy_from_slice
        while i < buf.len() && j < scheme_name.len() {
            buf[i] = scheme_name[j];
            i += 1;
            j += 1;
        }

        if i < buf.len() {
            buf[i] = b':';
            i += 1;
        }

        match handle {
            RingHandle::List(_) => (),
            RingHandle::Disk { num, pt, .. } => {
                let number_str = if let Some(part_num) = pt {
                    format!("{}p{}", num, part_num)
                } else {
                    format!("{}", num)
                };

                let number_bytes = number_str.as_bytes();
                j = 0;
                while i < buf.len() && j < number_bytes.len() {
                    buf[i] = number_bytes[j];
                    i += 1;
                    j += 1;
                }
            }
            RingHandle::SchemeRoot => return Err(Error::new(EBADF)),
        }

        Ok(i)
    }

    fn read(
        &mut self,
        id: usize,
        buf: &mut [u8],
        offset: u64,
        _fcntl_flags: u32,
        _ctx: &CallerCtx,
    ) -> Result<usize> {
        match *self.handles.get_mut(&id).ok_or(Error::new(EBADF))? {
            RingHandle::List(ref handle) => {
                let src = usize::try_from(offset)
                    .ok()
                    .and_then(|o| handle.get(o..))
                    .unwrap_or(&[]);
                let count = core::cmp::min(src.len(), buf.len());
                buf[..count].copy_from_slice(&src[..count]);
                Ok(count)
            }
            RingHandle::SchemeRoot | RingHandle::Disk { .. } => Err(Error::new(EBADF)),
        }
    }

    fn fsize(&mut self, id: usize, _ctx: &CallerCtx) -> Result<u64> {
        Ok(match *self.handles.get_mut(&id).ok_or(Error::new(EBADF))? {
            RingHandle::List(ref handle) => handle.len() as u64,
            RingHandle::Disk { num, pt, .. } => {
                let disk = self.disks.get_mut(&num).ok_or(Error::new(EBADF))?;
                if let Some(part_num) = pt {
                    let part = disk
                        .pt
                        .as_ref()
                        .ok_or(Error::new(EBADF))?
                        .partitions
                        .get(part_num as usize)
                        .ok_or(Error::new(EBADF))?;

                    part.size * u64::from(disk.block_size())
                } else {
                    disk.size()
                }
            }
            RingHandle::SchemeRoot => return Err(Error::new(EBADF)),
        })
    }
}

use redox_rings::sync::{BlockingConsumer, BlockingProducer};

const POOL_SIZE: usize = 16 * 1024 * 1024; // 16 MB pool
const CHUNK_SIZE: usize = 256 * 1024;
const RING_SIZE: usize = 65536; // 64 KB rings

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
enum DiskOpcode {
    Read = 0,
    Write = 1,
}

impl DiskOpcode {
    pub fn try_from_raw(raw: u8) -> Option<Self> {
        Some(match raw {
            0 => Self::Read,
            1 => Self::Write,
            _ => return None,
        })
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct DiskOpSqe {
    pub block: u64,
    pub id: u64,
    pub buf_offset: u32,
    pub buf_len: u32,
    pub opcode: u8, // 0 = Read, 1 = Write
    pub pad: [u8; 7],
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct DiskOpCqe {
    pub id: u64,
    pub count: u32,
    pub status: u16, // 0 = Success
    pub pad: u16,
}

pub struct DiskWorker<T> {
    sq: BlockingConsumer<DiskOpSqe>,
    cq: BlockingProducer<DiskOpCqe>,
    disk: RingDiskWrapper<T>,
    pt: Option<usize>,
    shm_base: *mut u8,
    shm_fd: Fd,
    pipe: Fd,
}

impl<T: Disk> DiskWorker<T> {
    pub async fn handle_request(&mut self, req: DiskOpSqe) -> std::result::Result<(), String> {
        if req.buf_offset as usize + req.buf_len as usize > POOL_SIZE {
            log::error!(
                "Bounds Check Failed: Offset {} + Len {} > Pool {}",
                req.buf_offset,
                req.buf_len,
                POOL_SIZE
            );
            return Err("Request buffer out of bounds".into());
        }

        let buffer = unsafe {
            slice::from_raw_parts_mut(
                self.shm_base.add(req.buf_offset as usize),
                req.buf_len as usize,
            )
        };

        let result = match DiskOpcode::try_from_raw(req.opcode) {
            Some(opcode) => match opcode {
                DiskOpcode::Read => self.disk.read(self.pt, req.block, buffer).await,
                DiskOpcode::Write => self.disk.write(self.pt, req.block, buffer).await,
            },
            None => {
                log::warn!("Unsupported opcode: {}", req.opcode);
                Err(syscall::Error::new(syscall::EOPNOTSUPP))
            }
        };

        let (status, count) = match result {
            Ok(cnt) => (0, cnt as u32),
            Err(e) => {
                log::error!("Disk read Error: ID={} Errno={}", req.id, e.errno);
                (e.errno as u16, 0)
            }
        };

        let cqe = DiskOpCqe {
            id: req.id,
            status,
            count,
            pad: 0,
        };

        loop {
            match self.cq.try_push(cqe) {
                Ok(_) => break,
                Err(RingPushError::Full(_)) => {
                    yield_now().await;
                }
                Err(e) => {
                    log::error!("Failed to push Cqe {:?} with error: {:?}", cqe, e);
                    return Err(format!("Failed to push response: {:?}", e));
                }
            }
        }

        Ok(())
    }
}

pub struct YieldNow {
    yielded: bool,
}

impl Future for YieldNow {
    type Output = ();

    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Self::Output> {
        if self.yielded {
            Poll::Ready(())
        } else {
            self.yielded = true;
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    }
}

pub fn yield_now() -> YieldNow {
    YieldNow { yielded: false }
}
