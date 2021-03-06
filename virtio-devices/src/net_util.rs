// Copyright (c) 2019 Intel Corporation. All rights reserved.
//
// SPDX-License-Identifier: Apache-2.0 AND BSD-3-Clause

use super::Error as DeviceError;
use super::{DescriptorChain, DeviceEventT, Queue};
use net_util::{register_listener, MacAddr};
use serde::ser::{Serialize, SerializeStruct, Serializer};
use std::fs::File;
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use virtio_bindings::bindings::virtio_net::*;
use vm_memory::{
    ByteValued, Bytes, GuestAddressSpace, GuestMemoryAtomic, GuestMemoryError, GuestMemoryMmap,
};
use vmm_sys_util::eventfd::EventFd;

type Result<T> = std::result::Result<T, Error>;

const QUEUE_SIZE: usize = 256;

// The device has been dropped.
pub const KILL_EVENT: DeviceEventT = 3;
// The device should be paused.
pub const PAUSE_EVENT: DeviceEventT = 4;
// Number of DeviceEventT events supported by this implementation.
pub const NET_EVENTS_COUNT: usize = 5;
// The device has been dropped.
const CTRL_QUEUE_EVENT: DeviceEventT = 0;
// Number of DeviceEventT events supported by this implementation.
const CTRL_EVENT_COUNT: usize = 3;

#[repr(C, packed)]
#[derive(Copy, Clone, Debug, Default, Deserialize)]
pub struct VirtioNetConfig {
    pub mac: [u8; 6],
    pub status: u16,
    pub max_virtqueue_pairs: u16,
    pub mtu: u16,
    pub speed: u32,
    pub duplex: u8,
}

// We must explicitly implement Serialize since the structure is packed and
// it's unsafe to borrow from a packed structure. And by default, if we derive
// Serialize from serde, it will borrow the values from the structure.
// That's why this implementation copies each field separately before it
// serializes the entire structure field by field.
impl Serialize for VirtioNetConfig {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mac = self.mac;
        let status = self.status;
        let max_virtqueue_pairs = self.max_virtqueue_pairs;
        let mtu = self.mtu;
        let speed = self.speed;
        let duplex = self.duplex;

        let mut virtio_net_config = serializer.serialize_struct("VirtioNetConfig", 17)?;
        virtio_net_config.serialize_field("mac", &mac)?;
        virtio_net_config.serialize_field("status", &status)?;
        virtio_net_config.serialize_field("max_virtqueue_pairs", &max_virtqueue_pairs)?;
        virtio_net_config.serialize_field("mtu", &mtu)?;
        virtio_net_config.serialize_field("speed", &speed)?;
        virtio_net_config.serialize_field("duplex", &duplex)?;
        virtio_net_config.end()
    }
}

// Safe because it only has data and has no implicit padding.
unsafe impl ByteValued for VirtioNetConfig {}

#[derive(Debug)]
pub enum Error {
    /// Read process MQ.
    FailedProcessMQ,
    /// Read queue failed.
    GuestMemory(GuestMemoryError),
    /// Invalid ctrl class
    InvalidCtlClass,
    /// Invalid ctrl command
    InvalidCtlCmd,
    /// Invalid descriptor
    InvalidDesc,
    /// Invalid queue pairs number
    InvalidQueuePairsNum,
    /// No memory passed in.
    NoMemory,
    /// No ueue pairs nummber.
    NoQueuePairsNum,
}

pub struct CtrlVirtio {
    pub queue_evt: EventFd,
    pub queue: Queue,
}

impl std::clone::Clone for CtrlVirtio {
    fn clone(&self) -> Self {
        CtrlVirtio {
            queue_evt: self.queue_evt.try_clone().unwrap(),
            queue: self.queue.clone(),
        }
    }
}

impl CtrlVirtio {
    pub fn new(queue: Queue, queue_evt: EventFd) -> Self {
        CtrlVirtio { queue_evt, queue }
    }

    fn process_mq(&self, mem: &GuestMemoryMmap, avail_desc: DescriptorChain) -> Result<()> {
        let mq_desc = if avail_desc.has_next() {
            avail_desc.next_descriptor().unwrap()
        } else {
            return Err(Error::NoQueuePairsNum);
        };
        let queue_pairs = mem
            .read_obj::<u16>(mq_desc.addr)
            .map_err(Error::GuestMemory)?;
        if (queue_pairs < VIRTIO_NET_CTRL_MQ_VQ_PAIRS_MIN as u16)
            || (queue_pairs > VIRTIO_NET_CTRL_MQ_VQ_PAIRS_MAX as u16)
        {
            return Err(Error::InvalidQueuePairsNum);
        }
        let status_desc = if mq_desc.has_next() {
            mq_desc.next_descriptor().unwrap()
        } else {
            return Err(Error::NoQueuePairsNum);
        };
        mem.write_obj::<u8>(0, status_desc.addr)
            .map_err(Error::GuestMemory)?;

        Ok(())
    }

    pub fn process_cvq(&mut self, mem: &GuestMemoryMmap) -> Result<()> {
        let mut used_desc_heads = [(0, 0); QUEUE_SIZE];
        let mut used_count = 0;
        if let Some(avail_desc) = self.queue.iter(&mem).next() {
            used_desc_heads[used_count] = (avail_desc.index, avail_desc.len);
            used_count += 1;
            let ctrl_hdr = mem
                .read_obj::<u16>(avail_desc.addr)
                .map_err(Error::GuestMemory)?;
            let ctrl_hdr_v = ctrl_hdr.as_slice();
            let class = ctrl_hdr_v[0];
            let cmd = ctrl_hdr_v[1];
            match u32::from(class) {
                VIRTIO_NET_CTRL_MQ => {
                    if u32::from(cmd) != VIRTIO_NET_CTRL_MQ_VQ_PAIRS_SET {
                        return Err(Error::InvalidCtlCmd);
                    }
                    if let Err(_e) = self.process_mq(&mem, avail_desc) {
                        return Err(Error::FailedProcessMQ);
                    }
                }
                _ => return Err(Error::InvalidCtlClass),
            }
        } else {
            return Err(Error::InvalidDesc);
        }
        for &(desc_index, len) in &used_desc_heads[..used_count] {
            self.queue.add_used(&mem, desc_index, len);
            self.queue.update_avail_event(&mem);
        }

        Ok(())
    }
}

pub struct NetCtrlEpollHandler {
    pub mem: GuestMemoryAtomic<GuestMemoryMmap>,
    pub kill_evt: EventFd,
    pub pause_evt: EventFd,
    pub ctrl_q: CtrlVirtio,
    pub epoll_fd: RawFd,
}

impl NetCtrlEpollHandler {
    pub fn run_ctrl(&mut self, paused: Arc<AtomicBool>) -> std::result::Result<(), DeviceError> {
        // Create the epoll file descriptor
        self.epoll_fd = epoll::create(true).map_err(DeviceError::EpollCreateFd)?;
        // Use 'File' to enforce closing on 'epoll_fd'
        let epoll_file = unsafe { File::from_raw_fd(self.epoll_fd) };

        register_listener(
            epoll_file.as_raw_fd(),
            self.ctrl_q.queue_evt.as_raw_fd(),
            epoll::Events::EPOLLIN,
            u64::from(CTRL_QUEUE_EVENT),
        )
        .unwrap();
        register_listener(
            epoll_file.as_raw_fd(),
            self.kill_evt.as_raw_fd(),
            epoll::Events::EPOLLIN,
            u64::from(KILL_EVENT),
        )
        .unwrap();
        register_listener(
            epoll_file.as_raw_fd(),
            self.pause_evt.as_raw_fd(),
            epoll::Events::EPOLLIN,
            u64::from(PAUSE_EVENT),
        )
        .unwrap();

        let mut events = vec![epoll::Event::new(epoll::Events::empty(), 0); CTRL_EVENT_COUNT];

        // Before jumping into the epoll loop, check if the device is expected
        // to be in a paused state. This is helpful for the restore code path
        // as the device thread should not start processing anything before the
        // device has been resumed.
        while paused.load(Ordering::SeqCst) {
            thread::park();
        }

        'epoll: loop {
            let num_events = match epoll::wait(epoll_file.as_raw_fd(), -1, &mut events[..]) {
                Ok(res) => res,
                Err(e) => {
                    if e.kind() == std::io::ErrorKind::Interrupted {
                        continue;
                    }
                    return Err(DeviceError::EpollWait(e));
                }
            };

            for event in events.iter().take(num_events) {
                let ev_type = event.data as u16;

                match ev_type {
                    CTRL_QUEUE_EVENT => {
                        let mem = self.mem.memory();
                        if let Err(e) = self.ctrl_q.queue_evt.read() {
                            error!("failed to get ctl queue event: {:?}", e);
                        }
                        if let Err(e) = self.ctrl_q.process_cvq(&mem) {
                            error!("failed to process ctrl queue: {:?}", e);
                        }
                    }
                    KILL_EVENT => {
                        break 'epoll;
                    }
                    PAUSE_EVENT => {
                        debug!("PAUSE_EVENT received, pausing vhost-user epoll loop");
                        // We loop here to handle spurious park() returns.
                        // Until we have not resumed, the paused boolean will
                        // be true.
                        while paused.load(Ordering::SeqCst) {
                            std::thread::park();
                        }

                        // Drain pause event after the device has been resumed.
                        // This ensures the pause event has been seen by each
                        // and every thread related to this virtio device.
                        let _ = self.pause_evt.read();
                    }
                    _ => {
                        error!("Unknown event for virtio-net");
                    }
                }
            }
        }

        Ok(())
    }
}

pub fn build_net_config_space(
    mut config: &mut VirtioNetConfig,
    mac: MacAddr,
    num_queues: usize,
    mut avail_features: &mut u64,
) {
    config.mac.copy_from_slice(mac.get_bytes());
    *avail_features |= 1 << VIRTIO_NET_F_MAC;

    build_net_config_space_with_mq(&mut config, num_queues, &mut avail_features);
}

pub fn build_net_config_space_with_mq(
    config: &mut VirtioNetConfig,
    num_queues: usize,
    avail_features: &mut u64,
) {
    let num_queue_pairs = (num_queues / 2) as u16;
    if (num_queue_pairs >= VIRTIO_NET_CTRL_MQ_VQ_PAIRS_MIN as u16)
        && (num_queue_pairs <= VIRTIO_NET_CTRL_MQ_VQ_PAIRS_MAX as u16)
    {
        config.max_virtqueue_pairs = num_queue_pairs;
        *avail_features |= 1 << VIRTIO_NET_F_MQ;
    }
}
