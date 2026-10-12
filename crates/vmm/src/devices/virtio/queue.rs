//! Split virtqueues (virtio 1.2, section 2.7).
//!
//! The driver in the guest fills a descriptor table and an available ring;
//! the device takes chains of descriptors from the available ring, uses the
//! buffers, and returns them through the used ring. Every access goes
//! through [`GuestMemory`], which copies, so the guest can never make the
//! host hold a reference into memory it is changing.
//!
//! Indirect descriptors are supported. `VIRTIO_F_EVENT_IDX` is not offered,
//! so notification suppression is the plain `VIRTQ_AVAIL_F_NO_INTERRUPT`
//! flag.

use std::fmt;
use std::sync::atomic::{Ordering, fence};

use crate::memory::{self, GuestMemory};

/// The descriptor continues in `next`.
pub const VIRTQ_DESC_F_NEXT: u16 = 1;
/// The buffer is device-writable (otherwise device-readable).
pub const VIRTQ_DESC_F_WRITE: u16 = 2;
/// The buffer holds a table of indirect descriptors.
pub const VIRTQ_DESC_F_INDIRECT: u16 = 4;
/// Set by the driver when it does not want an interrupt for used buffers.
pub const VIRTQ_AVAIL_F_NO_INTERRUPT: u16 = 1;

/// Size of one descriptor in the table.
const DESC_SIZE: u64 = 16;

/// An error from a queue. All of them are guest bugs or a hostile guest;
/// the device stops using the queue rather than crashing the VMM.
#[derive(Debug)]
pub enum Error {
    /// The queue was used before the driver made it ready, or its
    /// configuration is invalid.
    NotReady,
    /// A ring or descriptor points outside guest memory.
    Memory(memory::Error),
    /// The available index moved by more than the queue size.
    AvailOverrun { avail: u16, next: u16 },
    /// A descriptor index is not inside the table.
    BadIndex(u16),
    /// A chain is longer than its table, so it must loop.
    ChainTooLong,
    /// An indirect table is not a whole number of descriptors, is empty, or
    /// is itself chained or nested.
    BadIndirect,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::NotReady => write!(f, "virtqueue is not ready"),
            Error::Memory(e) => write!(f, "virtqueue access failed: {e}"),
            Error::AvailOverrun { avail, next } => write!(
                f,
                "virtqueue available index {avail} is too far ahead of {next}"
            ),
            Error::BadIndex(i) => write!(f, "virtqueue descriptor index {i} is out of range"),
            Error::ChainTooLong => write!(f, "virtqueue descriptor chain loops"),
            Error::BadIndirect => write!(f, "virtqueue indirect descriptor table is malformed"),
        }
    }
}

impl std::error::Error for Error {}

impl From<memory::Error> for Error {
    fn from(e: memory::Error) -> Error {
        Error::Memory(e)
    }
}

/// Result of a queue operation.
pub type Result<T> = std::result::Result<T, Error>;

/// One buffer of a descriptor chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Descriptor {
    pub addr: u64,
    pub len: u32,
    pub flags: u16,
    pub next: u16,
}

impl Descriptor {
    /// Whether the device may write this buffer.
    pub fn is_write_only(&self) -> bool {
        self.flags & VIRTQ_DESC_F_WRITE != 0
    }

    fn has_next(&self) -> bool {
        self.flags & VIRTQ_DESC_F_NEXT != 0
    }

    fn is_indirect(&self) -> bool {
        self.flags & VIRTQ_DESC_F_INDIRECT != 0
    }

    fn read(mem: &GuestMemory, gpa: u64) -> Result<Descriptor> {
        let mut raw = [0u8; DESC_SIZE as usize];
        mem.read(gpa, &mut raw)?;
        Ok(Descriptor {
            addr: u64::from_le_bytes(raw[0..8].try_into().expect("8 bytes")),
            len: u32::from_le_bytes(raw[8..12].try_into().expect("4 bytes")),
            flags: u16::from_le_bytes(raw[12..14].try_into().expect("2 bytes")),
            next: u16::from_le_bytes(raw[14..16].try_into().expect("2 bytes")),
        })
    }
}

/// A chain of buffers the driver made available: device-readable buffers
/// first, then device-writable ones.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DescriptorChain {
    /// Index of the first descriptor, returned to the driver with
    /// [`Queue::add_used`].
    pub head: u16,
    pub descriptors: Vec<Descriptor>,
}

impl DescriptorChain {
    /// The device-readable buffers.
    pub fn readable(&self) -> impl Iterator<Item = &Descriptor> {
        self.descriptors.iter().filter(|d| !d.is_write_only())
    }

    /// The device-writable buffers.
    pub fn writable(&self) -> impl Iterator<Item = &Descriptor> {
        self.descriptors.iter().filter(|d| d.is_write_only())
    }

    /// Total bytes the device may read.
    pub fn readable_len(&self) -> u64 {
        self.readable().map(|d| u64::from(d.len)).sum()
    }

    /// Total bytes the device may write.
    pub fn writable_len(&self) -> u64 {
        self.writable().map(|d| u64::from(d.len)).sum()
    }

    /// Copies up to `buf.len()` device-readable bytes, starting `offset`
    /// bytes into the readable part. Returns how many were copied.
    pub fn read_at(&self, mem: &GuestMemory, offset: u64, buf: &mut [u8]) -> Result<usize> {
        let mut done = 0;
        for (gpa, len) in spans(self.readable(), offset, buf.len()) {
            mem.read(gpa, &mut buf[done..done + len])?;
            done += len;
        }
        Ok(done)
    }

    /// Copies `data` into the device-writable part, starting `offset` bytes
    /// into it, as far as it fits. Returns how many bytes were written.
    pub fn write_at(&self, mem: &GuestMemory, offset: u64, data: &[u8]) -> Result<usize> {
        let mut done = 0;
        for (gpa, len) in spans(self.writable(), offset, data.len()) {
            mem.write(gpa, &data[done..done + len])?;
            done += len;
        }
        Ok(done)
    }
}

/// Splits `[offset, offset + want)` of the concatenated `descriptors` into
/// guest address ranges.
fn spans<'a>(
    descriptors: impl Iterator<Item = &'a Descriptor>,
    mut offset: u64,
    mut want: usize,
) -> Vec<(u64, usize)> {
    let mut out = Vec::new();
    for d in descriptors {
        if want == 0 {
            break;
        }
        let len = u64::from(d.len);
        if offset >= len {
            offset -= len;
            continue;
        }
        let take = (len - offset).min(want as u64) as usize;
        out.push((d.addr + offset, take));
        offset = 0;
        want -= take;
    }
    out
}

/// One virtqueue: the driver's configuration plus the device's position in
/// the rings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Queue {
    /// Largest size the device accepts (`QueueNumMax`).
    pub max_size: u16,
    /// Size the driver chose (`QueueNum`).
    pub size: u16,
    /// Whether the driver enabled the queue (`QueueReady`).
    pub ready: bool,
    /// Guest address of the descriptor table.
    pub desc_table: u64,
    /// Guest address of the available ring (the "driver area").
    pub avail_ring: u64,
    /// Guest address of the used ring (the "device area").
    pub used_ring: u64,
    /// Next available-ring slot the device will take.
    pub next_avail: u16,
    /// Next used-ring slot the device will fill.
    pub next_used: u16,
}

impl Queue {
    /// A queue that accepts up to `max_size` entries, which must be a power
    /// of two.
    pub fn new(max_size: u16) -> Queue {
        assert!(
            max_size.is_power_of_two(),
            "queue size must be a power of two"
        );
        Queue {
            max_size,
            size: max_size,
            ready: false,
            desc_table: 0,
            avail_ring: 0,
            used_ring: 0,
            next_avail: 0,
            next_used: 0,
        }
    }

    /// Returns the queue to its state after a device reset.
    pub fn reset(&mut self) {
        *self = Queue::new(self.max_size);
    }

    /// Whether the configuration the driver wrote can be used: ready, a
    /// power-of-two size no larger than the maximum, aligned rings, and
    /// every ring inside guest memory.
    pub fn is_valid(&self, mem: &GuestMemory) -> bool {
        let size = u64::from(self.size);
        let fits = |gpa: u64, len: u64| {
            usize::try_from(len).is_ok_and(|len| memory::locate(&layout_of(mem), gpa, len).is_ok())
        };
        self.ready
            && self.size != 0
            && self.size <= self.max_size
            && self.size.is_power_of_two()
            && self.desc_table.is_multiple_of(16)
            && self.avail_ring.is_multiple_of(2)
            && self.used_ring.is_multiple_of(4)
            && fits(self.desc_table, DESC_SIZE * size)
            && fits(self.avail_ring, 6 + 2 * size)
            && fits(self.used_ring, 6 + 8 * size)
    }

    /// Takes the next chain the driver made available, if any.
    pub fn pop(&mut self, mem: &GuestMemory) -> Result<Option<DescriptorChain>> {
        if !self.ready || self.size == 0 {
            return Err(Error::NotReady);
        }
        let avail = read_u16(mem, self.avail_ring + 2)?;
        if avail == self.next_avail {
            return Ok(None);
        }
        if avail.wrapping_sub(self.next_avail) > self.size {
            return Err(Error::AvailOverrun {
                avail,
                next: self.next_avail,
            });
        }
        // Read the ring entry only after seeing the index that covers it.
        fence(Ordering::Acquire);
        let slot = u64::from(self.next_avail % self.size);
        let head = read_u16(mem, self.avail_ring + 4 + 2 * slot)?;
        self.next_avail = self.next_avail.wrapping_add(1);
        let descriptors = self.walk(mem, head)?;
        Ok(Some(DescriptorChain { head, descriptors }))
    }

    /// Follows the chain starting at `head`, expanding an indirect table.
    fn walk(&self, mem: &GuestMemory, head: u16) -> Result<Vec<Descriptor>> {
        let mut out = Vec::new();
        let mut index = head;
        let mut seen = 0u32;
        loop {
            if index >= self.size {
                return Err(Error::BadIndex(index));
            }
            seen += 1;
            if seen > u32::from(self.size) {
                return Err(Error::ChainTooLong);
            }
            let d = Descriptor::read(mem, self.desc_table + DESC_SIZE * u64::from(index))?;
            if d.is_indirect() {
                // An indirect descriptor must stand alone in the chain.
                if d.has_next() || !out.is_empty() {
                    return Err(Error::BadIndirect);
                }
                return read_indirect(mem, &d);
            }
            let next = d.has_next().then_some(d.next);
            out.push(d);
            match next {
                Some(n) => index = n,
                None => return Ok(out),
            }
        }
    }

    /// Returns a chain to the driver, saying how many bytes the device
    /// wrote into it.
    pub fn add_used(&mut self, mem: &GuestMemory, head: u16, written: u32) -> Result<()> {
        if !self.ready || self.size == 0 {
            return Err(Error::NotReady);
        }
        let slot = u64::from(self.next_used % self.size);
        let mut elem = [0u8; 8];
        elem[0..4].copy_from_slice(&u32::from(head).to_le_bytes());
        elem[4..8].copy_from_slice(&written.to_le_bytes());
        mem.write(self.used_ring + 4 + 8 * slot, &elem)?;
        self.next_used = self.next_used.wrapping_add(1);
        // The driver must see the element before the index that covers it.
        fence(Ordering::Release);
        mem.write(self.used_ring + 2, &self.next_used.to_le_bytes())?;
        Ok(())
    }

    /// Whether the driver wants an interrupt for buffers just used.
    pub fn needs_interrupt(&self, mem: &GuestMemory) -> Result<bool> {
        fence(Ordering::SeqCst);
        let flags = read_u16(mem, self.avail_ring)?;
        Ok(flags & VIRTQ_AVAIL_F_NO_INTERRUPT == 0)
    }
}

fn read_indirect(mem: &GuestMemory, table: &Descriptor) -> Result<Vec<Descriptor>> {
    let len = u64::from(table.len);
    if len == 0 || !len.is_multiple_of(DESC_SIZE) {
        return Err(Error::BadIndirect);
    }
    let count = len / DESC_SIZE;
    // Indirect tables are limited to the 16-bit index space like any table.
    if count > u64::from(u16::MAX) + 1 {
        return Err(Error::BadIndirect);
    }
    let mut out = Vec::new();
    let mut index = 0u64;
    loop {
        if index >= count {
            return Err(Error::BadIndex(index as u16));
        }
        if out.len() as u64 >= count {
            return Err(Error::ChainTooLong);
        }
        let d = Descriptor::read(mem, table.addr + DESC_SIZE * index)?;
        if d.is_indirect() {
            return Err(Error::BadIndirect);
        }
        let next = d.has_next().then_some(d.next);
        out.push(d);
        match next {
            Some(n) => index = u64::from(n),
            None => return Ok(out),
        }
    }
}

fn read_u16(mem: &GuestMemory, gpa: u64) -> Result<u16> {
    let mut raw = [0u8; 2];
    mem.read(gpa, &mut raw)?;
    Ok(u16::from_le_bytes(raw))
}

fn layout_of(mem: &GuestMemory) -> Vec<(u64, u64)> {
    mem.regions()
        .iter()
        .map(|r| (r.gpa(), r.size() as u64))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devices::virtio::testing::{DriverQueue, guest_memory};

    #[test]
    fn empty_queue_pops_nothing() {
        let mem = guest_memory();
        let driver = DriverQueue::new(&mem, 8);
        let mut queue = driver.device_queue();
        assert_eq!(queue.pop(&mem).unwrap(), None);
    }

    #[test]
    fn unready_queue_is_an_error() {
        let mem = guest_memory();
        let mut queue = Queue::new(8);
        assert!(matches!(queue.pop(&mem), Err(Error::NotReady)));
        assert!(matches!(queue.add_used(&mem, 0, 0), Err(Error::NotReady)));
    }

    #[test]
    fn chain_round_trips_through_the_rings() {
        let mem = guest_memory();
        let mut driver = DriverQueue::new(&mem, 8);
        let head = driver.add(&mem, &[b"hello, "], &[16]);
        let mut queue = driver.device_queue();

        let chain = queue.pop(&mem).unwrap().expect("one chain");
        assert_eq!(chain.head, head);
        assert_eq!(chain.readable_len(), 7);
        assert_eq!(chain.writable_len(), 16);

        let mut buf = [0u8; 7];
        assert_eq!(chain.read_at(&mem, 0, &mut buf).unwrap(), 7);
        assert_eq!(&buf, b"hello, ");
        assert_eq!(chain.write_at(&mem, 0, b"world").unwrap(), 5);
        queue.add_used(&mem, chain.head, 5).unwrap();

        assert_eq!(queue.pop(&mem).unwrap(), None);
        let used = driver.used(&mem);
        assert_eq!(used, vec![(u32::from(head), 5)]);
        assert_eq!(&driver.written(&mem, head, 0, 5), b"world");
    }

    #[test]
    fn reads_and_writes_cross_buffer_boundaries() {
        let mem = guest_memory();
        let mut driver = DriverQueue::new(&mem, 8);
        driver.add(&mem, &[b"abc", b"defg", b"h"], &[2, 2, 4]);
        let mut queue = driver.device_queue();
        let chain = queue.pop(&mem).unwrap().unwrap();

        let mut buf = [0u8; 5];
        assert_eq!(chain.read_at(&mem, 2, &mut buf).unwrap(), 5);
        assert_eq!(&buf, b"cdefg");
        let mut tail = [0u8; 4];
        assert_eq!(
            chain.read_at(&mem, 6, &mut tail).unwrap(),
            2,
            "only two left"
        );
        assert_eq!(&tail[..2], b"gh");

        assert_eq!(chain.write_at(&mem, 1, b"0123456789").unwrap(), 7);
        assert_eq!(chain.writable_len(), 8);
    }

    #[test]
    fn indirect_table_is_expanded() {
        let mem = guest_memory();
        let mut driver = DriverQueue::new(&mem, 4);
        let head = driver.add_indirect(&mem, &[b"req"], &[8]);
        let mut queue = driver.device_queue();
        let chain = queue.pop(&mem).unwrap().unwrap();
        assert_eq!(chain.head, head);
        assert_eq!(chain.descriptors.len(), 2);
        assert_eq!(chain.readable_len(), 3);
        assert_eq!(chain.writable_len(), 8);
    }

    #[test]
    fn looping_chain_is_rejected() {
        let mem = guest_memory();
        let mut driver = DriverQueue::new(&mem, 4);
        let head = driver.add(&mem, &[b"a", b"b"], &[]);
        // Point the second descriptor back at the first.
        driver.set_desc(&mem, 1, 0x1_0000, 1, VIRTQ_DESC_F_NEXT, head);
        let mut queue = driver.device_queue();
        assert!(matches!(queue.pop(&mem), Err(Error::ChainTooLong)));
    }

    #[test]
    fn out_of_range_index_is_rejected() {
        let mem = guest_memory();
        let mut driver = DriverQueue::new(&mem, 4);
        driver.add(&mem, &[b"a"], &[]);
        driver.set_desc(&mem, 0, 0x1_0000, 1, VIRTQ_DESC_F_NEXT, 9);
        let mut queue = driver.device_queue();
        assert!(matches!(queue.pop(&mem), Err(Error::BadIndex(9))));
    }

    #[test]
    fn avail_index_far_ahead_is_rejected() {
        let mem = guest_memory();
        let driver = DriverQueue::new(&mem, 4);
        driver.set_avail_idx(&mem, 5);
        let mut queue = driver.device_queue();
        assert!(matches!(
            queue.pop(&mem),
            Err(Error::AvailOverrun { avail: 5, next: 0 })
        ));
    }

    #[test]
    fn buffer_outside_memory_is_an_error_not_a_crash() {
        let mem = guest_memory();
        let mut driver = DriverQueue::new(&mem, 4);
        driver.add(&mem, &[b"a"], &[]);
        driver.set_desc(&mem, 0, 0xdead_0000_0000, 64, 0, 0);
        let mut queue = driver.device_queue();
        let chain = queue.pop(&mem).unwrap().unwrap();
        let mut buf = [0u8; 4];
        assert!(matches!(
            chain.read_at(&mem, 0, &mut buf),
            Err(Error::Memory(_))
        ));
    }

    #[test]
    fn indices_wrap_around() {
        let mem = guest_memory();
        let mut driver = DriverQueue::new(&mem, 2);
        let mut queue = driver.device_queue();
        // Start both sides just below the wrap point.
        driver.set_next_avail(&mem, u16::MAX - 1);
        queue.next_avail = u16::MAX - 1;
        queue.next_used = u16::MAX - 1;
        for round in 0..4u8 {
            let head = driver.add(&mem, &[&[round]], &[]);
            let chain = queue.pop(&mem).unwrap().unwrap();
            assert_eq!(chain.head, head);
            let mut b = [0u8];
            chain.read_at(&mem, 0, &mut b).unwrap();
            assert_eq!(b[0], round);
            queue.add_used(&mem, head, 0).unwrap();
            driver.recycle(head);
        }
        assert_eq!(queue.next_avail, 2);
        assert_eq!(queue.next_used, 2);
    }

    #[test]
    fn no_interrupt_flag_is_honoured() {
        let mem = guest_memory();
        let driver = DriverQueue::new(&mem, 4);
        let queue = driver.device_queue();
        assert!(queue.needs_interrupt(&mem).unwrap());
        driver.set_avail_flags(&mem, VIRTQ_AVAIL_F_NO_INTERRUPT);
        assert!(!queue.needs_interrupt(&mem).unwrap());
    }

    #[test]
    fn validity_checks_size_alignment_and_bounds() {
        let mem = guest_memory();
        let driver = DriverQueue::new(&mem, 8);
        let good = driver.device_queue();
        assert!(good.is_valid(&mem));

        let mut q = good.clone();
        q.size = 6;
        assert!(!q.is_valid(&mem), "not a power of two");
        let mut q = good.clone();
        q.size = 16;
        assert!(!q.is_valid(&mem), "above the maximum");
        let mut q = good.clone();
        q.desc_table += 8;
        assert!(!q.is_valid(&mem), "misaligned table");
        let mut q = good.clone();
        q.used_ring = 0xdead_0000_0000;
        assert!(!q.is_valid(&mem), "outside memory");
        let mut q = good;
        q.ready = false;
        assert!(!q.is_valid(&mem), "not ready");
    }
}
