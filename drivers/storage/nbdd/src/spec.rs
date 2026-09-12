#![allow(dead_code)]

// ==== Handshake phase ====

// Handshake flags
pub(crate) const NBD_FLAG_FIXED_NEWSTYLE: u16 = 1 << 0;
pub(crate) const NBD_FLAG_NO_ZEROES: u16 = 1 << 1;

// Client flags
pub(crate) const NBD_FLAG_C_FIXED_NEWSTYLE: u32 = 1 << 0;
pub(crate) const NBD_FLAG_C_NO_ZEROES: u32 = 1 << 1;

// Transmission flags
pub(crate) const NBD_FLAG_HAS_FLAGS: u16 = 1 << 0;
pub(crate) const NBD_FLAG_READ_ONLY: u16 = 1 << 1;
pub(crate) const NBD_FLAG_SEND_FLUSH: u16 = 1 << 2;
pub(crate) const NBD_FLAG_SEND_FUA: u16 = 1 << 3;
pub(crate) const NBD_FLAG_ROTATIONAL: u16 = 1 << 4;
pub(crate) const NBD_FLAG_SEND_TRIM: u16 = 1 << 5;
pub(crate) const NBD_FLAG_SEND_WRITE_ZEROES: u16 = 1 << 6;
pub(crate) const NBD_FLAG_SEND_DF: u16 = 1 << 7;
pub(crate) const NBD_FLAG_CAN_MULTI_CONN: u16 = 1 << 8;
pub(crate) const NBD_FLAG_SEND_RESIZE: u16 = 1 << 9;
pub(crate) const NBD_FLAG_SEND_CACHE: u16 = 1 << 10;
pub(crate) const NBD_FLAG_SEND_FAST_ZERO: u16 = 1 << 11;
pub(crate) const NBD_FLAG_BLOCK_STATUS_PAYLOAD: u16 = 1 << 12;

// Option types
pub(crate) const NBD_OPT_EXPORT_NAME: u32 = 1;
pub(crate) const NBD_OPT_ABORT: u32 = 2;
pub(crate) const NBD_OPT_LIST: u32 = 3;
// Option 4 is unused
pub(crate) const NBD_OPT_STARTTLS: u32 = 5;
pub(crate) const NBD_OPT_INFO: u32 = 6;
pub(crate) const NBD_OPT_GO: u32 = 7;
pub(crate) const NBD_OPT_STRUCTURED_REPLY: u32 = 8;
pub(crate) const NBD_OPT_LIST_META_CONTEXT: u32 = 9;
pub(crate) const NBD_OPT_SET_META_CONTEXT: u32 = 10;

// Option reply types
pub(crate) const NBD_REP_ACK: u32 = 1;
pub(crate) const NBD_REP_SERVER: u32 = 2;
pub(crate) const NBD_REP_INFO: u32 = 3;
pub(crate) const NBD_REP_META_CONTEXT: u32 = 4;
pub(crate) const NBD_REP_ERR_UNSUP: u32 = (1 << 31) + 1;
pub(crate) const NBD_REP_ERR_POLICY: u32 = (1 << 31) + 2;
pub(crate) const NBD_REP_ERR_INVALID: u32 = (1 << 31) + 3;
pub(crate) const NBD_REP_ERR_PLATFORM: u32 = (1 << 31) + 4;
pub(crate) const NBD_REP_ERR_TLS_REQD: u32 = (1 << 31) + 5;
pub(crate) const NBD_REP_ERR_UNKNOWN: u32 = (1 << 31) + 6;
pub(crate) const NBD_REP_ERR_SHUTDOWN: u32 = (1 << 31) + 7;
pub(crate) const NBD_REP_ERR_BLOCK_SIZE_REQD: u32 = (1 << 31) + 8;
pub(crate) const NBD_REP_ERR_TOO_BIG: u32 = (1 << 31) + 9;
pub(crate) const NBD_REP_ERR_EXT_HEADER_REQD: u32 = (1 << 31) + 10;

// ==== Transmission phase ====

// Command flags
pub(crate) const NBD_CMD_FLAG_FUA: u16 = 1 << 0;
pub(crate) const NBD_CMD_FLAG_NO_HOLE: u16 = 1 << 1;
pub(crate) const NBD_CMD_FLAG_DF: u16 = 1 << 2;
pub(crate) const NBD_CMD_FLAG_REQ_ONE: u16 = 1 << 3;
pub(crate) const NBD_CMD_FLAG_FAST_ZERO: u16 = 1 << 4;
pub(crate) const NBD_CMD_FLAG_PAYLOAD_LEN: u16 = 1 << 5;

// Request types
pub(crate) const NBD_REQUEST_MAGIC: u32 = 0x25609513;
pub(crate) const NBD_SIMPLE_REPLY_MAGIC: u32 = 0x67446698;
pub(crate) const NBD_CMD_READ: u16 = 0;
pub(crate) const NBD_CMD_WRITE: u16 = 1;
pub(crate) const NBD_CMD_DISC: u16 = 2;
pub(crate) const NBD_CMD_FLUSH: u16 = 3;
pub(crate) const NBD_CMD_TRIM: u16 = 4;
pub(crate) const NBD_CMD_CACHE: u16 = 5;
pub(crate) const NBD_CMD_WRITE_ZEROES: u16 = 6;
pub(crate) const NBD_CMD_BLOCK_STATUS: u16 = 7;
pub(crate) const NBD_CMD_RESIZE: u16 = 8;

// Error values
pub(crate) const NBD_EPERM: u32 = 1;
pub(crate) const NBD_EIO: u32 = 5;
pub(crate) const NBD_ENOMEM: u32 = 12;
pub(crate) const NBD_EINVAL: u32 = 22;
pub(crate) const NBD_ENOSPC: u32 = 28;
pub(crate) const NBD_EOVERFLOW: u32 = 75;
pub(crate) const NBD_ENOTSUP: u32 = 95;
pub(crate) const NBD_ESHUTDOWN: u32 = 108;
