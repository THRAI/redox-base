#![feature(macro_metavar_expr)]

use std::cmp;
use std::collections::HashMap;
use std::fmt::Debug;
use std::fs::File;
use std::io::{self, Write};
use std::marker::PhantomData;
use std::ops::ControlFlow;
use std::sync::{Arc, Mutex};

use inputd::{DisplayHandle, VtEvent, VtEventKind};
use libredox::Fd;
use redox_scheme::scheme::{SchemeSync, register_scheme_inner};
use redox_scheme::{CallerCtx, Socket};
use scheme_utils::{Blocking, FpathWriter, ResourceOpenResult, ResourceSync, resource_scheme};
use syscall::schemev2::NewFdFlags;
use syscall::{EINVAL, ENOENT, Error, MapFlags, Result};

use crate::kms::connector::{KmsConnectorDriver, KmsConnectorState};
use crate::kms::objects::{
    KmsCrtc, KmsCrtcDriver, KmsCrtcState, KmsObjectId, KmsObjects, KmsPlane, KmsPlaneDriver,
    KmsPlaneState,
};

mod ioctl;
pub mod kms;

#[derive(Debug, Copy, Clone)]
pub struct Damage {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl Damage {
    fn merge(self, other: Self) -> Self {
        if self.width == 0 || self.height == 0 {
            return other;
        }

        if other.width == 0 || other.height == 0 {
            return self;
        }

        let x = cmp::min(self.x, other.x);
        let y = cmp::min(self.y, other.y);
        let x2 = cmp::max(self.x + self.width, other.x + other.width);
        let y2 = cmp::max(self.y + self.height, other.y + other.height);

        Damage {
            x,
            y,
            width: x2 - x,
            height: y2 - y,
        }
    }

    #[must_use]
    pub fn clip(mut self, width: u32, height: u32) -> Self {
        // Clip damage
        let x2 = self.x + self.width;
        self.x = cmp::min(self.x, width);
        if x2 > width {
            self.width = width - self.x;
        }

        let y2 = self.y + self.height;
        self.y = cmp::min(self.y, height);
        if y2 > height {
            self.height = height - self.y;
        }
        self
    }
}

pub trait GraphicsAdapter: Sized + Debug {
    type Connector: KmsConnectorDriver;
    type Crtc: KmsCrtcDriver;
    type Plane: KmsPlaneDriver;

    type Buffer: Buffer;
    type Framebuffer: Framebuffer;

    fn name(&self) -> &'static [u8];
    fn desc(&self) -> &'static [u8];

    fn init(&mut self, objects: &mut KmsObjects<Self>);

    fn get_unique(&self) -> String;
    fn get_cap(&self, cap: u32) -> Result<u64>;
    fn set_client_cap(&self, cap: u32, value: u64) -> Result<()>;

    fn probe_connector(&mut self, objects: &mut KmsObjects<Self>, id: KmsObjectId);

    fn create_dumb_buffer(&mut self, width: u32, height: u32) -> (Self::Buffer, u32);
    fn map_dumb_buffer(&mut self, buffer: &Self::Buffer) -> *mut u8;

    fn create_framebuffer(&mut self, buffer: &Self::Buffer) -> Self::Framebuffer;

    fn set_crtc(
        &mut self,
        objects: &KmsObjects<Self>,
        crtc: &Mutex<KmsCrtc<Self>>,
        new_state: KmsCrtcState<Self>,
    ) -> syscall::Result<()>;

    fn set_plane(
        &mut self,
        objects: &KmsObjects<Self>,
        plane: &Mutex<KmsPlane<Self>>,
        new_plane_state: KmsPlaneState<Self>,
        damage: Damage,
    ) -> syscall::Result<()>;
}

pub trait Buffer: Debug {
    fn size(&self) -> usize;
}

pub trait Framebuffer: Debug {}

impl Framebuffer for () {}

pub struct GraphicsScheme<T: GraphicsAdapter> {
    inner: GraphicsSchemeImpl<T>,
    _inputd_handle: DisplayHandle,
    handler: Blocking<Box<Socket>>,
}

impl<T: GraphicsAdapter> GraphicsScheme<T> {
    pub fn new(mut adapter: T, scheme_name: String, early: bool) -> Self {
        assert!(scheme_name.starts_with("display"));
        let socket = Socket::nonblock().expect("failed to create graphics scheme");

        let disable_graphical_debug = Some(
            File::open("/scheme/debug/disable-graphical-debug")
                .expect("vesad: Failed to open /scheme/debug/disable-graphical-debug"),
        );

        let mut objects = KmsObjects::new();
        adapter.init(&mut objects);
        for connector_id in objects.connector_ids().to_vec() {
            adapter.probe_connector(&mut objects, connector_id)
        }

        let mut inner = GraphicsSchemeImpl::new(
            scheme_name.clone(),
            GraphicsSchemeData {
                adapter,
                disable_graphical_debug,
                objects,
                active_vt: 0,
                vts: HashMap::new(),
            },
            GraphicsResource::SchemeRoot(SchemeRoot::<T>(PhantomData)),
        );

        let cap_id = inner.scheme_root().expect("failed to get this scheme root");
        register_scheme_inner(&socket, &scheme_name, cap_id)
            .expect("failed to register graphics scheme root");

        let control_id = inner
            .handles
            .insert(GraphicsResource::Control(Control::<T>(PhantomData)));
        let control_cap = Fd::new(socket.create_this_scheme_fd(0, control_id, 0, 0).unwrap());

        let display_handle = DisplayHandle::new(&scheme_name, control_cap, early).unwrap();

        Self {
            inner,
            _inputd_handle: display_handle,
            handler: Blocking::new(Box::new(socket), 16),
        }
    }

    pub fn event_handle(&self) -> &Fd {
        self.handler.socket().inner()
    }

    pub fn adapter(&self) -> &T {
        &self.inner.scheme_data().adapter
    }

    pub fn adapter_mut(&mut self) -> &mut T {
        &mut self.inner.scheme_data_mut().adapter
    }

    pub fn kms_objects(&self) -> &KmsObjects<T> {
        &self.inner.scheme_data().objects
    }

    pub fn kms_objects_mut(&mut self) -> &mut KmsObjects<T> {
        &mut self.inner.scheme_data_mut().objects
    }

    pub fn adapter_and_kms_objects_mut(&mut self) -> (&mut T, &mut KmsObjects<T>) {
        let inner = self.inner.scheme_data_mut();
        (&mut inner.adapter, &mut inner.objects)
    }

    pub fn notify_displays_changed(&mut self) {
        // FIXME notify clients
    }

    /// Process new scheme requests.
    ///
    /// This needs to be called each time there is a new event on the scheme
    /// file.
    pub fn tick(&mut self) -> io::Result<()> {
        loop {
            match self
                .handler
                .process_requests_nonblocking(&mut self.inner)
                .expect("driver-graphics: failed to process requests")
            {
                ControlFlow::Continue(()) => {}
                ControlFlow::Break(()) => break,
            }
        }

        Ok(())
    }
}

resource_scheme! {
    GraphicsSchemeImpl<T: GraphicsAdapter>;
    type SchemeData = GraphicsSchemeData<T>;

    enum GraphicsResource {
        SchemeRoot(SchemeRoot<T>),
        Control(Control<T>),
        DrmHandle(DrmHandle<T>),
    }
}

struct GraphicsSchemeData<T: GraphicsAdapter> {
    adapter: T,

    disable_graphical_debug: Option<File>,
    objects: KmsObjects<T>,

    active_vt: usize,
    vts: HashMap<usize, VtState<T>>,
}

struct VtState<T: GraphicsAdapter> {
    connector_state: Vec<KmsConnectorState<T>>,
    crtc_state: Vec<KmsCrtcState<T>>,
    plane_state: Vec<KmsPlaneState<T>>,
}

impl<T: GraphicsAdapter> VtState<T> {
    fn fb_has_any_use(vts: &HashMap<usize, Self>, fb_id: KmsObjectId) -> bool {
        let mut has_any_use = false;
        for vt_data in vts.values() {
            for plane_state in vt_data.plane_state.iter() {
                if plane_state.fb_id == Some(fb_id) {
                    has_any_use = true;
                    break;
                }
            }
        }
        has_any_use
    }
}

impl<T: GraphicsAdapter> GraphicsSchemeData<T> {
    fn get_or_create_vt<'a>(
        objects: &KmsObjects<T>,
        vts: &'a mut HashMap<usize, VtState<T>>,
        vt: usize,
    ) -> &'a mut VtState<T> {
        vts.entry(vt).or_insert_with(|| VtState {
            connector_state: objects
                .connectors()
                .map(|connector| connector.lock().unwrap().state.clone())
                .collect(),
            crtc_state: objects
                .crtcs()
                .map(|crtc| crtc.lock().unwrap().state.clone())
                .collect(),
            plane_state: objects
                .planes()
                .map(|plane| plane.lock().unwrap().state.clone())
                .collect(),
        })
    }

    fn activate_vt(&mut self, vt: usize) {
        log::info!("activate {}", vt);

        // Disable the kernel graphical debug writing once switching vt's for the
        // first time. This way the kernel graphical debug remains enabled if the
        // userspace logging infrastructure doesn't start up because for example a
        // kernel panic happened prior to it starting up or logd crashed.
        if let Some(mut disable_graphical_debug) = self.disable_graphical_debug.take() {
            let _ = disable_graphical_debug.write(&[1]);
        }

        self.active_vt = vt;

        let vt_state = GraphicsSchemeData::get_or_create_vt(&self.objects, &mut self.vts, vt);

        for (connector_idx, connector_state) in vt_state.connector_state.iter().enumerate() {
            let connector_id = self.objects.connector_ids()[connector_idx];
            let mut connector = self
                .objects
                .get_connector(connector_id)
                .unwrap()
                .lock()
                .unwrap();
            connector.state = connector_state.clone();
        }

        for (crtc_idx, crtc_state) in vt_state.crtc_state.iter().enumerate() {
            let crtc_id = self.objects.crtc_ids()[crtc_idx];
            let crtc = self.objects.get_crtc(crtc_id).unwrap();
            let connector_id = self.objects.connector_ids()[crtc_idx];

            self.adapter
                .set_crtc(&self.objects, crtc, crtc_state.clone())
                .unwrap();

            self.objects
                .get_connector(connector_id)
                .unwrap()
                .lock()
                .unwrap()
                .state
                .crtc_id = crtc_id;
        }

        for (plane_idx, plane_state) in vt_state.plane_state.iter().enumerate() {
            let plane_id = self.objects.plane_ids()[plane_idx];
            let plane = self.objects.get_plane(plane_id).unwrap();

            let fb = plane_state.fb_id.map(|fb_id| {
                self.objects
                    .get_framebuffer_maybe_closed(fb_id)
                    .expect("removed framebuffers should be unset")
            });

            self.adapter
                .set_plane(
                    &self.objects,
                    plane,
                    plane_state.clone(),
                    Damage {
                        x: 0,
                        y: 0,
                        width: fb.map_or(0, |fb| fb.width),
                        height: fb.map_or(0, |fb| fb.height),
                    },
                )
                .unwrap();
        }
    }
}

struct SchemeRoot<T>(PhantomData<T>);

impl<T> std::fmt::Debug for SchemeRoot<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("SchemeRoot").finish()
    }
}

impl<T: GraphicsAdapter> ResourceSync for SchemeRoot<T> {
    type SchemeData = GraphicsSchemeData<T>;
    type ResourceEnum = GraphicsResource<T>;

    fn openat(
        &mut self,
        scheme_data: &mut Self::SchemeData,
        path: &str,
        _flags: usize,
        _fcntl_flags: u32,
        _ctx: &CallerCtx,
    ) -> Result<ResourceOpenResult<Self::ResourceEnum>> {
        if path.is_empty() {
            return Err(Error::new(EINVAL));
        }

        let handle = if path.starts_with("v") {
            if !path.starts_with("v2/") {
                return Err(Error::new(ENOENT));
            }
            let vt = path["v2/".len()..]
                .parse::<usize>()
                .map_err(|_| Error::new(EINVAL))?;

            // Ensure the VT exists such that the rest of the methods can freely access it.
            GraphicsSchemeData::get_or_create_vt(&scheme_data.objects, &mut scheme_data.vts, vt);

            GraphicsResource::DrmHandle(DrmHandle {
                vt,
                unique: None,
                next_id: 0,
                buffers: HashMap::new(),
            })
        } else {
            return Err(Error::new(EINVAL));
        };
        Ok(ResourceOpenResult::ThisScheme {
            data: handle,
            flags: NewFdFlags::empty(),
        })
    }
}

struct Control<T>(PhantomData<T>);

impl<T> std::fmt::Debug for Control<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Control").finish()
    }
}

impl<T: GraphicsAdapter> ResourceSync for Control<T> {
    type SchemeData = GraphicsSchemeData<T>;
    type ResourceEnum = GraphicsResource<T>;

    fn call(
        &mut self,
        scheme_data: &mut Self::SchemeData,
        payload: &mut [u8],
        _metadata: &[u64],
        _ctx: &CallerCtx,
    ) -> Result<usize> {
        let vt_event = unsafe { VtEvent::from_bytes(payload) }.ok_or_else(|| Error::new(EINVAL))?;
        match vt_event.kind {
            VtEventKind::Activate => scheme_data.activate_vt(vt_event.vt),
        }
        Ok(0)
    }
}

#[derive(Debug)]
struct DrmHandle<T: GraphicsAdapter> {
    vt: usize,
    unique: Option<String>,
    next_id: u32,
    buffers: HashMap<u32, Arc<T::Buffer>>,
}

impl<T: GraphicsAdapter> ResourceSync for DrmHandle<T> {
    type SchemeData = GraphicsSchemeData<T>;
    type ResourceEnum = GraphicsResource<T>;

    fn fstat(
        &mut self,
        _scheme_data: &mut Self::SchemeData,
        stat: &mut syscall::Stat,
    ) -> Result<()> {
        stat.st_dev = 226 /*DRM_MAJOR*/ << 8;
        Ok(())
    }

    fn fpath(
        &mut self,
        _scheme_data: &mut Self::SchemeData,
        w: &mut FpathWriter,
    ) -> syscall::Result<()> {
        write!(w, "v2/{}", self.vt).unwrap();
        Ok(())
    }

    fn call(
        &mut self,
        scheme_data: &mut Self::SchemeData,
        payload: &mut [u8],
        metadata: &[u64],
        _ctx: &CallerCtx,
    ) -> Result<usize> {
        ioctl::call_ioctl(
            &mut scheme_data.adapter,
            &mut scheme_data.objects,
            scheme_data.active_vt,
            &mut scheme_data.vts,
            self,
            metadata[0],
            payload,
        )
    }

    fn mmap_prep(
        &mut self,
        scheme_data: &mut Self::SchemeData,
        offset: u64,
        _size: usize,
        _flags: MapFlags,
    ) -> syscall::Result<usize> {
        // log::trace!("KSMSG MMAP {} {:?} {} {}", id, _flags, _offset, _size);
        let framebuffer = self
            .buffers
            .get(&((offset as usize / MAP_FAKE_OFFSET_MULTIPLIER) as u32))
            .ok_or(Error::new(EINVAL))
            .unwrap();
        let offset = offset & (MAP_FAKE_OFFSET_MULTIPLIER as u64 - 1);
        let ptr = T::map_dumb_buffer(&mut scheme_data.adapter, framebuffer);
        Ok(unsafe { ptr.add(offset as usize) } as usize)
    }
}

const MAP_FAKE_OFFSET_MULTIPLIER: usize = 0x10_000_000;
