// Copyright 2018 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![feature(test)]

extern crate byteorder;
extern crate libc;
extern crate test;

mod l2_cache;
mod qcow_raw_file;
mod refcount;

use l2_cache::{Cacheable, L2Cache, VecCache};
use qcow_raw_file::QcowRawFile;

use byteorder::{BigEndian, ReadBytesExt, WriteBytesExt};
use libc::EINVAL;

use std::cmp::min;
use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::mem::size_of;
use std::os::unix::io::{AsRawFd, RawFd};

#[derive(Debug)]
pub enum Error {
    BackingFilesNotSupported,
    CompressedBlocksNotSupported,
    GettingFileSize(io::Error),
    GettingRefcount(io::Error),
    InvalidClusterSize,
    InvalidL1TableOffset,
    InvalidMagic,
    InvalidOffset(u64),
    InvalidRefcountTableOffset,
    NoRefcountClusters,
    OpeningFile(io::Error),
    ReadingHeader(io::Error),
    SeekingFile(io::Error),
    SettingRefcountRefcount(io::Error),
    SizeTooSmallForNumberOfClusters,
    WritingHeader(io::Error),
    UnsupportedRefcountOrder,
    UnsupportedVersion(u32),
}
pub type Result<T> = std::result::Result<T, Error>;

// QCOW magic constant that starts the header.
const QCOW_MAGIC: u32 = 0x5146_49fb;
// Default to a cluster size of 2^DEFAULT_CLUSTER_BITS
const DEFAULT_CLUSTER_BITS: u32 = 16;
const MAX_CLUSTER_BITS: u32 = 30;
// Only support 2 byte refcounts, 2^refcount_order bits.
const DEFAULT_REFCOUNT_ORDER: u32 = 4;

const V3_BARE_HEADER_SIZE: u32 = 104;

// bits 0-8 and 56-63 are reserved.
const L1_TABLE_OFFSET_MASK: u64 = 0x00ff_ffff_ffff_fe00;
const L2_TABLE_OFFSET_MASK: u64 = 0x00ff_ffff_ffff_fe00;
// Flags
const COMPRESSED_FLAG: u64 = 1 << 62;
const CLUSTER_USED_FLAG: u64 = 1 << 63;

/// Contains the information from the header of a qcow file.
#[derive(Debug)]
pub struct QcowHeader {
    pub magic: u32,
    pub version: u32,

    pub backing_file_offset: u64,
    pub backing_file_size: u32,

    pub cluster_bits: u32,
    pub size: u64,
    pub crypt_method: u32,

    pub l1_size: u32,
    pub l1_table_offset: u64,

    pub refcount_table_offset: u64,
    pub refcount_table_clusters: u32,

    pub nb_snapshots: u32,
    pub snapshots_offset: u64,

    // v3 entries
    pub incompatible_features: u64,
    pub compatible_features: u64,
    pub autoclear_features: u64,
    pub refcount_order: u32,
    pub header_size: u32,
}

impl QcowHeader {
    /// Creates a QcowHeader from a reference to a file.
    pub fn new(f: &mut File) -> Result<QcowHeader> {
        f.seek(SeekFrom::Start(0)).map_err(Error::ReadingHeader)?;
        let magic = f.read_u32::<BigEndian>().map_err(Error::ReadingHeader)?;
        if magic != QCOW_MAGIC {
            return Err(Error::InvalidMagic);
        }

        // Reads the next u32 from the file.
        fn read_u32_from_file(f: &mut File) -> Result<u32> {
            f.read_u32::<BigEndian>().map_err(Error::ReadingHeader)
        }

        // Reads the next u64 from the file.
        fn read_u64_from_file(f: &mut File) -> Result<u64> {
            f.read_u64::<BigEndian>().map_err(Error::ReadingHeader)
        }

        Ok(QcowHeader {
            magic,
            version: read_u32_from_file(f)?,
            backing_file_offset: read_u64_from_file(f)?,
            backing_file_size: read_u32_from_file(f)?,
            cluster_bits: read_u32_from_file(f)?,
            size: read_u64_from_file(f)?,
            crypt_method: read_u32_from_file(f)?,
            l1_size: read_u32_from_file(f)?,
            l1_table_offset: read_u64_from_file(f)?,
            refcount_table_offset: read_u64_from_file(f)?,
            refcount_table_clusters: read_u32_from_file(f)?,
            nb_snapshots: read_u32_from_file(f)?,
            snapshots_offset: read_u64_from_file(f)?,
            incompatible_features: read_u64_from_file(f)?,
            compatible_features: read_u64_from_file(f)?,
            autoclear_features: read_u64_from_file(f)?,
            refcount_order: read_u32_from_file(f)?,
            header_size: read_u32_from_file(f)?,
        })
    }

    /// Create a header for the given `size`.
    pub fn create_for_size(size: u64) -> QcowHeader {
        let cluster_bits: u32 = DEFAULT_CLUSTER_BITS;
        let cluster_size: u32 = 0x01 << cluster_bits;
        // L2 blocks are always one cluster long. They contain cluster_size/sizeof(u64) addresses.
        let l2_size: u32 = cluster_size / size_of::<u64>() as u32;
        let num_clusters: u32 = div_round_up_u64(size, u64::from(cluster_size)) as u32;
        let num_l2_clusters: u32 = div_round_up_u32(num_clusters, l2_size);
        let l1_clusters: u32 = div_round_up_u32(num_l2_clusters, cluster_size);
        QcowHeader {
            magic: QCOW_MAGIC,
            version: 3,
            backing_file_offset: 0,
            backing_file_size: 0,
            cluster_bits: DEFAULT_CLUSTER_BITS,
            size,
            crypt_method: 0,
            l1_size: num_l2_clusters,
            l1_table_offset: u64::from(cluster_size),
             // The refcount table is after l1 + header.
            refcount_table_offset: u64::from(cluster_size * (l1_clusters + 1)),
            refcount_table_clusters: {
                // Pre-allocate enough clusters for the entire refcount table as it must be
                // continuous in the file. Allocate enough space to refcount all clusters, including
                // the refcount clusters.
                let max_refcount_clusters = max_refcount_clusters(DEFAULT_REFCOUNT_ORDER,
                                                                  cluster_size,
                                                                  num_clusters) as u32;
                // The refcount table needs to store the offset of each refcount cluster.
                div_round_up_u32(max_refcount_clusters * size_of::<u64>() as u32, cluster_size)
            },
            nb_snapshots: 0,
            snapshots_offset: 0,
            incompatible_features: 0,
            compatible_features: 0,
            autoclear_features: 0,
            refcount_order: DEFAULT_REFCOUNT_ORDER,
            header_size: V3_BARE_HEADER_SIZE,
       }
    }

    /// Write the header to `file`.
    pub fn write_to<F: Write + Seek>(&self, file: &mut F) -> Result<()> {
        // Writes the next u32 to the file.
        fn write_u32_to_file<F: Write>(f: &mut F, value: u32) -> Result<()> {
            f.write_u32::<BigEndian>(value).map_err(Error::WritingHeader)
        }

        // Writes the next u64 to the file.
        fn write_u64_to_file<F: Write>(f: &mut F, value: u64) -> Result<()> {
            f.write_u64::<BigEndian>(value).map_err(Error::WritingHeader)
        }

        write_u32_to_file(file, self.magic)?;
        write_u32_to_file(file, self.version)?;
        write_u64_to_file(file, self.backing_file_offset)?;
        write_u32_to_file(file, self.backing_file_size)?;
        write_u32_to_file(file, self.cluster_bits)?;
        write_u64_to_file(file, self.size)?;
        write_u32_to_file(file, self.crypt_method)?;
        write_u32_to_file(file, self.l1_size)?;
        write_u64_to_file(file, self.l1_table_offset)?;
        write_u64_to_file(file, self.refcount_table_offset)?;
        write_u32_to_file(file, self.refcount_table_clusters)?;
        write_u32_to_file(file, self.nb_snapshots)?;
        write_u64_to_file(file, self.snapshots_offset)?;
        write_u64_to_file(file, self.incompatible_features)?;
        write_u64_to_file(file, self.compatible_features)?;
        write_u64_to_file(file, self.autoclear_features)?;
        write_u32_to_file(file, self.refcount_order)?;
        write_u32_to_file(file, self.header_size)?;

        // Set the file length by seeking and writing a zero to the last byte. This avoids needing
        // a `File` instead of anything that implements seek as the `file` argument.
        // Zeros out the l1 and refcount table clusters.
        let cluster_size = 0x01u64 << self.cluster_bits;
        let refcount_blocks_size = u64::from(self.refcount_table_clusters) * cluster_size;
        file.seek(SeekFrom::Start(self.refcount_table_offset + refcount_blocks_size - 2))
            .map_err(Error::WritingHeader)?;
        file.write(&[0u8])
            .map_err(Error::WritingHeader)?;

        Ok(())
    }
}

fn max_refcount_clusters(refcount_order: u32, cluster_size: u32, num_clusters: u32) -> usize {
    let refcount_bytes = (0x01u32 << refcount_order) / 8;
    let for_data = div_round_up_u32(num_clusters * refcount_bytes, cluster_size);
    println!("for_data {}", for_data);
    let for_refcounts = div_round_up_u32(for_data * refcount_bytes, cluster_size);
    println!("for_refcounts {}", for_refcounts);
    for_data as usize + for_refcounts as usize
}

/// Represents a qcow2 file. This is a sparse file format maintained by the qemu project.
/// Full documentation of the format can be found in the qemu repository.
///
/// # Example
///
/// ```
/// # use std::io::{Read, Seek, SeekFrom};
/// # use qcow::{self, QcowFile};
/// # fn test(file: std::fs::File) -> std::io::Result<()> {
///     let mut q = QcowFile::from(file).expect("Can't open qcow file");
///     let mut buf = [0u8; 12];
///     q.seek(SeekFrom::Start(10 as u64))?;
///     q.read(&mut buf[..])?;
/// #   Ok(())
/// # }
/// ```
#[derive(Debug)]
pub struct QcowFile {
    raw_file: QcowRawFile,
    header: QcowHeader,
    l1_table: Vec<u64>,
    ref_table: Vec<u64>,
    l2_entries: u64,
    l2_cache: HashMap<usize, VecCache<u64>>,
    refblock_cache: L2Cache<VecCache<u16>>,
    current_offset: u64,
    refcount_block_entries: u64,
    unref_clusters: Vec<u64>, // List of freshly unreferenced clusters.
    // List of unreferenced clusters available to be used. unref clusters become available once the
    // removal of references to them have been synced to disk.
    avail_clusters: Vec<u64>,
    //TODO(dgreid) Add support for backing files. - backing_file: Option<Box<QcowFile<T>>>,
}

impl QcowFile {
    /// Creates a QcowFile from `file`. File must be a valid qcow2 image.
    pub fn from(mut file: File) -> Result<QcowFile> {
        let header = QcowHeader::new(&mut file)?;

        // Only v3 files are supported.
        if header.version != 3 {
            return Err(Error::UnsupportedVersion(header.version));
        }

        let cluster_bits: u32 = header.cluster_bits;
        if cluster_bits > MAX_CLUSTER_BITS {
            return Err(Error::InvalidClusterSize);
        }
        let cluster_size = 0x01u64 << cluster_bits;
        if cluster_size < size_of::<u64>() as u64 {
            // Can't fit an offset in a cluster, nothing is going to work.
            return Err(Error::InvalidClusterSize);
        }

        // No current support for backing files.
        if header.backing_file_offset != 0 {
            return Err(Error::BackingFilesNotSupported);
        }

        // Only support two byte refcounts.
        let refcount_bits: u64 = 0x01u64
            .checked_shl(header.refcount_order)
            .ok_or(Error::UnsupportedRefcountOrder)?;
        if refcount_bits != 16 {
            return Err(Error::UnsupportedRefcountOrder);
        }

        // Need at least one refcount cluster
        if header.refcount_table_clusters == 0 {
            return Err(Error::NoRefcountClusters);
        }
        offset_is_cluster_boundary(header.backing_file_offset, header.cluster_bits)?;
        offset_is_cluster_boundary(header.l1_table_offset, header.cluster_bits)?;
        offset_is_cluster_boundary(header.refcount_table_offset, header.cluster_bits)?;
        offset_is_cluster_boundary(header.snapshots_offset, header.cluster_bits)?;

        let mut raw_file = QcowRawFile {
                file,
                cluster_size,
                cluster_mask: cluster_size - 1,
        };

        let l1_table = raw_file.read_pointer_table(
            header.l1_table_offset,
            header.l1_size as u64,
            Some(L1_TABLE_OFFSET_MASK),
        ).map_err(Error::ReadingHeader)?;
        if l1_table.iter().any(|entry| entry & COMPRESSED_FLAG != 0) {
            return Err(Error::CompressedBlocksNotSupported);
        }

        let num_clusters = div_round_up_u64(header.size, u64::from(cluster_size)) as u32;
        let refcount_clusters = max_refcount_clusters(header.refcount_order,
                                                      cluster_size as u32,
                                                      num_clusters);
        let ref_table = raw_file.read_pointer_table(
            header.refcount_table_offset,
            refcount_clusters as u64,
            None,
        ).map_err(Error::ReadingHeader)?;

        let l2_entries = cluster_size / size_of::<u64>() as u64;

        let qcow = QcowFile {
            raw_file,
            header,
            l1_table,
            ref_table,
            l2_entries,
            l2_cache: HashMap::with_capacity(100),
            refblock_cache: L2Cache::new(refcount_clusters, 25),
            current_offset: 0,
            refcount_block_entries: cluster_size * size_of::<u64>() as u64 / refcount_bits,
            unref_clusters: Vec::new(),
            avail_clusters: Vec::new(),
        };

        // Check that the L1 and refcount tables fit in a 64bit address space.
        qcow.header
            .l1_table_offset
            .checked_add(qcow.l1_address_offset(qcow.virtual_size()))
            .ok_or(Error::InvalidL1TableOffset)?;
        qcow.header
            .refcount_table_offset
            .checked_add(u64::from(qcow.header.refcount_table_clusters) * cluster_size)
            .ok_or(Error::InvalidRefcountTableOffset)?;

        println!(
            "size: {} l2 ents: {} L1 size {}",
            qcow.header.size, qcow.l2_entries, qcow.header.l1_size
        );

        Ok(qcow)
    }

    /// Creates a new QcowFile at the given path.
    pub fn new(mut file: File, virtual_size: u64) -> Result<QcowFile> {
        let header = QcowHeader::create_for_size(virtual_size);
        file.seek(SeekFrom::Start(0)).map_err(Error::SeekingFile)?;
        header.write_to(&mut file)?;

        let mut qcow = Self::from(file)?;

        // Set the refcount for each refcount table cluster.
        let cluster_size = 0x01u64 << qcow.header.cluster_bits;
        let refcount_table_base = qcow.header.refcount_table_offset as u64;
        let end_cluster_addr = refcount_table_base +
            u64::from(qcow.header.refcount_table_clusters) * cluster_size;

        let mut cluster_addr = 0;
        while cluster_addr < end_cluster_addr {
            qcow.set_cluster_refcount(cluster_addr, 1).map_err(Error::SettingRefcountRefcount)?;
            cluster_addr += cluster_size;
        }

        Ok(qcow)
    }

    /// Returns the first cluster in the file with a 0 refcount. Used for testing.
    pub fn first_zero_refcount(&mut self) -> Result<Option<u64>> {
        let file_size = self.raw_file.file.metadata().map_err(Error::GettingFileSize)?.len();
        let cluster_size = 0x01u64 << self.header.cluster_bits;

        let mut cluster_addr = 0;
        while cluster_addr < file_size {
            match self.get_cluster_refcount(cluster_addr).map_err(Error::GettingRefcount)? {
                0 => return Ok(Some(cluster_addr)),
                _ => (),
            }
            cluster_addr += cluster_size;
        }
        Ok(None)
    }

    // Limits the range so that it doesn't exceed the virtual size of the file.
    fn limit_range_file(&self, address: u64, count: usize) -> usize {
        if address.checked_add(count as u64).is_none() || address > self.virtual_size() {
            return 0;
        }
        min(count as u64, self.virtual_size() - address) as usize
    }

    // Limits the range so that it doesn't overflow the end of a cluster.
    fn limit_range_cluster(&self, address: u64, count: usize) -> usize {
        let offset: u64 = address & self.raw_file.cluster_mask;
        let limit = self.raw_file.cluster_size - offset;
        min(count as u64, limit) as usize
    }

    // Gets the maximum virtual size of this image.
    fn virtual_size(&self) -> u64 {
        self.header.size
    }

    // Gets the offset of `address` in the L1 table.
    fn l1_address_offset(&self, address: u64) -> u64 {
        let l1_index = self.l1_table_index(address);
        l1_index * size_of::<u64>() as u64
    }

    // Gets the offset of `address` in the L1 table.
    fn l1_table_index(&self, address: u64) -> u64 {
        (address / self.raw_file.cluster_size) / self.l2_entries
    }

    // Gets the offset of `address` in the L2 table.
    fn l2_table_index(&self, address: u64) -> u64 {
        (address / self.raw_file.cluster_size) % self.l2_entries
    }

    // Returns the offset of address within a cluster.
    fn cluster_offset(&self, address: u64) -> u64 {
        address & self.raw_file.cluster_mask
    }

    fn check_l2_evict(&mut self, except: usize) -> std::io::Result<()> {
        // TODO(dgreid) - smarter eviction strategy.
        if self.l2_cache.len() == self.l2_cache.capacity() {
            let mut to_evict = 0;
            for k in self.l2_cache.keys() {
                if *k != except { // Don't remove the one we just added.
                    to_evict = *k;
                    break;
                }
            }
            if let Some(evicted) = self.l2_cache.remove(&to_evict) {
                if evicted.dirty() {
                    self.write_l2_cluster(to_evict, evicted.addrs())?;
                }
            }
        }
        Ok(())
    }

    fn file_offset_read(&mut self, address: u64) -> std::io::Result<Option<u64>> {
        if address >= self.virtual_size() as u64 {
            return Err(std::io::Error::from_raw_os_error(EINVAL));
        }

        let l1_index = self.l1_table_index(address) as usize;
        let l2_addr_disk = *self
            .l1_table
            .get(l1_index)
            .ok_or(std::io::Error::from_raw_os_error(EINVAL))?;

        let l2_index = self.l2_table_index(address) as usize;

        let cluster_addr_from_table = match self.l2_cache.entry(l1_index) {
            Entry::Occupied(e) => e.get().get(l2_index),
            Entry::Vacant(mut e) => {
                // Not in the cache.
                if l2_addr_disk == 0 {
                    // Reading from an unallocated cluster will return zeros.
                    return Ok(None);
                }
                let table = VecCache::from_vec(
                    self.raw_file.read_pointer_cluster(l2_addr_disk, Some(L2_TABLE_OFFSET_MASK))?);
                e.insert(table).get(l2_index)
            }
        };

        self.check_l2_evict(l1_index)?;

        let cluster_addr = match cluster_addr_from_table {
            0 => return Ok(None),
            a => a,
        };
        Ok(Some(cluster_addr + self.cluster_offset(address)))
    }

    // TODO(next) - convert to use local hash map.
    fn file_offset_write(&mut self, address: u64) -> std::io::Result<u64> {
        if address >= self.virtual_size() as u64 {
            return Err(std::io::Error::from_raw_os_error(EINVAL));
        }

        let l1_index = self.l1_table_index(address) as usize;
        let l2_addr_disk = *self
            .l1_table
            .get(l1_index)
            .ok_or(std::io::Error::from_raw_os_error(EINVAL))?;
        let l2_index = self.l2_table_index(address) as usize;

        let cluster_addr_from_table = match self.l2_cache.entry(l1_index) {
            Entry::Occupied(e) => e.get().get(l2_index),
            Entry::Vacant(mut e) => {
                // Not in the cache.
                let table = if l2_addr_disk == 0 {
                    // Allocate a new cluster to store the L2 table and update the L1 table to point to
                    // the new table.
                    let new_addr: u64 = Self::get_new_cluster(&mut self.raw_file,
                                                              &mut self.avail_clusters)?;
                    // The cluster refcount starts at one indicating it is used but doesn't need COW.
                    self.set_cluster_refcount(new_addr, 1)?;
                    self.l1_table[l1_index] = new_addr;
                    VecCache::new(self.l2_entries as usize)
                } else {
                    VecCache::from_vec(self.raw_file.read_pointer_cluster(
                        l2_addr_disk,
                        Some(L1_TABLE_OFFSET_MASK))?)
                };
                e.insert(table).get(l2_index)
            }
        };

        let cluster_addr = match cluster_addr_from_table {
            0 => {
                // Need to allocate a data cluster
                let cluster_addr = self.append_data_cluster()?;
                if !self.l2_cache.get(&l1_index).unwrap().dirty() {
                    // Free the previously used cluster if one exists. Modified tables are always
                    // witten to new clusters so the L1 table can be committed to disk after they
                    // are and L1 never points at an invalid table.
                    // The index must be valid from when it was insterted.
                    let addr = *self.l1_table.get(l1_index).unwrap_or(&0);
                    if addr != 0 {
                        self.unref_clusters.push(addr);
                        self.set_cluster_refcount(addr, 0)?;
                    }

                    // Allocate a new cluster to store the L2 table and update the L1 table to point
                    // to the new table.
                    let new_addr: u64 = Self::get_new_cluster(&mut self.raw_file,
                                                              &mut self.avail_clusters)?;
                    // The cluster refcount starts at one indicating it is used but doesn't need
                    // COW.
                    self.set_cluster_refcount(new_addr, 1)?;
                    self.l1_table[l1_index] = new_addr;
                    
                }
                self.l2_cache.get_mut(&l1_index)
                    .unwrap() // Just checked/inserted.
                    .set(l2_index, cluster_addr);
                cluster_addr
            }
            a => a,
        };

        self.check_l2_evict(l1_index)?;

        Ok(cluster_addr + self.cluster_offset(address))
    }

    fn cache_refcount_table(&mut self, table_index: u64, table: VecCache<u16>)
        -> std::io::Result<()>
    {
        // Read the table from the disk, add it to the cache, and write back a potentially evicted
        // block.
        if let Some((evicted_idx, evicted)) = self
            .refblock_cache
            .insert(table_index as usize, table)
        {
            if !evicted.dirty() {
                return Ok(());
            }

            let addr = *self.ref_table.get(evicted_idx).unwrap();
            if addr != 0 {
                self.raw_file.write_refcount_block(addr, evicted.addrs())?;
            }
        }
        Ok(())
    }

    // Writes an L2 cluster out to disk.
    fn write_l2_cluster(
        &mut self,
        l1_index: usize,
        cluster: &Vec<u64>)
        -> std::io::Result<()>
    {
        // The index must be from valid when we insterted it.
        let addr = *self.l1_table.get(l1_index).unwrap();
        if addr != 0 {
            self.raw_file.write_pointer_table(addr, cluster, CLUSTER_USED_FLAG)
        } else {
            Err(std::io::Error::from_raw_os_error(EINVAL))
        }
    }


    // Allocate a new cluster at the end of the current file, return the address.
    fn get_new_cluster(
        raw_file: &mut QcowRawFile,
        avail_clusters: &mut Vec<u64>)
        -> std::io::Result<u64>
    {
        // First use a pre allocated cluster if one is available.
        if let Some(free_cluster) = avail_clusters.pop() {
            return Ok(free_cluster);
        }

        raw_file.add_cluster_end()
    }

    // Allocate and initialize a new data cluster. Returns the offset of the
    // cluster in to the file on success.
    fn append_data_cluster(&mut self) -> std::io::Result<u64> {
        let new_addr: u64 = Self::get_new_cluster(&mut self.raw_file,
                                                  &mut self.avail_clusters)?;
        // The cluster refcount starts at one indicating it is used but doesn't need COW.
        self.set_cluster_refcount(new_addr, 1)?;
        Ok(new_addr)
    }

    // Gets the address of the refcount block and the index into the block for the given address.
    fn get_refcount_index(&self, address: u64) -> std::io::Result<(usize, usize)> {
        let cluster_size: u64 = self.raw_file.cluster_size;
        let block_index = (address / cluster_size) % self.refcount_block_entries;
        let refcount_table_index = (address / cluster_size) / self.refcount_block_entries;
        Ok((refcount_table_index as usize, block_index as usize))
    }

    // Set the refcount for a cluster with the given address.
    fn set_cluster_refcount(&mut self, address: u64, refcount: u16) -> std::io::Result<()> {
        let (table_index, block_index) = self.get_refcount_index(address)?;
        let stored_addr = self.ref_table[table_index];
        let mut new_cluster = None;
        let mut old_cluster = None;
        if !self.refblock_cache.contains(table_index) {
            let table = if stored_addr == 0 {
                let new_addr: u64 = Self::get_new_cluster(&mut self.raw_file,
                                                          &mut self.avail_clusters)?;
                self.ref_table[table_index] = new_addr;
                new_cluster = Some(new_addr);
                VecCache::new(self.l2_entries as usize)
            } else {
                VecCache::from_vec(self.raw_file.read_refcount_block(stored_addr)?)
            };
            self.cache_refcount_table(table_index as u64, table)?;
        }
        if !self.refblock_cache.get_table(table_index).unwrap().dirty() {
            // Free the previously used block and use a new one. Writing modified counts to new
            // blocks keeps the on-disk state consistent even if it's out of date.
            if stored_addr != 0 {
                self.unref_clusters.push(stored_addr);
                old_cluster = Some(stored_addr);
            }
            let new_addr: u64 = Self::get_new_cluster(&mut self.raw_file,
                                                      &mut self.avail_clusters)?;
            new_cluster = Some(new_addr);
            self.ref_table[table_index] = new_addr;
        }
        self.refblock_cache.get_table_mut(table_index).unwrap().set(block_index, refcount);
        if let Some(old_addr) = old_cluster {
            self.set_cluster_refcount(old_addr, 0)?;
        }
        if let Some(new_addr) = new_cluster {
            self.set_cluster_refcount(new_addr, 1)?;
        }
        Ok(())
    }

    // Gets the refcount for a cluster with the given address.
    fn get_cluster_refcount(&mut self, address: u64) -> std::io::Result<u16> {
        let (table_index, block_index) = self.get_refcount_index(address)?;
        let stored_addr = self.ref_table[table_index];
        if stored_addr == 0 {
            return Ok(0);
        }
        if !self.refblock_cache.contains(table_index) {
            let table = VecCache::from_vec(self.raw_file.read_refcount_block(stored_addr)?);
            self.cache_refcount_table(table_index as u64, table)?;
        }
        Ok(self.refblock_cache.get_table(table_index).unwrap().get(block_index))
    }

    fn sync_caches(&mut self) -> std::io::Result<()> {
        // Write out all dirty L2 tables.
        for (l1_index, l2_table) in self.l2_cache.iter_mut().filter(|(_k, v)| v.dirty())
        {
            // The index must be from valid when we insterted it.
            let addr = *self.l1_table.get(*l1_index).unwrap();
            if addr != 0 {
                self.raw_file.write_pointer_table(addr, l2_table.addrs(), CLUSTER_USED_FLAG)?;
            } else {
                return Err(std::io::Error::from_raw_os_error(EINVAL));
            }
            l2_table.mark_clean();
        }
        // Write the modified refcount blocks.
        for (ref_index, ref_table) in self.refblock_cache.dirty_iter_mut() {
            // The index must be from valid when we insterted it.
            let addr = self.ref_table[*ref_index];
            if addr != 0 {
                self.raw_file.write_refcount_block(addr, ref_table.addrs())?;
            } else {
                return Err(std::io::Error::from_raw_os_error(EINVAL));
            }
            ref_table.mark_clean();
        }
        self.raw_file.file.sync_all()?; // Make sure metadata(file len) and all data clusters are written.

        // Push L1 table and refcount table last as all the clusters they point to are now
        // guaranteed to be valid.
        self.raw_file.write_pointer_table(
            self.header.l1_table_offset,
            &self.l1_table,
            0,
        )?;
        self.raw_file.write_pointer_table(
            self.header.refcount_table_offset,
            &self.ref_table,
            0,
        )?;
        self.raw_file.file.sync_data()?;
        Ok(())
    }
}

impl Drop for QcowFile {
    fn drop(&mut self) {
        let _ = self.sync_caches();
    }
}

impl AsRawFd for QcowFile {
    fn as_raw_fd(&self) -> RawFd {
        self.raw_file.file.as_raw_fd()
    }
}

impl Read for QcowFile {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let address: u64 = self.current_offset as u64;
        let read_count: usize = self.limit_range_file(address, buf.len());

        let mut nread: usize = 0;
        while nread < read_count {
            let curr_addr = address + nread as u64;
            let file_offset = self.file_offset_read(curr_addr)?;
            let count = self.limit_range_cluster(curr_addr, read_count - nread);

            if let Some(offset) = file_offset {
                self.raw_file.file.seek(SeekFrom::Start(offset))?;
                self.raw_file.file.read_exact(&mut buf[nread..(nread + count)])?;
            } else {
                // Previously unwritten region, return zeros
                for b in (&mut buf[nread..(nread + count)]).iter_mut() {
                    *b = 0;
                }
            }

            nread += count;
        }
        self.current_offset += read_count as u64;
        Ok(read_count)
    }
}

impl Seek for QcowFile {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        let new_offset: Option<u64> = match pos {
            SeekFrom::Start(off) => Some(off),
            SeekFrom::End(off) => {
                if off < 0 {
                    0i64.checked_sub(off).and_then(|increment| {
                        self.virtual_size().checked_sub(increment as u64)
                    })
                } else {
                    self.virtual_size().checked_add(off as u64)
                }
            }
            SeekFrom::Current(off) => {
                if off < 0 {
                    0i64.checked_sub(off).and_then(|increment| {
                        self.current_offset.checked_sub(increment as u64)
                    })
                } else {
                    self.current_offset.checked_add(off as u64)
                }
            }
        };

        if let Some(o) = new_offset {
            if o <= self.virtual_size() {
                self.current_offset = o;
                return Ok(o);
            }
        }
        Err(std::io::Error::from_raw_os_error(EINVAL))
    }
}

impl Write for QcowFile {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let address: u64 = self.current_offset as u64;
        let write_count: usize = self.limit_range_file(address, buf.len());

        let mut nwritten: usize = 0;
        while nwritten < write_count {
            let curr_addr = address + nwritten as u64;
            let offset = self.file_offset_write(curr_addr)?;
            let count = self.limit_range_cluster(curr_addr, write_count - nwritten);

            if let Err(e) = self.raw_file.file.seek(SeekFrom::Start(offset)) {
                return Err(e);
            }
            if let Err(e) = self.raw_file.file.write(&buf[nwritten..(nwritten + count)]) {
                return Err(e);
            }

            nwritten += count;
        }
        self.current_offset += write_count as u64;
        Ok(write_count)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.sync_caches()?;
        self.avail_clusters.append(&mut self.unref_clusters);
        Ok(())
    }
}

// Returns an Error if the given offset doesn't align to a cluster boundary.
fn offset_is_cluster_boundary(offset: u64, cluster_bits: u32) -> Result<()> {
    if offset & ((0x01 << cluster_bits) - 1) != 0 {
        return Err(Error::InvalidOffset(offset));
    }
    Ok(())
}

// Ceiling of the division of `dividend`/`divisor`.
fn div_round_up_u64(dividend: u64, divisor: u64) -> u64 {
    (dividend + divisor - 1) / divisor
}

// Ceiling of the division of `dividend`/`divisor`.
fn div_round_up_u32(dividend: u32, divisor: u32) -> u32 {
    (dividend + divisor - 1) / divisor
}

#[cfg(test)]
extern crate sys_util;

#[cfg(test)]
mod tests {
    use std::fs::File;
    use std::io::{Read, Seek, SeekFrom, Write};
    use super::*;
    use sys_util::SharedMemory;
	use test::Bencher;

    fn valid_header() -> Vec<u8> {
        vec![
            0x51u8, 0x46, 0x49, 0xfb, // magic
            0x00, 0x00, 0x00, 0x03, // version
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // backing file offset
            0x00, 0x00, 0x00, 0x00, // backing file size
            0x00, 0x00, 0x00, 0x10, // cluster_bits
            0x00, 0x00, 0x00, 0x20, 0x00, 0x00, 0x00, 0x00, // size
            0x00, 0x00, 0x00, 0x00, // crypt method
            0x00, 0x00, 0x01, 0x00, // L1 size
            0x00, 0x00, 0x00, 0x00, 0x00, 0x04, 0x00, 0x00, // L1 table offset
            0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, // refcount table offset
            0x00, 0x00, 0x00, 0x03, // refcount table clusters
            0x00, 0x00, 0x00, 0x00, // nb snapshots
            0x00, 0x00, 0x00, 0x00, 0x00, 0x04, 0x00, 0x00, // snapshots offset
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // incompatible_features
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // compatible_features
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // autoclear_features
            0x00, 0x00, 0x00, 0x04, // refcount_order
            0x00, 0x00, 0x00, 0x68, // header_length
        ]
    }

    fn with_basic_file<F>(header: &[u8], mut testfn: F)
    where
        F: FnMut(File),
    {
        let shm = SharedMemory::new(None).unwrap();
        let mut disk_file: File = shm.into();
        disk_file.write_all(&header).unwrap();
        disk_file.set_len(0x5_0000).unwrap();
        disk_file.seek(SeekFrom::Start(0)).unwrap();

        testfn(disk_file); // File closed when the function exits.
    }

    fn with_default_file<F>(file_size: u64, mut testfn: F)
    where
        F: FnMut(QcowFile),
    {
        let shm = SharedMemory::new(None).unwrap();
        let qcow_file = QcowFile::new(shm.into(), file_size).unwrap();

        testfn(qcow_file); // File closed when the function exits.
    }

    #[test]
    fn default_header() {
        let header = QcowHeader::create_for_size(0x10_0000);
        let shm = SharedMemory::new(None).unwrap();
        let mut disk_file: File = shm.into();
        header.write_to(&mut disk_file).expect("Failed to write header to shm.");
        disk_file.seek(SeekFrom::Start(0)).unwrap();
        QcowFile::from(disk_file).expect("Failed to create Qcow from default Header");
    }

    #[test]
    fn header_read() {
        with_basic_file(&valid_header(), |mut disk_file: File| {
            QcowHeader::new(&mut disk_file).expect("Failed to create Header.");
        });
    }

    #[test]
    fn invalid_magic() {
        let invalid_header = vec![0x51u8, 0x46, 0x4a, 0xfb];
        with_basic_file(&invalid_header, |mut disk_file: File| {
            QcowHeader::new(&mut disk_file).expect_err("Invalid header worked.");
        });
    }

    #[test]
    fn invalid_refcount_order() {
        let mut header = valid_header();
        header[99] = 2;
        with_basic_file(&header, |disk_file: File| {
            QcowFile::from(disk_file).expect_err("Invalid refcount order worked.");
        });
    }

    #[test]
    fn write_read_start() {
        with_basic_file(&valid_header(), |disk_file: File| {
            let mut q = QcowFile::from(disk_file).unwrap();
            q.write(b"test first bytes").expect(
                "Failed to write test string.",
            );
            let mut buf = [0u8; 4];
            q.seek(SeekFrom::Start(0)).expect("Failed to seek.");
            q.read(&mut buf).expect("Failed to read.");
            assert_eq!(&buf, b"test");
        });
    }

    #[test]
    fn offset_write_read() {
        with_basic_file(&valid_header(), |disk_file: File| {
            let mut q = QcowFile::from(disk_file).unwrap();
            let b = [0x55u8; 0x1000];
            q.seek(SeekFrom::Start(0xfff2000)).expect("Failed to seek.");
            q.write(&b).expect("Failed to write test string.");
            let mut buf = [0u8; 4];
            q.seek(SeekFrom::Start(0xfff2000)).expect("Failed to seek.");
            q.read(&mut buf).expect("Failed to read.");
            assert_eq!(buf[0], 0x55);
        });
    }

    #[test]
    fn test_header() {
        with_basic_file(&valid_header(), |disk_file: File| {
            let q = QcowFile::from(disk_file).unwrap();
            assert_eq!(q.virtual_size(), 0x20_0000_0000);
        });
    }

    #[test]
    fn read_small_buffer() {
        with_basic_file(&valid_header(), |disk_file: File| {
            let mut q = QcowFile::from(disk_file).unwrap();
            let mut b = [5u8; 16];
            q.seek(SeekFrom::Start(1000)).expect("Failed to seek.");
            q.read(&mut b).expect("Failed to read.");
            assert_eq!(0, b[0]);
            assert_eq!(0, b[15]);
        });
    }

    #[test]
    fn replay_ext4() {
        with_basic_file(&valid_header(), |disk_file: File| {
            let mut q = QcowFile::from(disk_file).unwrap();
            const BUF_SIZE: usize = 0x1000;
            let mut b = [0u8; BUF_SIZE];

            struct Transfer {
                pub write: bool,
                pub addr: u64,
            };

            // Write transactions from mkfs.ext4.
            let xfers: Vec<Transfer> = vec![
                Transfer {write: false, addr: 0xfff0000},
                Transfer {write: false, addr: 0xfffe000},
                Transfer {write: false, addr: 0x0},
                Transfer {write: false, addr: 0x1000},
                Transfer {write: false, addr: 0xffff000},
                Transfer {write: false, addr: 0xffdf000},
                Transfer {write: false, addr: 0xfff8000},
                Transfer {write: false, addr: 0xffe0000},
                Transfer {write: false, addr: 0xffce000},
                Transfer {write: false, addr: 0xffb6000},
                Transfer {write: false, addr: 0xffab000},
                Transfer {write: false, addr: 0xffa4000},
                Transfer {write: false, addr: 0xff8e000},
                Transfer {write: false, addr: 0xff86000},
                Transfer {write: false, addr: 0xff84000},
                Transfer {write: false, addr: 0xff89000},
                Transfer {write: false, addr: 0xfe7e000},
                Transfer {write: false, addr: 0x100000},
                Transfer {write: false, addr: 0x3000},
                Transfer {write: false, addr: 0x7000},
                Transfer {write: false, addr: 0xf000},
                Transfer {write: false, addr: 0x2000},
                Transfer {write: false, addr: 0x4000},
                Transfer {write: false, addr: 0x5000},
                Transfer {write: false, addr: 0x6000},
                Transfer {write: false, addr: 0x8000},
                Transfer {write: false, addr: 0x9000},
                Transfer {write: false, addr: 0xa000},
                Transfer {write: false, addr: 0xb000},
                Transfer {write: false, addr: 0xc000},
                Transfer {write: false, addr: 0xd000},
                Transfer {write: false, addr: 0xe000},
                Transfer {write: false, addr: 0x10000},
                Transfer {write: false, addr: 0x11000},
                Transfer {write: false, addr: 0x12000},
                Transfer {write: false, addr: 0x13000},
                Transfer {write: false, addr: 0x14000},
                Transfer {write: false, addr: 0x15000},
                Transfer {write: false, addr: 0x16000},
                Transfer {write: false, addr: 0x17000},
                Transfer {write: false, addr: 0x18000},
                Transfer {write: false, addr: 0x19000},
                Transfer {write: false, addr: 0x1a000},
                Transfer {write: false, addr: 0x1b000},
                Transfer {write: false, addr: 0x1c000},
                Transfer {write: false, addr: 0x1d000},
                Transfer {write: false, addr: 0x1e000},
                Transfer {write: false, addr: 0x1f000},
                Transfer {write: false, addr: 0x21000},
                Transfer {write: false, addr: 0x22000},
                Transfer {write: false, addr: 0x24000},
                Transfer {write: false, addr: 0x40000},
                Transfer {write: false, addr: 0x0},
                Transfer {write: false, addr: 0x3000},
                Transfer {write: false, addr: 0x7000},
                Transfer {write: false, addr: 0x0},
                Transfer {write: false, addr: 0x1000},
                Transfer {write: false, addr: 0x2000},
                Transfer {write: false, addr: 0x3000},
                Transfer {write: false, addr: 0x0},
                Transfer {write: false, addr: 0x449000},
                Transfer {write: false, addr: 0x48000},
                Transfer {write: false, addr: 0x48000},
                Transfer {write: false, addr: 0x448000},
                Transfer {write: false, addr: 0x44a000},
                Transfer {write: false, addr: 0x48000},
                Transfer {write: false, addr: 0x48000},
                Transfer {write: true, addr: 0x0},
                Transfer {write: true, addr: 0x448000},
                Transfer {write: true, addr: 0x449000},
                Transfer {write: true, addr: 0x44a000},
                Transfer {write: true, addr: 0xfff0000},
                Transfer {write: true, addr: 0xfff1000},
                Transfer {write: true, addr: 0xfff2000},
                Transfer {write: true, addr: 0xfff3000},
                Transfer {write: true, addr: 0xfff4000},
                Transfer {write: true, addr: 0xfff5000},
                Transfer {write: true, addr: 0xfff6000},
                Transfer {write: true, addr: 0xfff7000},
                Transfer {write: true, addr: 0xfff8000},
                Transfer {write: true, addr: 0xfff9000},
                Transfer {write: true, addr: 0xfffa000},
                Transfer {write: true, addr: 0xfffb000},
                Transfer {write: true, addr: 0xfffc000},
                Transfer {write: true, addr: 0xfffd000},
                Transfer {write: true, addr: 0xfffe000},
                Transfer {write: true, addr: 0xffff000},
            ];

            for xfer in xfers.iter() {
                q.seek(SeekFrom::Start(xfer.addr)).expect("Failed to seek.");
                if xfer.write {
                    q.write(&b).expect("Failed to write.");
                } else {
                    let read_count: usize = q.read(&mut b).expect("Failed to read.");
                    assert_eq!(read_count, BUF_SIZE);
                }
            }
        });
    }

    #[test]
    fn combo_write_read() {
        with_default_file(1024 * 1024 * 1024 * 256, |mut qcow_file| {
            const NUM_BLOCKS: usize = 555;
            const BLOCK_SIZE: usize = 0x1_0000;
            const OFFSET: usize = 0x1_0000_0020;
            let data = [0x55u8; BLOCK_SIZE];
            let mut readback = [0u8; BLOCK_SIZE];
            for i in 0..NUM_BLOCKS {
                let seek_offset = OFFSET + i * BLOCK_SIZE;
                qcow_file.seek(SeekFrom::Start(seek_offset as u64)).expect("Failed to seek.");
                let nwritten = qcow_file.write(&data).expect("Failed to write test data.");
                assert_eq!(nwritten, BLOCK_SIZE);
                // Read back the data to check it was written correctly.
                qcow_file.seek(SeekFrom::Start(seek_offset as u64)).expect("Failed to seek.");
                let nread = qcow_file.read(&mut readback).expect("Failed to read.");
                assert_eq!(nread, BLOCK_SIZE);
                for (orig, read) in data.iter().zip(readback.iter()) {
                    assert_eq!(orig, read);
                }
            }
            // Check that address 0 is still zeros.
            qcow_file.seek(SeekFrom::Start(0)).expect("Failed to seek.");
            let nread = qcow_file.read(&mut readback).expect("Failed to read.");
            assert_eq!(nread, BLOCK_SIZE);
            for read in readback.iter() {
                assert_eq!(*read, 0);
            }
            // Check the data again after the writes have happened.
            for i in 0..NUM_BLOCKS {
                let seek_offset = OFFSET + i * BLOCK_SIZE;
                qcow_file.seek(SeekFrom::Start(seek_offset as u64)).expect("Failed to seek.");
                let nread = qcow_file.read(&mut readback).expect("Failed to read.");
                assert_eq!(nread, BLOCK_SIZE);
                for (orig, read) in data.iter().zip(readback.iter()) {
                    assert_eq!(orig, read);
                }
            }

            assert_eq!(qcow_file.first_zero_refcount().unwrap(), None);
        });
    }

    #[bench]
    fn bench_grow_blocks(b: &mut Bencher) {
		const WRITE_SIZE: usize = 1024;
		const TOTAL_SIZE: usize = 1024 * 1024 * 50;
		let data = [0xffu8; WRITE_SIZE];
        b.iter(|| {
			with_default_file(1024 * 1024 * 1024 * 256, |mut qcow_file| {
				for _i in 0..TOTAL_SIZE / WRITE_SIZE {
					qcow_file.write(&data).expect("Failed to write test data.");
				}
			});
		});
    }
}
