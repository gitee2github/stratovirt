// Copyright (c) Huawei Technologies Co., Ltd. 2022. All rights reserved.
//
// StratoVirt is licensed under Mulan PSL v2.
// You can use this software according to the terms and conditions of the Mulan
// PSL v2.
// You may obtain a copy of Mulan PSL v2 at:
//         http://license.coscl.org.cn/MulanPSL2
// THIS SOFTWARE IS PROVIDED ON AN "AS IS" BASIS, WITHOUT WARRANTIES OF ANY
// KIND, EITHER EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO
// NON-INFRINGEMENT, MERCHANTABILITY OR FIT FOR A PARTICULAR PURPOSE.
// See the Mulan PSL v2 for more details.

use std::cmp;
use std::io::Write;
use std::sync::{Arc, Mutex};

use address_space::AddressSpace;
use log::warn;
use machine_manager::config::NetworkInterfaceConfig;
use util::byte_code::ByteCode;
use util::num_ops::{read_u32, write_u32};
use vmm_sys_util::eventfd::EventFd;

use super::super::super::errors::{ErrorKind, Result, ResultExt};
use super::super::super::{
    net::{build_device_config_space, VirtioNetConfig},
    Queue, VirtioDevice, VirtioInterrupt, VIRTIO_F_RING_EVENT_IDX, VIRTIO_F_VERSION_1,
    VIRTIO_NET_F_GUEST_CSUM, VIRTIO_NET_F_GUEST_TSO4, VIRTIO_NET_F_GUEST_UFO,
    VIRTIO_NET_F_HOST_TSO4, VIRTIO_NET_F_HOST_UFO, VIRTIO_NET_F_MAC, VIRTIO_NET_F_MRG_RXBUF,
    VIRTIO_TYPE_NET,
};
use super::super::VhostOps;
use super::VhostUserClient;

/// Number of virtqueues.
const QUEUE_NUM_NET: usize = 2;
/// Size of each virtqueue.
const QUEUE_SIZE_NET: u16 = 256;

/// Network device structure.
pub struct Net {
    /// Configuration of the vhost user network device.
    net_cfg: NetworkInterfaceConfig,
    /// Bit mask of features supported by the backend.
    device_features: u64,
    /// Bit mask of features negotiated by the backend and the frontend.
    driver_features: u64,
    /// Virtio net configurations.
    device_config: VirtioNetConfig,
    /// System address space.
    mem_space: Arc<AddressSpace>,
    /// Vhost user client
    client: Option<VhostUserClient>,
    /// The notifier events from host.
    call_events: Vec<EventFd>,
}

impl Net {
    pub fn new(cfg: &NetworkInterfaceConfig, mem_space: &Arc<AddressSpace>) -> Self {
        Net {
            net_cfg: cfg.clone(),
            device_features: 0_u64,
            driver_features: 0_u64,
            device_config: VirtioNetConfig::default(),
            mem_space: mem_space.clone(),
            client: None,
            call_events: Vec::<EventFd>::new(),
        }
    }
}

impl VirtioDevice for Net {
    /// Realize vhost user network device.
    fn realize(&mut self) -> Result<()> {
        let socket_path = self
            .net_cfg
            .socket_path
            .as_ref()
            .map(|path| path.to_string())
            .chain_err(|| "vhost-user: socket path is not found")?;
        let client = VhostUserClient::new(&self.mem_space, &socket_path, self.queue_num() as u64)
            .chain_err(|| {
            "Failed to create the client which communicates with the server for vhost-user net"
        })?;
        client
            .add_event_notifier()
            .chain_err(|| "Failed to add event nitifier for vhost-user net")?;

        self.device_features = client
            .get_features()
            .chain_err(|| "Failed to get features for vhost-user net")?;

        let features = 1 << VIRTIO_F_VERSION_1
            | 1 << VIRTIO_NET_F_GUEST_CSUM
            | 1 << VIRTIO_NET_F_GUEST_TSO4
            | 1 << VIRTIO_NET_F_GUEST_UFO
            | 1 << VIRTIO_NET_F_HOST_TSO4
            | 1 << VIRTIO_NET_F_HOST_UFO
            | 1 << VIRTIO_NET_F_MRG_RXBUF
            | 1 << VIRTIO_F_RING_EVENT_IDX;
        self.device_features &= features;

        self.client = Some(client);

        if let Some(mac) = &self.net_cfg.mac {
            self.device_features |= build_device_config_space(&mut self.device_config, mac);
        }

        Ok(())
    }

    /// Get the virtio device type, refer to Virtio Spec.
    fn device_type(&self) -> u32 {
        VIRTIO_TYPE_NET
    }

    /// Get the count of virtio device queues.
    fn queue_num(&self) -> usize {
        QUEUE_NUM_NET
    }

    /// Get the queue size of virtio device.
    fn queue_size(&self) -> u16 {
        QUEUE_SIZE_NET
    }

    /// Get device features from host.
    fn get_device_features(&self, features_select: u32) -> u32 {
        read_u32(self.device_features, features_select)
    }

    /// Set driver features by guest.
    fn set_driver_features(&mut self, page: u32, value: u32) {
        let mut features = write_u32(value, page);
        let unsupported_features = features & !self.device_features;
        if unsupported_features != 0 {
            warn!(
                "Received acknowledge request with unsupported feature for vhost net: 0x{:x}",
                features
            );
            features &= !unsupported_features;
        }
        self.driver_features |= features;
    }

    /// Read data of config from guest.
    fn read_config(&self, offset: u64, mut data: &mut [u8]) -> Result<()> {
        let config_slice = self.device_config.as_bytes();
        let config_size = config_slice.len() as u64;
        if offset >= config_size {
            return Err(ErrorKind::DevConfigOverflow(offset, config_size).into());
        }
        if let Some(end) = offset.checked_add(data.len() as u64) {
            data.write_all(&config_slice[offset as usize..cmp::min(end, config_size) as usize])?;
        }

        Ok(())
    }

    /// Write data to config from guest.
    fn write_config(&mut self, offset: u64, data: &[u8]) -> Result<()> {
        let data_len = data.len();
        let config_slice = self.device_config.as_mut_bytes();
        let config_len = config_slice.len();
        if offset as usize + data_len > config_len {
            return Err(ErrorKind::DevConfigOverflow(offset, config_len as u64).into());
        }

        config_slice[(offset as usize)..(offset as usize + data_len)].copy_from_slice(data);

        Ok(())
    }

    /// Activate the virtio device, this function is called by vcpu thread when frontend
    /// virtio driver is ready and write `DRIVER_OK` to backend.
    fn activate(
        &mut self,
        _mem_space: Arc<AddressSpace>,
        _interrupt_cb: Arc<VirtioInterrupt>,
        queues: &[Arc<Mutex<Queue>>],
        queue_evts: Vec<EventFd>,
    ) -> Result<()> {
        let client = match &self.client {
            None => return Err("Failed to get client for vhost-user net".into()),
            Some(client_) => client_,
        };

        client
            .set_owner()
            .chain_err(|| "Failed to set owner for vhost-user net")?;

        let features = self.driver_features & !(1 << VIRTIO_NET_F_MAC);
        client
            .set_features(features)
            .chain_err(|| "Failed to set features for vhost-user net")?;

        client
            .set_mem_table()
            .chain_err(|| "Failed to set mem table for vhost-user net")?;

        for (queue_index, queue_mutex) in queues.iter().enumerate() {
            let queue = queue_mutex.lock().unwrap();
            let actual_size = queue.vring.actual_size();
            let queue_config = queue.vring.get_queue_config();

            client.set_vring_enable(queue_index, false).chain_err(|| {
                format!(
                    "Failed to set vring enable for vhost-user net, index: {}, false",
                    queue_index,
                )
            })?;
            client
                .set_vring_num(queue_index, actual_size)
                .chain_err(|| {
                    format!(
                        "Failed to set vring num for vhost-user net, index: {}, size: {}",
                        queue_index, actual_size,
                    )
                })?;
            client
                .set_vring_addr(&queue_config, queue_index, 0)
                .chain_err(|| {
                    format!(
                        "Failed to set vring addr for vhost-user net, index: {}",
                        queue_index,
                    )
                })?;
            client.set_vring_base(queue_index, 0).chain_err(|| {
                format!(
                    "Failed to set vring base for vhost-user net, index: {}",
                    queue_index,
                )
            })?;
            client
                .set_vring_kick(queue_index, &queue_evts[queue_index])
                .chain_err(|| {
                    format!(
                        "Failed to set vring kick for vhost-user net, index: {}",
                        queue_index,
                    )
                })?;

            client
                .set_vring_call(queue_index, &self.call_events[queue_index])
                .chain_err(|| {
                    format!(
                        "Failed to set vring call for vhost-user net, index: {}",
                        queue_index,
                    )
                })?;

            client.set_vring_enable(queue_index, true).chain_err(|| {
                format!(
                    "Failed to set vring enable for vhost-user net, index: {}",
                    queue_index,
                )
            })?;
        }

        Ok(())
    }

    /// Set guest notifiers for notifying the guest.
    fn set_guest_notifiers(&mut self, queue_evts: &[EventFd]) -> Result<()> {
        for fd in queue_evts.iter() {
            let cloned_evt_fd = fd.try_clone().unwrap();
            self.call_events.push(cloned_evt_fd);
        }

        Ok(())
    }

    fn deactivate(&mut self) -> Result<()> {
        self.call_events.clear();
        Ok(())
    }

    fn reset(&mut self) -> Result<()> {
        self.device_features = 0_u64;
        self.driver_features = 0_u64;
        self.device_config = VirtioNetConfig::default();

        let client = match &self.client {
            None => return Err("Failed to get client when reseting vhost-user net".into()),
            Some(client_) => client_,
        };
        client
            .delete_event()
            .chain_err(|| "Failed to delete vhost-user net event")?;
        self.client = None;

        self.realize()
    }
}
