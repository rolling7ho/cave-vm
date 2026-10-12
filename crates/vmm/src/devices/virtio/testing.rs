//! A minimal driver side of a virtqueue, for tests: it lays out the rings
//! in guest memory and adds chains the way a guest driver would.

use std::collections::{HashMap, VecDeque};

use super::queue::{Queue, VIRTQ_DESC_F_INDIRECT, VIRTQ_DESC_F_NEXT, VIRTQ_DESC_F_WRITE};
use crate::memory::{GuestMemory, Region};

/// Size of the test guest's memory, starting at guest address 0.
pub const MEMORY_SIZE: u64 = 4 << 20;
/// Where test data buffers start.
const DATA_BASE: u64 = 0x10_0000;

pub fn guest_memory() -> GuestMemory {
    GuestMemory::new(vec![Region::anonymous(0, MEMORY_SIZE).unwrap()]).unwrap()
}

/// The guest driver's view of one queue.
pub struct DriverQueue {
    pub size: u16,
    pub desc: u64,
    pub avail: u64,
    pub used: u64,
    free: VecDeque<u16>,
    avail_idx: u16,
    used_seen: u16,
    next_data: u64,
    /// Device-writable buffers of each chain, by head.
    writable: HashMap<u16, Vec<(u64, u32)>>,
    /// Descriptors of each chain, by head, to free on recycle.
    owned: HashMap<u16, Vec<u16>>,
}

impl DriverQueue {
    /// A queue of `size` entries. Tests that use two queues in one memory
    /// give each a different `slot`.
    pub fn new(mem: &GuestMemory, size: u16) -> DriverQueue {
        DriverQueue::at(mem, size, 0)
    }

    pub fn at(mem: &GuestMemory, size: u16, slot: u64) -> DriverQueue {
        let base = slot * 0x2_0000;
        let q = DriverQueue {
            size,
            desc: base,
            avail: base + 0x8000,
            used: base + 0x1_0000,
            free: (0..size).collect(),
            avail_idx: 0,
            used_seen: 0,
            next_data: DATA_BASE + slot * 0x10_0000,
            writable: HashMap::new(),
            owned: HashMap::new(),
        };
        mem.write(q.avail, &[0; 4]).unwrap();
        mem.write(q.used, &[0; 4]).unwrap();
        q
    }

    /// The device's queue, configured as the driver would through the
    /// transport.
    pub fn device_queue(&self) -> Queue {
        Queue {
            max_size: self.size,
            size: self.size,
            ready: true,
            desc_table: self.desc,
            avail_ring: self.avail,
            used_ring: self.used,
            next_avail: 0,
            next_used: 0,
        }
    }

    fn alloc_data(&mut self, len: u32) -> u64 {
        let addr = self.next_data;
        self.next_data += u64::from(len).max(1).next_multiple_of(64);
        addr
    }

    pub fn set_desc(
        &self,
        mem: &GuestMemory,
        index: u16,
        addr: u64,
        len: u32,
        flags: u16,
        next: u16,
    ) {
        write_desc(
            mem,
            self.desc + 16 * u64::from(index),
            addr,
            len,
            flags,
            next,
        );
    }

    fn buffers(
        &mut self,
        mem: &GuestMemory,
        readable: &[&[u8]],
        writable: &[u32],
    ) -> Vec<(u64, u32, u16)> {
        let mut out = Vec::new();
        for data in readable {
            let addr = self.alloc_data(data.len() as u32);
            mem.write(addr, data).unwrap();
            out.push((addr, data.len() as u32, 0));
        }
        for &len in writable {
            let addr = self.alloc_data(len);
            out.push((addr, len, VIRTQ_DESC_F_WRITE));
        }
        out
    }

    /// Adds a chain of device-readable buffers holding `readable`, then
    /// device-writable buffers of the given sizes. Returns the head.
    pub fn add(&mut self, mem: &GuestMemory, readable: &[&[u8]], writable: &[u32]) -> u16 {
        let buffers = self.buffers(mem, readable, writable);
        let indices: Vec<u16> = (0..buffers.len())
            .map(|_| self.free.pop_front().expect("free descriptor"))
            .collect();
        for (i, &(addr, len, flags)) in buffers.iter().enumerate() {
            let (flags, next) = match indices.get(i + 1) {
                Some(&n) => (flags | VIRTQ_DESC_F_NEXT, n),
                None => (flags, 0),
            };
            self.set_desc(mem, indices[i], addr, len, flags, next);
        }
        let head = indices[0];
        self.record(head, &buffers, indices);
        self.publish(mem, head);
        head
    }

    /// Like [`add`](Self::add), through one indirect descriptor.
    pub fn add_indirect(&mut self, mem: &GuestMemory, readable: &[&[u8]], writable: &[u32]) -> u16 {
        let buffers = self.buffers(mem, readable, writable);
        let table = self.alloc_data(16 * buffers.len() as u32);
        for (i, &(addr, len, flags)) in buffers.iter().enumerate() {
            let last = i + 1 == buffers.len();
            let flags = if last {
                flags
            } else {
                flags | VIRTQ_DESC_F_NEXT
            };
            write_desc(mem, table + 16 * i as u64, addr, len, flags, i as u16 + 1);
        }
        let head = self.free.pop_front().expect("free descriptor");
        self.set_desc(
            mem,
            head,
            table,
            16 * buffers.len() as u32,
            VIRTQ_DESC_F_INDIRECT,
            0,
        );
        self.record(head, &buffers, vec![head]);
        self.publish(mem, head);
        head
    }

    fn record(&mut self, head: u16, buffers: &[(u64, u32, u16)], indices: Vec<u16>) {
        let writable = buffers
            .iter()
            .filter(|b| b.2 & VIRTQ_DESC_F_WRITE != 0)
            .map(|b| (b.0, b.1))
            .collect();
        self.writable.insert(head, writable);
        self.owned.insert(head, indices);
    }

    fn publish(&mut self, mem: &GuestMemory, head: u16) {
        let slot = u64::from(self.avail_idx % self.size);
        mem.write(self.avail + 4 + 2 * slot, &head.to_le_bytes())
            .unwrap();
        self.avail_idx = self.avail_idx.wrapping_add(1);
        mem.write(self.avail + 2, &self.avail_idx.to_le_bytes())
            .unwrap();
    }

    /// Frees the descriptors of a chain the device has used.
    pub fn recycle(&mut self, head: u16) {
        if let Some(indices) = self.owned.remove(&head) {
            self.free.extend(indices);
        }
    }

    pub fn set_avail_idx(&self, mem: &GuestMemory, idx: u16) {
        mem.write(self.avail + 2, &idx.to_le_bytes()).unwrap();
    }

    /// Moves the driver's available index, as if it had already added
    /// `idx` chains.
    pub fn set_next_avail(&mut self, mem: &GuestMemory, idx: u16) {
        self.avail_idx = idx;
        self.used_seen = idx;
        self.set_avail_idx(mem, idx);
        mem.write(self.used + 2, &idx.to_le_bytes()).unwrap();
    }

    pub fn set_avail_flags(&self, mem: &GuestMemory, flags: u16) {
        mem.write(self.avail, &flags.to_le_bytes()).unwrap();
    }

    /// Used-ring entries `(id, len)` the device added since the last call.
    pub fn used(&mut self, mem: &GuestMemory) -> Vec<(u32, u32)> {
        let mut raw = [0u8; 2];
        mem.read(self.used + 2, &mut raw).unwrap();
        let idx = u16::from_le_bytes(raw);
        let mut out = Vec::new();
        while self.used_seen != idx {
            let slot = u64::from(self.used_seen % self.size);
            let mut elem = [0u8; 8];
            mem.read(self.used + 4 + 8 * slot, &mut elem).unwrap();
            out.push((
                u32::from_le_bytes(elem[0..4].try_into().unwrap()),
                u32::from_le_bytes(elem[4..8].try_into().unwrap()),
            ));
            self.used_seen = self.used_seen.wrapping_add(1);
        }
        out
    }

    /// `len` bytes the device wrote into chain `head`, from `offset`.
    pub fn written(&self, mem: &GuestMemory, head: u16, mut offset: u64, len: usize) -> Vec<u8> {
        let mut out = Vec::new();
        for &(addr, size) in &self.writable[&head] {
            if out.len() == len {
                break;
            }
            let size = u64::from(size);
            if offset >= size {
                offset -= size;
                continue;
            }
            let take = ((size - offset) as usize).min(len - out.len());
            let mut buf = vec![0u8; take];
            mem.read(addr + offset, &mut buf).unwrap();
            out.extend_from_slice(&buf);
            offset = 0;
        }
        out
    }
}

fn write_desc(mem: &GuestMemory, gpa: u64, addr: u64, len: u32, flags: u16, next: u16) {
    let mut raw = [0u8; 16];
    raw[0..8].copy_from_slice(&addr.to_le_bytes());
    raw[8..12].copy_from_slice(&len.to_le_bytes());
    raw[12..14].copy_from_slice(&flags.to_le_bytes());
    raw[14..16].copy_from_slice(&next.to_le_bytes());
    mem.write(gpa, &raw).unwrap();
}
