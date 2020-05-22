// Copyright 2020 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::io::{IoSlice, IoSliceMut};

use data_model::VolatileMemory;

use crate::uring_executor::{Error, Result};

/// Used to index subslices of backing memory. Like an iovec, but relative to the start of the
/// memory region instead of an absolute pointer.
#[derive(Debug)]
pub struct MemVec {
    pub offset: u64,
    pub len: usize,
}

/// Trait for memory that can yeild both `IoSlice` and `IoSliceMut` from a & ref borrow(not a &mut).
/// Must be OK to modify the backing memory without owning a mut able reference. For example,
/// this is safe for GuestMemory and VolatileSlices in crosvm as those types guarantee they are
/// dealt with as volatile.
pub unsafe trait BackingMemory {
    /// Returns a mutable slice to the backing memory. This is most commonly unsafe. To implement
    /// this safely the implementor must guarantee that the backing memory can be modified out of
    /// band without affecting safety guaratees.
    fn io_slice_mut(&self, mem_off: &MemVec) -> Result<IoSliceMut<'_>>;

    /// Returns a slice for reading the backing memory.
    fn io_slice(&self, mem_off: &MemVec) -> Result<IoSlice<'_>>;
}

// Safe to implement BackingMemory as VolatileMemory can be mutated any time.
unsafe impl<T: VolatileMemory> BackingMemory for T {
    fn io_slice_mut(&self, mem_off: &MemVec) -> Result<IoSliceMut<'_>> {
        let vs = self
            .get_slice(mem_off.offset, mem_off.len as u64)
            .map_err(Error::InvalidRange)?;
        // Safe because 'vs' is valid in the backing memory and that will be kept
        // alive longer than this iterator. And volatile memory can be modified at any time.
        unsafe {
            Ok(IoSliceMut::new(std::slice::from_raw_parts_mut(
                vs.as_ptr(),
                vs.size() as usize,
            )))
        }
    }

    fn io_slice(&self, mem_off: &MemVec) -> Result<IoSlice<'_>> {
        let vs = self
            .get_slice(mem_off.offset, mem_off.len as u64)
            .map_err(Error::InvalidRange)?;
        unsafe {
            // Safe because 'vs' is valid in the backing memory and that will be kept
            // alive longer than this iterator.
            Ok(IoSlice::new(std::slice::from_raw_parts_mut(
                vs.as_ptr(),
                vs.size() as usize,
            )))
        }
    }
}
/// Wrapper to be used for passing a Vec in as backing memory for asynchronous operations.  The
/// wrapper owns a Vec according to the borrow checker. It is loaning this vec out to the kernel(or
/// other modifiers) through the `BackingMemory` trait. This allows multiple modifiers of the array
/// in the `Vec` while this struct is alive.
/// To ensure that those operations can be done safely, no access is allowed to the `Vec`'s memory
/// from the time that `VecIoWrapper` is constructed and the time it is turned back in to a `Vec`
/// using `to_inner`. The returned `Vec` is guaranteed to be valid as any combination of bits in a
/// `Vec` of `u8` is valid.
#[derive(Debug)]
pub(crate) struct VecIoWrapper {
    inner: Vec<u8>,
}

impl From<Vec<u8>> for VecIoWrapper {
    fn from(vec: Vec<u8>) -> Self {
        VecIoWrapper { inner: vec }
    }
}

impl Into<Vec<u8>> for VecIoWrapper {
    fn into(self) -> Vec<u8> {
        self.inner
    }
}

impl VecIoWrapper {
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    fn check_addrs(&self, mem_off: &MemVec) -> Result<()> {
        let end = mem_off
            .offset
            .checked_add(mem_off.len as u64)
            .ok_or(Error::InvalidOffset)?;
        if end > self.inner.len() as u64 {
            return Err(Error::InvalidOffset);
        }
        Ok(())
    }
}

// Safe to implement BackingMemory as the vec is only accessible inside the wrapper and these slices
// are the only thing allowed to modify it.
// Nothing else can get a reference to the vec until all slice are dropped because they borrow Self.
// Nothing can borrow the owned inner vec until self is consumed by `into`, which can't happen
// if there are outstanding mut borrows.
unsafe impl BackingMemory for VecIoWrapper {
    fn io_slice_mut(&self, mem_off: &MemVec) -> Result<IoSliceMut<'_>> {
        // Safe because the vector is valid and will be kept alive longer than this IoSliceMut and
        // this memory is fully owned so it can be modified for the lifetime of this IoSlice safely.
        // The mem_off ranges are checked.
        unsafe {
            self.check_addrs(mem_off)?;
            Ok(IoSliceMut::new(std::slice::from_raw_parts_mut(
                self.inner.as_ptr().add(mem_off.offset as usize) as *mut _,
                mem_off.len,
            )))
        }
    }

    fn io_slice(&self, mem_off: &MemVec) -> Result<IoSlice<'_>> {
        // Safe because the vector is valid and will be kept alive longer than this IoSlice.
        // The mem_off ranges are checked.
        self.check_addrs(mem_off)?;
        Ok(IoSlice::new(
            &self.inner[mem_off.offset as usize..mem_off.offset as usize + mem_off.len],
        ))
    }
}