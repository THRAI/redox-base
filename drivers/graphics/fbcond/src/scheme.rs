use std::collections::BTreeMap;
use std::fs;
use std::os::fd::AsRawFd;

use console_draw::ConsoleFont;
use event::{EventQueue, UserData};
use redox_scheme::CallerCtx;
use scheme_utils::{resource_scheme, FpathWriter, ResourceOpenResult, ResourceSync};
use serde::Deserialize;
use syscall::schemev2::NewFdFlags;
use syscall::{Error, EventFlags, Result, EAGAIN, EBADF, ENOENT, O_NONBLOCK};

use crate::display::Display;
use crate::text::TextScreen;

#[derive(Clone, Copy, Eq, Ord, PartialEq, PartialOrd, Debug)]
pub struct VtIndex(usize);

impl VtIndex {
    pub const SCHEMA_SENTINEL: VtIndex = VtIndex(usize::MAX);
}

impl UserData for VtIndex {
    fn into_user_data(self) -> usize {
        self.0
    }

    fn from_user_data(user_data: usize) -> Self {
        VtIndex(user_data)
    }
}

resource_scheme! {
    pub(crate) FbconScheme<>;
    type SchemeData = FbconSchemeData;

    pub(crate) enum FbconResource {
        SchemeRoot(SchemeRoot),
        Vt(FdHandle),
    }
}

pub(crate) struct FbconSchemeData {
    pub(crate) vts: BTreeMap<VtIndex, TextScreen>,
}

impl FbconSchemeData {
    pub(crate) fn new(vt_ids: &[usize], event_queue: &mut EventQueue<VtIndex>) -> FbconSchemeData {
        let mut vts = BTreeMap::new();

        let config = match fs::read_to_string("/etc/fbcond.toml") {
            Ok(config) => config,
            Err(err) => {
                log::debug!("Failed to read config: {err}");
                String::new()
            }
        };
        let config = match toml::from_str::<FbconConfig>(&config) {
            Ok(config) => config,
            Err(err) => {
                log::debug!("Failed to parse config: {err}");
                log::debug!("Using fallback font");
                FbconConfig {
                    font: FontConfig {
                        path: String::new(),
                    },
                }
            }
        };

        let font = if !&config.font.path.is_empty() {
            match fs::read(&config.font.path) {
                Ok(contents) => Some(ConsoleFont::from_psf(&contents)),
                Err(err) => {
                    log::debug!("Failed to read font {}: {err}", config.font.path);
                    log::debug!("Using fallback font");
                    None
                }
            }
        } else {
            None
        };

        for &vt_i in vt_ids {
            let display = Display::open_new_vt().expect("Failed to open display for vt");
            event_queue
                .subscribe(
                    display.input_handle.event_handle().as_raw_fd() as usize,
                    VtIndex(vt_i),
                    event::EventFlags::READ,
                )
                .expect("Failed to subscribe to input events for vt");

            vts.insert(VtIndex(vt_i), TextScreen::new(display, font.clone()));
        }

        FbconSchemeData { vts }
    }
}

#[derive(Debug)]
pub(crate) struct SchemeRoot;

impl ResourceSync for SchemeRoot {
    type ResourceEnum = FbconResource;
    type SchemeData = FbconSchemeData;

    fn openat<'a>(
        &mut self,
        scheme_data: &mut Self::SchemeData,
        path: &str,
        flags: usize,
        fcntl_flags: u32,
        _ctx: &CallerCtx,
    ) -> Result<ResourceOpenResult<Self::ResourceEnum>> {
        let vt_i = VtIndex(path.parse::<usize>().map_err(|_| Error::new(ENOENT))?);
        if scheme_data.vts.contains_key(&vt_i) {
            Ok(ResourceOpenResult::ThisScheme {
                data: FbconResource::Vt(FdHandle {
                    vt_i,
                    flags: flags | fcntl_flags as usize,
                    events: EventFlags::empty(),
                    notified_read: false,
                }),
                flags: NewFdFlags::empty(),
            })
        } else {
            Err(Error::new(ENOENT))
        }
    }
}

#[derive(Debug)]
pub(crate) struct FdHandle {
    pub(crate) vt_i: VtIndex,
    flags: usize,
    pub(crate) events: EventFlags,
    pub(crate) notified_read: bool,
}

impl ResourceSync for FdHandle {
    type ResourceEnum = FbconResource;
    type SchemeData = FbconSchemeData;

    fn fevent(
        &mut self,
        _scheme_data: &mut Self::SchemeData,
        flags: syscall::EventFlags,
    ) -> Result<syscall::EventFlags> {
        self.notified_read = false;
        self.events = flags;

        Ok(syscall::EventFlags::empty())
    }

    fn fpath(&mut self, _scheme_data: &mut Self::SchemeData, w: &mut FpathWriter) -> Result<()> {
        write!(w, "{}", self.vt_i.0).unwrap();
        Ok(())
    }

    fn fsync(&mut self, _scheme_data: &mut Self::SchemeData) -> Result<()> {
        Ok(())
    }

    fn fcntl(
        &mut self,
        _scheme_data: &mut Self::SchemeData,
        _cmd: usize,
        _arg: usize,
    ) -> Result<usize> {
        Ok(0)
    }

    fn read(
        &mut self,
        scheme_data: &mut Self::SchemeData,
        buf: &mut [u8],
        _offset: u64,
        _fcntl_flags: u32,
    ) -> Result<usize> {
        if let Some(screen) = scheme_data.vts.get_mut(&self.vt_i) {
            if !screen.can_read() {
                if self.flags & O_NONBLOCK != 0 {
                    Err(Error::new(EAGAIN))
                } else {
                    Err(Error::new(EAGAIN))
                }
            } else {
                screen.read(buf)
            }
        } else {
            Err(Error::new(EBADF))
        }
    }

    fn write(
        &mut self,
        scheme_data: &mut Self::SchemeData,
        buf: &[u8],
        _offset: u64,
        _fcntl_flags: u32,
    ) -> Result<usize> {
        let vt_i = self.vt_i;

        if let Some(console) = scheme_data.vts.get_mut(&vt_i) {
            console.write(buf)
        } else {
            Err(Error::new(EBADF))
        }
    }
}

#[derive(Deserialize)]
struct FbconConfig {
    font: FontConfig,
}
#[derive(Deserialize)]
struct FontConfig {
    path: String,
}
