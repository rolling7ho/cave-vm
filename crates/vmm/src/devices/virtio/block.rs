//! Virtio block device (virtio 1.2, section 5.2) on a raw disk image.
//!
//! Requests are served on the vCPU thread that notifies the queue, in queue
//! order, so a flush completes only after every write the driver submitted
//! before it. `File::sync_all` is `F_FULLFSYNC` on macOS, so a flush
//! reaches stable storage, not just the drive's cache.
//!
//! Discard punches holes in the image file, so blocks the guest frees go
//! back to the Mac. Only whole 4 KiB blocks can be punched; the device
//! advertises that alignment, and the parts of a range outside it are left
//! as they are, which discard allows.

use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt;

use super::queue::{self, DescriptorChain, Queue};
use super::{ActivateError, Activation, Interrupt, VirtioDevice, device_type, interrupt};
use crate::memory::GuestMemory;

/// Bytes per sector, fixed by the virtio spec.
pub const SECTOR_SIZE: u64 = 512;
/// Size of the request queue.
pub const QUEUE_SIZE: u16 = 256;

/// Feature bits (virtio 1.2, 5.2.3).
pub mod features {
    pub const SEG_MAX: u64 = 1 << 2;
    pub const RO: u64 = 1 << 5;
    pub const BLK_SIZE: u64 = 1 << 6;
    pub const FLUSH: u64 = 1 << 9;
    pub const DISCARD: u64 = 1 << 13;
}

/// Request types.
mod req {
    pub const IN: u32 = 0;
    pub const OUT: u32 = 1;
    pub const FLUSH: u32 = 4;
    pub const GET_ID: u32 = 8;
    pub const DISCARD: u32 = 11;
}

/// Request status bytes.
mod status {
    pub const OK: u8 = 0;
    pub const IOERR: u8 = 1;
    pub const UNSUPP: u8 = 2;
}

/// Configuration space up to `discard_sector_alignment`.
const CONFIG_SIZE: usize = 48;
/// Size of the request header: type, reserved, sector.
const HEADER_SIZE: u64 = 16;
/// Length of the device ID string.
const ID_LEN: usize = 20;
/// Most data segments in one request the driver may use, advertised as
/// `seg_max`; the header and status take two more descriptors.
const SEG_MAX: u32 = QUEUE_SIZE as u32 - 2;
/// Most ranges in one discard request, advertised as `max_discard_seg`.
const MAX_DISCARD_SEG: u32 = 32;
/// Size of one discard range: sector, number of sectors, flags.
const DISCARD_RANGE_SIZE: u64 = 16;
/// The `unmap` flag of a discard range, the only flag defined for it.
const DISCARD_UNMAP: u32 = 1;
/// Discard alignment: the APFS block size, the unit holes are punched in.
pub const DISCARD_ALIGN: u64 = 4096;

/// Storage behind a block device. [`File`] implements it for raw images;
/// tests use their own.
pub trait Disk: Send {
    /// Reads exactly `buf.len()` bytes at `offset`.
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<()>;
    /// Writes all of `data` at `offset`.
    fn write_at(&mut self, offset: u64, data: &[u8]) -> io::Result<()>;
    /// Makes every completed write durable.
    fn flush(&mut self) -> io::Result<()>;
    /// Says `len` bytes at `offset` are no longer needed. Afterwards they
    /// may read as zeroes or keep their old contents.
    fn discard(&mut self, offset: u64, len: u64) -> io::Result<()>;
}

impl Disk for File {
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        FileExt::read_exact_at(self, buf, offset)
    }

    fn write_at(&mut self, offset: u64, data: &[u8]) -> io::Result<()> {
        FileExt::write_all_at(self, data, offset)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.sync_all()
    }

    fn discard(&mut self, offset: u64, len: u64) -> io::Result<()> {
        let start = offset.next_multiple_of(DISCARD_ALIGN);
        let end = (offset + len) / DISCARD_ALIGN * DISCARD_ALIGN;
        if end > start {
            crate::host::punch_hole(self, start, end - start)?;
        }
        Ok(())
    }
}

struct Active {
    memory: GuestMemory,
    queue: Queue,
    interrupt: Interrupt,
    /// Set after a malformed queue; the device stops serving it until the
    /// driver resets it.
    broken: bool,
}

/// A virtio block device.
pub struct Block {
    disk: Box<dyn Disk>,
    /// Size in sectors.
    capacity: u64,
    read_only: bool,
    id: [u8; ID_LEN],
    active: Option<Active>,
}

impl Block {
    /// A device over `disk`, which holds `size` bytes. A size that is not a
    /// whole number of sectors is rounded down. `id` is what the guest sees
    /// as the serial number (up to 20 bytes).
    pub fn new(disk: Box<dyn Disk>, size: u64, read_only: bool, id: &str) -> Block {
        let mut id_bytes = [0u8; ID_LEN];
        let n = id.len().min(ID_LEN);
        id_bytes[..n].copy_from_slice(&id.as_bytes()[..n]);
        Block {
            disk,
            capacity: size / SECTOR_SIZE,
            read_only,
            id: id_bytes,
            active: None,
        }
    }

    /// A device over a raw image file, opened read-write unless
    /// `read_only`.
    pub fn open(path: &std::path::Path, read_only: bool, id: &str) -> io::Result<Block> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(!read_only)
            .open(path)?;
        let size = file.metadata()?.len();
        Ok(Block::new(Box::new(file), size, read_only, id))
    }

    fn config(&self) -> [u8; CONFIG_SIZE] {
        let mut config = [0u8; CONFIG_SIZE];
        config[0..8].copy_from_slice(&self.capacity.to_le_bytes());
        // size_max (8..12) is not offered.
        config[12..16].copy_from_slice(&SEG_MAX.to_le_bytes());
        // geometry (16..20) is not offered.
        config[20..24].copy_from_slice(&(SECTOR_SIZE as u32).to_le_bytes());
        // topology, writeback and num_queues (24..36) are not offered.
        config[36..40].copy_from_slice(&u32::MAX.to_le_bytes());
        config[40..44].copy_from_slice(&MAX_DISCARD_SEG.to_le_bytes());
        config[44..48].copy_from_slice(&((DISCARD_ALIGN / SECTOR_SIZE) as u32).to_le_bytes());
        config
    }

    /// Serves every request waiting in the queue.
    fn process(&mut self) {
        let Some(active) = &mut self.active else {
            return;
        };
        if active.broken {
            return;
        }
        let mut used_any = false;
        loop {
            let chain = match active.queue.pop(&active.memory) {
                Ok(Some(chain)) => chain,
                Ok(None) => break,
                Err(_) => {
                    active.broken = true;
                    break;
                }
            };
            // A buffer outside guest memory leaves nothing written.
            let written = serve(
                &mut *self.disk,
                &active.memory,
                &chain,
                self.capacity,
                self.read_only,
                &self.id,
            )
            .unwrap_or(0);
            if active
                .queue
                .add_used(&active.memory, chain.head, written)
                .is_err()
            {
                active.broken = true;
                break;
            }
            used_any = true;
        }
        if used_any && active.queue.needs_interrupt(&active.memory).unwrap_or(true) {
            active.interrupt.raise(interrupt::USED_BUFFER);
        }
    }
}

/// Serves one request, writing its status byte. Returns the number of bytes
/// written into the chain, for the used ring.
fn serve(
    disk: &mut dyn Disk,
    mem: &GuestMemory,
    chain: &DescriptorChain,
    capacity: u64,
    read_only: bool,
    id: &[u8; ID_LEN],
) -> queue::Result<u32> {
    let writable = chain.writable_len();
    if writable == 0 {
        // No room for a status byte: return the chain untouched.
        return Ok(0);
    }
    // The status byte is the last device-writable byte.
    let status_at = writable - 1;
    let mut header = [0u8; HEADER_SIZE as usize];
    if chain.read_at(mem, 0, &mut header)? != header.len() {
        chain.write_at(mem, status_at, &[status::IOERR])?;
        return Ok(1);
    }
    let kind = u32::from_le_bytes(header[0..4].try_into().expect("4 bytes"));
    let sector = u64::from_le_bytes(header[8..16].try_into().expect("8 bytes"));
    let in_range = |len: u64| {
        len.is_multiple_of(SECTOR_SIZE)
            && sector
                .checked_add(len / SECTOR_SIZE)
                .is_some_and(|end| end <= capacity)
    };

    let (result, data_written) = match kind {
        req::IN => {
            let len = status_at;
            if !in_range(len) {
                (status::IOERR, 0)
            } else {
                match read_into_chain(disk, mem, chain, sector * SECTOR_SIZE, len) {
                    Ok(()) => (status::OK, len),
                    Err(_) => (status::IOERR, 0),
                }
            }
        }
        req::OUT => {
            let len = chain.readable_len() - HEADER_SIZE;
            if read_only || !in_range(len) {
                (status::IOERR, 0)
            } else {
                match write_from_chain(disk, mem, chain, sector * SECTOR_SIZE, len) {
                    Ok(()) => (status::OK, 0),
                    Err(_) => (status::IOERR, 0),
                }
            }
        }
        req::FLUSH => match disk.flush() {
            Ok(()) => (status::OK, 0),
            Err(_) => (status::IOERR, 0),
        },
        req::DISCARD => {
            let len = chain.readable_len() - HEADER_SIZE;
            if read_only {
                (status::IOERR, 0)
            } else {
                (discard(disk, mem, chain, capacity, len)?, 0)
            }
        }
        req::GET_ID => {
            let n = (ID_LEN as u64).min(status_at);
            chain.write_at(mem, 0, &id[..n as usize])?;
            (status::OK, n)
        }
        _ => (status::UNSUPP, 0),
    };
    chain.write_at(mem, status_at, &[result])?;
    Ok((data_written + 1) as u32)
}

/// Serves a discard request's ranges, all checked before any is applied.
fn discard(
    disk: &mut dyn Disk,
    mem: &GuestMemory,
    chain: &DescriptorChain,
    capacity: u64,
    len: u64,
) -> queue::Result<u8> {
    let count = len / DISCARD_RANGE_SIZE;
    if count == 0 || !len.is_multiple_of(DISCARD_RANGE_SIZE) || count > u64::from(MAX_DISCARD_SEG) {
        return Ok(status::IOERR);
    }
    let mut ranges = Vec::with_capacity(count as usize);
    for i in 0..count {
        let mut raw = [0u8; DISCARD_RANGE_SIZE as usize];
        chain.read_at(mem, HEADER_SIZE + i * DISCARD_RANGE_SIZE, &mut raw)?;
        let sector = u64::from_le_bytes(raw[0..8].try_into().expect("8 bytes"));
        let sectors = u32::from_le_bytes(raw[8..12].try_into().expect("4 bytes"));
        let flags = u32::from_le_bytes(raw[12..16].try_into().expect("4 bytes"));
        if flags & !DISCARD_UNMAP != 0 {
            return Ok(status::UNSUPP);
        }
        if sector
            .checked_add(u64::from(sectors))
            .is_none_or(|end| end > capacity)
        {
            return Ok(status::IOERR);
        }
        ranges.push((sector * SECTOR_SIZE, u64::from(sectors) * SECTOR_SIZE));
    }
    for (offset, len) in ranges {
        if disk.discard(offset, len).is_err() {
            return Ok(status::IOERR);
        }
    }
    Ok(status::OK)
}

/// Largest piece of a request copied at once, so a big request does not
/// need a buffer of its whole size.
const CHUNK: u64 = 128 * 1024;

fn read_into_chain(
    disk: &mut dyn Disk,
    mem: &GuestMemory,
    chain: &DescriptorChain,
    offset: u64,
    len: u64,
) -> io::Result<()> {
    let mut buf = vec![0u8; CHUNK.min(len) as usize];
    let mut done = 0;
    while done < len {
        let n = CHUNK.min(len - done) as usize;
        disk.read_at(offset + done, &mut buf[..n])?;
        chain
            .write_at(mem, done, &buf[..n])
            .map_err(io::Error::other)?;
        done += n as u64;
    }
    Ok(())
}

fn write_from_chain(
    disk: &mut dyn Disk,
    mem: &GuestMemory,
    chain: &DescriptorChain,
    offset: u64,
    len: u64,
) -> io::Result<()> {
    let mut buf = vec![0u8; CHUNK.min(len) as usize];
    let mut done = 0;
    while done < len {
        let n = CHUNK.min(len - done) as usize;
        chain
            .read_at(mem, HEADER_SIZE + done, &mut buf[..n])
            .map_err(io::Error::other)?;
        disk.write_at(offset + done, &buf[..n])?;
        done += n as u64;
    }
    Ok(())
}

impl VirtioDevice for Block {
    fn device_type(&self) -> u32 {
        device_type::BLOCK
    }

    fn features(&self) -> u64 {
        let mut f = features::SEG_MAX | features::BLK_SIZE | features::FLUSH;
        if self.read_only {
            f |= features::RO;
        } else {
            f |= features::DISCARD;
        }
        f
    }

    fn queue_max_sizes(&self) -> &[u16] {
        &[QUEUE_SIZE]
    }

    fn read_config(&self, offset: u64, data: &mut [u8]) {
        super::read_config_bytes(&self.config(), offset, data);
    }

    fn activate(&mut self, activation: Activation) -> Result<(), ActivateError> {
        let queue = activation
            .queues
            .into_iter()
            .next()
            .filter(|q| q.ready)
            .ok_or_else(|| ActivateError("the block request queue is not ready".into()))?;
        self.active = Some(Active {
            memory: activation.memory,
            queue,
            interrupt: activation.interrupt,
            broken: false,
        });
        Ok(())
    }

    fn queue_notify(&mut self, index: u16) {
        if index == 0 {
            self.process();
        }
    }

    fn reset(&mut self) {
        self.active = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devices::virtio::testing::{DriverQueue, guest_memory};
    use std::sync::{Arc, Mutex};

    /// What a recording disk saw.
    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Op {
        Read(u64, usize),
        Write(u64, Vec<u8>),
        Flush,
        Discard(u64, u64),
    }

    /// A disk in memory that records every operation.
    struct RecordingDisk {
        data: Vec<u8>,
        log: Arc<Mutex<Vec<Op>>>,
        fail: bool,
    }

    impl Disk for RecordingDisk {
        fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
            self.log.lock().unwrap().push(Op::Read(offset, buf.len()));
            if self.fail {
                return Err(io::Error::other("injected"));
            }
            let o = offset as usize;
            buf.copy_from_slice(&self.data[o..o + buf.len()]);
            Ok(())
        }
        fn write_at(&mut self, offset: u64, data: &[u8]) -> io::Result<()> {
            self.log
                .lock()
                .unwrap()
                .push(Op::Write(offset, data.to_vec()));
            if self.fail {
                return Err(io::Error::other("injected"));
            }
            let o = offset as usize;
            self.data[o..o + data.len()].copy_from_slice(data);
            Ok(())
        }
        fn flush(&mut self) -> io::Result<()> {
            self.log.lock().unwrap().push(Op::Flush);
            Ok(())
        }
        fn discard(&mut self, offset: u64, len: u64) -> io::Result<()> {
            self.log.lock().unwrap().push(Op::Discard(offset, len));
            Ok(())
        }
    }

    struct Rig {
        mem: GuestMemory,
        driver: DriverQueue,
        block: Block,
        log: Arc<Mutex<Vec<Op>>>,
        interrupt: Interrupt,
    }

    fn rig_with(sectors: u64, read_only: bool, fail: bool) -> Rig {
        let mem = guest_memory();
        let driver = DriverQueue::new(&mem, 16);
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut data = vec![0u8; (sectors * SECTOR_SIZE) as usize];
        for (i, b) in data.iter_mut().enumerate() {
            *b = (i / SECTOR_SIZE as usize) as u8;
        }
        let disk = RecordingDisk {
            data,
            log: Arc::clone(&log),
            fail,
        };
        let mut block = Block::new(
            Box::new(disk),
            sectors * SECTOR_SIZE,
            read_only,
            "cave-disk",
        );
        let interrupt = Interrupt::new();
        block
            .activate(Activation {
                memory: mem.clone(),
                queues: vec![driver.device_queue()],
                features: 0,
                interrupt: interrupt.clone(),
            })
            .unwrap();
        Rig {
            mem,
            driver,
            block,
            log,
            interrupt,
        }
    }

    fn rig() -> Rig {
        rig_with(8, false, false)
    }

    fn header(kind: u32, sector: u64) -> Vec<u8> {
        let mut h = Vec::new();
        h.extend_from_slice(&kind.to_le_bytes());
        h.extend_from_slice(&0u32.to_le_bytes());
        h.extend_from_slice(&sector.to_le_bytes());
        h
    }

    impl Rig {
        /// Submits a request and returns its head.
        fn submit(&mut self, readable: &[&[u8]], writable: &[u32]) -> u16 {
            self.driver.add(&self.mem, readable, writable)
        }

        fn status(&self, head: u16, writable_len: u64) -> u8 {
            self.driver.written(&self.mem, head, writable_len - 1, 1)[0]
        }
    }

    #[test]
    fn config_reports_capacity_and_block_size() {
        let r = rig();
        let mut cap = [0u8; 8];
        r.block.read_config(0, &mut cap);
        assert_eq!(u64::from_le_bytes(cap), 8);
        let mut bs = [0u8; 4];
        r.block.read_config(20, &mut bs);
        assert_eq!(u32::from_le_bytes(bs), 512);
        assert_eq!(r.block.features() & features::RO, 0);
        assert_ne!(r.block.features() & features::FLUSH, 0);
    }

    #[test]
    fn read_returns_sector_contents() {
        let mut r = rig();
        let head = r.submit(&[&header(req::IN, 2)], &[1024, 1]);
        r.block.queue_notify(0);
        assert_eq!(r.driver.used(&r.mem), vec![(u32::from(head), 1025)]);
        assert_eq!(r.status(head, 1025), status::OK);
        let data = r.driver.written(&r.mem, head, 0, 1024);
        assert!(data[..512].iter().all(|&b| b == 2));
        assert!(data[512..].iter().all(|&b| b == 3));
        assert!(r.interrupt.level());
    }

    #[test]
    fn write_lands_on_disk() {
        let mut r = rig();
        let payload = vec![0xab; 512];
        let head = r.submit(&[&header(req::OUT, 5), &payload], &[1]);
        r.block.queue_notify(0);
        assert_eq!(r.driver.used(&r.mem), vec![(u32::from(head), 1)]);
        assert_eq!(r.status(head, 1), status::OK);
        assert_eq!(*r.log.lock().unwrap(), vec![Op::Write(5 * 512, payload)]);
    }

    #[test]
    fn header_and_data_in_one_buffer_work() {
        let mut r = rig();
        let mut req = header(req::OUT, 1);
        req.extend_from_slice(&[7; 512]);
        let head = r.submit(&[&req], &[1]);
        r.block.queue_notify(0);
        assert_eq!(r.status(head, 1), status::OK);
        assert_eq!(*r.log.lock().unwrap(), vec![Op::Write(512, vec![7; 512])]);
    }

    #[test]
    fn flush_comes_after_earlier_writes() {
        let mut r = rig();
        let a = r.submit(&[&header(req::OUT, 0), &[1; 512]], &[1]);
        let b = r.submit(&[&header(req::OUT, 1), &[2; 512]], &[1]);
        let f = r.submit(&[&header(req::FLUSH, 0)], &[1]);
        let c = r.submit(&[&header(req::OUT, 2), &[3; 512]], &[1]);
        r.block.queue_notify(0);

        let ops = r.log.lock().unwrap().clone();
        assert_eq!(
            ops,
            vec![
                Op::Write(0, vec![1; 512]),
                Op::Write(512, vec![2; 512]),
                Op::Flush,
                Op::Write(1024, vec![3; 512]),
            ]
        );
        // Completions come back in the same order, so the driver never sees
        // the flush done before the writes it covers.
        let used: Vec<u32> = r.driver.used(&r.mem).into_iter().map(|u| u.0).collect();
        assert_eq!(used, [a, b, f, c].map(u32::from));
        assert_eq!(r.status(f, 1), status::OK);
    }

    #[test]
    fn out_of_range_and_partial_sectors_fail() {
        let mut r = rig();
        let past_end = r.submit(&[&header(req::IN, 7)], &[1024, 1]);
        let partial = r.submit(&[&header(req::OUT, 0), &[0; 100]], &[1]);
        let overflow = r.submit(&[&header(req::IN, u64::MAX)], &[512, 1]);
        r.block.queue_notify(0);
        assert_eq!(r.status(past_end, 1025), status::IOERR);
        assert_eq!(r.status(partial, 1), status::IOERR);
        assert_eq!(r.status(overflow, 513), status::IOERR);
        assert!(r.log.lock().unwrap().is_empty(), "nothing reached the disk");
    }

    #[test]
    fn read_only_refuses_writes() {
        let mut r = rig_with(8, true, false);
        assert_ne!(r.block.features() & features::RO, 0);
        let head = r.submit(&[&header(req::OUT, 0), &[1; 512]], &[1]);
        r.block.queue_notify(0);
        assert_eq!(r.status(head, 1), status::IOERR);
        assert!(r.log.lock().unwrap().is_empty());
    }

    #[test]
    fn disk_errors_become_ioerr() {
        let mut r = rig_with(8, false, true);
        let head = r.submit(&[&header(req::IN, 0)], &[512, 1]);
        r.block.queue_notify(0);
        assert_eq!(r.status(head, 513), status::IOERR);
    }

    #[test]
    fn get_id_and_unknown_requests() {
        let mut r = rig();
        let id = r.submit(&[&header(req::GET_ID, 0)], &[20, 1]);
        let unknown = r.submit(&[&header(99, 0)], &[1]);
        r.block.queue_notify(0);
        let mut want = [0u8; 20];
        want[..9].copy_from_slice(b"cave-disk");
        assert_eq!(r.driver.written(&r.mem, id, 0, 20), want);
        assert_eq!(r.status(id, 21), status::OK);
        assert_eq!(r.status(unknown, 1), status::UNSUPP);
    }

    #[test]
    fn real_file_round_trip() {
        let path = std::env::temp_dir().join(format!("cave-blk-test-{}", std::process::id()));
        std::fs::write(&path, vec![0u8; 4096]).unwrap();
        let mem = guest_memory();
        let mut driver = DriverQueue::new(&mem, 8);
        let mut block = Block::open(&path, false, "t").unwrap();
        block
            .activate(Activation {
                memory: mem.clone(),
                queues: vec![driver.device_queue()],
                features: 0,
                interrupt: Interrupt::new(),
            })
            .unwrap();
        let w = driver.add(&mem, &[&header(req::OUT, 3), &[0x5a; 512]], &[1]);
        let f = driver.add(&mem, &[&header(req::FLUSH, 0)], &[1]);
        let rd = driver.add(&mem, &[&header(req::IN, 3)], &[512, 1]);
        block.queue_notify(0);
        assert_eq!(driver.used(&mem).len(), 3);
        for (head, len) in [(w, 1), (f, 1), (rd, 513)] {
            assert_eq!(driver.written(&mem, head, len - 1, 1)[0], status::OK);
        }
        assert_eq!(driver.written(&mem, rd, 0, 512), vec![0x5a; 512]);
        let on_disk = std::fs::read(&path).unwrap();
        assert_eq!(&on_disk[3 * 512..4 * 512], &[0x5a; 512][..]);
        std::fs::remove_file(&path).unwrap();
    }

    fn range(sector: u64, sectors: u32, flags: u32) -> Vec<u8> {
        let mut r = sector.to_le_bytes().to_vec();
        r.extend_from_slice(&sectors.to_le_bytes());
        r.extend_from_slice(&flags.to_le_bytes());
        r
    }

    #[test]
    fn discard_is_offered_with_its_limits() {
        let r = rig();
        assert_ne!(r.block.features() & features::DISCARD, 0);
        let mut limits = [0u8; 12];
        r.block.read_config(36, &mut limits);
        assert_eq!(
            u32::from_le_bytes(limits[0..4].try_into().unwrap()),
            u32::MAX
        );
        assert_eq!(u32::from_le_bytes(limits[4..8].try_into().unwrap()), 32);
        assert_eq!(u32::from_le_bytes(limits[8..12].try_into().unwrap()), 8);
        let read_only = rig_with(8, true, false);
        assert_eq!(read_only.block.features() & features::DISCARD, 0);
    }

    #[test]
    fn discard_passes_each_range_to_the_disk() {
        let mut r = rig();
        let mut ranges = range(0, 2, 0);
        ranges.extend_from_slice(&range(4, 4, DISCARD_UNMAP));
        let head = r.submit(&[&header(req::DISCARD, 0), &ranges], &[1]);
        r.block.queue_notify(0);
        assert_eq!(r.status(head, 1), status::OK);
        assert_eq!(
            *r.log.lock().unwrap(),
            vec![Op::Discard(0, 1024), Op::Discard(2048, 2048)]
        );
    }

    #[test]
    fn bad_discards_touch_nothing() {
        let mut r = rig();
        // The second range runs past the end, so the first is not applied.
        let mut past_end = range(0, 1, 0);
        past_end.extend_from_slice(&range(6, 4, 0));
        let a = r.submit(&[&header(req::DISCARD, 0), &past_end], &[1]);
        let flags = r.submit(&[&header(req::DISCARD, 0), &range(0, 1, 2)], &[1]);
        let empty = r.submit(&[&header(req::DISCARD, 0)], &[1]);
        let ragged = r.submit(&[&header(req::DISCARD, 0), &[0; 10]], &[1]);
        r.block.queue_notify(0);
        assert_eq!(r.status(a, 1), status::IOERR);
        assert_eq!(r.status(flags, 1), status::UNSUPP);
        assert_eq!(r.status(empty, 1), status::IOERR);
        assert_eq!(r.status(ragged, 1), status::IOERR);
        assert!(r.log.lock().unwrap().is_empty());

        let mut ro = rig_with(8, true, false);
        let head = ro.submit(&[&header(req::DISCARD, 0), &range(0, 1, 0)], &[1]);
        ro.block.queue_notify(0);
        assert_eq!(ro.status(head, 1), status::IOERR);
    }

    #[test]
    fn discard_on_a_file_punches_the_aligned_blocks() {
        use std::os::unix::fs::MetadataExt;
        let path = std::env::temp_dir().join(format!("cave-blk-discard-{}", std::process::id()));
        std::fs::write(&path, vec![0xeeu8; 64 << 10]).unwrap();
        let blocks = std::fs::metadata(&path).unwrap().blocks();
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        // 1 KiB in, to 1 KiB short of the end: only the whole 4 KiB blocks
        // between go.
        Disk::discard(&mut file, 1024, (64 << 10) - 2048).unwrap();
        // A range inside one block punches nothing and is still fine.
        Disk::discard(&mut file, 100, 200).unwrap();
        let data = std::fs::read(&path).unwrap();
        assert!(data[..4096].iter().all(|&b| b == 0xee));
        assert!(data[4096..60 << 10].iter().all(|&b| b == 0));
        assert!(data[60 << 10..].iter().all(|&b| b == 0xee));
        assert!(std::fs::metadata(&path).unwrap().blocks() < blocks);
        assert_eq!(data.len(), 64 << 10);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn reset_stops_serving() {
        let mut r = rig();
        r.block.reset();
        r.submit(&[&header(req::IN, 0)], &[512, 1]);
        r.block.queue_notify(0);
        assert!(r.driver.used(&r.mem).is_empty());
    }
}
