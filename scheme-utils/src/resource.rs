use std::fmt::Debug;

use redox_scheme::{CallerCtx, OpenResult, RecvFdRequest, SendFdRequest};
use syscall::dirent::DirentBuf;
use syscall::schemev2::NewFdFlags;
use syscall::{
    EBADF, ENOENT, ENOTDIR, EOPNOTSUPP, ESPIPE, Error, EventFlags, MapFlags, MunmapFlags, Result,
    Stat, StatVfs, StdFsCallKind, StdFsCallMeta, TimeSpec,
};

use crate::{FpathWriter, HandleMap};

#[derive(Debug)]
pub enum ResourceOpenResult<H> {
    ThisScheme { data: H, flags: NewFdFlags },
    OtherScheme { fd: usize },
    WouldBlock,
}

impl<H> ResourceOpenResult<H> {
    pub fn into_scheme(self, handles: &mut HandleMap<H>) -> OpenResult {
        match self {
            ResourceOpenResult::ThisScheme { data, flags } => OpenResult::ThisScheme {
                number: handles.insert(data),
                flags,
            },
            ResourceOpenResult::OtherScheme { fd } => OpenResult::OtherScheme { fd },
            ResourceOpenResult::WouldBlock => OpenResult::WouldBlock,
        }
    }
}

#[allow(unused_variables)]
pub trait ResourceSync: Sized + Debug {
    type SchemeData;
    type ResourceEnum;

    fn openat<'a>(
        &mut self,
        scheme_data: &mut Self::SchemeData,
        path: &str,
        flags: usize,
        fcntl_flags: u32,
        ctx: &CallerCtx,
    ) -> Result<ResourceOpenResult<Self::ResourceEnum>> {
        Err(Error::new(EOPNOTSUPP))
    }

    fn unlinkat(
        &mut self,
        scheme_data: &mut Self::SchemeData,
        path: &str,
        flags: usize,
        ctx: &CallerCtx,
    ) -> Result<()> {
        Err(Error::new(ENOENT))
    }

    fn inode(&self, scheme_data: &Self::SchemeData) -> Result<usize> {
        Err(Error::new(EOPNOTSUPP))
    }

    /* Resource operations */
    fn dup<'a>(
        &mut self,
        scheme_data: &mut Self::SchemeData,
        buf: &[u8],
        ctx: &CallerCtx,
    ) -> Result<ResourceOpenResult<Self::ResourceEnum>> {
        Err(Error::new(EOPNOTSUPP))
    }

    fn read(
        &mut self,
        scheme_data: &mut Self::SchemeData,
        buf: &mut [u8],
        offset: u64,
        fcntl_flags: u32,
        ctx: &CallerCtx,
    ) -> Result<usize> {
        Err(Error::new(EBADF))
    }

    fn write(
        &mut self,
        scheme_data: &mut Self::SchemeData,
        buf: &[u8],
        offset: u64,
        fcntl_flags: u32,
        ctx: &CallerCtx,
    ) -> Result<usize> {
        Err(Error::new(EBADF))
    }

    fn fsize(&mut self, scheme_data: &mut Self::SchemeData, ctx: &CallerCtx) -> Result<u64> {
        Err(Error::new(ESPIPE))
    }

    fn fchmod(
        &mut self,
        scheme_data: &mut Self::SchemeData,
        new_mode: u16,
        ctx: &CallerCtx,
    ) -> Result<()> {
        Err(Error::new(EOPNOTSUPP))
    }

    fn fchown(
        &mut self,
        scheme_data: &mut Self::SchemeData,
        new_uid: u32,
        new_gid: u32,
        ctx: &CallerCtx,
    ) -> Result<()> {
        Err(Error::new(EOPNOTSUPP))
    }

    fn fcntl(
        &mut self,
        scheme_data: &mut Self::SchemeData,
        cmd: usize,
        arg: usize,
        ctx: &CallerCtx,
    ) -> Result<usize> {
        Err(Error::new(EOPNOTSUPP))
    }

    fn fevent(
        &mut self,
        scheme_data: &mut Self::SchemeData,
        flags: EventFlags,
        ctx: &CallerCtx,
    ) -> Result<EventFlags> {
        Ok(EventFlags::empty())
    }

    fn flink(
        &mut self,
        scheme_data: &mut Self::SchemeData,
        path: &str,
        ctx: &CallerCtx,
    ) -> Result<usize> {
        Err(Error::new(EOPNOTSUPP))
    }

    fn fpath(
        &mut self,
        scheme_data: &mut Self::SchemeData,
        w: &mut FpathWriter,
        ctx: &CallerCtx,
    ) -> Result<()> {
        Err(Error::new(EOPNOTSUPP))
    }

    fn frename(
        &mut self,
        scheme_data: &mut Self::SchemeData,
        path: &str,
        ctx: &CallerCtx,
    ) -> Result<usize> {
        Err(Error::new(EOPNOTSUPP))
    }

    fn fstat(
        &mut self,
        scheme_data: &mut Self::SchemeData,
        stat: &mut Stat,
        ctx: &CallerCtx,
    ) -> Result<()> {
        Err(Error::new(EOPNOTSUPP))
    }

    fn fstatvfs(
        &mut self,
        scheme_data: &mut Self::SchemeData,
        stat: &mut StatVfs,
        ctx: &CallerCtx,
    ) -> Result<()> {
        Err(Error::new(EOPNOTSUPP))
    }

    fn fsync(&mut self, scheme_data: &mut Self::SchemeData, ctx: &CallerCtx) -> Result<()> {
        Ok(())
    }

    fn ftruncate(
        &mut self,
        scheme_data: &mut Self::SchemeData,
        len: u64,
        ctx: &CallerCtx,
    ) -> Result<()> {
        Err(Error::new(EBADF))
    }

    fn futimens(
        &mut self,
        scheme_data: &mut Self::SchemeData,
        times: &[TimeSpec],
        ctx: &CallerCtx,
    ) -> Result<()> {
        Err(Error::new(EBADF))
    }

    fn call(
        &mut self,
        scheme_data: &mut Self::SchemeData,
        payload: &mut [u8],
        metadata: &[u64],
        ctx: &CallerCtx, // Only pid and id are correct here, uid/gid are not used
    ) -> Result<usize> {
        Err(Error::new(EOPNOTSUPP))
    }

    fn std_fs_call(
        &mut self,
        scheme_data: &mut Self::SchemeData,
        kind: StdFsCallKind,
        payload: &mut [u8],
        metadata: StdFsCallMeta,
        ctx: &CallerCtx, // Only pid and id are correct here, uid/gid are not used
    ) -> Result<usize> {
        Err(Error::new(EOPNOTSUPP))
    }

    fn getdents<'buf>(
        &mut self,
        scheme_data: &mut Self::SchemeData,
        buf: DirentBuf<&'buf mut [u8]>,
        opaque_offset: u64,
    ) -> Result<DirentBuf<&'buf mut [u8]>> {
        Err(Error::new(ENOTDIR))
    }

    fn mmap_prep(
        &mut self,
        scheme_data: &mut Self::SchemeData,
        offset: u64,
        size: usize,
        flags: MapFlags,
        ctx: &CallerCtx,
    ) -> Result<usize> {
        Err(Error::new(EOPNOTSUPP))
    }

    fn munmap(
        &mut self,
        scheme_data: &mut Self::SchemeData,
        offset: u64,
        size: usize,
        flags: MunmapFlags,
        ctx: &CallerCtx,
    ) -> Result<()> {
        Err(Error::new(EOPNOTSUPP))
    }

    fn on_close(self, scheme_data: &mut Self::SchemeData) {}

    fn on_sendfd(
        &mut self,
        scheme_data: &mut Self::SchemeData,
        sendfd_request: &SendFdRequest,
    ) -> Result<usize> {
        Err(Error::new(EOPNOTSUPP))
    }

    fn on_recvfd<'a>(
        &mut self,
        scheme_data: &mut Self::SchemeData,
        recvfd_request: &RecvFdRequest,
    ) -> Result<ResourceOpenResult<Self::ResourceEnum>> {
        Err(Error::new(EOPNOTSUPP))
    }
}

#[macro_export]
macro_rules! __resource_scheme {
    (@method enum $enum:ident {
        $($variant:ident($type:ty),)*
    } => $method:ident$(<$l:lifetime>)?($($arg:ident: $arg_ty:ty,)*) -> $ret:ty) => {
        fn $method$(<$l>)?(&mut self, id: usize, $($arg: $arg_ty,)*) -> syscall::Result<$ret> {
            resource_scheme!(@method_helper self id $enum { $($variant),* } => $method (&mut self.scheme_data, $($arg),*))
        }
    };
    (@method_helper $self:ident $id:ident $enum:ident {
        $($variant:ident),*
    } => $method:ident $args:tt) => {
        match $self.handles.get_mut($id)? {
            $($enum::$variant(arg) => arg.$method $args,)*
        }
    };
    (
        $scheme_vis:vis $scheme:ident<$($param:ident: $bound:ident),*>;
        type SchemeData = $scheme_data:ty;

        $enum_vis:vis enum $enum:ident {
            $($variant:ident($type:ty),)*
        }
    ) => {
        $scheme_vis struct $scheme<$($param: $bound),*> {
            scheme_name: String,
            scheme_data: $scheme_data,
            scheme_root: Option<usize>,
            handles: $crate::HandleMap<$enum<$($param),*>>,
        }

        $enum_vis enum $enum<$($param: $bound),*> {
            $($variant($type),)*
        }

        impl<$($param: $bound),*> std::fmt::Debug for $enum<$($param),*> {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                match self {
                    $($enum::$variant(arg) => f.debug_tuple(stringify!($variant)).field(arg).finish(),)*
                }
            }
        }

        impl<$($param: $bound),*> $scheme<$($param),*> {
            $scheme_vis fn new(scheme_name: String, scheme_data: $scheme_data, scheme_root: $enum<$($param),*>) -> Self {
                let mut handles = $crate::HandleMap::new();
                let scheme_root = Some(handles.insert(scheme_root));
                Self {
                    scheme_name,
                    scheme_data,
                    scheme_root,
                    handles,
                }
            }

            $scheme_vis fn scheme_data(&self) -> &$scheme_data {
                &self.scheme_data
            }

            $scheme_vis fn scheme_data_mut(&mut self) -> &mut $scheme_data {
                &mut self.scheme_data
            }

            $scheme_vis fn handle_ids(&self) -> std::collections::btree_map::Keys<'_, usize, $enum<$($param),*>> {
                self.handles.keys()
            }
        }

        impl<$($param: $bound),*> redox_scheme::scheme::SchemeSync for $scheme<$($param),*> {
            fn scheme_root(&mut self) -> Result<usize> {
                Ok(self
                    .scheme_root
                    .take()
                    .expect("scheme_root should be called only once"))
            }

            fn openat(
                &mut self,
                id: usize,
                path: &str,
                flags: usize,
                fcntl_flags: u32,
                ctx: &redox_scheme::CallerCtx,
            ) -> syscall::Result<redox_scheme::OpenResult> {
                match self.handles.get_mut(id)? {
                    $($enum::$variant(arg) => arg.openat(&mut self.scheme_data, path, flags, fcntl_flags, ctx),)*
                }.map(|res| res.into_scheme(&mut self.handles))
            }

            $crate::resource_scheme!(@method enum $enum { $($variant($type),)* } => unlinkat(
                path: &str,
                flags: usize,
                ctx: &redox_scheme::CallerCtx,
            ) -> ());

            fn inode(&self, id: usize) -> Result<usize> {
                match self.handles.get(id)? {
                    $($enum::$variant(arg) => arg.inode(&self.scheme_data),)*
                }
            }

            /* Resource operations */
            fn dup(
                &mut self,
                id: usize,
                buf: &[u8],
                ctx: &redox_scheme::CallerCtx,
            ) -> syscall::Result<redox_scheme::OpenResult> {
                match self.handles.get_mut(id)? {
                    $($enum::$variant(arg) => arg.dup(&mut self.scheme_data, buf, ctx),)*
                }.map(|res| res.into_scheme(&mut self.handles))
            }

            $crate::resource_scheme!(@method enum $enum { $($variant($type),)* } => read(
                buf: &mut [u8],
                offset: u64,
                fcntl_flags: u32,
                ctx: &redox_scheme::CallerCtx,
            ) -> usize);

            $crate::resource_scheme!(@method enum $enum { $($variant($type),)* } => write(
                buf: &[u8],
                offset: u64,
                fcntl_flags: u32,
                ctx: &redox_scheme::CallerCtx,
            ) -> usize);

            $crate::resource_scheme!(@method enum $enum { $($variant($type),)* } => fsize(
                ctx: &redox_scheme::CallerCtx,
            ) -> u64);

            $crate::resource_scheme!(@method enum $enum { $($variant($type),)* } => fchmod(
                new_mode: u16,
                ctx: &redox_scheme::CallerCtx,
            ) -> ());

            $crate::resource_scheme!(@method enum $enum { $($variant($type),)* } => fchown(
                new_uid: u32,
                new_gid: u32,
                ctx: &redox_scheme::CallerCtx,
            ) -> ());

            $crate::resource_scheme!(@method enum $enum { $($variant($type),)* } => fcntl(
                cmd: usize,
                arg: usize,
                ctx: &redox_scheme::CallerCtx,
            ) -> usize);

            $crate::resource_scheme!(@method enum $enum { $($variant($type),)* } => fevent(
                flags: syscall::EventFlags,
                ctx: &redox_scheme::CallerCtx,
            ) -> syscall::EventFlags);

            $crate::resource_scheme!(@method enum $enum { $($variant($type),)* } => flink(
                path: &str,
                ctx: &redox_scheme::CallerCtx,
            ) -> usize);

            fn fpath(&mut self, id: usize, buf: &mut [u8], ctx: &CallerCtx) -> Result<usize> {
                FpathWriter::with(buf, &self.scheme_name, |w| {
                    match self.handles.get_mut(id)? {
                        $($enum::$variant(arg) => arg.fpath(&mut self.scheme_data, w, ctx),)*
                    }
                })
            }

            $crate::resource_scheme!(@method enum $enum { $($variant($type),)* } => frename(
                path: &str,
                ctx: &redox_scheme::CallerCtx,
            ) -> usize);

            $crate::resource_scheme!(@method enum $enum { $($variant($type),)* } => fstat(
                stat: &mut syscall::Stat,
                ctx: &redox_scheme::CallerCtx,
            ) -> ());

            $crate::resource_scheme!(@method enum $enum { $($variant($type),)* } => fstatvfs(
                stat: &mut syscall::StatVfs,
                ctx: &redox_scheme::CallerCtx,
            ) -> ());

            $crate::resource_scheme!(@method enum $enum { $($variant($type),)* } => fsync(
                ctx: &redox_scheme::CallerCtx,
            ) -> ());

            $crate::resource_scheme!(@method enum $enum { $($variant($type),)* } => ftruncate(
                len: u64,
                ctx: &redox_scheme::CallerCtx,
            ) -> ());

            $crate::resource_scheme!(@method enum $enum { $($variant($type),)* } => futimens(
                times: &[syscall::TimeSpec],
                ctx: &redox_scheme::CallerCtx,
            ) -> ());

            $crate::resource_scheme!(@method enum $enum { $($variant($type),)* } => call(
                payload: &mut [u8],
                metadata: &[u64],
                ctx: &redox_scheme::CallerCtx, // Only pid and id are correct here, uid/gid are not used
            ) -> usize);

            $crate::resource_scheme!(@method enum $enum { $($variant($type),)* } => std_fs_call(
                kind: syscall::StdFsCallKind,
                payload: &mut [u8],
                metadata: syscall::StdFsCallMeta,
                ctx: &redox_scheme::CallerCtx, // Only pid and id are correct here, uid/gid are not used
            ) -> usize);

            $crate::resource_scheme!(@method enum $enum { $($variant($type),)* } => getdents<'buf>(
                buf: syscall::dirent::DirentBuf<&'buf mut [u8]>,
                opaque_offset: u64,
            ) -> syscall::dirent::DirentBuf<&'buf mut [u8]>);

            $crate::resource_scheme!(@method enum $enum { $($variant($type),)* } => mmap_prep(
                offset: u64,
                size: usize,
                flags: syscall::MapFlags,
                ctx: &redox_scheme::CallerCtx,
            ) -> usize);

            $crate::resource_scheme!(@method enum $enum { $($variant($type),)* } => munmap(
                offset: u64,
                size: usize,
                flags: syscall::MunmapFlags,
                ctx: &redox_scheme::CallerCtx,
            ) -> ());

            fn on_close(&mut self, id: usize) {
                match self.handles.remove(id).unwrap() {
                    $($enum::$variant(arg) => arg.on_close(&mut self.scheme_data),)*
                }
            }

            fn on_sendfd(&mut self, sendfd_request: &redox_scheme::SendFdRequest) -> Result<usize> {
                match self.handles.get_mut(sendfd_request.id())? {
                    $($enum::$variant(arg) => arg.on_sendfd(&mut self.scheme_data, sendfd_request),)*
                }
            }

            fn on_recvfd(&mut self, recvfd_request: &redox_scheme::RecvFdRequest) -> Result<redox_scheme::OpenResult> {
                match self.handles.get_mut(recvfd_request.id())? {
                    $($enum::$variant(arg) => arg.on_recvfd(&mut self.scheme_data, recvfd_request),)*
                }.map(|res| res.into_scheme(&mut self.handles))
            }
        }
    };
}
pub use __resource_scheme as resource_scheme;
