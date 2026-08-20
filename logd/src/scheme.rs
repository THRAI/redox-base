use std::collections::{BTreeMap, VecDeque};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::mem;
use std::os::fd::{FromRawFd, RawFd};
use std::rc::Rc;
use std::sync::mpsc::{self, Sender};

use redox_scheme::{CallerCtx, SendFdRequest, Socket};
use scheme_utils::{resource_scheme, FpathWriter, ResourceOpenResult, ResourceSync};
use syscall::error::*;
use syscall::schemev2::NewFdFlags;

resource_scheme! {
   pub(crate) LogScheme<>;
   type SchemeData = LogSchemeData;

   pub(crate) enum LogResource {
       SchemeRoot(SchemeRoot),
       AddSink(AddSink),
       Log(Log),
   }
}

pub struct LogSchemeData {
    socket: Rc<Socket>,
    kernel_debug: File,
    output_tx: Sender<OutputCmd>,
}

enum OutputCmd {
    Log(Vec<u8>),
    AddSink(usize),
}

impl LogSchemeData {
    pub fn new(socket: Rc<Socket>) -> Self {
        let kernel_debug = OpenOptions::new()
            .write(true)
            .open("/scheme/debug")
            .unwrap();

        let mut kernel_sys_log = std::fs::File::open("/scheme/sys/log").unwrap();

        let (output_tx, output_rx) = mpsc::channel::<OutputCmd>();

        std::thread::spawn(move || {
            let mut files: Vec<File> = vec![];
            let mut logs = VecDeque::new();
            for cmd in output_rx {
                match cmd {
                    OutputCmd::Log(line) => {
                        for file in &mut files {
                            let _ = file.write(&line);
                            let _ = file.flush();
                        }
                        logs.push_back(line);
                        // Keep a limited amount of logs for backfilling to bound memory usage
                        while logs.len() > 1000 {
                            logs.pop_front();
                        }
                    }
                    OutputCmd::AddSink(log_fd) => {
                        let mut file = unsafe { File::from_raw_fd(log_fd as RawFd) };
                        for line in &logs {
                            let _ = file.write(line);
                            let _ = file.flush();
                        }

                        files.push(file)
                    }
                }
            }
        });

        let output_tx2 = output_tx.clone();
        std::thread::spawn(move || {
            let mut handle_buf = vec![];
            let mut buf = [0; 4096];
            buf[.."kernel: ".len()].copy_from_slice(b"kernel: ");
            loop {
                let n = kernel_sys_log.read(&mut buf["kernel: ".len()..]).unwrap();
                if n == 0 {
                    // FIXME currently possible as /scheme/log/kernel presents a snapshot of the log queue
                    break;
                }
                Self::write_logs(&output_tx2, &mut handle_buf, "kernel", &buf, None);
            }
        });

        LogSchemeData {
            socket,
            kernel_debug,
            output_tx,
        }
    }

    fn write_logs(
        output_tx: &Sender<OutputCmd>,
        handle_buf: &mut Vec<u8>,
        context: &str,
        buf: &[u8],
        mut kernel_debug: Option<&mut File>,
    ) {
        let mut i = 0;
        while i < buf.len() {
            let b = buf[i];

            if handle_buf.is_empty() && !context.is_empty() {
                handle_buf.extend_from_slice(context.as_bytes());
                handle_buf.extend_from_slice(b": ");
            }

            handle_buf.push(b);

            if b == b'\n' {
                if let Some(kernel_debug) = kernel_debug.as_mut() {
                    // Writing to the kernel debug log never blocks
                    let _ = kernel_debug.write(handle_buf);
                    let _ = kernel_debug.flush();
                }

                output_tx
                    .send(OutputCmd::Log(mem::take(handle_buf)))
                    .unwrap();
            }

            i += 1;
        }
    }
}

#[derive(Debug)]
pub(crate) struct SchemeRoot;

impl ResourceSync for SchemeRoot {
    type ResourceEnum = LogResource;
    type SchemeData = LogSchemeData;

    fn openat<'a>(
        &mut self,
        _scheme_data: &mut Self::SchemeData,
        path: &str,
        _flags: usize,
        _fcntl_flags: u32,
        _ctx: &CallerCtx,
    ) -> Result<ResourceOpenResult<Self::ResourceEnum>> {
        let data = if path == "add_sink" {
            LogResource::AddSink(AddSink)
        } else {
            LogResource::Log(Log {
                context: path.to_string().into_boxed_str(),
                bufs: BTreeMap::new(),
            })
        };

        Ok(ResourceOpenResult::ThisScheme {
            data,
            flags: NewFdFlags::empty(),
        })
    }
}

#[derive(Debug)]
struct AddSink;

impl ResourceSync for AddSink {
    type ResourceEnum = LogResource;
    type SchemeData = LogSchemeData;

    fn on_sendfd(
        &mut self,
        scheme_data: &mut Self::SchemeData,
        sendfd_request: &SendFdRequest,
    ) -> Result<usize> {
        let mut new_fd = usize::MAX;
        if let Err(e) = sendfd_request.obtain_fd(
            &scheme_data.socket,
            syscall::FobtainFdFlags::CLOEXEC,
            std::slice::from_mut(&mut new_fd),
        ) {
            return Err(e);
        }
        scheme_data
            .output_tx
            .send(OutputCmd::AddSink(new_fd))
            .unwrap();

        Ok(1)
    }

    fn fpath(&mut self, _scheme_data: &mut Self::SchemeData, w: &mut FpathWriter) -> Result<()> {
        w.push_str("add_sink");
        Ok(())
    }
}

#[derive(Debug)]
struct Log {
    context: Box<str>,
    bufs: BTreeMap<usize, Vec<u8>>,
}

impl ResourceSync for Log {
    type ResourceEnum = LogResource;
    type SchemeData = LogSchemeData;

    fn read(
        &mut self,
        _scheme_data: &mut Self::SchemeData,
        _buf: &mut [u8],
        _offset: u64,
        _fcntl_flags: u32,
    ) -> Result<usize> {
        Ok(0)
    }

    fn write_with_ctx(
        &mut self,
        scheme_data: &mut Self::SchemeData,
        buf: &[u8],
        _offset: u64,
        _fcntl_flags: u32,
        ctx_do_not_use: &CallerCtx,
    ) -> Result<usize> {
        // FIXME remove CallerCtx usage
        let handle_buf = self
            .bufs
            .entry(ctx_do_not_use.pid)
            .or_insert_with(|| Vec::new());

        LogSchemeData::write_logs(
            &scheme_data.output_tx,
            handle_buf,
            &self.context,
            buf,
            Some(&mut scheme_data.kernel_debug),
        );

        Ok(buf.len())
    }

    fn fcntl(
        &mut self,
        _scheme_data: &mut Self::SchemeData,
        _cmd: usize,
        _arg: usize,
    ) -> Result<usize> {
        Ok(0)
    }

    fn fpath(&mut self, _scheme_data: &mut Self::SchemeData, w: &mut FpathWriter) -> Result<()> {
        w.push_str(&self.context);
        Ok(())
    }

    fn fsync(&mut self, _scheme_data: &mut Self::SchemeData) -> Result<()> {
        //TODO: flush remaining data?

        Ok(())
    }
}
