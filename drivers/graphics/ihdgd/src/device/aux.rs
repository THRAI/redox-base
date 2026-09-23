use common::io::Io;
use common::timeout::Timeout;
use embedded_hal::blocking::i2c::{self, Operation, SevenBitAddress, Transactional};

use super::ddi::*;

pub struct Aux<'a> {
    ddi: &'a mut Ddi,
}

impl<'a> Aux<'a> {
    pub fn new(ddi: &'a mut Ddi) -> Self {
        Self { ddi }
    }
}

impl<'a> Transactional for Aux<'a> {
    type Error = ();
    fn exec(&mut self, addr7: SevenBitAddress, full_ops: &mut [Operation<'_>]) -> Result<(), ()> {
        // Break ops into 16-byte chunks that will fit into aux data
        let mut ops = Vec::new();
        for op in full_ops.iter_mut() {
            match op {
                Operation::Read(buf) => {
                    for chunk in buf.chunks_mut(16) {
                        ops.push(Operation::Read(chunk));
                    }
                }
                Operation::Write(buf) => {
                    for chunk in buf.chunks(16) {
                        ops.push(Operation::Write(chunk));
                    }
                }
            }
        }

        let ops_len = ops.len();
        for (i, op) in ops.iter_mut().enumerate() {
            // Write header and data
            let mut header = 0;
            match op {
                Operation::Read(_) => {
                    header |= 1 << 4;
                }
                Operation::Write(_) => (),
            }
            if (i + 1) < ops_len {
                // Middle of transaction
                header |= 1 << 6;
            }
            let mut aux_datas = [0u8; 20];
            let mut aux_data_i = 0;
            aux_datas[aux_data_i] = header;
            aux_data_i += 1;
            //TODO: what is this byte?
            aux_datas[aux_data_i] = 0;
            aux_data_i += 1;
            aux_datas[aux_data_i] = addr7;
            aux_data_i += 1;
            match op {
                Operation::Read(buf) => {
                    if !buf.is_empty() {
                        aux_datas[aux_data_i] = (buf.len() - 1) as u8;
                        aux_data_i += 1;
                    }
                }
                Operation::Write(buf) => {
                    if !buf.is_empty() {
                        aux_datas[aux_data_i] = (buf.len() - 1) as u8;
                        aux_data_i += 1;
                        for b in buf.iter() {
                            aux_datas[aux_data_i] = *b;
                            aux_data_i += 1;
                        }
                    }
                }
            }

            // Write data to registers (big endian, dword access only)
            for (i, chunk) in aux_datas.chunks(4).enumerate() {
                let mut bytes = [0; 4];
                bytes[..chunk.len()].copy_from_slice(&chunk);
                self.ddi.aux_datas[i].write(u32::from_be_bytes(bytes));
            }

            self.ddi.aux_ctl.modify(|v| {
                // Set length
                v.set_size(aux_data_i as u32)
                    // Set timeout
                    .set_timeout_timeout4000us()
                    // Set I/O select to legacy (cleared)
                    //TODO: TBT support?
                    .set_io_select_legacy()
                    // Start transaction
                    .set_busy(true)
            });

            // Wait while busy
            let timeout = Timeout::from_secs(1);
            while self.ddi.aux_ctl.read().busy() {
                timeout.run().map_err(|()| {
                    log::debug!(
                        "AUX I2C transaction wait timeout 0x{:08X}",
                        self.ddi.aux_ctl.read().raw()
                    );
                    ()
                })?;
            }

            // Read result
            let v = self.ddi.aux_ctl.read();
            if v.timeout_error() {
                log::debug!("AUX I2C transaction timeout error");
                return Err(());
            }
            if v.receive_error() {
                log::debug!("AUX I2C transaction receive error");
                return Err(());
            }
            if !v.done() {
                log::debug!("AUX I2C transaction done not set");
                return Err(());
            }

            // Read data from registers (big endian, dword access only)
            for (i, chunk) in aux_datas.chunks_mut(4).enumerate() {
                let bytes = self.ddi.aux_datas[i].read().to_be_bytes();
                chunk.copy_from_slice(&bytes[..chunk.len()]);
            }

            aux_data_i = 0;
            let response = aux_datas[aux_data_i];
            if response != 0 {
                log::debug!("AUX I2C unexpected response {:02X}", response);
                return Err(());
            }
            aux_data_i += 1;
            match op {
                Operation::Read(buf) => {
                    if !buf.is_empty() {
                        for b in buf.iter_mut() {
                            *b = aux_datas[aux_data_i];
                            aux_data_i += 1;
                        }
                    }
                }
                Operation::Write(_) => (),
            }
        }

        Ok(())
    }
}

impl<'a> i2c::WriteRead for Aux<'a> {
    type Error = ();
    fn write_read(
        &mut self,
        addr7: SevenBitAddress,
        bytes: &[u8],
        buffer: &mut [u8],
    ) -> Result<(), ()> {
        self.exec(
            addr7,
            &mut [Operation::Write(bytes), Operation::Read(buffer)],
        )
    }
}
