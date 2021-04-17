// Copyright (c) 2020 Huawei Technologies Co.,Ltd. All rights reserved.
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

//! # Micro VM
//!
//! Micro VM is a extremely light machine type.
//! It has a very simple machine model, which benefits to a very short
//! boot-time and tiny memory usage.
//!
//! ## Design
//!
//! This module offers support for:
//! 1. Create and manage lifecycle for `Micro VM`.
//! 2. Set cmdline arguments parameters for `Micro VM`.
//! 3. Manage mainloop to handle events for `Micro VM` and its devices.
//!
//! ## Platform Support
//!
//! - `x86_64`
//! - `aarch64`

pub mod errors {
    error_chain! {
        links {
            Util(util::errors::Error, util::errors::ErrorKind);
            Virtio(virtio::errors::Error, virtio::errors::ErrorKind);
        }
        foreign_links {
            Io(std::io::Error);
            Kvm(kvm_ioctls::Error);
            Nul(std::ffi::NulError);
        }
        errors {
            RplDevLimitErr(dev: String, nr: usize) {
                display("A maximum of {} {} replaceble devices are supported.", nr, dev)
            }
            CreateRplDev {
                display("Failed to create replaceable device.")
            }
            UpdCfgErr(id: String) {
                display("{}: failed to update config.", id)
            }
        }
    }
}

mod mem_layout;
mod syscall;

pub use syscall::syscall_allow_list;

use std::fs::metadata;
use std::ops::Deref;
use std::os::linux::fs::MetadataExt;
use std::os::unix::io::RawFd;
use std::path::Path;
use std::sync::{Arc, Barrier, Condvar, Mutex};
use std::vec::Vec;

use address_space::{AddressSpace, GuestAddress, Region};
use boot_loader::{load_kernel, BootLoaderConfig};
use cpu::{ArchCPU, CPUBootConfig, CPUInterface, CpuTopology, CPU};
use devices::Serial;
#[cfg(target_arch = "aarch64")]
use devices::{InterruptController, InterruptControllerConfig, PL031};
use error_chain::ChainedError;
#[cfg(target_arch = "x86_64")]
use kvm_bindings::{kvm_pit_config, KVM_PIT_SPEAKER_DUMMY};
use kvm_ioctls::{Kvm, VmFd};
use machine_manager::machine::{
    DeviceInterface, KvmVmState, MachineAddressInterface, MachineExternalInterface,
    MachineInterface, MachineLifecycle,
};
use machine_manager::{
    config::{
        BalloonConfig, BootSource, ConfigCheck, ConsoleConfig, DriveConfig, NetworkInterfaceConfig,
        SerialConfig, VmConfig, VsockConfig,
    },
    event_loop::EventLoop,
    qmp::{qmp_schema, QmpChannel, Response},
};
use sysbus::SysBus;
#[cfg(target_arch = "aarch64")]
use sysbus::{SysBusDevType, SysRes};
#[cfg(target_arch = "aarch64")]
use util::device_tree;
#[cfg(target_arch = "aarch64")]
use util::device_tree::CompileFDT;
use util::loop_context::{EventLoopManager, EventNotifierHelper};
use util::seccomp::BpfRule;
use virtio::{
    create_tap, qmp_balloon, qmp_query_balloon, Balloon, Block, Console, Net, VhostKern,
    VirtioDevice, VirtioMmioDevice,
};
use vmm_sys_util::eventfd::EventFd;
use vmm_sys_util::terminal::Terminal;

use super::{
    errors::{ErrorKind as MachineErrorKind, Result as MachineResult},
    MachineOps,
};
use errors::{ErrorKind, Result};
use mem_layout::{LayoutEntryType, MEM_LAYOUT};

// The replaceable block device maximum count.
const MMIO_REPLACEABLE_BLK_NR: usize = 6;
// The replaceable network device maximum count.
const MMIO_REPLACEABLE_NET_NR: usize = 2;

// The config of replaceable device.
struct MmioReplaceableConfig {
    // Device id.
    id: String,
    // The dev_config of the related backend device.
    dev_config: Arc<dyn ConfigCheck>,
}

// The device information of replaceable device.
struct MmioReplaceableDevInfo {
    // The related MMIO device.
    device: Arc<Mutex<dyn VirtioDevice>>,
    // Device id.
    id: String,
    // Identify if this device is be used.
    used: bool,
}

// The gather of config, info and count of all replaceable devices.
struct MmioReplaceableInfo {
    // The arrays of all replaceable configs.
    configs: Arc<Mutex<Vec<MmioReplaceableConfig>>>,
    // The arrays of all replaceable device information.
    devices: Arc<Mutex<Vec<MmioReplaceableDevInfo>>>,
    // The count of block device which is plugin.
    block_count: usize,
    // The count of network device which is plugin.
    net_count: usize,
}

impl MmioReplaceableInfo {
    fn new() -> Self {
        MmioReplaceableInfo {
            configs: Arc::new(Mutex::new(Vec::new())),
            devices: Arc::new(Mutex::new(Vec::new())),
            block_count: 0_usize,
            net_count: 0_usize,
        }
    }
}

/// A wrapper around creating and using a kvm-based micro VM.
pub struct LightMachine {
    // `vCPU` topology, support sockets, cores, threads.
    cpu_topo: CpuTopology,
    // `vCPU` devices.
    cpus: Arc<Mutex<Vec<Arc<CPU>>>>,
    // Interrupt controller device.
    #[cfg(target_arch = "aarch64")]
    irq_chip: Option<Arc<InterruptController>>,
    // Memory address space.
    sys_mem: Arc<AddressSpace>,
    // IO address space.
    #[cfg(target_arch = "x86_64")]
    sys_io: Arc<AddressSpace>,
    // System bus.
    sysbus: SysBus,
    // All replaceable device information.
    replaceable_info: MmioReplaceableInfo,
    // VM running state.
    vm_state: Arc<(Mutex<KvmVmState>, Condvar)>,
    // Vm boot_source config.
    boot_source: Arc<Mutex<BootSource>>,
    // VM power button, handle VM `Shutdown` event.
    power_button: EventFd,
}

impl LightMachine {
    /// Constructs a new `LightMachine`.
    ///
    /// # Arguments
    ///
    /// * `vm_config` - Represents the configuration for VM.
    pub fn new(vm_config: &VmConfig) -> MachineResult<Self> {
        use crate::errors::ResultExt;

        let sys_mem = AddressSpace::new(Region::init_container_region(u64::max_value()))
            .chain_err(|| MachineErrorKind::CrtAddrSpaceErr("memory".to_string()))?;
        #[cfg(target_arch = "x86_64")]
        let sys_io = AddressSpace::new(Region::init_container_region(1 << 16))
            .chain_err(|| MachineErrorKind::CrtAddrSpaceErr("I/O".to_string()))?;
        #[cfg(target_arch = "x86_64")]
        let free_irqs: (i32, i32) = (5, 15);
        #[cfg(target_arch = "aarch64")]
        let free_irqs: (i32, i32) = (32, 191);
        let mmio_region: (u64, u64) = (
            MEM_LAYOUT[LayoutEntryType::Mmio as usize].0,
            MEM_LAYOUT[LayoutEntryType::Mmio as usize + 1].0,
        );
        let sysbus = SysBus::new(
            #[cfg(target_arch = "x86_64")]
            &sys_io,
            &sys_mem,
            free_irqs,
            mmio_region,
        );

        // Machine state init
        let vm_state = Arc::new((Mutex::new(KvmVmState::Created), Condvar::new()));
        let power_button = EventFd::new(libc::EFD_NONBLOCK)
            .chain_err(|| "Create EventFd for power-button failed.")?;

        Ok(LightMachine {
            cpu_topo: CpuTopology::new(vm_config.machine_config.nr_cpus),
            cpus: Arc::new(Mutex::new(Vec::new())),
            #[cfg(target_arch = "aarch64")]
            irq_chip: None,
            sys_mem,
            #[cfg(target_arch = "x86_64")]
            sys_io,
            sysbus,
            replaceable_info: MmioReplaceableInfo::new(),
            boot_source: Arc::new(Mutex::new(vm_config.clone().boot_source)),
            vm_state,
            power_button,
        })
    }

    #[cfg(target_arch = "x86_64")]
    fn arch_init(vm_fd: &VmFd) -> Result<()> {
        vm_fd.create_irq_chip()?;
        vm_fd.set_tss_address(0xfffb_d000_usize)?;

        let pit_config = kvm_pit_config {
            flags: KVM_PIT_SPEAKER_DUMMY,
            pad: Default::default(),
        };
        vm_fd.create_pit2(pit_config)?;

        Ok(())
    }

    fn create_replaceable_devices(&mut self, vm_fd: &Arc<VmFd>) -> Result<()> {
        let mut rpl_devs: Vec<VirtioMmioDevice> = Vec::new();
        for _ in 0..MMIO_REPLACEABLE_BLK_NR {
            let block = Arc::new(Mutex::new(Block::new()));
            let virtio_mmio = VirtioMmioDevice::new(&self.sys_mem, block);
            rpl_devs.push(virtio_mmio);
        }
        for _ in 0..MMIO_REPLACEABLE_NET_NR {
            let net = Arc::new(Mutex::new(Net::new()));
            let virtio_mmio = VirtioMmioDevice::new(&self.sys_mem, net);
            rpl_devs.push(virtio_mmio);
        }

        let mut region_base = self.sysbus.min_free_base;
        let region_size = MEM_LAYOUT[LayoutEntryType::Mmio as usize].1;
        for dev in rpl_devs {
            self.replaceable_info
                .devices
                .lock()
                .unwrap()
                .push(MmioReplaceableDevInfo {
                    device: dev.device.clone(),
                    id: "".to_string(),
                    used: false,
                });
            VirtioMmioDevice::realize(
                dev,
                &mut self.sysbus,
                region_base,
                MEM_LAYOUT[LayoutEntryType::Mmio as usize].1,
                #[cfg(target_arch = "x86_64")]
                &self.boot_source,
                vm_fd,
            )?;
            region_base += region_size;
        }
        self.sysbus.min_free_base = region_base;
        Ok(())
    }

    fn fill_replaceable_device(
        &mut self,
        id: &str,
        dev_config: Arc<dyn ConfigCheck>,
        index: usize,
    ) -> Result<()> {
        use errors::ResultExt;

        let mut replaceable_devices = self.replaceable_info.devices.lock().unwrap();
        if let Some(device_info) = replaceable_devices.get_mut(index) {
            if device_info.used {
                bail!("{}: index {} is already used.", id, index);
            }

            device_info.id = id.to_string();
            device_info.used = true;
            device_info
                .device
                .lock()
                .unwrap()
                .update_config(Some(dev_config.clone()))
                .chain_err(|| ErrorKind::UpdCfgErr(id.to_string()))?;
        }

        self.add_replaceable_config(id, dev_config)?;
        Ok(())
    }

    fn add_replaceable_config(&self, id: &str, dev_config: Arc<dyn ConfigCheck>) -> Result<()> {
        let mut configs_lock = self.replaceable_info.configs.lock().unwrap();
        let limit = MMIO_REPLACEABLE_BLK_NR + MMIO_REPLACEABLE_NET_NR;
        if configs_lock.len() >= limit {
            return Err(ErrorKind::RplDevLimitErr("".to_string(), limit).into());
        }

        for config in configs_lock.iter() {
            if config.id == id {
                bail!("{} is already registered.", id);
            }
        }

        let config = MmioReplaceableConfig {
            id: id.to_string(),
            dev_config,
        };
        configs_lock.push(config);
        Ok(())
    }

    fn add_replaceable_device(&self, id: &str, driver: &str, slot: usize) -> Result<()> {
        use errors::ResultExt;

        let index = if driver.contains("net") {
            if slot >= MMIO_REPLACEABLE_NET_NR {
                return Err(
                    ErrorKind::RplDevLimitErr("net".to_string(), MMIO_REPLACEABLE_NET_NR).into(),
                );
            }
            slot + MMIO_REPLACEABLE_BLK_NR
        } else if driver.contains("blk") {
            if slot >= MMIO_REPLACEABLE_BLK_NR {
                return Err(ErrorKind::RplDevLimitErr(
                    "block".to_string(),
                    MMIO_REPLACEABLE_BLK_NR,
                )
                .into());
            }
            slot
        } else {
            bail!("Unsupported replaceable device type.");
        };

        // Find the configuration by id.
        let configs_lock = self.replaceable_info.configs.lock().unwrap();
        let mut dev_config = None;
        for config in configs_lock.iter() {
            if config.id == id {
                dev_config = Some(config.dev_config.clone());
            }
        }
        if dev_config.is_none() {
            bail!("Failed to find the configuration.");
        }

        // Find the replaceable device and replace it.
        let mut replaceable_devices = self.replaceable_info.devices.lock().unwrap();
        if let Some(device_info) = replaceable_devices.get_mut(index) {
            if device_info.used {
                bail!("The slot {} is occupied", slot);
            }

            device_info.id = id.to_string();
            device_info.used = true;
            device_info
                .device
                .lock()
                .unwrap()
                .update_config(dev_config)
                .chain_err(|| ErrorKind::UpdCfgErr(id.to_string()))?;
        }
        Ok(())
    }

    fn del_replaceable_device(&self, id: &str) -> Result<String> {
        use errors::ResultExt;

        // find the index of configuration by name and remove it
        let mut is_exist = false;
        let mut configs_lock = self.replaceable_info.configs.lock().unwrap();
        for (index, config) in configs_lock.iter().enumerate() {
            if config.id == id {
                configs_lock.remove(index);
                is_exist = true;
                break;
            }
        }

        // set the status of the device to 'unused'
        let mut replaceable_devices = self.replaceable_info.devices.lock().unwrap();
        for device_info in replaceable_devices.iter_mut() {
            if device_info.id == id {
                device_info.id = "".to_string();
                device_info.used = false;
                device_info
                    .device
                    .lock()
                    .unwrap()
                    .update_config(None)
                    .chain_err(|| ErrorKind::UpdCfgErr(id.to_string()))?;
            }
        }

        if !is_exist {
            bail!("Device {} not found", id);
        }
        Ok(id.to_string())
    }

    /// Start VM, changed `LightMachine`'s `vmstate` to `Paused` or
    /// `Running`.
    ///
    /// # Arguments
    ///
    /// * `paused` - After started, paused all vcpu or not.
    pub fn vm_start(&self, paused: bool) -> Result<()> {
        use errors::ResultExt;

        let cpus_thread_barrier = Arc::new(Barrier::new((self.cpu_topo.max_cpus + 1) as usize));
        for cpu_index in 0..self.cpu_topo.max_cpus {
            let cpu_thread_barrier = cpus_thread_barrier.clone();
            let cpu = self.cpus.lock().unwrap()[cpu_index as usize].clone();
            CPU::start(cpu, cpu_thread_barrier, paused)
                .chain_err(|| format!("Failed to run vcpu{}.", cpu_index))?;
        }

        let mut vmstate = self.vm_state.deref().0.lock().unwrap();
        if paused {
            *vmstate = KvmVmState::Paused;
        } else {
            *vmstate = KvmVmState::Running;
        }
        cpus_thread_barrier.wait();

        Ok(())
    }

    /// Pause VM, sleepy all vcpu thread. Changed `LightMachine`'s `vmstate`
    /// from `Running` to `Paused`.
    fn vm_pause(&self) -> Result<()> {
        use errors::ResultExt;

        for cpu_index in 0..self.cpu_topo.max_cpus {
            self.cpus.lock().unwrap()[cpu_index as usize]
                .pause()
                .chain_err(|| format!("Failed to pause vcpu{}.", cpu_index))?;
        }

        #[cfg(target_arch = "aarch64")]
        self.irq_chip.as_ref().unwrap().stop();

        let mut vmstate = self.vm_state.deref().0.lock().unwrap();
        *vmstate = KvmVmState::Paused;

        Ok(())
    }

    /// Resume VM, awaken all vcpu thread. Changed `LightMachine`'s `vmstate`
    /// from `Paused` to `Running`.
    fn vm_resume(&self) -> Result<()> {
        use errors::ResultExt;

        for cpu_index in 0..self.cpu_topo.max_cpus {
            self.cpus.lock().unwrap()[cpu_index as usize]
                .resume()
                .chain_err(|| format!("Failed to resume vcpu{}.", cpu_index))?;
        }

        let mut vmstate = self.vm_state.deref().0.lock().unwrap();
        *vmstate = KvmVmState::Running;

        Ok(())
    }

    /// Destroy VM, kill all vcpu thread. Changed `LightMachine`'s `vmstate`
    /// to `KVM_VMSTATE_DESTROY`.
    fn vm_destroy(&self) -> Result<()> {
        use errors::ResultExt;

        let mut vmstate = self.vm_state.deref().0.lock().unwrap();
        *vmstate = KvmVmState::Shutdown;

        let mut cpus = self.cpus.lock().unwrap();
        for cpu_index in 0..self.cpu_topo.max_cpus {
            cpus[cpu_index as usize]
                .destroy()
                .chain_err(|| format!("Failed to destroy vcpu{}", cpu_index))?;
        }
        cpus.clear();

        Ok(())
    }
}

impl MachineOps for LightMachine {
    fn arch_ram_ranges(&self, mem_size: u64) -> Vec<(u64, u64)> {
        // ranges is the vector of (start_addr, size)
        let mut ranges = Vec::<(u64, u64)>::new();

        #[cfg(target_arch = "aarch64")]
        ranges.push((MEM_LAYOUT[LayoutEntryType::Mem as usize].0, mem_size));
        #[cfg(target_arch = "x86_64")]
        {
            let gap_start = MEM_LAYOUT[LayoutEntryType::MemBelow4g as usize].0
                + MEM_LAYOUT[LayoutEntryType::MemBelow4g as usize].1;
            ranges.push((0, std::cmp::min(gap_start, mem_size)));
            if mem_size > gap_start {
                let gap_end = MEM_LAYOUT[LayoutEntryType::MemAbove4g as usize].0;
                ranges.push((gap_end, mem_size - gap_start));
            }
        }
        ranges
    }

    #[cfg(target_arch = "aarch64")]
    fn add_rtc_device(&mut self, vm_fd: &Arc<VmFd>) -> MachineResult<()> {
        use crate::errors::ResultExt;

        PL031::realize(
            PL031::default(),
            &mut self.sysbus,
            MEM_LAYOUT[LayoutEntryType::Rtc as usize].0,
            MEM_LAYOUT[LayoutEntryType::Rtc as usize].1,
            vm_fd,
        )
        .chain_err(|| "Failed to realize pl031.")?;
        Ok(())
    }

    fn add_serial_device(&mut self, config: &SerialConfig, vm_fd: &Arc<VmFd>) -> MachineResult<()> {
        use crate::errors::ResultExt;

        #[cfg(target_arch = "x86_64")]
        let region_base: u64 = 0x3f8;
        #[cfg(target_arch = "aarch64")]
        let region_base: u64 = MEM_LAYOUT[LayoutEntryType::Uart as usize].0;
        #[cfg(target_arch = "x86_64")]
        let region_size: u64 = 8;
        #[cfg(target_arch = "aarch64")]
        let region_size: u64 = MEM_LAYOUT[LayoutEntryType::Uart as usize].1;

        let serial = Serial::realize(
            Serial::default(),
            &mut self.sysbus,
            region_base,
            region_size,
            #[cfg(target_arch = "aarch64")]
            &self.boot_source,
            vm_fd,
        )
        .chain_err(|| "Failed to realize serial device.")?;
        if config.stdio {
            EventLoop::update_event(EventNotifierHelper::internal_notifiers(serial), None)
                .chain_err(|| MachineErrorKind::RegNotiferErr)?;
        }
        Ok(())
    }

    fn add_block_device(&mut self, config: &DriveConfig) -> MachineResult<()> {
        if self.replaceable_info.block_count >= MMIO_REPLACEABLE_BLK_NR {
            bail!(
                "A maximum of {} replaceble block devices are supported.",
                MMIO_REPLACEABLE_BLK_NR
            );
        }

        let index = self.replaceable_info.block_count;
        self.fill_replaceable_device(&config.drive_id, Arc::new(config.clone()), index)?;
        self.replaceable_info.block_count += 1;
        Ok(())
    }

    fn add_vsock_device(&mut self, config: &VsockConfig, vm_fd: &Arc<VmFd>) -> MachineResult<()> {
        let vsock = Arc::new(Mutex::new(VhostKern::Vsock::new(config, &self.sys_mem)));
        let device = VirtioMmioDevice::new(&self.sys_mem, vsock);
        let region_base = self.sysbus.min_free_base;
        let region_size = MEM_LAYOUT[LayoutEntryType::Mmio as usize].1;

        VirtioMmioDevice::realize(
            device,
            &mut self.sysbus,
            region_base,
            region_size,
            #[cfg(target_arch = "x86_64")]
            &self.boot_source,
            vm_fd,
        )?;
        self.sysbus.min_free_base += region_size;
        Ok(())
    }

    fn add_net_device(
        &mut self,
        config: &NetworkInterfaceConfig,
        vm_fd: &Arc<VmFd>,
    ) -> MachineResult<()> {
        if config.vhost_type.is_some() {
            let net = Arc::new(Mutex::new(VhostKern::Net::new(config, &self.sys_mem)));
            let device = VirtioMmioDevice::new(&self.sys_mem, net);
            let region_base = self.sysbus.min_free_base;
            let region_size = MEM_LAYOUT[LayoutEntryType::Mmio as usize].1;

            VirtioMmioDevice::realize(
                device,
                &mut self.sysbus,
                region_base,
                region_size,
                #[cfg(target_arch = "x86_64")]
                &self.boot_source,
                vm_fd,
            )?;
            self.sysbus.min_free_base += region_size;
        } else {
            let index = MMIO_REPLACEABLE_BLK_NR + self.replaceable_info.net_count;
            if index >= MMIO_REPLACEABLE_BLK_NR + MMIO_REPLACEABLE_NET_NR {
                bail!(
                    "A maximum of {} net replaceble devices are supported.",
                    MMIO_REPLACEABLE_NET_NR
                );
            }

            self.fill_replaceable_device(&config.iface_id, Arc::new(config.clone()), index)?;
            self.replaceable_info.net_count += 1;
        }
        Ok(())
    }

    fn add_console_device(
        &mut self,
        config: &ConsoleConfig,
        vm_fd: &Arc<VmFd>,
    ) -> MachineResult<()> {
        let console = Arc::new(Mutex::new(Console::new(config.clone())));
        let device = VirtioMmioDevice::new(&self.sys_mem, console);
        let region_base = self.sysbus.min_free_base;
        let region_size = MEM_LAYOUT[LayoutEntryType::Mmio as usize].1;

        VirtioMmioDevice::realize(
            device,
            &mut self.sysbus,
            region_base,
            region_size,
            #[cfg(target_arch = "x86_64")]
            &self.boot_source,
            vm_fd,
        )?;
        self.sysbus.min_free_base += region_size;
        Ok(())
    }

    fn add_balloon_device(
        &mut self,
        config: &BalloonConfig,
        vm_fd: &Arc<VmFd>,
    ) -> MachineResult<()> {
        let balloon = Arc::new(Mutex::new(Balloon::new(config, self.sys_mem.clone())));
        Balloon::object_init(balloon.clone());
        let device = VirtioMmioDevice::new(&self.sys_mem, balloon);
        let region_base = self.sysbus.min_free_base;
        let region_size = MEM_LAYOUT[LayoutEntryType::Mmio as usize].1;

        VirtioMmioDevice::realize(
            device,
            &mut self.sysbus,
            region_base,
            region_size,
            #[cfg(target_arch = "x86_64")]
            &self.boot_source,
            vm_fd,
        )?;
        self.sysbus.min_free_base += region_size;
        Ok(())
    }

    fn add_devices(&mut self, vm_config: &VmConfig, vm_fd: &Arc<VmFd>) -> MachineResult<()> {
        use crate::errors::ResultExt;

        #[cfg(target_arch = "aarch64")]
        self.add_rtc_device(vm_fd)
            .chain_err(|| MachineErrorKind::AddDevErr("RTC".to_string()))?;

        if let Some(serial) = vm_config.serial.as_ref() {
            self.add_serial_device(serial, vm_fd)
                .chain_err(|| MachineErrorKind::AddDevErr("serial".to_string()))?;
        }

        if let Some(vsock) = vm_config.vsock.as_ref() {
            self.add_vsock_device(vsock, vm_fd)
                .chain_err(|| MachineErrorKind::AddDevErr("vsock".to_string()))?;
        }

        if let Some(drives) = vm_config.drives.as_ref() {
            for drive in drives {
                self.add_block_device(drive)
                    .chain_err(|| MachineErrorKind::AddDevErr("block".to_string()))?;
            }
        }

        if let Some(nets) = vm_config.nets.as_ref() {
            for net in nets {
                self.add_net_device(net, vm_fd)
                    .chain_err(|| MachineErrorKind::AddDevErr("net".to_string()))?;
            }
        }

        if let Some(consoles) = vm_config.consoles.as_ref() {
            for console in consoles {
                self.add_console_device(console, vm_fd)
                    .chain_err(|| MachineErrorKind::AddDevErr("console".to_string()))?;
            }
        }

        if let Some(balloon) = vm_config.balloon.as_ref() {
            self.add_balloon_device(balloon, vm_fd)
                .chain_err(|| MachineErrorKind::AddDevErr("balloon".to_string()))?;
        }

        Ok(())
    }

    fn syscall_whitelist(&self) -> Vec<BpfRule> {
        syscall_allow_list()
    }

    fn realize(mut self, vm_config: &VmConfig, fds: (Kvm, &Arc<VmFd>)) -> MachineResult<Arc<Self>> {
        use crate::errors::ResultExt;

        let vm_fd = fds.1;
        self.init_memory(
            fds,
            &vm_config.machine_config.mem_config,
            #[cfg(target_arch = "x86_64")]
            &self.sys_io,
            &self.sys_mem,
        )?;

        #[cfg(target_arch = "x86_64")]
        LightMachine::arch_init(vm_fd)?;

        let nrcpus = vm_config.machine_config.nr_cpus;
        let mut vcpu_fds = vec![];
        for cpu_id in 0..nrcpus {
            vcpu_fds.push(Arc::new(vm_fd.create_vcpu(cpu_id)?));
        }
        #[cfg(target_arch = "aarch64")]
        {
            // Interrupt Controller Chip init
            let intc_conf = InterruptControllerConfig {
                version: kvm_bindings::kvm_device_type_KVM_DEV_TYPE_ARM_VGIC_V3,
                vcpu_count: u64::from(vm_config.machine_config.nr_cpus),
                max_irq: 192,
                msi: true,
                dist_range: MEM_LAYOUT[LayoutEntryType::GicDist as usize],
                redist_region_ranges: vec![
                    MEM_LAYOUT[LayoutEntryType::GicRedist as usize],
                    MEM_LAYOUT[LayoutEntryType::HighGicRedist as usize],
                ],
                its_range: Some(MEM_LAYOUT[LayoutEntryType::GicIts as usize]),
            };
            let irq_chip = InterruptController::new(vm_fd.clone(), &intc_conf)
                .chain_err(|| "Failed to create interrupt controller.")?;
            self.irq_chip = Some(Arc::new(irq_chip));
            self.irq_chip
                .as_ref()
                .unwrap()
                .realize()
                .chain_err(|| "Failed to realize interrupt controller.")?;
        }

        // Add mmio devices
        self.create_replaceable_devices(&vm_fd)
            .chain_err(|| ErrorKind::CreateRplDev)?;
        self.add_devices(vm_config, &vm_fd)?;

        let vm = Arc::new(self);
        for vcpu_id in 0..nrcpus {
            #[cfg(target_arch = "aarch64")]
            let arch_cpu = ArchCPU::new(u32::from(vcpu_id));
            #[cfg(target_arch = "x86_64")]
            let arch_cpu = ArchCPU::new(u32::from(vcpu_id), u32::from(nrcpus));

            let cpu = CPU::new(
                vcpu_fds[vcpu_id as usize].clone(),
                vcpu_id,
                Arc::new(Mutex::new(arch_cpu)),
                vm.clone(),
            );
            let mut vcpus = vm.cpus.lock().unwrap();
            let newcpu = Arc::new(cpu);
            vcpus.push(newcpu.clone());
        }

        let boot_source = vm.boot_source.lock().unwrap();
        let boot_config: CPUBootConfig;
        let (initrd, initrd_size) = match &boot_source.initrd {
            Some(rd) => (Some(rd.initrd_file.clone()), rd.initrd_size),
            None => (None, 0),
        };
        #[cfg(target_arch = "aarch64")]
        {
            let bootloader_config = BootLoaderConfig {
                kernel: boot_source.kernel_file.clone(),
                initrd,
                initrd_size: initrd_size as u32,
                mem_start: MEM_LAYOUT[LayoutEntryType::Mem as usize].0,
            };
            let layout = load_kernel(&bootloader_config, &vm.sys_mem)
                .chain_err(|| MachineErrorKind::LoadKernErr)?;
            if let Some(rd) = &boot_source.initrd {
                *rd.initrd_addr.lock().unwrap() = layout.initrd_start;
            }

            boot_config = CPUBootConfig {
                fdt_addr: layout.dtb_start,
                kernel_addr: layout.kernel_start,
            };
        }
        #[cfg(target_arch = "x86_64")]
        {
            let gap_start = MEM_LAYOUT[LayoutEntryType::MemBelow4g as usize].0
                + MEM_LAYOUT[LayoutEntryType::MemBelow4g as usize].1;
            let gap_end = MEM_LAYOUT[LayoutEntryType::MemAbove4g as usize].0;
            let bootloader_config = BootLoaderConfig {
                kernel: boot_source.kernel_file.clone(),
                initrd,
                initrd_size: initrd_size as u32,
                kernel_cmdline: boot_source.kernel_cmdline.to_string(),
                cpu_count: vm.cpu_topo.nrcpus,
                gap_range: (gap_start, gap_end - gap_start),
                ioapic_addr: MEM_LAYOUT[LayoutEntryType::IoApic as usize].0 as u32,
                lapic_addr: MEM_LAYOUT[LayoutEntryType::LocalApic as usize].0 as u32,
            };

            let layout = load_kernel(&bootloader_config, &vm.sys_mem)
                .chain_err(|| MachineErrorKind::LoadKernErr)?;
            boot_config = CPUBootConfig {
                boot_ip: layout.kernel_start,
                boot_sp: layout.kernel_sp,
                zero_page: layout.zero_page_addr,
                code_segment: layout.segments.code_segment,
                data_segment: layout.segments.data_segment,
                gdt_base: layout.segments.gdt_base,
                gdt_size: layout.segments.gdt_limit,
                idt_base: layout.segments.idt_base,
                idt_size: layout.segments.idt_limit,
                pml4_start: layout.boot_pml4_addr,
            };
        }
        for cpu_index in 0..vm.cpu_topo.max_cpus {
            vm.cpus.lock().unwrap()[cpu_index as usize]
                .realize(vm_fd, &boot_config)
                .chain_err(|| format!("Failed to realize vcpu{}.", cpu_index))?;
        }

        // Needed to release lock here because generate_fdt_node() will
        // acquire it later, and the ownership of vm will be passed out
        // of the function.
        drop(boot_source);

        #[cfg(target_arch = "aarch64")]
        {
            let mut fdt = vec![0; device_tree::FDT_MAX_SIZE as usize];
            vm.generate_fdt_node(&mut fdt)
                .chain_err(|| MachineErrorKind::GenFdtErr)?;
            vm.sys_mem
                .write(
                    &mut fdt.as_slice(),
                    GuestAddress(boot_config.fdt_addr as u64),
                    fdt.len() as u64,
                )
                .chain_err(|| {
                    format!(
                        "Failed to write guest memory: addr={},size={}",
                        boot_config.fdt_addr,
                        fdt.len()
                    )
                })?;
        }
        vm.register_power_event(&vm.power_button)?;
        Ok(vm)
    }
}

impl MachineLifecycle for LightMachine {
    fn pause(&self) -> bool {
        if self.notify_lifecycle(KvmVmState::Running, KvmVmState::Paused) {
            event!(STOP);

            true
        } else {
            false
        }
    }

    fn resume(&self) -> bool {
        if !self.notify_lifecycle(KvmVmState::Paused, KvmVmState::Running) {
            return false;
        }

        event!(RESUME);

        true
    }

    fn destroy(&self) -> bool {
        let vmstate = {
            let state = self.vm_state.deref().0.lock().unwrap();
            *state
        };

        if !self.notify_lifecycle(vmstate, KvmVmState::Shutdown) {
            return false;
        }

        true
    }

    fn notify_lifecycle(&self, old: KvmVmState, new: KvmVmState) -> bool {
        use KvmVmState::*;

        let vmstate = self.vm_state.deref().0.lock().unwrap();
        if *vmstate != old {
            error!("Vm lifecycle error: state check failed.");
            return false;
        }
        drop(vmstate);

        match (old, new) {
            (Created, Running) => {
                if let Err(e) = self.vm_start(false) {
                    error!("Vm lifecycle error:{}", e);
                };
            }
            (Running, Paused) => {
                if let Err(e) = self.vm_pause() {
                    error!("Vm lifecycle error:{}", e);
                };
            }
            (Paused, Running) => {
                if let Err(e) = self.vm_resume() {
                    error!("Vm lifecycle error:{}", e);
                };
            }
            (_, Shutdown) => {
                if let Err(e) = self.vm_destroy() {
                    error!("Vm lifecycle error:{}", e);
                };
                self.power_button.write(1).unwrap();
            }
            (_, _) => {
                error!("Vm lifecycle error: this transform is illegal.");
                return false;
            }
        }

        let vmstate = self.vm_state.deref().0.lock().unwrap();
        if *vmstate != new {
            error!("Vm lifecycle error: state transform failed.");
            return false;
        }

        true
    }
}

impl MachineAddressInterface for LightMachine {
    #[cfg(target_arch = "x86_64")]
    fn pio_in(&self, addr: u64, mut data: &mut [u8]) -> bool {
        // The function pit_calibrate_tsc() in kernel gets stuck if data read from
        // io-port 0x61 is not 0x20.
        // This problem only happens before Linux version 4.18 (fixed by 368a540e0)
        if addr == 0x61 {
            data[0] = 0x20;
            return true;
        }
        let length = data.len() as u64;
        self.sys_io
            .read(&mut data, GuestAddress(addr), length)
            .is_ok()
    }

    #[cfg(target_arch = "x86_64")]
    fn pio_out(&self, addr: u64, mut data: &[u8]) -> bool {
        let count = data.len() as u64;
        self.sys_io
            .write(&mut data, GuestAddress(addr), count)
            .is_ok()
    }

    fn mmio_read(&self, addr: u64, mut data: &mut [u8]) -> bool {
        let length = data.len() as u64;
        self.sys_mem
            .read(&mut data, GuestAddress(addr), length)
            .is_ok()
    }

    fn mmio_write(&self, addr: u64, mut data: &[u8]) -> bool {
        let count = data.len() as u64;
        self.sys_mem
            .write(&mut data, GuestAddress(addr), count)
            .is_ok()
    }
}

impl DeviceInterface for LightMachine {
    fn query_status(&self) -> Response {
        let vmstate = self.vm_state.deref().0.lock().unwrap();
        let qmp_state = match *vmstate {
            KvmVmState::Running => qmp_schema::StatusInfo {
                singlestep: false,
                running: true,
                status: qmp_schema::RunState::running,
            },
            KvmVmState::Paused => qmp_schema::StatusInfo {
                singlestep: false,
                running: true,
                status: qmp_schema::RunState::paused,
            },
            _ => Default::default(),
        };

        Response::create_response(serde_json::to_value(&qmp_state).unwrap(), None)
    }

    fn query_cpus(&self) -> Response {
        let mut cpu_vec: Vec<serde_json::Value> = Vec::new();
        for cpu_index in 0..self.cpu_topo.max_cpus {
            if self.cpu_topo.get_mask(cpu_index as usize) == 1 {
                let thread_id = self.cpus.lock().unwrap()[cpu_index as usize].tid();
                let (socketid, coreid, threadid) = self.cpu_topo.get_topo(cpu_index as usize);
                let cpu_instance = qmp_schema::CpuInstanceProperties {
                    node_id: None,
                    socket_id: Some(socketid as isize),
                    core_id: Some(coreid as isize),
                    thread_id: Some(threadid as isize),
                };
                #[cfg(target_arch = "x86_64")]
                {
                    let cpu_info = qmp_schema::CpuInfo::x86 {
                        current: true,
                        qom_path: String::from("/machine/unattached/device[")
                            + &cpu_index.to_string()
                            + &"]".to_string(),
                        halted: false,
                        props: Some(cpu_instance),
                        CPU: cpu_index as isize,
                        thread_id: thread_id as isize,
                        x86: qmp_schema::CpuInfoX86 {},
                    };
                    cpu_vec.push(serde_json::to_value(cpu_info).unwrap());
                }
                #[cfg(target_arch = "aarch64")]
                {
                    let cpu_info = qmp_schema::CpuInfo::Arm {
                        current: true,
                        qom_path: String::from("/machine/unattached/device[")
                            + &cpu_index.to_string()
                            + &"]".to_string(),
                        halted: false,
                        props: Some(cpu_instance),
                        CPU: cpu_index as isize,
                        thread_id: thread_id as isize,
                        arm: qmp_schema::CpuInfoArm {},
                    };
                    cpu_vec.push(serde_json::to_value(cpu_info).unwrap());
                }
            }
        }
        Response::create_response(cpu_vec.into(), None)
    }

    fn query_hotpluggable_cpus(&self) -> Response {
        let mut hotplug_vec: Vec<serde_json::Value> = Vec::new();
        #[cfg(target_arch = "x86_64")]
        let cpu_type = String::from("host-x86-cpu");
        #[cfg(target_arch = "aarch64")]
        let cpu_type = String::from("host-aarch64-cpu");

        for cpu_index in 0..self.cpu_topo.max_cpus {
            if self.cpu_topo.get_mask(cpu_index as usize) == 0 {
                let (socketid, coreid, threadid) = self.cpu_topo.get_topo(cpu_index as usize);
                let cpu_instance = qmp_schema::CpuInstanceProperties {
                    node_id: None,
                    socket_id: Some(socketid as isize),
                    core_id: Some(coreid as isize),
                    thread_id: Some(threadid as isize),
                };
                let hotpluggable_cpu = qmp_schema::HotpluggableCPU {
                    type_: cpu_type.clone(),
                    vcpus_count: 1,
                    props: cpu_instance,
                    qom_path: None,
                };
                hotplug_vec.push(serde_json::to_value(hotpluggable_cpu).unwrap());
            } else {
                let (socketid, coreid, threadid) = self.cpu_topo.get_topo(cpu_index as usize);
                let cpu_instance = qmp_schema::CpuInstanceProperties {
                    node_id: None,
                    socket_id: Some(socketid as isize),
                    core_id: Some(coreid as isize),
                    thread_id: Some(threadid as isize),
                };
                let hotpluggable_cpu = qmp_schema::HotpluggableCPU {
                    type_: cpu_type.clone(),
                    vcpus_count: 1,
                    props: cpu_instance,
                    qom_path: Some(
                        String::from("/machine/unattached/device[")
                            + &cpu_index.to_string()
                            + &"]".to_string(),
                    ),
                };
                hotplug_vec.push(serde_json::to_value(hotpluggable_cpu).unwrap());
            }
        }
        Response::create_response(hotplug_vec.into(), None)
    }

    fn balloon(&self, value: u64) -> Response {
        if qmp_balloon(value) {
            return Response::create_empty_response();
        }
        Response::create_error_response(
            qmp_schema::QmpErrorClass::DeviceNotActive(
                "No balloon device has been activated".to_string(),
            ),
            None,
        )
    }

    fn query_balloon(&self) -> Response {
        if let Some(actual) = qmp_query_balloon() {
            let ret = qmp_schema::BalloonInfo { actual };
            return Response::create_response(serde_json::to_value(&ret).unwrap(), None);
        }
        Response::create_error_response(
            qmp_schema::QmpErrorClass::DeviceNotActive(
                "No balloon device has been activated".to_string(),
            ),
            None,
        )
    }

    fn device_add(
        &self,
        id: String,
        driver: String,
        addr: Option<String>,
        lun: Option<usize>,
    ) -> Response {
        // get slot of bus by addr or lun
        let mut slot = 0;
        if let Some(addr) = addr {
            let slot_str = addr.as_str().trim_start_matches("0x");

            if let Ok(n) = usize::from_str_radix(slot_str, 16) {
                slot = n;
            }
        } else if let Some(lun) = lun {
            slot = lun + 1;
        }

        match self.add_replaceable_device(&id, &driver, slot) {
            Ok(()) => Response::create_empty_response(),
            Err(ref e) => {
                error!("{}", e.display_chain());
                error!("Failed to add device: id {}, type {}", id, driver);
                Response::create_error_response(
                    qmp_schema::QmpErrorClass::GenericError(e.to_string()),
                    None,
                )
            }
        }
    }

    fn device_del(&self, device_id: String) -> Response {
        match self.del_replaceable_device(&device_id) {
            Ok(path) => {
                let block_del_event = qmp_schema::DEVICE_DELETED {
                    device: Some(device_id),
                    path,
                };
                event!(DEVICE_DELETED; block_del_event);

                Response::create_empty_response()
            }
            Err(ref e) => {
                error!("Failed to delete device: {}", e.display_chain());
                Response::create_error_response(
                    qmp_schema::QmpErrorClass::GenericError(e.to_string()),
                    None,
                )
            }
        }
    }

    fn blockdev_add(
        &self,
        node_name: String,
        file: qmp_schema::FileOptions,
        cache: Option<qmp_schema::CacheOptions>,
        read_only: Option<bool>,
    ) -> Response {
        const MAX_STRING_LENGTH: usize = 255;
        let read_only = if let Some(ro) = read_only { ro } else { false };

        let direct = if let Some(cache) = cache {
            match cache.direct {
                Some(direct) => direct,
                _ => true,
            }
        } else {
            true
        };

        let blk = Path::new(&file.filename);
        match metadata(blk) {
            Ok(meta) => {
                if (meta.st_mode() & libc::S_IFREG != libc::S_IFREG)
                    && (meta.st_mode() & libc::S_IFBLK != libc::S_IFBLK)
                {
                    error!("File {:?} is not a regular file or block device", blk);
                    return Response::create_error_response(
                        qmp_schema::QmpErrorClass::GenericError(
                            "File is not a regular file or block device".to_string(),
                        ),
                        None,
                    );
                }
            }
            Err(ref e) => {
                error!("Blockdev_add failed: {}", e);
                return Response::create_error_response(
                    qmp_schema::QmpErrorClass::GenericError(e.to_string()),
                    None,
                );
            }
        }

        if let Some(file_name) = blk.file_name() {
            if file_name.len() > MAX_STRING_LENGTH {
                error!("File name {:?} is illegal", file_name);
                return Response::create_error_response(
                    qmp_schema::QmpErrorClass::GenericError("Illegal block name".to_string()),
                    None,
                );
            }
        } else {
            error!("Path: {:?} is not valid", blk);
            return Response::create_error_response(
                qmp_schema::QmpErrorClass::GenericError("Invalid block path".to_string()),
                None,
            );
        }

        use errors::ResultExt;
        let config = DriveConfig {
            drive_id: node_name.clone(),
            path_on_host: file.filename,
            read_only,
            direct,
            serial_num: None,
            iothread: None,
            iops: None,
        };
        match self
            .add_replaceable_config(&node_name, Arc::new(config))
            .chain_err(|| "Failed to add virtio-blk.")
        {
            Ok(()) => Response::create_empty_response(),
            Err(ref e) => {
                error!("{}", e.display_chain());
                Response::create_error_response(
                    qmp_schema::QmpErrorClass::GenericError(e.to_string()),
                    None,
                )
            }
        }
    }

    fn netdev_add(&self, id: String, if_name: Option<String>, fds: Option<String>) -> Response {
        let mut config = NetworkInterfaceConfig {
            iface_id: id.clone(),
            host_dev_name: "".to_string(),
            mac: None,
            tap_fd: None,
            vhost_type: None,
            vhost_fd: None,
            iothread: None,
        };

        if let Some(fds) = fds {
            let netdev_fd = if fds.contains(':') {
                let col: Vec<_> = fds.split(':').collect();
                String::from(col[col.len() - 1])
            } else {
                String::from(&fds)
            };

            if let Some(fd_num) = QmpChannel::get_fd(&netdev_fd) {
                config.tap_fd = Some(fd_num);
            } else {
                // try to convert string to RawFd
                let fd_num = match netdev_fd.parse::<i32>() {
                    Ok(fd) => fd,
                    _ => {
                        error!(
                            "Add netdev error: failed to convert {} to RawFd.",
                            netdev_fd
                        );
                        return Response::create_error_response(
                            qmp_schema::QmpErrorClass::GenericError(
                                "Add netdev error: failed to convert {} to RawFd.".to_string(),
                            ),
                            None,
                        );
                    }
                };
                config.tap_fd = Some(fd_num);
            }
        } else if let Some(if_name) = if_name {
            config.host_dev_name = if_name.clone();
            if create_tap(None, Some(&if_name)).is_err() {
                return Response::create_error_response(
                    qmp_schema::QmpErrorClass::GenericError(
                        "Tap device already in use".to_string(),
                    ),
                    None,
                );
            }
        }

        match self.add_replaceable_config(&id, Arc::new(config)) {
            Ok(()) => Response::create_empty_response(),
            Err(ref e) => {
                error!("{}", e.display_chain());
                error!("Failed to add net device {}", id);
                Response::create_error_response(
                    qmp_schema::QmpErrorClass::GenericError(e.to_string()),
                    None,
                )
            }
        }
    }

    fn getfd(&self, fd_name: String, if_fd: Option<RawFd>) -> Response {
        if let Some(fd) = if_fd {
            QmpChannel::set_fd(fd_name, fd);
            Response::create_empty_response()
        } else {
            let err_resp =
                qmp_schema::QmpErrorClass::GenericError("Invalid SCM message".to_string());
            Response::create_error_response(err_resp, None)
        }
    }
}

impl MachineInterface for LightMachine {}
impl MachineExternalInterface for LightMachine {}

impl EventLoopManager for LightMachine {
    fn loop_should_exit(&self) -> bool {
        let vmstate = self.vm_state.deref().0.lock().unwrap();
        *vmstate == KvmVmState::Shutdown
    }

    fn loop_cleanup(&self) -> util::errors::Result<()> {
        if let Err(e) = std::io::stdin().lock().set_canon_mode() {
            error!(
                "destroy virtual machine: reset stdin to canonical mode failed, {}",
                e
            );
        }

        Ok(())
    }
}

// Function that helps to generate serial node in device-tree.
//
// # Arguments
//
// * `dev_info` - Device resource info of serial device.
// * `fdt` - Flatted device-tree blob where serial node will be filled into.
#[cfg(target_arch = "aarch64")]
fn generate_serial_device_node(fdt: &mut Vec<u8>, res: &SysRes) -> util::errors::Result<()> {
    let node = format!("/uart@{:x}", res.region_base);
    device_tree::add_sub_node(fdt, &node)?;
    device_tree::set_property_string(fdt, &node, "compatible", "ns16550a")?;
    device_tree::set_property_string(fdt, &node, "clock-names", "apb_pclk")?;
    device_tree::set_property_u32(fdt, &node, "clocks", device_tree::CLK_PHANDLE)?;
    device_tree::set_property_array_u64(fdt, &node, "reg", &[res.region_base, res.region_size])?;
    device_tree::set_property_array_u32(
        fdt,
        &node,
        "interrupts",
        &[
            device_tree::GIC_FDT_IRQ_TYPE_SPI,
            res.irq as u32,
            device_tree::IRQ_TYPE_EDGE_RISING,
        ],
    )?;
    Ok(())
}

// Function that helps to generate RTC node in device-tree.
//
// # Arguments
//
// * `dev_info` - Device resource info of RTC device.
// * `fdt` - Flatted device-tree blob where RTC node will be filled into.
#[cfg(target_arch = "aarch64")]
fn generate_rtc_device_node(fdt: &mut Vec<u8>, res: &SysRes) -> util::errors::Result<()> {
    let node = format!("/pl031@{:x}", res.region_base);
    device_tree::add_sub_node(fdt, &node)?;
    device_tree::set_property_string(fdt, &node, "compatible", "arm,pl031\0arm,primecell\0")?;
    device_tree::set_property_string(fdt, &node, "clock-names", "apb_pclk")?;
    device_tree::set_property_u32(fdt, &node, "clocks", device_tree::CLK_PHANDLE)?;
    device_tree::set_property_array_u64(fdt, &node, "reg", &[res.region_base, res.region_size])?;
    device_tree::set_property_array_u32(
        fdt,
        &node,
        "interrupts",
        &[
            device_tree::GIC_FDT_IRQ_TYPE_SPI,
            res.irq as u32,
            device_tree::IRQ_TYPE_LEVEL_HIGH,
        ],
    )?;
    Ok(())
}

// Function that helps to generate Virtio-Mmio device's node in device-tree.
//
// # Arguments
//
// * `dev_info` - Device resource info of Virtio-Mmio device.
// * `fdt` - Flatted device-tree blob where node will be filled into.
#[cfg(target_arch = "aarch64")]
fn generate_virtio_devices_node(fdt: &mut Vec<u8>, res: &SysRes) -> util::errors::Result<()> {
    let node = format!("/virtio_mmio@{:x}", res.region_base);
    device_tree::add_sub_node(fdt, &node)?;
    device_tree::set_property_string(fdt, &node, "compatible", "virtio,mmio")?;
    device_tree::set_property_u32(fdt, &node, "interrupt-parent", device_tree::GIC_PHANDLE)?;
    device_tree::set_property_array_u64(fdt, &node, "reg", &[res.region_base, res.region_size])?;
    device_tree::set_property_array_u32(
        fdt,
        &node,
        "interrupts",
        &[
            device_tree::GIC_FDT_IRQ_TYPE_SPI,
            res.irq as u32,
            device_tree::IRQ_TYPE_EDGE_RISING,
        ],
    )?;
    Ok(())
}

/// Trait that helps to generate all nodes in device-tree.
#[cfg(target_arch = "aarch64")]
trait CompileFDTHelper {
    /// Function that helps to generate cpu nodes.
    fn generate_cpu_nodes(&self, fdt: &mut Vec<u8>) -> util::errors::Result<()>;
    /// Function that helps to generate memory nodes.
    fn generate_memory_node(&self, fdt: &mut Vec<u8>) -> util::errors::Result<()>;
    /// Function that helps to generate Virtio-mmio devices' nodes.
    fn generate_devices_node(&self, fdt: &mut Vec<u8>) -> util::errors::Result<()>;
    /// Function that helps to generate the chosen node.
    fn generate_chosen_node(&self, fdt: &mut Vec<u8>) -> util::errors::Result<()>;
}

#[cfg(target_arch = "aarch64")]
impl CompileFDTHelper for LightMachine {
    fn generate_cpu_nodes(&self, fdt: &mut Vec<u8>) -> util::errors::Result<()> {
        let node = "/cpus";

        device_tree::add_sub_node(fdt, node)?;
        device_tree::set_property_u32(fdt, node, "#address-cells", 0x02)?;
        device_tree::set_property_u32(fdt, node, "#size-cells", 0x0)?;

        // Generate CPU topology
        if self.cpu_topo.max_cpus > 0 && self.cpu_topo.max_cpus % 8 == 0 {
            device_tree::add_sub_node(fdt, "/cpus/cpu-map")?;

            let sockets = self.cpu_topo.max_cpus / 8;
            for cluster in 0..u32::from(sockets) {
                let clster = format!("/cpus/cpu-map/cluster{}", cluster);
                device_tree::add_sub_node(fdt, &clster)?;

                for i in 0..2 as u32 {
                    let sub_cluster = format!("{}/cluster{}", clster, i);
                    device_tree::add_sub_node(fdt, &sub_cluster)?;

                    let core0 = format!("{}/core0", sub_cluster);
                    device_tree::add_sub_node(fdt, &core0)?;
                    let thread0 = format!("{}/thread0", core0);
                    device_tree::add_sub_node(fdt, &thread0)?;
                    device_tree::set_property_u32(fdt, &thread0, "cpu", cluster * 8 + i * 4 + 10)?;

                    let thread1 = format!("{}/thread1", core0);
                    device_tree::add_sub_node(fdt, &thread1)?;
                    device_tree::set_property_u32(
                        fdt,
                        &thread1,
                        "cpu",
                        cluster * 8 + i * 4 + 10 + 1,
                    )?;

                    let core1 = format!("{}/core1", sub_cluster);
                    device_tree::add_sub_node(fdt, &core1)?;
                    let thread0 = format!("{}/thread0", core1);
                    device_tree::add_sub_node(fdt, &thread0)?;
                    device_tree::set_property_u32(
                        fdt,
                        &thread0,
                        "cpu",
                        cluster * 8 + i * 4 + 10 + 2,
                    )?;

                    let thread1 = format!("{}/thread1", core1);
                    device_tree::add_sub_node(fdt, &thread1)?;
                    device_tree::set_property_u32(
                        fdt,
                        &thread1,
                        "cpu",
                        cluster * 8 + i * 4 + 10 + 3,
                    )?;
                }
            }
        }

        let cpu_list = self.cpus.lock().unwrap();
        for cpu_index in 0..self.cpu_topo.max_cpus {
            let mpidr = cpu_list[cpu_index as usize]
                .arch()
                .lock()
                .unwrap()
                .get_mpidr(cpu_list[cpu_index as usize].fd());

            let node = format!("/cpus/cpu@{:x}", mpidr);
            device_tree::add_sub_node(fdt, &node)?;
            device_tree::set_property_u32(
                fdt,
                &node,
                "phandle",
                u32::from(cpu_index) + device_tree::CPU_PHANDLE_START,
            )?;
            device_tree::set_property_string(fdt, &node, "device_type", "cpu")?;
            device_tree::set_property_string(fdt, &node, "compatible", "arm,arm-v8")?;
            if self.cpu_topo.max_cpus > 1 {
                device_tree::set_property_string(fdt, &node, "enable-method", "psci")?;
            }
            device_tree::set_property_u64(fdt, &node, "reg", mpidr & 0x007F_FFFF)?;
        }

        Ok(())
    }

    fn generate_memory_node(&self, fdt: &mut Vec<u8>) -> util::errors::Result<()> {
        let mem_base = MEM_LAYOUT[LayoutEntryType::Mem as usize].0;
        let mem_size = self.sys_mem.memory_end_address().raw_value()
            - MEM_LAYOUT[LayoutEntryType::Mem as usize].0;
        let node = "/memory";
        device_tree::add_sub_node(fdt, node)?;
        device_tree::set_property_string(fdt, node, "device_type", "memory")?;
        device_tree::set_property_array_u64(fdt, node, "reg", &[mem_base, mem_size as u64])?;

        Ok(())
    }

    fn generate_devices_node(&self, fdt: &mut Vec<u8>) -> util::errors::Result<()> {
        // timer
        let mut cells: Vec<u32> = Vec::new();
        for &irq in [13, 14, 11, 10].iter() {
            cells.push(device_tree::GIC_FDT_IRQ_TYPE_PPI);
            cells.push(irq);
            cells.push(device_tree::IRQ_TYPE_LEVEL_HIGH);
        }
        let node = "/timer";
        device_tree::add_sub_node(fdt, node)?;
        device_tree::set_property_string(fdt, node, "compatible", "arm,armv8-timer")?;
        device_tree::set_property(fdt, node, "always-on", None)?;
        device_tree::set_property_array_u32(fdt, node, "interrupts", &cells)?;

        // clock
        let node = "/apb-pclk";
        device_tree::add_sub_node(fdt, node)?;
        device_tree::set_property_string(fdt, node, "compatible", "fixed-clock")?;
        device_tree::set_property_string(fdt, node, "clock-output-names", "clk24mhz")?;
        device_tree::set_property_u32(fdt, node, "#clock-cells", 0x0)?;
        device_tree::set_property_u32(fdt, node, "clock-frequency", 24_000_000)?;
        device_tree::set_property_u32(fdt, node, "phandle", device_tree::CLK_PHANDLE)?;

        // psci
        let node = "/psci";
        device_tree::add_sub_node(fdt, node)?;
        device_tree::set_property_string(fdt, node, "compatible", "arm,psci-0.2")?;
        device_tree::set_property_string(fdt, node, "method", "hvc")?;

        // Reversing vector is needed because FDT node is added in reverse.
        for dev in self.sysbus.devices.iter().rev() {
            let mut locked_dev = dev.lock().unwrap();
            let dev_type = locked_dev.get_type();
            let sys_res = locked_dev.get_sys_resource();
            match dev_type {
                SysBusDevType::Serial => generate_serial_device_node(fdt, sys_res)?,
                SysBusDevType::Rtc => generate_rtc_device_node(fdt, sys_res)?,
                SysBusDevType::VirtioMmio => generate_virtio_devices_node(fdt, sys_res)?,
                _ => (),
            }
        }
        Ok(())
    }

    fn generate_chosen_node(&self, fdt: &mut Vec<u8>) -> util::errors::Result<()> {
        let node = "/chosen";

        let boot_source = self.boot_source.lock().unwrap();

        device_tree::add_sub_node(fdt, node)?;
        let cmdline = &boot_source.kernel_cmdline.to_string();
        device_tree::set_property_string(fdt, node, "bootargs", cmdline.as_str())?;

        match &boot_source.initrd {
            Some(initrd) => {
                device_tree::set_property_u64(
                    fdt,
                    node,
                    "linux,initrd-start",
                    *initrd.initrd_addr.lock().unwrap(),
                )?;
                device_tree::set_property_u64(
                    fdt,
                    node,
                    "linux,initrd-end",
                    *initrd.initrd_addr.lock().unwrap() + initrd.initrd_size,
                )?;
            }
            None => {}
        }

        Ok(())
    }
}

#[cfg(target_arch = "aarch64")]
impl device_tree::CompileFDT for LightMachine {
    fn generate_fdt_node(&self, fdt: &mut Vec<u8>) -> util::errors::Result<()> {
        device_tree::create_device_tree(fdt)?;

        device_tree::set_property_string(fdt, "/", "compatible", "linux,dummy-virt")?;
        device_tree::set_property_u32(fdt, "/", "#address-cells", 0x2)?;
        device_tree::set_property_u32(fdt, "/", "#size-cells", 0x2)?;
        device_tree::set_property_u32(fdt, "/", "interrupt-parent", device_tree::GIC_PHANDLE)?;

        self.generate_cpu_nodes(fdt)?;
        self.generate_memory_node(fdt)?;
        self.generate_devices_node(fdt)?;
        self.generate_chosen_node(fdt)?;
        self.irq_chip.as_ref().unwrap().generate_fdt_node(fdt)?;

        Ok(())
    }
}