/* automatically generated by rust-bindgen */
/* bindgen --with-derive-default include/uapi/linux/aio_abi.h
 * __kernel_rwf_t had to be replaced with int
 * Then delete everything that isn't needed.
 * TODO(dgreid) - this is x86_64 only, need to do other arches */

#![allow(non_upper_case_globals)]
#![allow(non_camel_case_types)]
#![allow(non_snake_case)]
#![allow(dead_code)]

pub type aio_context_t = ::std::os::raw::c_ulong;
pub const IOCB_CMD_PREAD: u32 = 0;
pub const IOCB_CMD_PWRITE: u32 = 1;
pub const IOCB_CMD_FSYNC: u32 = 2;
pub const IOCB_CMD_FDSYNC: u32 = 3;
pub const IOCB_CMD_POLL: u32 = 5;
pub const IOCB_CMD_NOOP: u32 = 6;
pub const IOCB_CMD_PREADV: u32 = 7;
pub const IOCB_CMD_PWRITEV: u32 = 8;

#[repr(C)]
#[derive(Debug, Default, Copy, Clone)]
pub struct io_event {
    pub data: u64,
    pub obj: u64,
    pub res: i64,
    pub res2: i64,
}

#[repr(C)]
#[derive(Debug, Default, Copy, Clone)]
pub struct iocb {
    pub aio_data: u64,
    pub aio_key: u32,
    pub aio_rw_flags: ::std::os::raw::c_int,
    pub aio_lio_opcode: u16,
    pub aio_reqprio: i16,
    pub aio_fildes: u32,
    pub aio_buf: u64,
    pub aio_nbytes: u64,
    pub aio_offset: i64,
    pub aio_reserved2: u64,
    pub aio_flags: u32,
    pub aio_resfd: u32,
}