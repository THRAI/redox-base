use std::marker::PhantomData;
use std::{cmp, io};

use libredox::flag::O_NONBLOCK;
use libredox::Fd;
use redox_scheme::{CallerCtx, Response, SignalBehavior, Socket};
use scheme_utils::{
    resource_scheme, FpathWriter, ReadinessBased, ResourceOpenResult, ResourceSync,
};
use syscall::{
    schemev2::NewFdFlags, Error, EventFlags, Result, Stat, EACCES, EAGAIN, EINVAL, EWOULDBLOCK,
    MODE_FILE,
};

pub trait NetworkAdapter {
    /// The [MAC address](https://en.wikipedia.org/wiki/MAC_address) of this
    /// network adapter.
    fn mac_address(&mut self) -> [u8; 6];

    /// The amount of network packets that can be read without blocking.
    fn available_for_read(&mut self) -> usize;

    /// Attempt to read a network packet without blocking.
    ///
    /// Returns `Ok(None)` when there is no pending network packet.
    fn read_packet(&mut self, buf: &mut [u8]) -> Result<Option<usize>>;

    /// Write a single network packet.
    // FIXME support back pressure on writes by returning EWOULDBLOCK or not
    // returning from the write syscall until there is room.
    fn write_packet(&mut self, buf: &[u8]) -> Result<usize>;
}

pub struct NetworkScheme<T: NetworkAdapter> {
    scheme: NetworkSchemeImpl<T>,
    handler: ReadinessBased<Box<Socket>>,
}

fn post_fevent(socket: &Socket, id: usize, flags: usize) -> Result<()> {
    let fevent_response = Response::post_fevent(id, flags);
    match socket.write_response(fevent_response, SignalBehavior::Restart) {
        Ok(true) => Ok(()),                            // Write response success
        Ok(false) => Err(Error::new(syscall::EAGAIN)), // Write response failed, retry.
        Err(err) => Err(err),                          // Error writing response
    }
}

impl<T: NetworkAdapter> NetworkScheme<T> {
    pub fn new(
        adapter_fn: impl FnOnce() -> T,
        daemon: daemon::Daemon,
        scheme_name: String,
    ) -> Self {
        assert!(scheme_name.starts_with("network"));
        let socket = Socket::nonblock().expect("failed to create network scheme");
        let adapter = adapter_fn();
        let mut scheme = NetworkSchemeImpl::new(
            scheme_name.clone(),
            NetworkSchemeData::new(adapter),
            NetworkResource::SchemeRoot(SchemeRoot::<T>(PhantomData)),
        );
        redox_scheme::scheme::register_sync_scheme(&socket, &scheme_name, &mut scheme)
            .expect("failed to regitster network scheme");
        daemon.ready();
        Self {
            scheme,
            handler: ReadinessBased::new(Box::new(socket), 16),
        }
    }

    pub fn event_handle(&self) -> &Fd {
        self.handler.socket().inner()
    }

    pub fn adapter(&self) -> &T {
        &self.scheme.scheme_data().adapter
    }

    pub fn adapter_mut(&mut self) -> &mut T {
        &mut self.scheme.scheme_data_mut().adapter
    }

    /// Process pending and new requests.
    ///
    /// This needs to be called each time there is a new event on the scheme
    /// file and each time a new network packet has been received by the
    /// driver.
    // FIXME maybe split into one method for events on the scheme fd and one
    // to call when an irq is received to indicate that blocked requests can
    // be processed.
    pub fn tick(&mut self) -> io::Result<()> {
        self.handler
            .read_and_process_requests(&mut self.scheme)
            .expect("driver-network: failed to read from socket");
        self.handler
            .poll_all_requests(&mut self.scheme)
            .expect("driver-network: failed to poll requests");
        self.handler
            .write_responses()
            .expect("driver-network: failed to write to socket");

        // Notify readers about incoming events
        let available_for_read = self.scheme.scheme_data_mut().adapter.available_for_read();
        if available_for_read > 0 {
            for &handle_id in self.scheme.handle_ids() {
                post_fevent(
                    &self.handler.socket(),
                    handle_id,
                    syscall::flag::EVENT_READ.bits(),
                )?;
            }
            return Ok(());
        }

        Ok(())
    }
}

resource_scheme! {
    NetworkSchemeImpl<T: NetworkAdapter>;
    type SchemeData = NetworkSchemeData<T>;

    enum NetworkResource {
        SchemeRoot(SchemeRoot<T>),
        Data(Data<T>),
        Mac(Mac<T>),
    }
}

struct NetworkSchemeData<T: NetworkAdapter> {
    adapter: T,
}

impl<T: NetworkAdapter> NetworkSchemeData<T> {
    pub fn new(adapter: T) -> Self {
        Self { adapter }
    }
}

struct SchemeRoot<T>(PhantomData<T>);

impl<T> std::fmt::Debug for SchemeRoot<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("SchemeRoot").finish()
    }
}

impl<T: NetworkAdapter> ResourceSync for SchemeRoot<T> {
    type SchemeData = NetworkSchemeData<T>;
    type ResourceEnum = NetworkResource<T>;

    fn openat<'a>(
        &mut self,
        _scheme_data: &mut Self::SchemeData,
        path: &str,
        _flags: usize,
        _fcntl_flags: u32,
        ctx: &CallerCtx,
    ) -> Result<ResourceOpenResult<Self::ResourceEnum>> {
        if ctx.uid != 0 {
            return Err(Error::new(EACCES));
        }

        let (data, flags) = match path {
            "" => (
                NetworkResource::Data(Data(PhantomData)),
                NewFdFlags::empty(),
            ),
            "mac" => (
                NetworkResource::Mac(Mac(PhantomData)),
                NewFdFlags::POSITIONED,
            ),
            _ => return Err(Error::new(EINVAL)),
        };

        Ok(ResourceOpenResult::ThisScheme { data, flags })
    }

    fn fpath(&mut self, _scheme_data: &mut Self::SchemeData, _w: &mut FpathWriter) -> Result<()> {
        Ok(())
    }
}

struct Data<T>(PhantomData<T>);

impl<T> std::fmt::Debug for Data<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Data").finish()
    }
}

impl<T: NetworkAdapter> ResourceSync for Data<T> {
    type SchemeData = NetworkSchemeData<T>;
    type ResourceEnum = NetworkResource<T>;

    fn read(
        &mut self,
        scheme_data: &mut Self::SchemeData,
        buf: &mut [u8],
        _offset: u64,
        fcntl_flags: u32,
    ) -> Result<usize> {
        match scheme_data.adapter.read_packet(buf)? {
            Some(count) => Ok(count),
            None => {
                if fcntl_flags & O_NONBLOCK as u32 != 0 {
                    Err(Error::new(EAGAIN))
                } else {
                    Err(Error::new(EWOULDBLOCK))
                }
            }
        }
    }

    fn write(
        &mut self,
        scheme_data: &mut Self::SchemeData,
        buf: &[u8],
        _offset: u64,
        _fcntl_flags: u32,
    ) -> Result<usize> {
        Ok(scheme_data.adapter.write_packet(buf)?)
    }

    fn fevent(
        &mut self,
        _scheme_data: &mut Self::SchemeData,
        _flags: EventFlags,
    ) -> Result<EventFlags> {
        Ok(EventFlags::empty())
    }

    fn fpath(&mut self, _scheme_data: &mut Self::SchemeData, _w: &mut FpathWriter) -> Result<()> {
        Ok(())
    }

    fn fstat(&mut self, _scheme_data: &mut Self::SchemeData, stat: &mut Stat) -> Result<()> {
        stat.st_mode = MODE_FILE | 0o700;
        Ok(())
    }
}

struct Mac<T>(PhantomData<T>);

impl<T> std::fmt::Debug for Mac<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Mac").finish()
    }
}

impl<T: NetworkAdapter> ResourceSync for Mac<T> {
    type SchemeData = NetworkSchemeData<T>;
    type ResourceEnum = NetworkResource<T>;

    fn read(
        &mut self,
        scheme_data: &mut Self::SchemeData,
        buf: &mut [u8],
        offset: u64,
        _fcntl_flags: u32,
    ) -> Result<usize> {
        let data = &scheme_data.adapter.mac_address()[offset as usize..];
        let i = cmp::min(buf.len(), data.len());
        buf[..i].copy_from_slice(&data[..i]);
        Ok(i)
    }

    fn write(
        &mut self,
        _scheme_data: &mut Self::SchemeData,
        _buf: &[u8],
        _offset: u64,
        _fcntl_flags: u32,
    ) -> Result<usize> {
        Err(Error::new(EINVAL))
    }

    fn fpath(&mut self, _scheme_data: &mut Self::SchemeData, w: &mut FpathWriter) -> Result<()> {
        w.push_str("mac");
        Ok(())
    }

    fn fstat(&mut self, _scheme_data: &mut Self::SchemeData, stat: &mut Stat) -> Result<()> {
        stat.st_mode = MODE_FILE | 0o400;
        stat.st_size = 6;
        Ok(())
    }
}
