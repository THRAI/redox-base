use std::fs;

use console_draw::ConsoleFont;
use inputd::ConsumerHandle;
use redox_scheme::CallerCtx;
use scheme_utils::{resource_scheme, FpathWriter, ResourceOpenResult, ResourceSync};
use serde::Deserialize;
use syscall::schemev2::NewFdFlags;
use syscall::{Error, EventFlags, Result, EAGAIN, ENOENT, O_NONBLOCK};

use crate::text::TextScreen;

resource_scheme! {
    pub(crate) FbconScheme<>;
    type SchemeData = FbconSchemeData;

    pub(crate) enum FbconResource {
        SchemeRoot(SchemeRoot),
        Vt(FdHandle),
    }
}

pub(crate) struct FbconSchemeData {
    pub(crate) console: TextScreen,
}

impl FbconSchemeData {
    pub(crate) fn new(input_handle: ConsumerHandle) -> FbconSchemeData {
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

        FbconSchemeData {
            console: TextScreen::new(input_handle, font.clone()),
        }
    }
}

#[derive(Debug)]
pub(crate) struct SchemeRoot;

impl ResourceSync for SchemeRoot {
    type ResourceEnum = FbconResource;
    type SchemeData = FbconSchemeData;

    fn openat<'a>(
        &mut self,
        _scheme_data: &mut Self::SchemeData,
        path: &str,
        flags: usize,
        fcntl_flags: u32,
        _ctx: &CallerCtx,
    ) -> Result<ResourceOpenResult<Self::ResourceEnum>> {
        if !path.is_empty() {
            return Err(Error::new(ENOENT));
        }
        Ok(ResourceOpenResult::ThisScheme {
            data: FbconResource::Vt(FdHandle {
                flags: flags | fcntl_flags as usize,
                events: EventFlags::empty(),
                notified_read: false,
            }),
            flags: NewFdFlags::empty(),
        })
    }
}

#[derive(Debug)]
pub(crate) struct FdHandle {
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

    fn fpath(&mut self, _scheme_data: &mut Self::SchemeData, _w: &mut FpathWriter) -> Result<()> {
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
        if !scheme_data.console.can_read() {
            if self.flags & O_NONBLOCK != 0 {
                Err(Error::new(EAGAIN))
            } else {
                Err(Error::new(EAGAIN))
            }
        } else {
            scheme_data.console.read(buf)
        }
    }

    fn write(
        &mut self,
        scheme_data: &mut Self::SchemeData,
        buf: &[u8],
        _offset: u64,
        _fcntl_flags: u32,
    ) -> Result<usize> {
        scheme_data.console.write(buf)
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
