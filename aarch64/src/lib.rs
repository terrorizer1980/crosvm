// Copyright 2018 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! ARM 64-bit architecture support.

#![cfg(any(target_arch = "arm", target_arch = "aarch64"))]

use std::collections::BTreeMap;
use std::io;
use std::sync::mpsc;
use std::sync::Arc;

use arch::get_serial_cmdline;
use arch::GetSerialCmdlineError;
use arch::MsrConfig;
use arch::MsrExitHandlerError;
use arch::RunnableLinuxVm;
use arch::VmComponents;
use arch::VmImage;
use base::Event;
use base::MemoryMappingBuilder;
use base::SendTube;
use devices::serial_device::SerialHardware;
use devices::serial_device::SerialParameters;
use devices::vmwdt::VMWDT_DEFAULT_CLOCK_HZ;
use devices::vmwdt::VMWDT_DEFAULT_TIMEOUT_SEC;
use devices::Bus;
use devices::BusDeviceObj;
use devices::BusError;
use devices::IrqChip;
use devices::IrqChipAArch64;
use devices::IrqEventSource;
use devices::PciAddress;
use devices::PciConfigMmio;
use devices::PciDevice;
use devices::PciRootCommand;
use devices::Serial;
#[cfg(all(target_arch = "aarch64", feature = "gdb"))]
use gdbstub::arch::Arch;
#[cfg(all(target_arch = "aarch64", feature = "gdb"))]
use gdbstub_arch::aarch64::AArch64 as GdbArch;
use hypervisor::CpuConfigAArch64;
use hypervisor::DeviceKind;
use hypervisor::Hypervisor;
use hypervisor::HypervisorCap;
use hypervisor::ProtectionType;
use hypervisor::VcpuAArch64;
use hypervisor::VcpuFeature;
use hypervisor::VcpuInitAArch64;
use hypervisor::VcpuRegAArch64;
use hypervisor::Vm;
use hypervisor::VmAArch64;
use minijail::Minijail;
use remain::sorted;
use resources::AddressRange;
use resources::SystemAllocator;
use resources::SystemAllocatorConfig;
use sync::Mutex;
use thiserror::Error;
use vm_control::BatControl;
use vm_control::BatteryType;
use vm_memory::GuestAddress;
use vm_memory::GuestMemory;
use vm_memory::GuestMemoryError;

mod fdt;

// We place the kernel at offset 8MB
const AARCH64_KERNEL_OFFSET: u64 = 0x800000;
const AARCH64_FDT_MAX_SIZE: u64 = 0x200000;
const AARCH64_INITRD_ALIGN: u64 = 0x1000000;

// These constants indicate the address space used by the ARM vGIC.
const AARCH64_GIC_DIST_SIZE: u64 = 0x10000;
const AARCH64_GIC_CPUI_SIZE: u64 = 0x20000;

// This indicates the start of DRAM inside the physical address space.
const AARCH64_PHYS_MEM_START: u64 = 0x80000000;
const AARCH64_AXI_BASE: u64 = 0x40000000;
const AARCH64_PLATFORM_MMIO_SIZE: u64 = 0x800000;

// FDT is placed at the front of RAM when booting in BIOS mode.
const AARCH64_FDT_OFFSET_IN_BIOS_MODE: u64 = 0x0;
// Therefore, the BIOS is placed after the FDT in memory.
const AARCH64_BIOS_OFFSET: u64 = AARCH64_FDT_MAX_SIZE;
const AARCH64_BIOS_MAX_LEN: u64 = 1 << 20;

const AARCH64_PROTECTED_VM_FW_MAX_SIZE: u64 = 0x400000;
const AARCH64_PROTECTED_VM_FW_START: u64 =
    AARCH64_PHYS_MEM_START - AARCH64_PROTECTED_VM_FW_MAX_SIZE;

const AARCH64_PVTIME_IPA_MAX_SIZE: u64 = 0x10000;
const AARCH64_PVTIME_IPA_START: u64 = AARCH64_MMIO_BASE - AARCH64_PVTIME_IPA_MAX_SIZE;
const AARCH64_PVTIME_SIZE: u64 = 64;

// These constants indicate the placement of the GIC registers in the physical
// address space.
const AARCH64_GIC_DIST_BASE: u64 = AARCH64_AXI_BASE - AARCH64_GIC_DIST_SIZE;
const AARCH64_GIC_CPUI_BASE: u64 = AARCH64_GIC_DIST_BASE - AARCH64_GIC_CPUI_SIZE;
const AARCH64_GIC_REDIST_SIZE: u64 = 0x20000;

// PSR (Processor State Register) bits
const PSR_MODE_EL1H: u64 = 0x00000005;
const PSR_F_BIT: u64 = 0x00000040;
const PSR_I_BIT: u64 = 0x00000080;
const PSR_A_BIT: u64 = 0x00000100;
const PSR_D_BIT: u64 = 0x00000200;

fn get_kernel_addr() -> GuestAddress {
    GuestAddress(AARCH64_PHYS_MEM_START + AARCH64_KERNEL_OFFSET)
}

fn get_bios_addr() -> GuestAddress {
    GuestAddress(AARCH64_PHYS_MEM_START + AARCH64_BIOS_OFFSET)
}

// Serial device requires 8 bytes of registers;
const AARCH64_SERIAL_SIZE: u64 = 0x8;
// This was the speed kvmtool used, not sure if it matters.
const AARCH64_SERIAL_SPEED: u32 = 1843200;
// The serial device gets the first interrupt line
// Which gets mapped to the first SPI interrupt (physical 32).
const AARCH64_SERIAL_1_3_IRQ: u32 = 0;
const AARCH64_SERIAL_2_4_IRQ: u32 = 2;

// Place the RTC device at page 2
const AARCH64_RTC_ADDR: u64 = 0x2000;
// The RTC device gets one 4k page
const AARCH64_RTC_SIZE: u64 = 0x1000;
// The RTC device gets the second interrupt line
const AARCH64_RTC_IRQ: u32 = 1;

// Place the virtual watchdog device at page 3
const AARCH64_VMWDT_ADDR: u64 = 0x3000;
// The virtual watchdog device gets one 4k page
const AARCH64_VMWDT_SIZE: u64 = 0x1000;

// PCI MMIO configuration region base address.
const AARCH64_PCI_CFG_BASE: u64 = 0x10000;
// PCI MMIO configuration region size.
const AARCH64_PCI_CFG_SIZE: u64 = 0x1000000;
// This is the base address of MMIO devices.
const AARCH64_MMIO_BASE: u64 = 0x2000000;
// Size of the whole MMIO region.
const AARCH64_MMIO_SIZE: u64 = 0x2000000;
// Virtio devices start at SPI interrupt number 3
const AARCH64_IRQ_BASE: u32 = 3;

// PMU PPI interrupt, same as qemu
const AARCH64_PMU_IRQ: u32 = 7;

#[sorted]
#[derive(Error, Debug)]
pub enum Error {
    #[error("failed to allocate IRQ number")]
    AllocateIrq,
    #[error("bios could not be loaded: {0}")]
    BiosLoadFailure(arch::LoadImageError),
    #[error("failed to build arm pvtime memory: {0}")]
    BuildPvtimeError(base::MmapError),
    #[error("unable to clone an Event: {0}")]
    CloneEvent(base::Error),
    #[error("failed to clone IRQ chip: {0}")]
    CloneIrqChip(base::Error),
    #[error("the given kernel command line was invalid: {0}")]
    Cmdline(kernel_cmdline::Error),
    #[error("unable to create battery devices: {0}")]
    CreateBatDevices(arch::DeviceRegistrationError),
    #[error("unable to make an Event: {0}")]
    CreateEvent(base::Error),
    #[error("FDT could not be created: {0}")]
    CreateFdt(arch::fdt::Error),
    #[error("failed to create GIC: {0}")]
    CreateGICFailure(base::Error),
    #[error("failed to create a PCI root hub: {0}")]
    CreatePciRoot(arch::DeviceRegistrationError),
    #[error("failed to create platform bus: {0}")]
    CreatePlatformBus(arch::DeviceRegistrationError),
    #[error("unable to create serial devices: {0}")]
    CreateSerialDevices(arch::DeviceRegistrationError),
    #[error("failed to create socket: {0}")]
    CreateSocket(io::Error),
    #[error("failed to create VCPU: {0}")]
    CreateVcpu(base::Error),
    #[error("vm created wrong kind of vcpu")]
    DowncastVcpu,
    #[error("failed to enable singlestep execution: {0}")]
    EnableSinglestep(base::Error),
    #[error("failed to finalize IRQ chip: {0}")]
    FinalizeIrqChip(base::Error),
    #[error("failed to get HW breakpoint count: {0}")]
    GetMaxHwBreakPoint(base::Error),
    #[error("failed to get PSCI version: {0}")]
    GetPsciVersion(base::Error),
    #[error("failed to get serial cmdline: {0}")]
    GetSerialCmdline(GetSerialCmdlineError),
    #[error("failed to initialize arm pvtime: {0}")]
    InitPvtimeError(base::Error),
    #[error("initrd could not be loaded: {0}")]
    InitrdLoadFailure(arch::LoadImageError),
    #[error("kernel could not be loaded: {0}")]
    KernelLoadFailure(arch::LoadImageError),
    #[error("error loading Kernel from Elf image: {0}")]
    LoadElfKernel(kernel_loader::Error),
    #[error("failed to map arm pvtime memory: {0}")]
    MapPvtimeError(base::Error),
    #[error("failed to protect vm: {0}")]
    ProtectVm(base::Error),
    #[error("pVM firmware could not be loaded: {0}")]
    PvmFwLoadFailure(arch::LoadImageError),
    #[error("ramoops address is different from high_mmio_base: {0} vs {1}")]
    RamoopsAddress(u64, u64),
    #[error("error reading guest memory: {0}")]
    ReadGuestMemory(vm_memory::GuestMemoryError),
    #[error("error reading CPU register: {0}")]
    ReadReg(base::Error),
    #[error("error reading CPU registers: {0}")]
    ReadRegs(base::Error),
    #[error("failed to register irq fd: {0}")]
    RegisterIrqfd(base::Error),
    #[error("error registering PCI bus: {0}")]
    RegisterPci(BusError),
    #[error("error registering virtual socket device: {0}")]
    RegisterVsock(arch::DeviceRegistrationError),
    #[error("failed to set device attr: {0}")]
    SetDeviceAttr(base::Error),
    #[error("failed to set a hardware breakpoint: {0}")]
    SetHwBreakpoint(base::Error),
    #[error("failed to set register: {0}")]
    SetReg(base::Error),
    #[error("failed to set up guest memory: {0}")]
    SetupGuestMemory(GuestMemoryError),
    #[error("this function isn't supported")]
    Unsupported,
    #[error("failed to initialize VCPU: {0}")]
    VcpuInit(base::Error),
    #[error("error writing guest memory: {0}")]
    WriteGuestMemory(GuestMemoryError),
    #[error("error writing CPU register: {0}")]
    WriteReg(base::Error),
    #[error("error writing CPU registers: {0}")]
    WriteRegs(base::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

fn fdt_offset(mem_size: u64, has_bios: bool) -> u64 {
    // TODO(rammuthiah) make kernel and BIOS startup use FDT from the same location. ARCVM startup
    // currently expects the kernel at 0x80080000 and the FDT at the end of RAM for unknown reasons.
    // Root cause and figure out how to fold these code paths together.
    if has_bios {
        AARCH64_FDT_OFFSET_IN_BIOS_MODE
    } else {
        // Put fdt up near the top of memory
        // TODO(sonnyrao): will have to handle this differently if there's
        // > 4GB memory
        mem_size - AARCH64_FDT_MAX_SIZE - 0x10000
    }
}

pub struct AArch64;

impl arch::LinuxArch for AArch64 {
    type Error = Error;

    /// Returns a Vec of the valid memory addresses.
    /// These should be used to configure the GuestMemory structure for the platform.
    fn guest_memory_layout(
        components: &VmComponents,
    ) -> std::result::Result<Vec<(GuestAddress, u64)>, Self::Error> {
        let mut memory_regions =
            vec![(GuestAddress(AARCH64_PHYS_MEM_START), components.memory_size)];

        // Allocate memory for the pVM firmware.
        if matches!(
            components.hv_cfg.protection_type,
            ProtectionType::Protected | ProtectionType::UnprotectedWithFirmware
        ) {
            memory_regions.push((
                GuestAddress(AARCH64_PROTECTED_VM_FW_START),
                AARCH64_PROTECTED_VM_FW_MAX_SIZE,
            ));
        }

        Ok(memory_regions)
    }

    fn get_system_allocator_config<V: Vm>(vm: &V) -> SystemAllocatorConfig {
        Self::get_resource_allocator_config(
            vm.get_memory().memory_size(),
            vm.get_guest_phys_addr_bits(),
        )
    }

    fn build_vm<V, Vcpu>(
        mut components: VmComponents,
        _vm_evt_wrtube: &SendTube,
        system_allocator: &mut SystemAllocator,
        serial_parameters: &BTreeMap<(SerialHardware, u8), SerialParameters>,
        serial_jail: Option<Minijail>,
        (bat_type, bat_jail): (Option<BatteryType>, Option<Minijail>),
        mut vm: V,
        ramoops_region: Option<arch::pstore::RamoopsRegion>,
        devs: Vec<(Box<dyn BusDeviceObj>, Option<Minijail>)>,
        irq_chip: &mut dyn IrqChipAArch64,
        vcpu_ids: &mut Vec<usize>,
        _debugcon_jail: Option<Minijail>,
    ) -> std::result::Result<RunnableLinuxVm<V, Vcpu>, Self::Error>
    where
        V: VmAArch64,
        Vcpu: VcpuAArch64,
    {
        let has_bios = matches!(components.vm_image, VmImage::Bios(_));
        let mem = vm.get_memory().clone();

        // separate out image loading from other setup to get a specific error for
        // image loading
        let mut initrd = None;
        let image_size = match components.vm_image {
            VmImage::Bios(ref mut bios) => {
                arch::load_image(&mem, bios, get_bios_addr(), AARCH64_BIOS_MAX_LEN)
                    .map_err(Error::BiosLoadFailure)?
            }
            VmImage::Kernel(ref mut kernel_image) => {
                let kernel_end: u64;
                let kernel_size: usize;
                let elf_result = kernel_loader::load_elf64(&mem, get_kernel_addr(), kernel_image);
                if elf_result == Err(kernel_loader::Error::InvalidElfMagicNumber) {
                    kernel_size =
                        arch::load_image(&mem, kernel_image, get_kernel_addr(), u64::max_value())
                            .map_err(Error::KernelLoadFailure)?;
                    kernel_end = get_kernel_addr().offset() + kernel_size as u64;
                } else {
                    let loaded_kernel = elf_result.map_err(Error::LoadElfKernel)?;
                    kernel_size = loaded_kernel.size as usize;
                    kernel_end = loaded_kernel.address_range.end;
                }
                initrd = match components.initrd_image {
                    Some(initrd_file) => {
                        let mut initrd_file = initrd_file;
                        let initrd_addr =
                            (kernel_end + (AARCH64_INITRD_ALIGN - 1)) & !(AARCH64_INITRD_ALIGN - 1);
                        let initrd_max_size =
                            components.memory_size - (initrd_addr - AARCH64_PHYS_MEM_START);
                        let initrd_addr = GuestAddress(initrd_addr);
                        let initrd_size =
                            arch::load_image(&mem, &mut initrd_file, initrd_addr, initrd_max_size)
                                .map_err(Error::InitrdLoadFailure)?;
                        Some((initrd_addr, initrd_size))
                    }
                    None => None,
                };
                kernel_size
            }
        };

        let mut use_pmu = vm
            .get_hypervisor()
            .check_capability(HypervisorCap::ArmPmuV3);
        let vcpu_count = components.vcpu_count;
        let mut has_pvtime = true;
        let mut vcpus = Vec::with_capacity(vcpu_count);
        for vcpu_id in 0..vcpu_count {
            let vcpu: Vcpu = *vm
                .create_vcpu(vcpu_id)
                .map_err(Error::CreateVcpu)?
                .downcast::<Vcpu>()
                .map_err(|_| Error::DowncastVcpu)?;
            Self::configure_vcpu_early(
                vm.get_memory(),
                &vcpu,
                vcpu_id,
                use_pmu,
                has_bios,
                image_size,
                components.hv_cfg.protection_type,
            )?;
            has_pvtime &= vcpu.has_pvtime_support();
            vcpus.push(vcpu);
            vcpu_ids.push(vcpu_id);
        }

        irq_chip.finalize().map_err(Error::FinalizeIrqChip)?;

        if has_pvtime {
            let pvtime_mem = MemoryMappingBuilder::new(AARCH64_PVTIME_IPA_MAX_SIZE as usize)
                .build()
                .map_err(Error::BuildPvtimeError)?;
            vm.add_memory_region(
                GuestAddress(AARCH64_PVTIME_IPA_START),
                Box::new(pvtime_mem),
                false,
                false,
            )
            .map_err(Error::MapPvtimeError)?;
        }

        match components.hv_cfg.protection_type {
            ProtectionType::Protected => {
                // Tell the hypervisor to load the pVM firmware.
                vm.load_protected_vm_firmware(
                    GuestAddress(AARCH64_PROTECTED_VM_FW_START),
                    AARCH64_PROTECTED_VM_FW_MAX_SIZE,
                )
                .map_err(Error::ProtectVm)?;
            }
            ProtectionType::UnprotectedWithFirmware => {
                // Load pVM firmware ourself, as the VM is not really protected.
                // `components.pvm_fw` is safe to unwrap because `protection_type` is
                // `UnprotectedWithFirmware`.
                arch::load_image(
                    &mem,
                    &mut components.pvm_fw.unwrap(),
                    GuestAddress(AARCH64_PROTECTED_VM_FW_START),
                    AARCH64_PROTECTED_VM_FW_MAX_SIZE,
                )
                .map_err(Error::PvmFwLoadFailure)?;
            }
            ProtectionType::Unprotected | ProtectionType::ProtectedWithoutFirmware => {}
        }

        for (vcpu_id, vcpu) in vcpus.iter().enumerate() {
            use_pmu &= vcpu.init_pmu(AARCH64_PMU_IRQ as u64 + 16).is_ok();
            if has_pvtime {
                vcpu.init_pvtime(AARCH64_PVTIME_IPA_START + (vcpu_id as u64 * AARCH64_PVTIME_SIZE))
                    .map_err(Error::InitPvtimeError)?;
            }
        }

        let mmio_bus = Arc::new(devices::Bus::new());

        // ARM doesn't really use the io bus like x86, so just create an empty bus.
        let io_bus = Arc::new(devices::Bus::new());

        // Event used by PMDevice to notify crosvm that
        // guest OS is trying to suspend.
        let suspend_evt = Event::new().map_err(Error::CreateEvent)?;

        let (pci_devices, others): (Vec<_>, Vec<_>) = devs
            .into_iter()
            .partition(|(dev, _)| dev.as_pci_device().is_some());

        let pci_devices = pci_devices
            .into_iter()
            .map(|(dev, jail_orig)| (dev.into_pci_device().unwrap(), jail_orig))
            .collect();
        let (pci, pci_irqs, mut pid_debug_label_map, _amls) = arch::generate_pci_root(
            pci_devices,
            irq_chip.as_irq_chip_mut(),
            mmio_bus.clone(),
            io_bus.clone(),
            system_allocator,
            &mut vm,
            (devices::AARCH64_GIC_NR_SPIS - AARCH64_IRQ_BASE) as usize,
            None,
        )
        .map_err(Error::CreatePciRoot)?;

        let pci_root = Arc::new(Mutex::new(pci));
        let pci_bus = Arc::new(Mutex::new(PciConfigMmio::new(pci_root.clone(), 8)));
        let (platform_devices, _others): (Vec<_>, Vec<_>) = others
            .into_iter()
            .partition(|(dev, _)| dev.as_platform_device().is_some());

        let platform_devices = platform_devices
            .into_iter()
            .map(|(dev, jail_orig)| (*(dev.into_platform_device().unwrap()), jail_orig))
            .collect();
        let (platform_devices, mut platform_pid_debug_label_map) =
            arch::sys::unix::generate_platform_bus(
                platform_devices,
                irq_chip.as_irq_chip_mut(),
                &mmio_bus,
                system_allocator,
            )
            .map_err(Error::CreatePlatformBus)?;
        pid_debug_label_map.append(&mut platform_pid_debug_label_map);

        Self::add_arch_devs(
            irq_chip.as_irq_chip_mut(),
            &mmio_bus,
            vcpu_count,
            _vm_evt_wrtube,
        )?;

        let com_evt_1_3 = devices::IrqEdgeEvent::new().map_err(Error::CreateEvent)?;
        let com_evt_2_4 = devices::IrqEdgeEvent::new().map_err(Error::CreateEvent)?;
        arch::add_serial_devices(
            components.hv_cfg.protection_type,
            &mmio_bus,
            com_evt_1_3.get_trigger(),
            com_evt_2_4.get_trigger(),
            serial_parameters,
            serial_jail,
        )
        .map_err(Error::CreateSerialDevices)?;

        let source = IrqEventSource {
            device_id: Serial::device_id(),
            queue_id: 0,
            device_name: Serial::debug_label(),
        };
        irq_chip
            .register_edge_irq_event(AARCH64_SERIAL_1_3_IRQ, &com_evt_1_3, source.clone())
            .map_err(Error::RegisterIrqfd)?;
        irq_chip
            .register_edge_irq_event(AARCH64_SERIAL_2_4_IRQ, &com_evt_2_4, source)
            .map_err(Error::RegisterIrqfd)?;

        mmio_bus
            .insert(pci_bus, AARCH64_PCI_CFG_BASE, AARCH64_PCI_CFG_SIZE)
            .map_err(Error::RegisterPci)?;

        let mut cmdline = Self::get_base_linux_cmdline();
        get_serial_cmdline(&mut cmdline, serial_parameters, "mmio")
            .map_err(Error::GetSerialCmdline)?;
        for param in components.extra_kernel_params {
            cmdline.insert_str(&param).map_err(Error::Cmdline)?;
        }

        if let Some(ramoops_region) = ramoops_region {
            arch::pstore::add_ramoops_kernel_cmdline(&mut cmdline, &ramoops_region)
                .map_err(Error::Cmdline)?;
        }

        let psci_version = vcpus[0].get_psci_version().map_err(Error::GetPsciVersion)?;

        let pci_cfg = fdt::PciConfigRegion {
            base: AARCH64_PCI_CFG_BASE,
            size: AARCH64_PCI_CFG_SIZE,
        };

        let pci_ranges: Vec<fdt::PciRange> = system_allocator
            .mmio_pools()
            .iter()
            .map(|range| fdt::PciRange {
                space: fdt::PciAddressSpace::Memory64,
                bus_address: range.start,
                cpu_physical_address: range.start,
                size: range.len().unwrap(),
                prefetchable: false,
            })
            .collect();

        let (bat_control, bat_mmio_base_and_irq) = match bat_type {
            Some(BatteryType::Goldfish) => {
                let bat_irq = system_allocator.allocate_irq().ok_or(Error::AllocateIrq)?;

                // a dummy AML buffer. Aarch64 crosvm doesn't use ACPI.
                let mut amls = Vec::new();
                let (control_tube, mmio_base) = arch::sys::unix::add_goldfish_battery(
                    &mut amls,
                    bat_jail,
                    &mmio_bus,
                    irq_chip.as_irq_chip_mut(),
                    bat_irq,
                    system_allocator,
                )
                .map_err(Error::CreateBatDevices)?;
                (
                    Some(BatControl {
                        type_: BatteryType::Goldfish,
                        control_tube,
                    }),
                    Some((mmio_base, bat_irq)),
                )
            }
            None => (None, None),
        };

        let vmwdt_cfg = fdt::VmWdtConfig {
            base: AARCH64_VMWDT_ADDR,
            size: AARCH64_VMWDT_SIZE,
            clock_hz: VMWDT_DEFAULT_CLOCK_HZ,
            timeout_sec: VMWDT_DEFAULT_TIMEOUT_SEC,
        };

        fdt::create_fdt(
            AARCH64_FDT_MAX_SIZE as usize,
            &mem,
            pci_irqs,
            pci_cfg,
            &pci_ranges,
            vcpu_count as u32,
            components.cpu_clusters,
            components.cpu_capacity,
            fdt_offset(components.memory_size, has_bios),
            cmdline.as_str(),
            initrd,
            components.android_fstab,
            irq_chip.get_vgic_version() == DeviceKind::ArmVgicV3,
            use_pmu,
            psci_version,
            components.swiotlb,
            bat_mmio_base_and_irq,
            vmwdt_cfg,
        )
        .map_err(Error::CreateFdt)?;

        let vcpu_init = vec![VcpuInitAArch64::default(); vcpu_count];

        Ok(RunnableLinuxVm {
            vm,
            vcpu_count,
            vcpus: Some(vcpus),
            vcpu_init,
            vcpu_affinity: components.vcpu_affinity,
            no_smt: components.no_smt,
            irq_chip: irq_chip.try_box_clone().map_err(Error::CloneIrqChip)?,
            has_bios,
            io_bus,
            mmio_bus,
            pid_debug_label_map,
            suspend_evt,
            rt_cpus: components.rt_cpus,
            delay_rt: components.delay_rt,
            bat_control,
            #[cfg(all(target_arch = "aarch64", feature = "gdb"))]
            gdb: components.gdb,
            pm: None,
            resume_notify_devices: Vec::new(),
            root_config: pci_root,
            platform_devices,
            hotplug_bus: BTreeMap::new(),
        })
    }

    fn configure_vcpu<V: Vm>(
        _vm: &V,
        _hypervisor: &dyn Hypervisor,
        _irq_chip: &mut dyn IrqChipAArch64,
        _vcpu: &mut dyn VcpuAArch64,
        _vcpu_init: VcpuInitAArch64,
        _vcpu_id: usize,
        _num_cpus: usize,
        _has_bios: bool,
        _cpu_config: Option<CpuConfigAArch64>,
    ) -> std::result::Result<(), Self::Error> {
        // AArch64 doesn't configure vcpus on the vcpu thread, so nothing to do here.
        Ok(())
    }

    fn register_pci_device<V: VmAArch64, Vcpu: VcpuAArch64>(
        _linux: &mut RunnableLinuxVm<V, Vcpu>,
        _device: Box<dyn PciDevice>,
        _minijail: Option<Minijail>,
        _resources: &mut SystemAllocator,
        _tube: &mpsc::Sender<PciRootCommand>,
    ) -> std::result::Result<PciAddress, Self::Error> {
        // hotplug function isn't verified on AArch64, so set it unsupported here.
        Err(Error::Unsupported)
    }
}

#[cfg(all(target_arch = "aarch64", feature = "gdb"))]
impl<T: VcpuAArch64> arch::GdbOps<T> for AArch64 {
    type Error = Error;

    fn read_memory(
        _vcpu: &T,
        guest_mem: &GuestMemory,
        vaddr: GuestAddress,
        len: usize,
    ) -> Result<Vec<u8>> {
        let mut buf = vec![0; len];

        guest_mem
            .read_exact_at_addr(&mut buf, vaddr)
            .map_err(Error::ReadGuestMemory)?;

        Ok(buf)
    }

    fn write_memory(
        _vcpu: &T,
        guest_mem: &GuestMemory,
        vaddr: GuestAddress,
        buf: &[u8],
    ) -> Result<()> {
        guest_mem
            .write_all_at_addr(buf, vaddr)
            .map_err(Error::WriteGuestMemory)
    }

    fn read_registers(vcpu: &T) -> Result<<GdbArch as Arch>::Registers> {
        let mut regs: <GdbArch as Arch>::Registers = Default::default();

        vcpu.get_gdb_registers(&mut regs).map_err(Error::ReadRegs)?;

        Ok(regs)
    }

    fn write_registers(vcpu: &T, regs: &<GdbArch as Arch>::Registers) -> Result<()> {
        vcpu.set_gdb_registers(regs).map_err(Error::WriteRegs)
    }

    fn read_register(vcpu: &T, reg_id: <GdbArch as Arch>::RegId) -> Result<Vec<u8>> {
        let mut reg = vec![0; std::mem::size_of::<u128>()];
        let size = vcpu
            .get_gdb_register(reg_id, reg.as_mut_slice())
            .map_err(Error::ReadReg)?;
        reg.truncate(size);
        Ok(reg)
    }

    fn write_register(vcpu: &T, reg_id: <GdbArch as Arch>::RegId, data: &[u8]) -> Result<()> {
        vcpu.set_gdb_register(reg_id, data).map_err(Error::WriteReg)
    }

    fn enable_singlestep(vcpu: &T) -> Result<()> {
        const SINGLE_STEP: bool = true;
        vcpu.set_guest_debug(&[], SINGLE_STEP)
            .map_err(Error::EnableSinglestep)
    }

    fn get_max_hw_breakpoints(vcpu: &T) -> Result<usize> {
        vcpu.get_max_hw_bps().map_err(Error::GetMaxHwBreakPoint)
    }

    fn set_hw_breakpoints(vcpu: &T, breakpoints: &[GuestAddress]) -> Result<()> {
        const SINGLE_STEP: bool = false;
        vcpu.set_guest_debug(breakpoints, SINGLE_STEP)
            .map_err(Error::SetHwBreakpoint)
    }
}

impl AArch64 {
    /// This returns a base part of the kernel command for this architecture
    fn get_base_linux_cmdline() -> kernel_cmdline::Cmdline {
        let mut cmdline = kernel_cmdline::Cmdline::new(base::pagesize());
        cmdline.insert_str("panic=-1").unwrap();
        cmdline
    }

    /// Returns a system resource allocator configuration.
    ///
    /// # Arguments
    ///
    /// * `mem_size` - Size of guest memory (RAM) in bytes.
    /// * `guest_phys_addr_bits` - Size of guest physical addresses (IPA) in bits.
    fn get_resource_allocator_config(
        mem_size: u64,
        guest_phys_addr_bits: u8,
    ) -> SystemAllocatorConfig {
        let guest_phys_end = 1u64 << guest_phys_addr_bits;
        // The platform MMIO region is immediately past the end of RAM.
        let plat_mmio_base = AARCH64_PHYS_MEM_START + mem_size;
        let plat_mmio_size = AARCH64_PLATFORM_MMIO_SIZE;
        // The high MMIO region is the rest of the address space after the platform MMIO region.
        let high_mmio_base = plat_mmio_base + plat_mmio_size;
        let high_mmio_size = guest_phys_end
            .checked_sub(high_mmio_base)
            .unwrap_or_else(|| {
                panic!(
                    "guest_phys_end {:#x} < high_mmio_base {:#x}",
                    guest_phys_end, high_mmio_base,
                );
            });
        SystemAllocatorConfig {
            io: None,
            low_mmio: AddressRange::from_start_and_size(AARCH64_MMIO_BASE, AARCH64_MMIO_SIZE)
                .expect("invalid mmio region"),
            high_mmio: AddressRange::from_start_and_size(high_mmio_base, high_mmio_size)
                .expect("invalid high mmio region"),
            platform_mmio: Some(
                AddressRange::from_start_and_size(plat_mmio_base, plat_mmio_size)
                    .expect("invalid platform mmio region"),
            ),
            first_irq: AARCH64_IRQ_BASE,
        }
    }

    /// This adds any early platform devices for this architecture.
    ///
    /// # Arguments
    ///
    /// * `irq_chip` - The IRQ chip to add irqs to.
    /// * `bus` - The bus to add devices to.
    /// * `vcpu_count` - The number of virtual CPUs for this guest VM
    /// * `vm_evt_wrtube` - The notification channel
    fn add_arch_devs(
        irq_chip: &mut dyn IrqChip,
        bus: &Bus,
        vcpu_count: usize,
        vm_evt_wrtube: &SendTube,
    ) -> Result<()> {
        let rtc_evt = devices::IrqEdgeEvent::new().map_err(Error::CreateEvent)?;
        let rtc = devices::pl030::Pl030::new(rtc_evt.try_clone().map_err(Error::CloneEvent)?);
        irq_chip
            .register_edge_irq_event(AARCH64_RTC_IRQ, &rtc_evt, IrqEventSource::from_device(&rtc))
            .map_err(Error::RegisterIrqfd)?;

        bus.insert(
            Arc::new(Mutex::new(rtc)),
            AARCH64_RTC_ADDR,
            AARCH64_RTC_SIZE,
        )
        .expect("failed to add rtc device");

        let vm_wdt = Arc::new(Mutex::new(
            devices::vmwdt::Vmwdt::new(vcpu_count, vm_evt_wrtube.try_clone().unwrap()).unwrap(),
        ));
        bus.insert(vm_wdt, AARCH64_VMWDT_ADDR, AARCH64_VMWDT_SIZE)
            .expect("failed to add vmwdt device");

        Ok(())
    }

    /// Sets up `vcpu`.
    ///
    /// AArch64 needs vcpus set up before its kernel IRQ chip is created, so `configure_vcpu_early`
    /// is called from `build_vm` on the main thread.  `LinuxArch::configure_vcpu`, which is used
    /// by X86_64 to do setup later from the vcpu thread, is a no-op on AArch64 since vcpus were
    /// already configured here.
    ///
    /// # Arguments
    ///
    /// * `guest_mem` - The guest memory object.
    /// * `vcpu` - The vcpu to configure.
    /// * `vcpu_id` - The VM's index for `vcpu`.
    /// * `use_pmu` - Should `vcpu` be configured to use the Performance Monitor Unit.
    fn configure_vcpu_early(
        guest_mem: &GuestMemory,
        vcpu: &dyn VcpuAArch64,
        vcpu_id: usize,
        use_pmu: bool,
        has_bios: bool,
        image_size: usize,
        protection_type: ProtectionType,
    ) -> Result<()> {
        let mut features = vec![VcpuFeature::PsciV0_2];
        if use_pmu {
            features.push(VcpuFeature::PmuV3);
        }
        // Non-boot cpus are powered off initially
        if vcpu_id != 0 {
            features.push(VcpuFeature::PowerOff)
        }
        vcpu.init(&features).map_err(Error::VcpuInit)?;

        // All interrupts masked
        let pstate = PSR_D_BIT | PSR_A_BIT | PSR_I_BIT | PSR_F_BIT | PSR_MODE_EL1H;
        vcpu.set_one_reg(VcpuRegAArch64::Pstate, pstate)
            .map_err(Error::SetReg)?;

        // Other cpus are powered off initially
        if vcpu_id == 0 {
            let image_addr = if has_bios {
                get_bios_addr()
            } else {
                get_kernel_addr()
            };

            let entry_addr = match protection_type {
                ProtectionType::Protected => None, // Hypervisor controls the entry point
                ProtectionType::UnprotectedWithFirmware => Some(AARCH64_PROTECTED_VM_FW_START),
                ProtectionType::Unprotected | ProtectionType::ProtectedWithoutFirmware => {
                    Some(image_addr.offset())
                }
            };

            /* PC -- entry point */
            if let Some(entry) = entry_addr {
                vcpu.set_one_reg(VcpuRegAArch64::Pc, entry)
                    .map_err(Error::SetReg)?;
            }

            /* X0 -- fdt address */
            let mem_size = guest_mem.memory_size();
            let fdt_addr = (AARCH64_PHYS_MEM_START + fdt_offset(mem_size, has_bios)) as u64;
            vcpu.set_one_reg(VcpuRegAArch64::X(0), fdt_addr)
                .map_err(Error::SetReg)?;

            if matches!(
                protection_type,
                ProtectionType::Protected | ProtectionType::UnprotectedWithFirmware
            ) {
                /* X1 -- payload entry point */
                vcpu.set_one_reg(VcpuRegAArch64::X(1), image_addr.offset())
                    .map_err(Error::SetReg)?;

                /* X2 -- image size */
                vcpu.set_one_reg(VcpuRegAArch64::X(2), image_size as u64)
                    .map_err(Error::SetReg)?;
            }
        }

        Ok(())
    }
}

pub struct MsrHandlers;

impl MsrHandlers {
    pub fn new() -> Self {
        Self {}
    }

    pub fn read(&self, _index: u32) -> Option<u64> {
        None
    }

    pub fn write(&self, _index: u32, _data: u64) -> Option<()> {
        None
    }

    pub fn add_handler(
        &mut self,
        _index: u32,
        _msr_config: MsrConfig,
        _cpu_id: usize,
    ) -> std::result::Result<(), MsrExitHandlerError> {
        Ok(())
    }
}
