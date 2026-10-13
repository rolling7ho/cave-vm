//! Guest memory.
//!
//! One of the two modules in `vmm` allowed to use `unsafe` (ROADMAP.md D5;
//! the other is `host`, D24). Every `unsafe` block needs a `// SAFETY:`
//! comment.
//!
//! Guest RAM is one or more regions of host memory from `mmap`, either
//! anonymous or backed by a file. A file mapped copy-on-write is how a
//! snapshot's memory is restored, and how forks share it (ROADMAP.md A3).
//!
//! The guest writes this memory whenever a vCPU runs, so the host never
//! holds Rust references into it: every access copies through raw pointers,
//! as if the memory were shared with another process.

#![allow(unsafe_code)]

use std::fmt;

/// Host page size on Apple Silicon. Guest regions start and end on it,
/// because Hypervisor.framework maps whole host pages (ROADMAP.md A3).
pub const PAGE_SIZE: u64 = 16 * 1024;

/// An error building or accessing guest memory.
#[derive(Debug)]
pub enum Error {
    /// A region's guest address or size is not a multiple of [`PAGE_SIZE`].
    Misaligned { gpa: u64, size: u64 },
    /// A region has size zero.
    Empty { gpa: u64 },
    /// A region runs past the end of the 64-bit address space.
    Overflow { gpa: u64, size: u64 },
    /// Two regions overlap.
    Overlap { first: u64, second: u64 },
    /// An access is not inside a single region.
    OutOfRange { gpa: u64, len: usize },
    /// A memory file is shorter than the region mapped from it.
    FileTooSmall { needed: u64, actual: u64 },
    /// `mmap` failed.
    Mmap(std::io::Error),
    /// Hypervisor.framework refused a mapping.
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    Hypervisor(cave_vm_hvf::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Misaligned { gpa, size } => write!(
                f,
                "guest memory region at {gpa:#x} with size {size:#x} is not aligned to {PAGE_SIZE:#x}"
            ),
            Error::Empty { gpa } => write!(f, "guest memory region at {gpa:#x} is empty"),
            Error::Overflow { gpa, size } => {
                write!(
                    f,
                    "guest memory region at {gpa:#x} with size {size:#x} overflows"
                )
            }
            Error::Overlap { first, second } => write!(
                f,
                "guest memory regions at {first:#x} and {second:#x} overlap"
            ),
            Error::OutOfRange { gpa, len } => {
                write!(
                    f,
                    "guest access of {len} bytes at {gpa:#x} is outside guest memory"
                )
            }
            Error::FileTooSmall { needed, actual } => write!(
                f,
                "memory file is {actual} bytes but the region needs {needed}"
            ),
            Error::Mmap(e) => write!(f, "could not allocate guest memory: {e}"),
            #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
            Error::Hypervisor(e) => write!(f, "could not map guest memory: {e}"),
        }
    }
}

impl std::error::Error for Error {}

/// Result of a guest memory operation.
pub type Result<T> = std::result::Result<T, Error>;

/// Checks a set of regions, given as `(guest address, size)`: each is
/// non-empty, page-aligned and in range, and none overlap.
pub fn check_layout(regions: &[(u64, u64)]) -> Result<()> {
    let mut sorted = regions.to_vec();
    sorted.sort_unstable();
    for &(gpa, size) in &sorted {
        if size == 0 {
            return Err(Error::Empty { gpa });
        }
        if gpa % PAGE_SIZE != 0 || size % PAGE_SIZE != 0 {
            return Err(Error::Misaligned { gpa, size });
        }
        if gpa.checked_add(size).is_none() {
            return Err(Error::Overflow { gpa, size });
        }
    }
    for pair in sorted.windows(2) {
        let ((a, a_size), (b, _)) = (pair[0], pair[1]);
        if a + a_size > b {
            return Err(Error::Overlap {
                first: a,
                second: b,
            });
        }
    }
    Ok(())
}

/// Finds the region holding `[gpa, gpa + len)`, returning its index and the
/// offset of `gpa` inside it. Regions are `(guest address, size)`.
pub fn locate(regions: &[(u64, u64)], gpa: u64, len: usize) -> Result<(usize, usize)> {
    let out_of_range = Error::OutOfRange { gpa, len };
    let end = gpa
        .checked_add(len as u64)
        .ok_or(Error::OutOfRange { gpa, len })?;
    regions
        .iter()
        .position(|&(base, size)| gpa >= base && end <= base + size)
        .map(|i| (i, (gpa - regions[i].0) as usize))
        .ok_or(out_of_range)
}

#[cfg(target_os = "macos")]
pub use host::{FileMode, GuestMemory, Region};

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub use host::Mapping;

#[cfg(target_os = "macos")]
mod host {
    use std::ffi::{c_int, c_void};
    use std::fs::File;
    use std::os::fd::AsRawFd;
    use std::ptr::{self, NonNull};
    use std::sync::Arc;

    use super::{Error, Result, check_layout, locate};

    // From <sys/mman.h> on macOS.
    const PROT_READ: c_int = 0x01;
    const PROT_WRITE: c_int = 0x02;
    const MAP_SHARED: c_int = 0x0001;
    const MAP_PRIVATE: c_int = 0x0002;
    const MAP_ANON: c_int = 0x1000;
    const MAP_FAILED: *mut c_void = !0usize as *mut c_void;

    unsafe extern "C" {
        fn mmap(
            addr: *mut c_void,
            len: usize,
            prot: c_int,
            flags: c_int,
            fd: c_int,
            offset: i64,
        ) -> *mut c_void;
        fn munmap(addr: *mut c_void, len: usize) -> c_int;
    }

    /// How a file-backed region shares changes with its file.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum FileMode {
        /// Guest writes stay private to this VM; the file is unchanged.
        /// Restores and forks use this (`MAP_PRIVATE`).
        CopyOnWrite,
        /// Guest writes go to the file (`MAP_SHARED`).
        Shared,
    }

    /// One contiguous range of guest RAM backed by host memory.
    #[derive(Debug)]
    pub struct Region {
        gpa: u64,
        size: usize,
        host: NonNull<u8>,
    }

    // SAFETY: a `Region` owns its mapping exclusively and only exposes it
    // through raw-pointer copies, never references, so moving it to or
    // sharing it with another thread cannot create aliasing references.
    unsafe impl Send for Region {}
    // SAFETY: as above; concurrent copies are the same as the guest and
    // host touching shared memory, which the module docs accept.
    unsafe impl Sync for Region {}

    impl Region {
        /// Allocates zeroed anonymous memory for `[gpa, gpa + size)`.
        pub fn anonymous(gpa: u64, size: u64) -> Result<Region> {
            check_layout(&[(gpa, size)])?;
            Region::map(gpa, size, MAP_PRIVATE | MAP_ANON, -1)
        }

        /// Maps the first `size` bytes of `file` at `[gpa, gpa + size)`. The
        /// file must be at least `size` bytes long, and its contents become
        /// the guest's memory.
        pub fn from_file(file: &File, gpa: u64, size: u64, mode: FileMode) -> Result<Region> {
            check_layout(&[(gpa, size)])?;
            let actual = file.metadata().map_err(Error::Mmap)?.len();
            if actual < size {
                return Err(Error::FileTooSmall {
                    needed: size,
                    actual,
                });
            }
            let flags = match mode {
                FileMode::CopyOnWrite => MAP_PRIVATE,
                FileMode::Shared => MAP_SHARED,
            };
            Region::map(gpa, size, flags, file.as_raw_fd())
        }

        fn map(gpa: u64, size: u64, flags: c_int, fd: c_int) -> Result<Region> {
            let size = usize::try_from(size).map_err(|_| Error::Overflow { gpa, size })?;
            // SAFETY: asks the kernel for a new mapping at an address of its
            // choosing, so no existing memory is affected. `fd` is either -1
            // with MAP_ANON or a file descriptor borrowed for this call; the
            // mapping stays valid after the descriptor is closed.
            let raw = unsafe { mmap(ptr::null_mut(), size, PROT_READ | PROT_WRITE, flags, fd, 0) };
            if raw == MAP_FAILED {
                return Err(Error::Mmap(std::io::Error::last_os_error()));
            }
            let host = NonNull::new(raw.cast::<u8>())
                .ok_or_else(|| Error::Mmap(std::io::Error::other("mmap returned null")))?;
            Ok(Region { gpa, size, host })
        }

        /// Guest physical address of the first byte.
        pub fn gpa(&self) -> u64 {
            self.gpa
        }

        /// Size in bytes.
        pub fn size(&self) -> usize {
            self.size
        }

        /// Host address of the first byte, for alignment checks and
        /// diagnostics. Not for dereferencing outside this module.
        pub fn host_addr(&self) -> usize {
            self.host.as_ptr() as usize
        }
    }

    impl Drop for Region {
        fn drop(&mut self) {
            // SAFETY: `host` and `size` describe a mapping this region created
            // and owns, and nothing else unmaps it. `GuestMemory` keeps
            // regions alive for as long as any hypervisor mapping of them.
            unsafe { munmap(self.host.as_ptr().cast(), self.size) };
        }
    }

    /// All of a guest's RAM. Cheap to clone; clones share the regions.
    #[derive(Debug, Clone)]
    pub struct GuestMemory {
        regions: Arc<Vec<Region>>,
    }

    impl GuestMemory {
        /// Combines regions, rejecting overlaps.
        pub fn new(mut regions: Vec<Region>) -> Result<GuestMemory> {
            regions.sort_unstable_by_key(|r| r.gpa);
            check_layout(&Self::layout_of(&regions))?;
            Ok(GuestMemory {
                regions: Arc::new(regions),
            })
        }

        /// The regions, in guest address order.
        pub fn regions(&self) -> &[Region] {
            &self.regions
        }

        fn layout_of(regions: &[Region]) -> Vec<(u64, u64)> {
            regions.iter().map(|r| (r.gpa, r.size as u64)).collect()
        }

        /// Host pointer for `[gpa, gpa + len)`, which must lie in one region.
        fn host_ptr(&self, gpa: u64, len: usize) -> Result<*mut u8> {
            let (index, offset) = locate(&Self::layout_of(&self.regions), gpa, len)?;
            // SAFETY: `locate` checked that `offset + len` is within the
            // region, so the result stays inside its mapping.
            Ok(unsafe { self.regions[index].host.as_ptr().add(offset) })
        }

        /// Copies guest memory at `gpa` into `buf`.
        pub fn read(&self, gpa: u64, buf: &mut [u8]) -> Result<()> {
            let src = self.host_ptr(gpa, buf.len())?;
            // SAFETY: `src` is valid for `buf.len()` bytes (checked by
            // `host_ptr`), and guest memory never overlaps a host buffer.
            unsafe { ptr::copy_nonoverlapping(src, buf.as_mut_ptr(), buf.len()) };
            Ok(())
        }

        /// Copies `data` into guest memory at `gpa`.
        pub fn write(&self, gpa: u64, data: &[u8]) -> Result<()> {
            let dst = self.host_ptr(gpa, data.len())?;
            // SAFETY: `dst` is valid for `data.len()` bytes (checked by
            // `host_ptr`), and guest memory never overlaps a host buffer.
            unsafe { ptr::copy_nonoverlapping(data.as_ptr(), dst, data.len()) };
            Ok(())
        }

        /// Maps every region into `vm` with read, write and execute access.
        /// The mappings last until the returned [`Mapping`] is dropped, and
        /// the memory stays allocated at least that long.
        #[cfg(target_arch = "aarch64")]
        pub fn map_into<'vm>(&self, vm: &'vm cave_vm_hvf::Vm) -> Result<Mapping<'vm>> {
            let mut mapping = Mapping {
                vm,
                memory: self.clone(),
                mapped: 0,
            };
            for region in self.regions.iter() {
                // SAFETY: the region is page-aligned and page-sized
                // (`check_layout`), and `mapping` holds a clone of the
                // regions, so the memory stays allocated until `mapping`
                // unmaps it on drop. If `mapping` is leaked, the memory is
                // leaked with it rather than freed while mapped. The host
                // only touches it through raw-pointer copies.
                unsafe {
                    vm.map(
                        region.host.as_ptr(),
                        region.gpa,
                        region.size,
                        cave_vm_hvf::MemFlags::RWX,
                    )
                }
                .map_err(Error::Hypervisor)?;
                mapping.mapped += 1;
            }
            Ok(mapping)
        }
    }

    /// Guest memory mapped into a VM. Unmaps on drop.
    #[cfg(target_arch = "aarch64")]
    #[derive(Debug)]
    pub struct Mapping<'vm> {
        vm: &'vm cave_vm_hvf::Vm,
        memory: GuestMemory,
        mapped: usize,
    }

    #[cfg(target_arch = "aarch64")]
    impl Drop for Mapping<'_> {
        fn drop(&mut self) {
            for region in self.memory.regions.iter().take(self.mapped) {
                // Nothing useful can be done with an error while dropping;
                // the VM's own teardown removes any mapping left behind.
                let _ = self.vm.unmap(region.gpa, region.size);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const P: u64 = PAGE_SIZE;

    #[test]
    fn page_aligned_regions_pass() {
        check_layout(&[(0x4000_0000, 64 * P), (0x8000_0000, P)]).unwrap();
    }

    #[test]
    fn misaligned_address_or_size_fails() {
        assert!(matches!(
            check_layout(&[(0x1000, P)]),
            Err(Error::Misaligned { .. })
        ));
        assert!(matches!(
            check_layout(&[(0, P + 4096)]),
            Err(Error::Misaligned { .. })
        ));
    }

    #[test]
    fn empty_region_fails() {
        assert!(matches!(check_layout(&[(0, 0)]), Err(Error::Empty { .. })));
    }

    #[test]
    fn overflow_fails() {
        assert!(matches!(
            check_layout(&[(u64::MAX - P + 1, 2 * P)]),
            Err(Error::Overflow { .. })
        ));
    }

    #[test]
    fn overlap_fails_in_any_order() {
        let err = check_layout(&[(4 * P, 4 * P), (0, 5 * P)]).unwrap_err();
        assert!(matches!(err, Error::Overlap { first: 0, second: x } if x == 4 * P));
        check_layout(&[(4 * P, 4 * P), (0, 4 * P)]).unwrap();
    }

    #[test]
    fn locate_finds_region_and_offset() {
        let regions = [(0, 2 * P), (8 * P, 2 * P)];
        assert_eq!(locate(&regions, 8 * P + 10, 4).unwrap(), (1, 10));
        assert_eq!(
            locate(&regions, 2 * P - 8, 8).unwrap(),
            (0, 2 * P as usize - 8)
        );
    }

    #[test]
    fn locate_rejects_gaps_and_straddles() {
        let regions = [(0, 2 * P), (2 * P, 2 * P)];
        assert!(
            locate(&regions, 2 * P - 4, 8).is_err(),
            "straddles two regions"
        );
        assert!(locate(&regions, 4 * P, 1).is_err(), "past the end");
        assert!(locate(&regions, u64::MAX, 2).is_err(), "wraps around");
    }

    #[cfg(target_os = "macos")]
    mod host {
        use super::*;
        use std::io::{Read, Seek, Write};

        fn temp_file(name: &str, len: usize, fill: u8) -> std::fs::File {
            let path = std::env::temp_dir().join(format!("cave-vm-{name}-{}", std::process::id()));
            let mut file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(true)
                .open(&path)
                .unwrap();
            std::fs::remove_file(&path).unwrap();
            file.write_all(&vec![fill; len]).unwrap();
            file
        }

        #[test]
        fn anonymous_memory_is_zeroed_and_page_aligned() {
            let mem =
                GuestMemory::new(vec![Region::anonymous(0x4000_0000, 4 * P).unwrap()]).unwrap();
            assert_eq!(mem.regions()[0].host_addr() % PAGE_SIZE as usize, 0);
            let mut buf = [0xffu8; 32];
            mem.read(0x4000_0000 + 3 * P, &mut buf).unwrap();
            assert_eq!(buf, [0; 32]);
        }

        #[test]
        fn write_then_read_round_trips() {
            let mem = GuestMemory::new(vec![
                Region::anonymous(0, P).unwrap(),
                Region::anonymous(4 * P, P).unwrap(),
            ])
            .unwrap();
            mem.write(4 * P + 100, b"hello").unwrap();
            let mut buf = [0u8; 5];
            mem.read(4 * P + 100, &mut buf).unwrap();
            assert_eq!(&buf, b"hello");
            assert!(mem.write(P - 2, b"abcd").is_err(), "past the first region");
        }

        #[test]
        fn overlapping_regions_are_rejected() {
            let err = GuestMemory::new(vec![
                Region::anonymous(0, 2 * P).unwrap(),
                Region::anonymous(P, P).unwrap(),
            ])
            .unwrap_err();
            assert!(matches!(err, Error::Overlap { .. }));
        }

        #[test]
        fn copy_on_write_file_is_left_unchanged() {
            let mut file = temp_file("cow", P as usize, 7);
            let mem = GuestMemory::new(vec![
                Region::from_file(&file, 0, P, FileMode::CopyOnWrite).unwrap(),
            ])
            .unwrap();
            let mut first = [0u8; 4];
            mem.read(0, &mut first).unwrap();
            assert_eq!(first, [7; 4], "guest sees the file contents");

            mem.write(0, &[1, 2, 3, 4]).unwrap();
            let mut on_disk = [0u8; 4];
            file.rewind().unwrap();
            file.read_exact(&mut on_disk).unwrap();
            assert_eq!(on_disk, [7; 4], "file unchanged");
        }

        #[test]
        fn shared_file_receives_writes() {
            let mut file = temp_file("shared", P as usize, 0);
            let mem = GuestMemory::new(vec![
                Region::from_file(&file, 0, P, FileMode::Shared).unwrap(),
            ])
            .unwrap();
            mem.write(8, &[9, 9]).unwrap();
            drop(mem);
            let mut on_disk = [0u8; 10];
            file.rewind().unwrap();
            file.read_exact(&mut on_disk).unwrap();
            assert_eq!(&on_disk[8..], &[9, 9]);
        }

        #[test]
        fn short_file_is_rejected() {
            let file = temp_file("short", P as usize, 0);
            let err = Region::from_file(&file, 0, 2 * P, FileMode::CopyOnWrite).unwrap_err();
            assert!(matches!(err, Error::FileTooSmall { .. }));
        }
    }
}
