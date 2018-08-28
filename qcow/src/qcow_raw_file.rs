// Copyright 2018 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::fs::File;
use std::io::{self, Seek, SeekFrom};
use std::mem::size_of;

use byteorder::{BigEndian, ReadBytesExt, WriteBytesExt};

/// A qcow file. Allows reading/writing clusters and appending clusters.
#[derive(Debug)]
pub struct QcowRawFile {
    file: File,
    cluster_size: u64,
    cluster_mask: u64,
}

impl QcowRawFile {
    pub fn from(file: File, cluster_size: u64, cluster_mask: u64) -> Self {
        QcowRawFile {
            file,
            cluster_size,
            cluster_mask,
        }
    }

    /// Reads a `count` 64 bit offsets and returns them as a vector.
    /// `mask` optionally ands out some of the bits on the file.
    pub fn read_pointer_table(
        &mut self,
        offset: u64,
        count: u64,
        mask: Option<u64>,
    ) -> io::Result<Vec<u64>> {
        let mut table = Vec::with_capacity(count as usize);
        self.file.seek(SeekFrom::Start(offset))?;
        let mask = mask.unwrap_or(0xffff_ffff_ffff_ffff);
        for _ in 0..count {
            table.push(self.file.read_u64::<BigEndian>()? & mask);
        }
        Ok(table)
    }

    /// Reads a cluster's worth of 64 bit offsets and returns them as a vector.
    /// `mask` optionally ands out some of the bits on the file.
    pub fn read_pointer_cluster(
        &mut self,
        offset: u64,
        mask: Option<u64>,
    ) -> io::Result<Vec<u64>> {
        let count = self.cluster_size / size_of::<u64>() as u64;
        self.read_pointer_table(offset, count, mask)
    }

    /// Writes `table`, a `Vec` of u64 pointers to `offset` in the file.
    /// `non_zero_flags` will be ORed with all non-zero values in `table`.
    /// writing.
    pub fn write_pointer_table(
        &mut self,
        offset: u64,
        table: &Vec<u64>,
        non_zero_flags: u64,
    ) -> io::Result<()> {
        self.file.seek(SeekFrom::Start(offset))?;
        for addr in table {
            let val = if *addr == 0 {
                0
            } else {
                *addr | non_zero_flags
            };
            self.file.write_u64::<BigEndian>(val)?;
        }
        Ok(())
    }

    /// Read a refcount block from the file and returns a Vec containing the block.
    /// Always returns a cluster's worth of data.
    pub fn read_refcount_block(&mut self, offset: u64) -> io::Result<Vec<u16>> {
        let count = self.cluster_size / size_of::<u16>() as u64;
        let mut table = Vec::with_capacity(count as usize);
        self.file.seek(SeekFrom::Start(offset))?;
        for _ in 0..count {
            table.push(self.file.read_u16::<BigEndian>()?);
        }
        Ok(table)
    }

    /// Writes a refcount block to the file.
    pub fn write_refcount_block(
        &mut self,
        offset: u64,
        table: &Vec<u16>
    ) -> io::Result<()> {
        self.file.seek(SeekFrom::Start(offset))?;
        for count in table {
            self.file.write_u16::<BigEndian>(*count)?;
        }
        Ok(())
    }

    /// Allocates a new cluster at the end of the current file, return the address.
    pub fn add_cluster_end(&mut self) -> io::Result<u64>
    {
        // Determine where the new end of the file should be and set_len, which
        // translates to truncate(2).
        let file_end: u64 = self.file.seek(SeekFrom::End(0))?;
        let new_cluster_address: u64 = (file_end + self.cluster_size - 1) & !self.cluster_mask;
        self.file.set_len(new_cluster_address + self.cluster_size)?;

        Ok(new_cluster_address)
    }

    /// Returns a reference to the underlying file.
    pub fn file(&self) -> &File {
        &self.file
    }
    
    /// Returns a mutable reference to the underlying file.
    pub fn file_mut(&mut self) -> &mut File {
        &mut self.file
    }
    
    /// Returns the size of the file's clusters.
    pub fn cluster_size(&self) -> u64 {
        self.cluster_size
    }

    /// Returns the offset of `address` within a cluster.
    pub fn cluster_offset(&self, address: u64) -> u64 {
        address & self.cluster_mask
    }
}
