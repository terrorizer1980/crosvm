// Copyright 2017 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Track memory regions that are mapped to the guest VM.

use std::convert::AsRef;
use std::convert::TryFrom;
use std::fs::File;
use std::io::Read;
use std::io::Write;
use std::marker::Send;
use std::marker::Sync;
use std::mem::size_of;
use std::result;
use std::sync::Arc;

use base::pagesize;
use base::AsRawDescriptor;
use base::AsRawDescriptors;
use base::Error as SysError;
use base::MappedRegion;
use base::MemoryMapping;
use base::MemoryMappingBuilder;
use base::MmapError;
use base::RawDescriptor;
use base::SharedMemory;
use cros_async::mem;
use cros_async::BackingMemory;
use data_model::volatile_memory::*;
use data_model::DataInit;
use remain::sorted;
use thiserror::Error;

use crate::guest_address::GuestAddress;

mod sys;
pub use sys::MemoryPolicy;

#[sorted]
#[derive(Error, Debug)]
pub enum Error {
    #[error("invalid guest address {0}")]
    InvalidGuestAddress(GuestAddress),
    #[error("invalid offset {0}")]
    InvalidOffset(u64),
    #[error("size {0} must not be zero")]
    InvalidSize(usize),
    #[error("invalid guest memory access at addr={0}: {1}")]
    MemoryAccess(GuestAddress, #[source] MmapError),
    #[error("failed to set seals on shm region: {0}")]
    MemoryAddSealsFailed(#[source] SysError),
    #[error("failed to create shm region: {0}")]
    MemoryCreationFailed(#[source] SysError),
    #[error("failed to map guest memory: {0}")]
    MemoryMappingFailed(#[source] MmapError),
    #[error("shm regions must be page aligned")]
    MemoryNotAligned,
    #[error("memory regions overlap")]
    MemoryRegionOverlap,
    #[error("memory region size {0} is too large")]
    MemoryRegionTooLarge(u128),
    #[error("incomplete read of {completed} instead of {expected} bytes")]
    ShortRead { expected: usize, completed: usize },
    #[error("incomplete write of {completed} instead of {expected} bytes")]
    ShortWrite { expected: usize, completed: usize },
    #[error("DescriptorChain split is out of bounds: {0}")]
    SplitOutOfBounds(usize),
    #[error("{0}")]
    VolatileMemoryAccess(#[source] VolatileMemoryError),
}

pub type Result<T> = result::Result<T, Error>;

/// A file-like object backing `MemoryRegion`.
#[derive(Clone, Debug)]
pub enum BackingObject {
    Shm(Arc<SharedMemory>),
    File(Arc<File>),
}

impl AsRawDescriptor for BackingObject {
    fn as_raw_descriptor(&self) -> RawDescriptor {
        match self {
            Self::Shm(shm) => shm.as_raw_descriptor(),
            Self::File(f) => f.as_raw_descriptor(),
        }
    }
}

impl AsRef<dyn AsRawDescriptor + Sync + Send> for BackingObject {
    fn as_ref(&self) -> &(dyn AsRawDescriptor + Sync + Send + 'static) {
        match self {
            BackingObject::Shm(shm) => shm.as_ref(),
            BackingObject::File(f) => f.as_ref(),
        }
    }
}

/// A regions of memory mapped memory.
/// Holds the memory mapping with its offset in guest memory.
/// Also holds the backing object for the mapping and the offset in that object of the mapping.
#[derive(Debug)]
pub struct MemoryRegion {
    mapping: MemoryMapping,
    guest_base: GuestAddress,

    shared_obj: BackingObject,
    obj_offset: u64,
}

impl MemoryRegion {
    /// Creates a new MemoryRegion using the given SharedMemory object to later be attached to a VM
    /// at `guest_base` address in the guest.
    pub fn new_from_shm(
        size: u64,
        guest_base: GuestAddress,
        offset: u64,
        shm: Arc<SharedMemory>,
    ) -> Result<Self> {
        let mapping = MemoryMappingBuilder::new(size as usize)
            .from_shared_memory(shm.as_ref())
            .offset(offset)
            .build()
            .map_err(Error::MemoryMappingFailed)?;
        Ok(MemoryRegion {
            mapping,
            guest_base,
            shared_obj: BackingObject::Shm(shm),
            obj_offset: offset,
        })
    }

    /// Creates a new MemoryRegion using the given file to get available later at `guest_base`
    /// address in the guest.
    pub fn new_from_file(
        size: u64,
        guest_base: GuestAddress,
        offset: u64,
        file: Arc<File>,
    ) -> Result<Self> {
        let mapping = MemoryMappingBuilder::new(size as usize)
            .from_file(&file)
            .offset(offset)
            .build()
            .map_err(Error::MemoryMappingFailed)?;
        Ok(MemoryRegion {
            mapping,
            guest_base,
            shared_obj: BackingObject::File(file),
            obj_offset: offset,
        })
    }

    fn start(&self) -> GuestAddress {
        self.guest_base
    }

    fn end(&self) -> GuestAddress {
        // unchecked_add is safe as the region bounds were checked when it was created.
        self.guest_base.unchecked_add(self.mapping.size() as u64)
    }

    fn contains(&self, addr: GuestAddress) -> bool {
        addr >= self.guest_base && addr < self.end()
    }
}

/// Tracks memory regions and where they are mapped in the guest, along with shm
/// descriptors of the underlying memory regions.
#[derive(Clone, Debug)]
pub struct GuestMemory {
    regions: Arc<[MemoryRegion]>,
}

impl AsRawDescriptors for GuestMemory {
    /// USE WITH CAUTION, the descriptors returned here are not necessarily
    /// files!
    fn as_raw_descriptors(&self) -> Vec<RawDescriptor> {
        self.regions
            .iter()
            .map(|r| r.shared_obj.as_raw_descriptor())
            .collect()
    }
}

impl GuestMemory {
    /// Creates backing shm for GuestMemory regions
    fn create_shm(ranges: &[(GuestAddress, u64)]) -> Result<SharedMemory> {
        let mut aligned_size = 0;
        let pg_size = pagesize();
        for range in ranges {
            if range.1 % pg_size as u64 != 0 {
                return Err(Error::MemoryNotAligned);
            }

            aligned_size += range.1;
        }

        // NOTE: Some tests rely on the GuestMemory's name when capturing metrics.
        let name = "crosvm_guest";
        // Shm must be mut even though it is only updated on Unix systems.
        #[allow(unused_mut)]
        let mut shm = SharedMemory::new(name, aligned_size).map_err(Error::MemoryCreationFailed)?;

        sys::finalize_shm(&mut shm)?;

        Ok(shm)
    }

    /// Creates a container for guest memory regions.
    /// Valid memory regions are specified as a Vec of (Address, Size) tuples sorted by Address.
    pub fn new(ranges: &[(GuestAddress, u64)]) -> Result<GuestMemory> {
        // Create shm
        let shm = Arc::new(GuestMemory::create_shm(ranges)?);

        // Create memory regions
        let mut regions = Vec::<MemoryRegion>::new();
        let mut offset = 0;

        for range in ranges {
            if let Some(last) = regions.last() {
                if last
                    .guest_base
                    .checked_add(last.mapping.size() as u64)
                    .map_or(true, |a| a > range.0)
                {
                    return Err(Error::MemoryRegionOverlap);
                }
            }

            let size = usize::try_from(range.1)
                .map_err(|_| Error::MemoryRegionTooLarge(range.1 as u128))?;
            let mapping = MemoryMappingBuilder::new(size)
                .from_shared_memory(shm.as_ref())
                .offset(offset)
                .build()
                .map_err(Error::MemoryMappingFailed)?;

            regions.push(MemoryRegion {
                mapping,
                guest_base: range.0,
                shared_obj: BackingObject::Shm(shm.clone()),
                obj_offset: offset,
            });

            offset += size as u64;
        }

        Ok(GuestMemory {
            regions: Arc::from(regions),
        })
    }

    /// Creates a `GuestMemory` from a collection of MemoryRegions.
    pub fn from_regions(mut regions: Vec<MemoryRegion>) -> Result<Self> {
        // Sort the regions and ensure non overlap.
        regions.sort_by(|a, b| a.guest_base.cmp(&b.guest_base));

        if regions.len() > 1 {
            let mut prev_end = regions[0]
                .guest_base
                .checked_add(regions[0].mapping.size() as u64)
                .ok_or(Error::MemoryRegionOverlap)?;
            for region in &regions[1..] {
                if prev_end > region.guest_base {
                    return Err(Error::MemoryRegionOverlap);
                }
                prev_end = region
                    .guest_base
                    .checked_add(region.mapping.size() as u64)
                    .ok_or(Error::MemoryRegionTooLarge(
                        region.guest_base.0 as u128 + region.mapping.size() as u128,
                    ))?;
            }
        }

        Ok(GuestMemory {
            regions: Arc::from(regions),
        })
    }

    /// Returns the end address of memory.
    ///
    /// # Examples
    ///
    /// ```
    /// # use base::MemoryMapping;
    /// # use vm_memory::{GuestAddress, GuestMemory};
    /// # fn test_end_addr() -> Result<(), ()> {
    ///     let start_addr = GuestAddress(0x1000);
    ///     let mut gm = GuestMemory::new(&vec![(start_addr, 0x400)]).map_err(|_| ())?;
    ///     assert_eq!(start_addr.checked_add(0x400), Some(gm.end_addr()));
    ///     Ok(())
    /// # }
    /// ```
    pub fn end_addr(&self) -> GuestAddress {
        self.regions
            .iter()
            .max_by_key(|region| region.start())
            .map_or(GuestAddress(0), MemoryRegion::end)
    }

    /// Returns the total size of memory in bytes.
    pub fn memory_size(&self) -> u64 {
        self.regions
            .iter()
            .map(|region| region.mapping.size() as u64)
            .sum()
    }

    /// Returns true if the given address is within the memory range available to the guest.
    pub fn address_in_range(&self, addr: GuestAddress) -> bool {
        self.regions.iter().any(|region| region.contains(addr))
    }

    /// Returns true if the given range (start, end) is overlap with the memory range
    /// available to the guest.
    pub fn range_overlap(&self, start: GuestAddress, end: GuestAddress) -> bool {
        self.regions
            .iter()
            .any(|region| region.start() < end && start < region.end())
    }

    /// Returns an address `addr + offset` if it's in range.
    ///
    /// This function doesn't care whether a region `[addr, addr + offset)` is in range or not. To
    /// guarantee it's a valid range, use `is_valid_range()` instead.
    pub fn checked_offset(&self, addr: GuestAddress, offset: u64) -> Option<GuestAddress> {
        addr.checked_add(offset).and_then(|a| {
            if self.address_in_range(a) {
                Some(a)
            } else {
                None
            }
        })
    }

    /// Returns true if the given range `[start, start + length)` is a valid contiguous memory
    /// range available to the guest and it's backed by a single underlying memory region.
    pub fn is_valid_range(&self, start: GuestAddress, length: u64) -> bool {
        if length == 0 {
            return false;
        }

        let end = if let Some(end) = start.checked_add(length - 1) {
            end
        } else {
            return false;
        };

        self.regions
            .iter()
            .any(|region| region.start() <= start && end < region.end())
    }

    /// Returns the size of the memory region in bytes.
    pub fn num_regions(&self) -> u64 {
        self.regions.len() as u64
    }

    /// Perform the specified action on each region's addresses.
    ///
    /// Callback is called with arguments:
    ///  * index: usize
    ///  * guest_addr : GuestAddress
    ///  * size: usize
    ///  * host_addr: usize
    ///  * shm: Descriptor of the backing memory region
    ///  * shm_offset: usize
    pub fn with_regions<F, E>(&self, mut cb: F) -> result::Result<(), E>
    where
        F: FnMut(usize, GuestAddress, usize, usize, &BackingObject, u64) -> result::Result<(), E>,
    {
        for (index, region) in self.regions.iter().enumerate() {
            cb(
                index,
                region.start(),
                region.mapping.size(),
                region.mapping.as_ptr() as usize,
                &region.shared_obj,
                region.obj_offset,
            )?;
        }
        Ok(())
    }

    /// Writes a slice to guest memory at the specified guest address.
    /// Returns the number of bytes written.  The number of bytes written can
    /// be less than the length of the slice if there isn't enough room in the
    /// memory region.
    ///
    /// # Examples
    /// * Write a slice at guestaddress 0x200.
    ///
    /// ```
    /// # use base::MemoryMapping;
    /// # use vm_memory::{GuestAddress, GuestMemory};
    /// # fn test_write_u64() -> Result<(), ()> {
    /// #   let start_addr = GuestAddress(0x1000);
    /// #   let mut gm = GuestMemory::new(&vec![(start_addr, 0x400)]).map_err(|_| ())?;
    ///     let res = gm.write_at_addr(&[1,2,3,4,5], GuestAddress(0x200)).map_err(|_| ())?;
    ///     assert_eq!(5, res);
    ///     Ok(())
    /// # }
    /// ```
    pub fn write_at_addr(&self, buf: &[u8], guest_addr: GuestAddress) -> Result<usize> {
        self.do_in_region(guest_addr, move |mapping, offset, _| {
            mapping
                .write_slice(buf, offset)
                .map_err(|e| Error::MemoryAccess(guest_addr, e))
        })
    }

    /// Writes the entire contents of a slice to guest memory at the specified
    /// guest address.
    ///
    /// Returns an error if there isn't enough room in the memory region to
    /// complete the entire write. Part of the data may have been written
    /// nevertheless.
    ///
    /// # Examples
    ///
    /// ```
    /// use vm_memory::{guest_memory, GuestAddress, GuestMemory};
    ///
    /// fn test_write_all() -> guest_memory::Result<()> {
    ///     let ranges = &[(GuestAddress(0x1000), 0x400)];
    ///     let gm = GuestMemory::new(ranges)?;
    ///     gm.write_all_at_addr(b"zyxwvut", GuestAddress(0x1200))
    /// }
    /// ```
    pub fn write_all_at_addr(&self, buf: &[u8], guest_addr: GuestAddress) -> Result<()> {
        let expected = buf.len();
        let completed = self.write_at_addr(buf, guest_addr)?;
        if expected == completed {
            Ok(())
        } else {
            Err(Error::ShortWrite {
                expected,
                completed,
            })
        }
    }

    /// Reads to a slice from guest memory at the specified guest address.
    /// Returns the number of bytes read.  The number of bytes read can
    /// be less than the length of the slice if there isn't enough room in the
    /// memory region.
    ///
    /// # Examples
    /// * Read a slice of length 16 at guestaddress 0x200.
    ///
    /// ```
    /// # use base::MemoryMapping;
    /// # use vm_memory::{GuestAddress, GuestMemory};
    /// # fn test_write_u64() -> Result<(), ()> {
    /// #   let start_addr = GuestAddress(0x1000);
    /// #   let mut gm = GuestMemory::new(&vec![(start_addr, 0x400)]).map_err(|_| ())?;
    ///     let buf = &mut [0u8; 16];
    ///     let res = gm.read_at_addr(buf, GuestAddress(0x200)).map_err(|_| ())?;
    ///     assert_eq!(16, res);
    ///     Ok(())
    /// # }
    /// ```
    pub fn read_at_addr(&self, buf: &mut [u8], guest_addr: GuestAddress) -> Result<usize> {
        self.do_in_region(guest_addr, move |mapping, offset, _| {
            mapping
                .read_slice(buf, offset)
                .map_err(|e| Error::MemoryAccess(guest_addr, e))
        })
    }

    /// Reads from guest memory at the specified address to fill the entire
    /// buffer.
    ///
    /// Returns an error if there isn't enough room in the memory region to fill
    /// the entire buffer. Part of the buffer may have been filled nevertheless.
    ///
    /// # Examples
    ///
    /// ```
    /// use vm_memory::{guest_memory, GuestAddress, GuestMemory};
    ///
    /// fn test_read_exact() -> guest_memory::Result<()> {
    ///     let ranges = &[(GuestAddress(0x1000), 0x400)];
    ///     let gm = GuestMemory::new(ranges)?;
    ///     let mut buffer = [0u8; 0x200];
    ///     gm.read_exact_at_addr(&mut buffer, GuestAddress(0x1200))
    /// }
    /// ```
    pub fn read_exact_at_addr(&self, buf: &mut [u8], guest_addr: GuestAddress) -> Result<()> {
        let expected = buf.len();
        let completed = self.read_at_addr(buf, guest_addr)?;
        if expected == completed {
            Ok(())
        } else {
            Err(Error::ShortRead {
                expected,
                completed,
            })
        }
    }

    /// Reads an object from guest memory at the given guest address.
    /// Reading from a volatile area isn't strictly safe as it could change
    /// mid-read.  However, as long as the type T is plain old data and can
    /// handle random initialization, everything will be OK.
    ///
    /// # Examples
    /// * Read a u64 from two areas of guest memory backed by separate mappings.
    ///
    /// ```
    /// # use base::MemoryMapping;
    /// # use vm_memory::{GuestAddress, GuestMemory};
    /// # fn test_read_u64() -> Result<u64, ()> {
    /// #     let start_addr1 = GuestAddress(0x0);
    /// #     let start_addr2 = GuestAddress(0x400);
    /// #     let mut gm = GuestMemory::new(&vec![(start_addr1, 0x400), (start_addr2, 0x400)])
    /// #         .map_err(|_| ())?;
    ///       let num1: u64 = gm.read_obj_from_addr(GuestAddress(32)).map_err(|_| ())?;
    ///       let num2: u64 = gm.read_obj_from_addr(GuestAddress(0x400+32)).map_err(|_| ())?;
    /// #     Ok(num1 + num2)
    /// # }
    /// ```
    pub fn read_obj_from_addr<T: DataInit>(&self, guest_addr: GuestAddress) -> Result<T> {
        self.do_in_region(guest_addr, |mapping, offset, _| {
            mapping
                .read_obj(offset)
                .map_err(|e| Error::MemoryAccess(guest_addr, e))
        })
    }

    /// Writes an object to the memory region at the specified guest address.
    /// Returns Ok(()) if the object fits, or Err if it extends past the end.
    ///
    /// # Examples
    /// * Write a u64 at guest address 0x1100.
    ///
    /// ```
    /// # use base::MemoryMapping;
    /// # use vm_memory::{GuestAddress, GuestMemory};
    /// # fn test_write_u64() -> Result<(), ()> {
    /// #   let start_addr = GuestAddress(0x1000);
    /// #   let mut gm = GuestMemory::new(&vec![(start_addr, 0x400)]).map_err(|_| ())?;
    ///     gm.write_obj_at_addr(55u64, GuestAddress(0x1100))
    ///         .map_err(|_| ())
    /// # }
    /// ```
    pub fn write_obj_at_addr<T: DataInit>(&self, val: T, guest_addr: GuestAddress) -> Result<()> {
        self.do_in_region(guest_addr, move |mapping, offset, _| {
            mapping
                .write_obj(val, offset)
                .map_err(|e| Error::MemoryAccess(guest_addr, e))
        })
    }

    /// Returns a `VolatileSlice` of `len` bytes starting at `addr`. Returns an error if the slice
    /// is not a subset of this `GuestMemory`.
    ///
    /// # Examples
    /// * Write `99` to 30 bytes starting at guest address 0x1010.
    ///
    /// ```
    /// # use base::MemoryMapping;
    /// # use vm_memory::{GuestAddress, GuestMemory, GuestMemoryError};
    /// # fn test_volatile_slice() -> Result<(), GuestMemoryError> {
    /// #   let start_addr = GuestAddress(0x1000);
    /// #   let mut gm = GuestMemory::new(&vec![(start_addr, 0x400)])?;
    ///     let vslice = gm.get_slice_at_addr(GuestAddress(0x1010), 30)?;
    ///     vslice.write_bytes(99);
    /// #   Ok(())
    /// # }
    /// ```
    pub fn get_slice_at_addr(&self, addr: GuestAddress, len: usize) -> Result<VolatileSlice> {
        self.regions
            .iter()
            .find(|region| region.contains(addr))
            .ok_or(Error::InvalidGuestAddress(addr))
            .and_then(|region| {
                // The cast to a usize is safe here because we know that `region.contains(addr)` and
                // it's not possible for a memory region to be larger than what fits in a usize.
                region
                    .mapping
                    .get_slice(addr.offset_from(region.start()) as usize, len)
                    .map_err(Error::VolatileMemoryAccess)
            })
    }

    /// Returns a `VolatileRef` to an object at `addr`. Returns Ok(()) if the object fits, or Err if
    /// it extends past the end.
    ///
    /// # Examples
    /// * Get a &u64 at offset 0x1010.
    ///
    /// ```
    /// # use base::MemoryMapping;
    /// # use vm_memory::{GuestAddress, GuestMemory, GuestMemoryError};
    /// # fn test_ref_u64() -> Result<(), GuestMemoryError> {
    /// #   let start_addr = GuestAddress(0x1000);
    /// #   let mut gm = GuestMemory::new(&vec![(start_addr, 0x400)])?;
    ///     gm.write_obj_at_addr(47u64, GuestAddress(0x1010))?;
    ///     let vref = gm.get_ref_at_addr::<u64>(GuestAddress(0x1010))?;
    ///     assert_eq!(vref.load(), 47u64);
    /// #   Ok(())
    /// # }
    /// ```
    pub fn get_ref_at_addr<T: DataInit>(&self, addr: GuestAddress) -> Result<VolatileRef<T>> {
        let buf = self.get_slice_at_addr(addr, size_of::<T>())?;
        // Safe because we have know that `buf` is at least `size_of::<T>()` bytes and that the
        // returned reference will not outlive this `GuestMemory`.
        Ok(unsafe { VolatileRef::new(buf.as_mut_ptr() as *mut T) })
    }

    /// Reads data from a file descriptor and writes it to guest memory.
    ///
    /// # Arguments
    /// * `guest_addr` - Begin writing memory at this offset.
    /// * `src` - Read from `src` to memory.
    /// * `count` - Read `count` bytes from `src` to memory.
    ///
    /// # Examples
    ///
    /// * Read bytes from /dev/urandom
    ///
    /// ```
    /// # use base::MemoryMapping;
    /// # use vm_memory::{GuestAddress, GuestMemory};
    /// # use std::fs::File;
    /// # use std::path::Path;
    /// # fn test_read_random() -> Result<u32, ()> {
    /// #     let start_addr = GuestAddress(0x1000);
    /// #     let gm = GuestMemory::new(&vec![(start_addr, 0x400)]).map_err(|_| ())?;
    ///       let mut file = File::open(Path::new("/dev/urandom")).map_err(|_| ())?;
    ///       let addr = GuestAddress(0x1010);
    ///       gm.read_to_memory(addr, &mut file, 128).map_err(|_| ())?;
    ///       let read_addr = addr.checked_add(8).ok_or(())?;
    ///       let rand_val: u32 = gm.read_obj_from_addr(read_addr).map_err(|_| ())?;
    /// #     Ok(rand_val)
    /// # }
    /// ```
    pub fn read_to_memory<F: Read + AsRawDescriptor>(
        &self,
        guest_addr: GuestAddress,
        src: &mut F,
        count: usize,
    ) -> Result<()> {
        self.do_in_region(guest_addr, move |mapping, offset, _| {
            mapping
                .read_to_memory(offset, src, count)
                .map_err(|e| Error::MemoryAccess(guest_addr, e))
        })
    }

    /// Writes data from memory to a file descriptor.
    ///
    /// # Arguments
    /// * `guest_addr` - Begin reading memory from this offset.
    /// * `dst` - Write from memory to `dst`.
    /// * `count` - Read `count` bytes from memory to `src`.
    ///
    /// # Examples
    ///
    /// * Write 128 bytes to /dev/null
    ///
    /// ```
    /// # use base::MemoryMapping;
    /// # use vm_memory::{GuestAddress, GuestMemory};
    /// # use std::fs::File;
    /// # use std::path::Path;
    /// # fn test_write_null() -> Result<(), ()> {
    /// #     let start_addr = GuestAddress(0x1000);
    /// #     let gm = GuestMemory::new(&vec![(start_addr, 0x400)]).map_err(|_| ())?;
    ///       let mut file = File::open(Path::new("/dev/null")).map_err(|_| ())?;
    ///       let addr = GuestAddress(0x1010);
    ///       gm.write_from_memory(addr, &mut file, 128).map_err(|_| ())?;
    /// #     Ok(())
    /// # }
    /// ```
    pub fn write_from_memory<F: Write + AsRawDescriptor>(
        &self,
        guest_addr: GuestAddress,
        dst: &mut F,
        count: usize,
    ) -> Result<()> {
        self.do_in_region(guest_addr, move |mapping, offset, _| {
            mapping
                .write_from_memory(offset, dst, count)
                .map_err(|e| Error::MemoryAccess(guest_addr, e))
        })
    }

    /// Convert a GuestAddress into a pointer in the address space of this
    /// process. This should only be necessary for giving addresses to the
    /// kernel, as with vhost ioctls. Normal reads/writes to guest memory should
    /// be done through `write_from_memory`, `read_obj_from_addr`, etc.
    ///
    /// # Arguments
    /// * `guest_addr` - Guest address to convert.
    ///
    /// # Examples
    ///
    /// ```
    /// # use vm_memory::{GuestAddress, GuestMemory};
    /// # fn test_host_addr() -> Result<(), ()> {
    ///     let start_addr = GuestAddress(0x1000);
    ///     let mut gm = GuestMemory::new(&vec![(start_addr, 0x500)]).map_err(|_| ())?;
    ///     let addr = gm.get_host_address(GuestAddress(0x1200)).unwrap();
    ///     println!("Host address is {:p}", addr);
    ///     Ok(())
    /// # }
    /// ```
    pub fn get_host_address(&self, guest_addr: GuestAddress) -> Result<*const u8> {
        self.do_in_region(guest_addr, |mapping, offset, _| {
            // This is safe; `do_in_region` already checks that offset is in
            // bounds.
            Ok(unsafe { mapping.as_ptr().add(offset) } as *const u8)
        })
    }

    /// Convert a GuestAddress into a pointer in the address space of this
    /// process, and verify that the provided size define a valid range within
    /// a single memory region. Similar to get_host_address(), this should only
    /// be used for giving addresses to the kernel.
    ///
    /// # Arguments
    /// * `guest_addr` - Guest address to convert.
    /// * `size` - Size of the address range to be converted.
    ///
    /// # Examples
    ///
    /// ```
    /// # use vm_memory::{GuestAddress, GuestMemory};
    /// # fn test_host_addr() -> Result<(), ()> {
    ///     let start_addr = GuestAddress(0x1000);
    ///     let mut gm = GuestMemory::new(&vec![(start_addr, 0x500)]).map_err(|_| ())?;
    ///     let addr = gm.get_host_address_range(GuestAddress(0x1200), 0x200).unwrap();
    ///     println!("Host address is {:p}", addr);
    ///     Ok(())
    /// # }
    /// ```
    pub fn get_host_address_range(
        &self,
        guest_addr: GuestAddress,
        size: usize,
    ) -> Result<*const u8> {
        if size == 0 {
            return Err(Error::InvalidSize(size));
        }

        // Assume no overlap among regions
        self.do_in_region(guest_addr, |mapping, offset, _| {
            if mapping
                .size()
                .checked_sub(offset)
                .map_or(true, |v| v < size)
            {
                return Err(Error::InvalidGuestAddress(guest_addr));
            }

            // This is safe; `do_in_region` already checks that offset is in
            // bounds.
            Ok(unsafe { mapping.as_ptr().add(offset) } as *const u8)
        })
    }

    /// Returns a reference to the region that backs the given address.
    pub fn shm_region(
        &self,
        guest_addr: GuestAddress,
    ) -> Result<&(dyn AsRawDescriptor + Send + Sync)> {
        self.regions
            .iter()
            .find(|region| region.contains(guest_addr))
            .ok_or(Error::InvalidGuestAddress(guest_addr))
            .map(|region| region.shared_obj.as_ref())
    }

    /// Returns the region that contains the memory at `offset` from the base of guest memory.
    pub fn offset_region(&self, offset: u64) -> Result<&(dyn AsRawDescriptor + Send + Sync)> {
        self.shm_region(
            self.checked_offset(self.regions[0].guest_base, offset)
                .ok_or(Error::InvalidOffset(offset))?,
        )
    }

    /// Loops over all guest memory regions of `self`, and performs the callback function `F` in
    /// the target region that contains `guest_addr`.  The callback function `F` takes in:
    ///
    /// (i) the memory mapping associated with the target region.
    /// (ii) the relative offset from the start of the target region to `guest_addr`.
    /// (iii) the absolute offset from the start of the memory mapping to the target region.
    ///
    /// If no target region is found, an error is returned.  The callback function `F` may return
    /// an Ok(`T`) on success or a `GuestMemoryError` on failure.
    pub fn do_in_region<F, T>(&self, guest_addr: GuestAddress, cb: F) -> Result<T>
    where
        F: FnOnce(&MemoryMapping, usize, u64) -> Result<T>,
    {
        self.regions
            .iter()
            .find(|region| region.contains(guest_addr))
            .ok_or(Error::InvalidGuestAddress(guest_addr))
            .and_then(|region| {
                cb(
                    &region.mapping,
                    guest_addr.offset_from(region.start()) as usize,
                    region.obj_offset,
                )
            })
    }

    /// Convert a GuestAddress into an offset within the associated shm region.
    ///
    /// Due to potential gaps within GuestMemory, it is helpful to know the
    /// offset within the shm where a given address is found. This offset
    /// can then be passed to another process mapping the shm to read data
    /// starting at that address.
    ///
    /// # Arguments
    /// * `guest_addr` - Guest address to convert.
    ///
    /// # Examples
    ///
    /// ```
    /// # use vm_memory::{GuestAddress, GuestMemory};
    /// let addr_a = GuestAddress(0x10000);
    /// let addr_b = GuestAddress(0x80000);
    /// let mut gm = GuestMemory::new(&vec![
    ///     (addr_a, 0x20000),
    ///     (addr_b, 0x30000)]).expect("failed to create GuestMemory");
    /// let offset = gm.offset_from_base(GuestAddress(0x95000))
    ///                .expect("failed to get offset");
    /// assert_eq!(offset, 0x35000);
    /// ```
    pub fn offset_from_base(&self, guest_addr: GuestAddress) -> Result<u64> {
        self.regions
            .iter()
            .find(|region| region.contains(guest_addr))
            .ok_or(Error::InvalidGuestAddress(guest_addr))
            .map(|region| region.obj_offset + guest_addr.offset_from(region.start()))
    }
}

// It is safe to implement BackingMemory because GuestMemory can be mutated any time already.
unsafe impl BackingMemory for GuestMemory {
    fn get_volatile_slice(
        &self,
        mem_range: cros_async::MemRegion,
    ) -> mem::Result<VolatileSlice<'_>> {
        self.get_slice_at_addr(GuestAddress(mem_range.offset as u64), mem_range.len)
            .map_err(|_| mem::Error::InvalidOffset(mem_range.offset, mem_range.len))
    }
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use base::kernel_has_memfd;

    use super::*;

    #[test]
    fn test_alignment() {
        let start_addr1 = GuestAddress(0x0);
        let start_addr2 = GuestAddress(0x10000);

        assert!(GuestMemory::new(&[(start_addr1, 0x100), (start_addr2, 0x400)]).is_err());
        assert!(GuestMemory::new(&[(start_addr1, 0x10000), (start_addr2, 0x10000)]).is_ok());
    }

    #[test]
    fn two_regions() {
        let start_addr1 = GuestAddress(0x0);
        let start_addr2 = GuestAddress(0x10000);
        // The memory regions are `[0x0, 0x10000)`, `[0x10000, 0x20000)`.
        let gm = GuestMemory::new(&[(start_addr1, 0x10000), (start_addr2, 0x10000)]).unwrap();

        // Although each address in `[0x0, 0x20000)` is valid, `is_valid_range()` returns false for
        // a range that is across multiple underlying regions.
        assert!(gm.is_valid_range(GuestAddress(0x5000), 0x5000));
        assert!(gm.is_valid_range(GuestAddress(0x10000), 0x5000));
        assert!(!gm.is_valid_range(GuestAddress(0x5000), 0x10000));
    }

    #[test]
    fn overlap_memory() {
        let start_addr1 = GuestAddress(0x0);
        let start_addr2 = GuestAddress(0x10000);
        assert!(GuestMemory::new(&[(start_addr1, 0x20000), (start_addr2, 0x20000)]).is_err());
    }

    #[test]
    fn region_hole() {
        let start_addr1 = GuestAddress(0x0);
        let start_addr2 = GuestAddress(0x40000);
        // The memory regions are `[0x0, 0x20000)`, `[0x40000, 0x60000)`.
        let gm = GuestMemory::new(&[(start_addr1, 0x20000), (start_addr2, 0x20000)]).unwrap();

        assert!(gm.address_in_range(GuestAddress(0x10000)));
        assert!(!gm.address_in_range(GuestAddress(0x30000)));
        assert!(gm.address_in_range(GuestAddress(0x50000)));
        assert!(!gm.address_in_range(GuestAddress(0x60000)));
        assert!(!gm.address_in_range(GuestAddress(0x60000)));
        assert!(gm.range_overlap(GuestAddress(0x10000), GuestAddress(0x30000)),);
        assert!(!gm.range_overlap(GuestAddress(0x30000), GuestAddress(0x40000)),);
        assert!(gm.range_overlap(GuestAddress(0x30000), GuestAddress(0x70000)),);
        assert_eq!(gm.checked_offset(GuestAddress(0x10000), 0x10000), None);
        assert_eq!(
            gm.checked_offset(GuestAddress(0x50000), 0x8000),
            Some(GuestAddress(0x58000))
        );
        assert_eq!(gm.checked_offset(GuestAddress(0x50000), 0x10000), None);
        assert!(gm.is_valid_range(GuestAddress(0x0), 0x10000));
        assert!(gm.is_valid_range(GuestAddress(0x0), 0x20000));
        assert!(!gm.is_valid_range(GuestAddress(0x0), 0x20000 + 1));

        // While `checked_offset(GuestAddress(0x10000), 0x40000)` succeeds because 0x50000 is a
        // valid address, `is_valid_range(GuestAddress(0x10000), 0x40000)` returns `false`
        // because there is a hole inside of [0x10000, 0x50000).
        assert_eq!(
            gm.checked_offset(GuestAddress(0x10000), 0x40000),
            Some(GuestAddress(0x50000))
        );
        assert!(!gm.is_valid_range(GuestAddress(0x10000), 0x40000));
    }

    #[test]
    fn test_read_u64() {
        let start_addr1 = GuestAddress(0x0);
        let start_addr2 = GuestAddress(0x10000);
        let gm = GuestMemory::new(&[(start_addr1, 0x10000), (start_addr2, 0x10000)]).unwrap();

        let val1: u64 = 0xaa55aa55aa55aa55;
        let val2: u64 = 0x55aa55aa55aa55aa;
        gm.write_obj_at_addr(val1, GuestAddress(0x500)).unwrap();
        gm.write_obj_at_addr(val2, GuestAddress(0x10000 + 32))
            .unwrap();
        let num1: u64 = gm.read_obj_from_addr(GuestAddress(0x500)).unwrap();
        let num2: u64 = gm.read_obj_from_addr(GuestAddress(0x10000 + 32)).unwrap();
        assert_eq!(val1, num1);
        assert_eq!(val2, num2);
    }

    #[test]
    fn test_ref_load_u64() {
        let start_addr1 = GuestAddress(0x0);
        let start_addr2 = GuestAddress(0x10000);
        let gm = GuestMemory::new(&[(start_addr1, 0x10000), (start_addr2, 0x10000)]).unwrap();

        let val1: u64 = 0xaa55aa55aa55aa55;
        let val2: u64 = 0x55aa55aa55aa55aa;
        gm.write_obj_at_addr(val1, GuestAddress(0x500)).unwrap();
        gm.write_obj_at_addr(val2, GuestAddress(0x10000 + 32))
            .unwrap();
        let num1: u64 = gm.get_ref_at_addr(GuestAddress(0x500)).unwrap().load();
        let num2: u64 = gm
            .get_ref_at_addr(GuestAddress(0x10000 + 32))
            .unwrap()
            .load();
        assert_eq!(val1, num1);
        assert_eq!(val2, num2);
    }

    #[test]
    fn test_ref_store_u64() {
        let start_addr1 = GuestAddress(0x0);
        let start_addr2 = GuestAddress(0x10000);
        let gm = GuestMemory::new(&[(start_addr1, 0x10000), (start_addr2, 0x10000)]).unwrap();

        let val1: u64 = 0xaa55aa55aa55aa55;
        let val2: u64 = 0x55aa55aa55aa55aa;
        gm.get_ref_at_addr(GuestAddress(0x500)).unwrap().store(val1);
        gm.get_ref_at_addr(GuestAddress(0x1000 + 32))
            .unwrap()
            .store(val2);
        let num1: u64 = gm.read_obj_from_addr(GuestAddress(0x500)).unwrap();
        let num2: u64 = gm.read_obj_from_addr(GuestAddress(0x1000 + 32)).unwrap();
        assert_eq!(val1, num1);
        assert_eq!(val2, num2);
    }

    #[test]
    fn test_memory_size() {
        let start_region1 = GuestAddress(0x0);
        let size_region1 = 0x10000;
        let start_region2 = GuestAddress(0x10000);
        let size_region2 = 0x20000;
        let gm = GuestMemory::new(&[(start_region1, size_region1), (start_region2, size_region2)])
            .unwrap();

        let mem_size = gm.memory_size();
        assert_eq!(mem_size, size_region1 + size_region2);
    }

    // Get the base address of the mapping for a GuestAddress.
    fn get_mapping(mem: &GuestMemory, addr: GuestAddress) -> Result<*const u8> {
        mem.do_in_region(addr, |mapping, _, _| Ok(mapping.as_ptr() as *const u8))
    }

    #[test]
    fn guest_to_host() {
        let start_addr1 = GuestAddress(0x0);
        let start_addr2 = GuestAddress(0x10000);
        let mem = GuestMemory::new(&[(start_addr1, 0x10000), (start_addr2, 0x40000)]).unwrap();

        // Verify the host addresses match what we expect from the mappings.
        let addr1_base = get_mapping(&mem, start_addr1).unwrap();
        let addr2_base = get_mapping(&mem, start_addr2).unwrap();
        let host_addr1 = mem.get_host_address(start_addr1).unwrap();
        let host_addr2 = mem.get_host_address(start_addr2).unwrap();
        assert_eq!(host_addr1, addr1_base);
        assert_eq!(host_addr2, addr2_base);

        // Check that a bad address returns an error.
        let bad_addr = GuestAddress(0x123456);
        assert!(mem.get_host_address(bad_addr).is_err());
    }

    #[test]
    fn guest_to_host_range() {
        let start_addr1 = GuestAddress(0x0);
        let start_addr2 = GuestAddress(0x10000);
        let mem = GuestMemory::new(&[(start_addr1, 0x10000), (start_addr2, 0x40000)]).unwrap();

        // Verify the host addresses match what we expect from the mappings.
        let addr1_base = get_mapping(&mem, start_addr1).unwrap();
        let addr2_base = get_mapping(&mem, start_addr2).unwrap();
        let host_addr1 = mem.get_host_address_range(start_addr1, 0x10000).unwrap();
        let host_addr2 = mem.get_host_address_range(start_addr2, 0x10000).unwrap();
        assert_eq!(host_addr1, addr1_base);
        assert_eq!(host_addr2, addr2_base);

        let host_addr3 = mem.get_host_address_range(start_addr2, 0x20000).unwrap();
        assert_eq!(host_addr3, addr2_base);

        // Check that a valid guest address with an invalid size returns an error.
        assert!(mem.get_host_address_range(start_addr1, 0x20000).is_err());

        // Check that a bad address returns an error.
        let bad_addr = GuestAddress(0x123456);
        assert!(mem.get_host_address_range(bad_addr, 0x10000).is_err());
    }

    #[test]
    fn shm_offset() {
        #[cfg(unix)]
        {
            // This is simply a sanity check on linux platforms to ensure the tests
            // won't run on old kernels that don't support memfd_create.
            if !kernel_has_memfd() {
                return;
            }
        }

        let start_region1 = GuestAddress(0x0);
        let size_region1 = 0x10000;
        let start_region2 = GuestAddress(0x10000);
        let size_region2 = 0x20000;
        let gm = GuestMemory::new(&[(start_region1, size_region1), (start_region2, size_region2)])
            .unwrap();

        gm.write_obj_at_addr(0x1337u16, GuestAddress(0x0)).unwrap();
        gm.write_obj_at_addr(0x0420u16, GuestAddress(0x10000))
            .unwrap();

        let _ = gm.with_regions::<_, ()>(|index, _, size, _, obj, offset| {
            let shm = match obj {
                BackingObject::Shm(s) => s,
                _ => {
                    panic!("backing object isn't SharedMemory");
                }
            };
            let mmap = MemoryMappingBuilder::new(size)
                .from_shared_memory(shm)
                .offset(offset)
                .build()
                .unwrap();

            if index == 0 {
                assert!(mmap.read_obj::<u16>(0x0).unwrap() == 0x1337u16);
            }

            if index == 1 {
                assert!(mmap.read_obj::<u16>(0x0).unwrap() == 0x0420u16);
            }

            Ok(())
        });
    }
}
