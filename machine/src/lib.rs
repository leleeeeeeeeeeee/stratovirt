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

#[macro_use]
extern crate error_chain;
#[macro_use]
extern crate log;
#[macro_use]
extern crate machine_manager;
#[cfg(target_arch = "x86_64")]
#[macro_use]
extern crate migration_derive;
#[macro_use]
extern crate vmm_sys_util;

pub mod errors {
    error_chain! {
        links {
            AddressSpace(address_space::errors::Error, address_space::errors::ErrorKind);
            IntCtrl(devices::IntCtrlErrs::Error, devices::IntCtrlErrs::ErrorKind) #[cfg(target_arch = "aarch64")];
            Legacy(devices::LegacyErrs::Error, devices::LegacyErrs::ErrorKind);
            MicroVm(super::micro_vm::errors::Error, super::micro_vm::errors::ErrorKind);
            StdVm(super::standard_vm::errors::Error, super::standard_vm::errors::ErrorKind);
            Util(util::errors::Error, util::errors::ErrorKind);
            Virtio(virtio::errors::Error, virtio::errors::ErrorKind);
            MachineManager(machine_manager::config::errors::Error, machine_manager::config::errors::ErrorKind);
        }

        foreign_links {
            KvmIoctl(kvm_ioctls::Error);
            Io(std::io::Error);
        }

        errors {
            AddDevErr(dev: String) {
                display("Failed to add {} device.", dev)
            }
            LoadKernErr {
                display("Failed to load kernel.")
            }
            CrtMemSpaceErr {
                display("Failed to create memory address space")
            }
            CrtIoSpaceErr {
                display("Failed to create I/O address space")
            }
            RegMemRegionErr(base: u64, size: u64) {
                display("Failed to register region in memory space: base={},size={}", base, size)
            }
            InitPwrBtnErr {
                display("Failed to init power button.")
            }
            RlzVirtioMmioErr {
                display("Failed to realize virtio mmio.")
            }
            #[cfg(target_arch = "x86_64")]
            CrtIrqchipErr {
                display("Failed to create irq chip.")
            }
            #[cfg(target_arch = "x86_64")]
            SetTssErr {
                display("Failed to set tss address.")
            }
            #[cfg(target_arch = "x86_64")]
            CrtPitErr {
                display("Failed to create PIT.")
            }
            #[cfg(target_arch = "aarch64")]
            GenFdtErr {
                display("Failed to generate FDT.")
            }
            #[cfg(target_arch = "aarch64")]
            WrtFdtErr(addr: u64, size: usize) {
                display("Failed to write FDT: addr={}, size={}", addr, size)
            }
            RegNotifierErr {
                display("Failed to register event notifier.")
            }
            StartVcpuErr(id: u8) {
                display("Failed to run vcpu{}.", id)
            }
            PauseVcpuErr(id: u8) {
                display("Failed to pause vcpu{}.", id)
            }
            ResumeVcpuErr(id: u8) {
                display("Failed to resume vcpu{}.", id)
            }
            DestroyVcpuErr(id: u8) {
                display("Failed to destroy vcpu{}.", id)
            }
        }
    }
}

mod micro_vm;
mod standard_vm;

pub use micro_vm::LightMachine;
pub use standard_vm::StdMachine;
pub use virtio::{VhostKern, VirtioMmioState};

use std::os::unix::io::AsRawFd;
use std::sync::{Arc, Barrier, Mutex};

#[cfg(target_arch = "x86_64")]
use address_space::KvmIoListener;
use address_space::{create_host_mmaps, AddressSpace, KvmMemoryListener, Region};
use cpu::{ArchCPU, CPUBootConfig, CPUInterface, CPU};
use devices::legacy::FwCfgOps;
#[cfg(target_arch = "aarch64")]
use devices::InterruptController;
use hypervisor::KVM_FDS;
use kvm_ioctls::VcpuFd;
use machine_manager::config::{
    parse_vsock, BalloonConfig, ConsoleConfig, DriveConfig, MachineMemConfig,
    NetworkInterfaceConfig, PFlashConfig, RngConfig, SerialConfig, VmConfig,
};
use machine_manager::event_loop::EventLoop;
use machine_manager::machine::{KvmVmState, MachineInterface};
use migration::MigrationManager;
use util::loop_context::{EventNotifier, NotifierCallback, NotifierOperation};
use util::seccomp::{BpfRule, SeccompOpt, SyscallFilter};
use virtio::{balloon_allow_list, VirtioMmioDevice};
use vmm_sys_util::epoll::EventSet;
use vmm_sys_util::eventfd::EventFd;

use errors::{ErrorKind, Result, ResultExt};

pub trait MachineOps {
    /// Calculate the ranges of memory according to architecture.
    ///
    /// # Arguments
    ///
    /// * `mem_size` - memory size of VM.
    ///
    /// # Returns
    ///
    /// A array of ranges, it's element represents (start_addr, size).
    /// On x86_64, there is a gap ranged from (4G - 768M) to 4G, which will be skipped.
    fn arch_ram_ranges(&self, mem_size: u64) -> Vec<(u64, u64)>;

    fn load_boot_source(&self, fwcfg: Option<&Arc<Mutex<dyn FwCfgOps>>>) -> Result<CPUBootConfig>;

    /// Init I/O & memory address space and mmap guest memory.
    ///
    /// # Arguments
    ///
    /// * `mem_config` - Memory setting.
    /// * `sys_io` - IO address space required for x86_64.
    /// * `sys_mem` - Memory address space.
    fn init_memory(
        &self,
        mem_config: &MachineMemConfig,
        #[cfg(target_arch = "x86_64")] sys_io: &Arc<AddressSpace>,
        sys_mem: &Arc<AddressSpace>,
        is_migrate: bool,
    ) -> Result<()> {
        sys_mem
            .register_listener(Box::new(KvmMemoryListener::new(
                KVM_FDS.load().fd.as_ref().unwrap().get_nr_memslots() as u32,
            )))
            .chain_err(|| "Failed to register KVM listener for memory space.")?;
        #[cfg(target_arch = "x86_64")]
        sys_io
            .register_listener(Box::new(KvmIoListener::default()))
            .chain_err(|| "Failed to register KVM listener for I/O address space.")?;

        // Init guest-memory
        // Define ram-region ranges according to architectures
        if !is_migrate {
            let ram_ranges = self.arch_ram_ranges(mem_config.mem_size);
            let mem_mappings = create_host_mmaps(&ram_ranges, &mem_config)
                .chain_err(|| "Failed to mmap guest ram.")?;
            for mmap in mem_mappings.iter() {
                let base = mmap.start_address().raw_value();
                let size = mmap.size();
                sys_mem
                    .root()
                    .add_subregion(Region::init_ram_region(mmap.clone()), base)
                    .chain_err(|| ErrorKind::RegMemRegionErr(base, size))?;
            }
        }

        MigrationManager::register_memory_instance(sys_mem.clone());

        Ok(())
    }

    /// Init vcpu register with boot message.
    ///
    /// # Arguments
    ///
    /// * `vm` - `MachineInterface` to obtain functions cpu can use.
    /// * `nr_cpus` - The number of vcpus.
    /// * `fds` - File descriptors obtained by creating new Vcpu in KVM.
    /// * `boot_cfg` - Boot message generated by reading boot source to guest memory.
    fn init_vcpu(
        vm: Arc<Mutex<dyn MachineInterface + Send + Sync>>,
        nr_cpus: u8,
        fds: &[Arc<VcpuFd>],
        boot_cfg: &Option<CPUBootConfig>,
    ) -> Result<Vec<Arc<CPU>>>
    where
        Self: Sized,
    {
        let mut cpus = Vec::<Arc<CPU>>::new();

        for vcpu_id in 0..nr_cpus {
            #[cfg(target_arch = "aarch64")]
            let arch_cpu = ArchCPU::new(u32::from(vcpu_id));
            #[cfg(target_arch = "x86_64")]
            let arch_cpu = ArchCPU::new(u32::from(vcpu_id), u32::from(nr_cpus));

            let cpu = Arc::new(CPU::new(
                fds[vcpu_id as usize].clone(),
                vcpu_id,
                Arc::new(Mutex::new(arch_cpu)),
                vm.clone(),
            ));
            cpus.push(cpu.clone());

            MigrationManager::register_device_instance(cpu::ArchCPU::descriptor(), cpu);
        }

        if let Some(boot_config) = boot_cfg {
            for cpu_index in 0..nr_cpus as usize {
                cpus[cpu_index as usize]
                    .realize(&boot_config)
                    .chain_err(|| {
                        format!(
                            "Failed to realize arch cpu register for CPU {}/KVM",
                            cpu_index
                        )
                    })?;
            }
        }

        Ok(cpus)
    }

    /// Add interrupt controller.
    ///
    /// # Arguments
    ///
    /// * `vcpu_count` - The number of vcpu.
    fn init_interrupt_controller(&mut self, vcpu_count: u64) -> Result<()>;

    /// Add RTC device.
    fn add_rtc_device(&mut self, #[cfg(target_arch = "x86_64")] mem_size: u64) -> Result<()>;

    /// Add serial device.
    ///
    /// # Arguments
    ///
    /// * `config` - Device configuration.
    fn add_serial_device(&mut self, config: &SerialConfig) -> Result<()>;

    /// Add block device.
    ///
    /// # Arguments
    ///
    /// * `config` - Device configuration.
    fn add_block_device(&mut self, config: &DriveConfig) -> Result<()>;

    /// Add virtio mmio vsock device.
    ///
    /// # Arguments
    ///
    /// * `cfg_args` - Device configuration.
    fn add_virtio_vsock(&mut self, cfg_args: &str) -> Result<()> {
        let device_cfg = parse_vsock(cfg_args)?;
        let sys_mem = self.get_sys_mem();
        let vsock = Arc::new(Mutex::new(VhostKern::Vsock::new(&device_cfg, &sys_mem)));
        let device = VirtioMmioDevice::new(&sys_mem, vsock.clone());

        MigrationManager::register_device_instance_mutex(
            VirtioMmioState::descriptor(),
            self.realize_virtio_mmio_device(device)
                .chain_err(|| ErrorKind::RlzVirtioMmioErr)?,
        );
        MigrationManager::register_device_instance_mutex(
            VhostKern::VsockState::descriptor(),
            vsock,
        );

        Ok(())
    }

    fn realize_virtio_mmio_device(
        &mut self,
        _dev: VirtioMmioDevice,
    ) -> Result<Arc<Mutex<VirtioMmioDevice>>> {
        bail!("Virtio mmio devices not supported");
    }

    fn get_sys_mem(&mut self) -> &Arc<AddressSpace>;

    /// Add net device.
    ///
    /// # Arguments
    ///
    /// * `config` - Device configuration.
    fn add_net_device(&mut self, config: &NetworkInterfaceConfig) -> Result<()>;

    /// Add console device.
    ///
    /// # Arguments
    ///
    /// * `config` - Device configuration.
    fn add_console_device(&mut self, config: &ConsoleConfig) -> Result<()>;

    /// Add memory balloon device.
    ///
    /// # Arguments
    ///
    /// * `config` - Device configuration.
    fn add_balloon_device(&mut self, config: &BalloonConfig) -> Result<()>;

    /// Add virtio-rng device.
    ///
    /// # Arguments
    ///
    /// * `config` - Device configuration.
    fn add_rng_device(&mut self, _config: &RngConfig) -> Result<()> {
        Ok(())
    }

    /// Add peripheral devices.
    ///
    /// # Arguments
    ///
    /// * `vm_config` - VM Configuration.
    fn add_devices(&mut self, vm_config: &VmConfig) -> Result<()> {
        self.add_rtc_device(
            #[cfg(target_arch = "x86_64")]
            vm_config.machine_config.mem_config.mem_size,
        )
        .chain_err(|| ErrorKind::AddDevErr("RTC".to_string()))?;

        if let Some(serial) = vm_config.serial.as_ref() {
            self.add_serial_device(serial)
                .chain_err(|| ErrorKind::AddDevErr("serial".to_string()))?;
        }

        if let Some(pflashs) = vm_config.pflashs.as_ref() {
            for pflash in pflashs {
                self.add_pflash_device(pflash)
                    .chain_err(|| ErrorKind::AddDevErr("pflash".to_string()))?;
            }
        }

        if let Some(drives) = vm_config.drives.as_ref() {
            for drive in drives {
                self.add_block_device(drive)
                    .chain_err(|| ErrorKind::AddDevErr("block".to_string()))?;
            }
        }

        if let Some(nets) = vm_config.nets.as_ref() {
            for net in nets {
                self.add_net_device(net)
                    .chain_err(|| ErrorKind::AddDevErr("net".to_string()))?;
            }
        }

        if let Some(consoles) = vm_config.consoles.as_ref() {
            for console in consoles {
                self.add_console_device(console)
                    .chain_err(|| ErrorKind::AddDevErr("console".to_string()))?;
            }
        }

        if let Some(balloon) = vm_config.balloon.as_ref() {
            self.add_balloon_device(balloon)
                .chain_err(|| ErrorKind::AddDevErr("balloon".to_string()))?;
        }

        if let Some(rng) = vm_config.rng.as_ref() {
            self.add_rng_device(rng)?;
        }

        for dev in &vm_config.devices {
            let cfg_args = dev.1.as_str();
            match dev.0.as_str() {
                "vhost-vsock-device" => {
                    self.add_virtio_vsock(cfg_args)?;
                }
                _ => {
                    bail!("Unsupported device: {:?}", dev.0.as_str());
                }
            }
        }

        Ok(())
    }

    /// Add pflash device.
    fn add_pflash_device(&mut self, _config: &PFlashConfig) -> Result<()> {
        bail!("Pflash device is not supported!");
    }

    /// Return the syscall whitelist for seccomp.
    fn syscall_whitelist(&self) -> Vec<BpfRule>;

    /// Register seccomp rules in syscall whitelist to seccomp.
    fn register_seccomp(&self, balloon_enable: bool) -> Result<()> {
        let mut seccomp_filter = SyscallFilter::new(SeccompOpt::Trap);
        let mut bpf_rules = self.syscall_whitelist();
        if balloon_enable {
            balloon_allow_list(&mut bpf_rules);
        }

        for bpf_rule in &mut bpf_rules {
            seccomp_filter.push(bpf_rule);
        }
        seccomp_filter
            .realize()
            .chain_err(|| "Failed to init seccomp filter.")?;
        Ok(())
    }

    /// Register event notifier for power button of mainboard.
    ///
    /// # Arguments
    ///
    /// * `power_button` - Eventfd of the power button.
    fn register_power_event(&self, power_button: &EventFd) -> Result<()> {
        let power_button = power_button.try_clone().unwrap();
        let button_fd = power_button.as_raw_fd();
        let power_button_handler: Arc<Mutex<Box<NotifierCallback>>> =
            Arc::new(Mutex::new(Box::new(move |_, _| {
                let _ret = power_button.read().unwrap();
                None
            })));
        let notifier = EventNotifier::new(
            NotifierOperation::AddShared,
            button_fd,
            None,
            EventSet::IN,
            vec![power_button_handler],
        );

        EventLoop::update_event(vec![notifier], None).chain_err(|| ErrorKind::RegNotifierErr)?;
        Ok(())
    }

    /// Realize the machine.
    ///
    /// # Arguments
    ///
    /// * `vm` - The machine structure.
    /// * `vm_config` - VM configuration.
    fn realize(vm: &Arc<Mutex<Self>>, vm_config: &VmConfig, is_migrate: bool) -> Result<()>
    where
        Self: Sized;

    /// Run `LightMachine` with `paused` flag.
    ///
    /// # Arguments
    ///
    /// * `paused` - Flag for `paused` when `LightMachine` starts to run.
    fn run(&self, paused: bool) -> Result<()>;

    /// Start machine as `Running` or `Paused` state.
    ///
    /// # Arguments
    ///
    /// * `paused` - After started, paused all vcpu or not.
    /// * `cpus` - Cpus vector restore cpu structure.
    /// * `vm_state` - Vm kvm vm state.
    fn vm_start(paused: bool, cpus: &[Arc<CPU>], vm_state: &mut KvmVmState) -> Result<()>
    where
        Self: Sized,
    {
        let nr_vcpus = cpus.len();
        let cpus_thread_barrier = Arc::new(Barrier::new((nr_vcpus + 1) as usize));
        for cpu_index in 0..nr_vcpus {
            let cpu_thread_barrier = cpus_thread_barrier.clone();
            let cpu = cpus[cpu_index as usize].clone();
            CPU::start(cpu, cpu_thread_barrier, paused)
                .chain_err(|| format!("Failed to run vcpu{}", cpu_index))?;
        }

        if paused {
            *vm_state = KvmVmState::Paused;
        } else {
            *vm_state = KvmVmState::Running;
        }
        cpus_thread_barrier.wait();

        Ok(())
    }

    /// Pause VM as `Paused` state, sleepy all vcpu thread.
    ///
    /// # Arguments
    ///
    /// * `cpus` - Cpus vector restore cpu structure.
    /// * `vm_state` - Vm kvm vm state.
    fn vm_pause(
        cpus: &[Arc<CPU>],
        #[cfg(target_arch = "aarch64")] irq_chip: &Option<Arc<InterruptController>>,
        vm_state: &mut KvmVmState,
    ) -> Result<()>
    where
        Self: Sized,
    {
        for (cpu_index, cpu) in cpus.iter().enumerate() {
            cpu.pause()
                .chain_err(|| format!("Failed to pause vcpu{}", cpu_index))?;
        }

        #[cfg(target_arch = "aarch64")]
        irq_chip.as_ref().unwrap().stop();

        *vm_state = KvmVmState::Paused;

        Ok(())
    }

    /// Resume VM as `Running` state, awaken all vcpu thread.
    ///
    /// # Arguments
    ///
    /// * `cpus` - Cpus vector restore cpu structure.
    /// * `vm_state` - Vm kvm vm state.
    fn vm_resume(cpus: &[Arc<CPU>], vm_state: &mut KvmVmState) -> Result<()>
    where
        Self: Sized,
    {
        for (cpu_index, cpu) in cpus.iter().enumerate() {
            cpu.resume()
                .chain_err(|| format!("Failed to resume vcpu{}", cpu_index))?;
        }

        *vm_state = KvmVmState::Running;

        Ok(())
    }

    /// Destroy VM as `Shutdown` state, destroy vcpu thread.
    ///
    /// # Arguments
    ///
    /// * `cpus` - Cpus vector restore cpu structure.
    /// * `vm_state` - Vm kvm vm state.
    fn vm_destroy(cpus: &[Arc<CPU>], vm_state: &mut KvmVmState) -> Result<()>
    where
        Self: Sized,
    {
        for (cpu_index, cpu) in cpus.iter().enumerate() {
            cpu.destroy()
                .chain_err(|| format!("Failed to destroy vcpu{}", cpu_index))?;
        }

        *vm_state = KvmVmState::Shutdown;

        Ok(())
    }

    /// Transfer VM state from `old` to `new`.
    ///
    /// # Arguments
    ///
    /// * `cpus` - Cpus vector restore cpu structure.
    /// * `vm_state` - Vm kvm vm state.
    /// * `old_state` - Old vm state want to leave.
    /// * `new_state` - New vm state want to transfer to.
    fn vm_state_transfer(
        cpus: &[Arc<CPU>],
        #[cfg(target_arch = "aarch64")] irq_chip: &Option<Arc<InterruptController>>,
        vm_state: &mut KvmVmState,
        old_state: KvmVmState,
        new_state: KvmVmState,
    ) -> Result<()>
    where
        Self: Sized,
    {
        use KvmVmState::*;

        if *vm_state != old_state {
            bail!("Vm lifecycle error: state check failed.");
        }

        match (old_state, new_state) {
            (Created, Running) => <Self as MachineOps>::vm_start(false, cpus, vm_state)
                .chain_err(|| "Failed to start vm.")?,
            (Running, Paused) => <Self as MachineOps>::vm_pause(
                cpus,
                #[cfg(target_arch = "aarch64")]
                irq_chip,
                vm_state,
            )
            .chain_err(|| "Failed to pause vm.")?,
            (Paused, Running) => <Self as MachineOps>::vm_resume(cpus, vm_state)
                .chain_err(|| "Failed to resume vm.")?,
            (_, Shutdown) => {
                <Self as MachineOps>::vm_destroy(cpus, vm_state)
                    .chain_err(|| "Failed to destroy vm.")?;
            }
            (_, _) => {
                bail!("Vm lifecycle error: this transform is illegal.");
            }
        }

        if *vm_state != new_state {
            bail!(
                "Vm lifecycle error: state '{:?} -> {:?}' transform failed.",
                old_state,
                new_state
            );
        }

        Ok(())
    }
}
