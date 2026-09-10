//! KVM initialisation — §1.4.
//!
//! Follows the specified bring-up order exactly:
//!
//! ```text
//! open /dev/kvm -> KVM_CREATE_VM
//!               -> KVM_SET_USER_MEMORY_REGION x3 (low, high, OVMF read-only)
//!               -> KVM_CAP_SPLIT_IRQCHIP(24)
//!               -> KVM_CREATE_VCPU / CPUID / MSRs / SREGS+REGS
//!               -> KVM_SET_GSI_ROUTING (MSI-X only)
//! ```
//!
//! **No legacy interrupt controllers.** `KVM_CAP_SPLIT_IRQCHIP` gives an
//! in-kernel LAPIC and a userspace I/O APIC, so the PIC and PIT are never
//! instantiated. If the host cannot enable it we abort rather than fall back
//! to a full in-kernel irqchip, because that would create the PIC/PIT §1.4
//! forbids.

use crate::error::{KvmError, VmmResult};
use crate::memory::{self, GuestMemoryMap, Region, RegionKind};

/// The split-irqchip GSI count from §1.4.
pub const SPLIT_IRQCHIP_GSI_COUNT: u32 = 24;

/// Reset vector the guest starts at (§3.1).
pub const RESET_VECTOR: u64 = memory::RESET_VECTOR;

/// A host mapping backing one guest RAM region.
///
/// Guest RAM is `mmap`ed anonymously with `MAP_HUGETLB | MAP_HUGE_1GB` when
/// `hugepages_1gb` is set, matching the 1 GiB granularity §1.3 requires.
pub struct HostMapping {
    ptr: *mut libc::c_void,
    len: usize,
    pub gpa: u64,
    pub slot: u32,
    pub read_only: bool,
}

// The pointer is an owned mmap region; sharing it across threads is how the
// vCPU and queue-worker threads reach guest RAM.
unsafe impl Send for HostMapping {}
unsafe impl Sync for HostMapping {}

/// The 1 GiB hugepage pool, as sysfs exposes it.
const HUGEPAGE_1GB_SYSFS: &str = "/sys/kernel/mm/hugepages/hugepages-1048576kB";

/// Refuse the mapping up front when the 1 GiB pool cannot cover it.
///
/// Without this the failure mode is a SIGBUS on first touch, with no
/// message and a core dump — the kernel only discovers the shortfall when
/// the page is faulted in. Reading the pool is a far better diagnosis, and
/// it can name the exact `nr_hugepages` value the operator needs.
fn check_hugepage_pool(len: usize) -> VmmResult<()> {
    let needed = (len as u64).div_ceil(memory::GIB);

    let read = |name: &str| -> Option<u64> {
        std::fs::read_to_string(format!("{HUGEPAGE_1GB_SYSFS}/{name}"))
            .ok()
            .and_then(|s| s.trim().parse().ok())
    };

    // No sysfs directory means the kernel has no 1 GiB pool at all, which is
    // usually a missing `default_hugepagesz=1G hugepagesz=1G` on the command
    // line. Let mmap produce the error in that case rather than guessing.
    let Some(free) = read("free_hugepages") else {
        return Ok(());
    };
    let total = read("nr_hugepages").unwrap_or(0);

    if free >= needed {
        return Ok(());
    }

    Err(KvmError::MemoryMap {
        size_mb: (len as u64) / memory::MIB,
        detail: format!(
            "this mapping needs {needed} x 1 GiB hugepage(s) but only {free} of the \
             pool's {total} are free. Reserve them with \
             `echo {} > /proc/sys/vm/nr_hugepages` (1 GiB pages require \
             `default_hugepagesz=1G hugepagesz=1G` on the kernel command line), \
             or set memory.hugepages_1gb = false",
            total + (needed - free)
        ),
    }
    .into())
}

impl HostMapping {
    /// Map `len` bytes of anonymous memory for guest use.
    pub fn anonymous(
        len: usize,
        gpa: u64,
        slot: u32,
        hugepages_1gb: bool,
        read_only: bool,
    ) -> VmmResult<Self> {
        let mut flags = libc::MAP_PRIVATE | libc::MAP_ANONYMOUS;
        if hugepages_1gb {
            // Check the pool before mapping, so a shortfall is reported in
            // pages rather than as an errno.
            check_hugepage_pool(len)?;
            // MAP_NORESERVE must NOT be set here. With hugetlb it disables
            // the very reservation that guarantees the pages exist, so the
            // mmap succeeds and the *first write* takes SIGBUS instead —
            // which lands in the firmware load, far from the real cause.
            // MAP_POPULATE then faults the pages in now, turning any
            // remaining shortfall into an mmap error rather than a crash
            // once the guest is running.
            flags |= libc::MAP_HUGETLB | (30 << libc::MAP_HUGE_SHIFT) | libc::MAP_POPULATE;
        } else {
            // Ordinary pages are overcommitted deliberately: guest RAM is
            // sparse in practice and reserving all of it up front would
            // refuse machines the host can comfortably run.
            flags |= libc::MAP_NORESERVE;
        }
        // SAFETY: a fresh anonymous mapping with no fixed address; the
        // returned pointer is checked against MAP_FAILED before use.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                flags,
                -1,
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(KvmError::MemoryMap {
                size_mb: (len as u64) / memory::MIB,
                detail: format!(
                    "mmap failed: {}{}",
                    std::io::Error::last_os_error(),
                    if hugepages_1gb {
                        " (1 GiB hugepages: see /proc/sys/vm/nr_hugepages_mempolicy \
                          and /sys/kernel/mm/hugepages/hugepages-1048576kB)"
                    } else {
                        ""
                    }
                ),
            }
            .into());
        }
        Ok(HostMapping {
            ptr,
            len,
            gpa,
            slot,
            read_only,
        })
    }

    pub fn host_addr(&self) -> u64 {
        self.ptr as u64
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Copy `data` into the mapping at `offset`.
    pub fn write_at(&self, offset: usize, data: &[u8]) -> VmmResult<()> {
        if offset + data.len() > self.len {
            return Err(KvmError::MemoryMap {
                size_mb: (self.len as u64) / memory::MIB,
                detail: format!(
                    "write of {} bytes at offset {offset:#x} exceeds the {} byte region",
                    data.len(),
                    self.len
                ),
            }
            .into());
        }
        // SAFETY: bounds checked immediately above; the mapping is live for
        // the lifetime of `self` and is writable (PROT_WRITE).
        unsafe {
            std::ptr::copy_nonoverlapping(
                data.as_ptr(),
                (self.ptr as *mut u8).add(offset),
                data.len(),
            );
        }
        Ok(())
    }

    /// Copy into the mapping using a guest physical address.
    pub fn write_gpa(&self, gpa: u64, data: &[u8]) -> VmmResult<()> {
        let offset = gpa
            .checked_sub(self.gpa)
            .ok_or_else(|| KvmError::MemoryMap {
                size_mb: (self.len as u64) / memory::MIB,
                detail: format!("gpa {gpa:#x} is below the region base {:#x}", self.gpa),
            })?;
        self.write_at(offset as usize, data)
    }

    /// Copy `data.len()` bytes out of the mapping at `offset`.
    pub fn read_at(&self, offset: usize, data: &mut [u8]) -> VmmResult<()> {
        if offset.saturating_add(data.len()) > self.len {
            return Err(KvmError::MemoryMap {
                size_mb: (self.len as u64) / memory::MIB,
                detail: format!(
                    "read of {} bytes at offset {offset:#x} exceeds the {} byte region",
                    data.len(),
                    self.len
                ),
            }
            .into());
        }
        // SAFETY: bounds checked immediately above; the mapping is live for
        // the lifetime of `self` and readable.
        //
        // The guest may be writing these bytes concurrently — it is another
        // thread with its own view of this memory — so this is a racy read
        // by construction. That is inherent to a shared-memory device
        // interface and is why every value read here is treated as hostile.
        unsafe {
            std::ptr::copy_nonoverlapping(
                (self.ptr as *const u8).add(offset),
                data.as_mut_ptr(),
                data.len(),
            );
        }
        Ok(())
    }

    /// Copy out of the mapping using a guest physical address.
    pub fn read_gpa(&self, gpa: u64, data: &mut [u8]) -> VmmResult<()> {
        let offset = gpa
            .checked_sub(self.gpa)
            .ok_or_else(|| KvmError::MemoryMap {
                size_mb: (self.len as u64) / memory::MIB,
                detail: format!("gpa {gpa:#x} is below the region base {:#x}", self.gpa),
            })?;
        self.read_at(offset as usize, data)
    }

    /// Does this mapping back `gpa`?
    pub fn contains(&self, gpa: u64) -> bool {
        gpa >= self.gpa && gpa < self.gpa + self.len as u64
    }
}

impl Drop for HostMapping {
    fn drop(&mut self) {
        // SAFETY: `ptr`/`len` are exactly what mmap returned and the region
        // is not aliased after drop.
        unsafe {
            libc::munmap(self.ptr, self.len);
        }
    }
}

/// Describes what bring-up would do, without touching /dev/kvm.
///
/// Used by `custom-vmm --check` and by the tests, so the §1.4 ordering can be
/// asserted on a host with no KVM access.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BringupStep {
    OpenKvm,
    CreateVm,
    SetMemoryRegion {
        slot: u32,
        gpa: u64,
        size: u64,
        read_only: bool,
    },
    EnableSplitIrqchip {
        gsi_count: u32,
    },
    CreateVcpu {
        index: u32,
    },
    SetCpuid {
        index: u32,
        passthrough: bool,
        hypervisor_bit: bool,
    },
    SetMsrs {
        index: u32,
        kvm_ptp_clock: bool,
    },
    SetRegs {
        index: u32,
        rip: u64,
    },
    SetGsiRouting {
        msi_routes: u32,
    },
}

/// The §1.4 sequence for a given machine, in order.
pub fn bringup_plan(
    cfg: &libvmm_config::MachineConfig,
    map: &GuestMemoryMap,
    msi_routes: u32,
) -> Vec<BringupStep> {
    let mut steps = vec![BringupStep::OpenKvm, BringupStep::CreateVm];
    for r in map.kvm_slots() {
        steps.push(BringupStep::SetMemoryRegion {
            slot: r.slot.unwrap_or_default(),
            gpa: r.gpa,
            size: r.size,
            read_only: r.kind == RegionKind::RomReadOnly,
        });
    }
    steps.push(BringupStep::EnableSplitIrqchip {
        gsi_count: SPLIT_IRQCHIP_GSI_COUNT,
    });
    for index in 0..cfg.compute.vcpus {
        steps.push(BringupStep::CreateVcpu { index });
        steps.push(BringupStep::SetCpuid {
            index,
            passthrough: cfg.compute.cpu_passthrough,
            hypervisor_bit: cfg.compute.hypervisor_bit,
        });
        steps.push(BringupStep::SetMsrs {
            index,
            kvm_ptp_clock: cfg.compute.kvm_ptp_clock,
        });
        steps.push(BringupStep::SetRegs {
            index,
            rip: RESET_VECTOR,
        });
    }
    steps.push(BringupStep::SetGsiRouting { msi_routes });
    steps
}

// ---------------------------------------------------------------------------
// The live KVM implementation.
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
mod live {
    use super::*;
    use kvm_bindings::{kvm_userspace_memory_region, KVM_MEM_READONLY};
    use kvm_ioctls::{Cap, Kvm, VcpuFd, VmFd};

    /// An initialised VM: the KVM handles plus the host mappings that back
    /// guest RAM.
    pub struct Machine {
        pub kvm: Kvm,
        pub vm: std::sync::Arc<VmFd>,
        pub vcpus: Vec<VcpuFd>,
        pub mappings: Vec<HostMapping>,
        pub map: GuestMemoryMap,
    }

    impl Machine {
        /// Run the §1.4 bring-up sequence.
        pub fn bringup(cfg: &libvmm_config::MachineConfig, map: GuestMemoryMap) -> VmmResult<Self> {
            let kvm = Kvm::new().map_err(|e| KvmError::OpenDevice(e.to_string()))?;

            let api = kvm.get_api_version();
            if api != kvm_bindings::KVM_API_VERSION as i32 {
                return Err(KvmError::ApiVersion {
                    got: api,
                    expected: kvm_bindings::KVM_API_VERSION as i32,
                }
                .into());
            }

            // §1.4 forbids a legacy PIC/PIT, so split irqchip is mandatory,
            // not a preference. Check before we allocate anything large.
            if !kvm.check_extension(Cap::SplitIrqchip) {
                return Err(KvmError::MissingCapability("KVM_CAP_SPLIT_IRQCHIP").into());
            }
            for cap in [
                (Cap::Irqfd, "KVM_CAP_IRQFD"),
                (Cap::Ioeventfd, "KVM_CAP_IOEVENTFD"),
                (Cap::IrqRouting, "KVM_CAP_IRQ_ROUTING"),
                (Cap::UserMemory, "KVM_CAP_USER_MEMORY"),
            ]
            .iter()
            {
                if !kvm.check_extension(cap.0) {
                    return Err(KvmError::MissingCapability(cap.1).into());
                }
            }

            let vm = kvm
                .create_vm()
                .map_err(|e| KvmError::CreateVm(e.to_string()))?;

            // Slots 0, 1, 2 in the order §1.4 lists them.
            let mut mappings = Vec::new();
            for region in map.kvm_slots() {
                let mapping = Self::register_region(&vm, region, cfg.memory.hugepages_1gb)?;
                mappings.push(mapping);
            }

            // In-kernel LAPIC, userspace IOAPIC: no PIC, no PIT.
            // KVM_CAP_SPLIT_IRQCHIP's argument is the number of GSIs the
            // userspace IOAPIC serves (§1.4: 24).
            let mut split = kvm_bindings::kvm_enable_cap {
                cap: kvm_bindings::KVM_CAP_SPLIT_IRQCHIP,
                ..Default::default()
            };
            split.args[0] = SPLIT_IRQCHIP_GSI_COUNT as u64;
            vm.enable_cap(&split)
                .map_err(|e| KvmError::SplitIrqchip(e.to_string()))?;

            let mut vcpus = Vec::with_capacity(cfg.compute.vcpus as usize);
            for index in 0..cfg.compute.vcpus {
                let vcpu = vm
                    .create_vcpu(index as u64)
                    .map_err(|e| KvmError::CreateVcpu {
                        index,
                        detail: e.to_string(),
                    })?;
                configure_cpuid(&kvm, &vcpu, index, cfg)?;
                configure_registers(&vcpu, index)?;
                vcpus.push(vcpu);
            }

            Ok(Machine {
                kvm,
                vm: std::sync::Arc::new(vm),
                vcpus,
                mappings,
                map,
            })
        }

        fn register_region(vm: &VmFd, region: &Region, hugepages: bool) -> VmmResult<HostMapping> {
            let slot = region.slot.unwrap_or_default();
            let read_only = region.kind == RegionKind::RomReadOnly;
            // The OVMF ROM slot is small and file-backed in practice; it does
            // not come from the 1 GiB hugepage pool.
            let use_hugepages = hugepages && region.is_ram();
            let mapping = HostMapping::anonymous(
                region.size as usize,
                region.gpa,
                slot,
                use_hugepages,
                read_only,
            )?;

            let kvm_region = kvm_userspace_memory_region {
                slot,
                guest_phys_addr: region.gpa,
                memory_size: region.size,
                userspace_addr: mapping.host_addr(),
                flags: if read_only { KVM_MEM_READONLY } else { 0 },
            };
            // SAFETY: `userspace_addr` points at a live mapping of exactly
            // `memory_size` bytes, owned by `mapping`, which outlives the
            // registration because it is returned to the caller.
            unsafe { vm.set_user_memory_region(kvm_region) }.map_err(|e| {
                KvmError::SetMemRegion {
                    slot,
                    detail: e.to_string(),
                }
            })?;
            Ok(mapping)
        }

        /// Hand the vCPUs to the run loop.
        ///
        /// They are moved rather than borrowed because `KVM_RUN` needs
        /// `&mut` and must happen on the vCPU's own thread (§1.2). Moving
        /// them makes "only one thread may run this vCPU" a fact the
        /// compiler enforces rather than a convention someone has to know.
        /// The `Machine` keeps the VM and its memory, which must outlive
        /// them.
        /// Load a PVH kernel and return where to start the guest.
        ///
        /// The register half is [`configure_pvh_entry`]; they are separate
        /// because the memory layout is testable without a hypervisor and
        /// the register state is not.
        pub fn load_pvh_kernel(
            &self,
            image: &[u8],
            boot: &crate::pvh::BootInfo,
        ) -> VmmResult<crate::pvh::EntryState> {
            let kernel = crate::pvh::parse(image)?;

            // Refuse to load a kernel into memory that is not there. The
            // guest's own failure mode here is a triple fault with no
            // output, so the check has to happen on this side.
            for seg in &kernel.segments {
                let end = seg.paddr.saturating_add(seg.memsz);
                if !self.map.is_ram_address(seg.paddr) || !self.map.is_ram_address(end - 1) {
                    return Err(KvmError::FirmwareLoad {
                        path: "<pvh kernel>".to_string(),
                        detail: format!(
                            "segment {:#x}..{end:#x} is not backed by guest RAM; the machine                              needs more memory than it has",
                            seg.paddr
                        ),
                    }
                    .into());
                }
            }
            crate::pvh::load(self, image, &kernel, boot)
        }

        /// Guest RAM as `(gpa, host pointer, length)`, for device models
        /// that must reach virtqueues in guest memory.
        ///
        /// Read-only mappings are excluded: a device writing a completion
        /// into the firmware ROM is a bug, and it should fault here rather
        /// than silently succeed against a private copy.
        pub fn ram_regions(&self) -> Vec<(u64, *mut u8, usize)> {
            self.mappings
                .iter()
                .filter(|m| !m.read_only)
                .map(|m| (m.gpa, m.host_addr() as *mut u8, m.len()))
                .collect()
        }

        /// The memory map this machine was built from.
        pub fn map_ref(&self) -> &GuestMemoryMap {
            &self.map
        }

        /// A wall clock reading the same source the guest's paravirtual
        /// clock does. Hand this to
        /// [`DeviceModel::set_wall_clock`](crate::devices::DeviceModel::set_wall_clock)
        /// so the RTC and `ptp_kvm` cannot disagree.
        pub fn wall_clock(&self) -> std::sync::Arc<dyn crate::devices::WallClock> {
            std::sync::Arc::new(KvmWallClock {
                vm: std::sync::Arc::clone(&self.vm),
                reported: std::sync::atomic::AtomicU8::new(SOURCE_UNKNOWN),
            })
        }

        pub fn take_vcpus(&mut self) -> Vec<VcpuFd> {
            std::mem::take(&mut self.vcpus)
        }

        /// Find the mapping backing a guest physical address.
        pub fn mapping_for(&self, gpa: u64) -> Option<&HostMapping> {
            self.mappings.iter().find(|m| m.contains(gpa))
        }

        /// Load the OVMF code image into the read-only slot at 0xFFC0_0000.
        pub fn load_firmware(&self, image: &[u8]) -> VmmResult<()> {
            let slot = self
                .mappings
                .iter()
                .find(|m| m.slot == memory::SLOT_OVMF_CODE)
                .ok_or_else(|| KvmError::FirmwareLoad {
                    path: "<ovmf slot>".to_string(),
                    detail: "OVMF slot is not mapped".to_string(),
                })?;
            if image.len() > slot.len() {
                return Err(KvmError::FirmwareLoad {
                    path: "<ovmf image>".to_string(),
                    detail: format!(
                        "image is {} bytes, the code region is {} bytes",
                        image.len(),
                        slot.len()
                    ),
                }
                .into());
            }
            // OVMF is loaded at the *end* of the region so the reset vector
            // lands at 0xFFFF_FFF0.
            let offset = slot.len() - image.len();
            slot.write_at(offset, image)
        }

        /// Copy the ACPI table set into the low-memory staging area (§3.3).
        pub fn load_acpi(&self, set: &crate::acpi::AcpiTableSet) -> VmmResult<()> {
            let low = self
                .mappings
                .iter()
                .find(|m| m.slot == memory::SLOT_LOW_RAM)
                .ok_or_else(|| KvmError::MemoryMap {
                    size_mb: 0,
                    detail: "low RAM slot is not mapped".to_string(),
                })?;
            low.write_gpa(set.rsdp_gpa, &set.rsdp)?;
            for t in &set.tables {
                low.write_gpa(t.gpa, &t.bytes)?;
            }
            Ok(())
        }
    }

    /// Programme CPUID: host model passthrough plus the hypervisor bit.
    fn configure_cpuid(
        kvm: &Kvm,
        vcpu: &VcpuFd,
        index: u32,
        cfg: &libvmm_config::MachineConfig,
    ) -> VmmResult<()> {
        const CPUID_HYPERVISOR_BIT: u32 = 1 << 31; // CPUID.1:ECX[31]
        const KVM_CPUID_SIGNATURE: u32 = 0x4000_0000;
        const KVM_CPUID_FEATURES: u32 = 0x4000_0001;
        const KVM_FEATURE_PTP_KVM: u32 = 1 << 9;

        let mut cpuid = kvm
            .get_supported_cpuid(kvm_bindings::KVM_MAX_CPUID_ENTRIES)
            .map_err(|e| KvmError::VcpuState {
                index,
                detail: format!("get_supported_cpuid: {e}"),
            })?;

        for entry in cpuid.as_mut_slice() {
            match entry.function {
                1 => {
                    if cfg.compute.hypervisor_bit {
                        entry.ecx |= CPUID_HYPERVISOR_BIT;
                    } else {
                        entry.ecx &= !CPUID_HYPERVISOR_BIT;
                    }
                    // Report this vCPU's own APIC ID.
                    entry.ebx = (entry.ebx & 0x00FF_FFFF) | (index << 24);
                }
                KVM_CPUID_SIGNATURE => {
                    entry.eax = KVM_CPUID_FEATURES;
                    entry.ebx = u32::from_le_bytes(*b"KVMK");
                    entry.ecx = u32::from_le_bytes(*b"VMKV");
                    entry.edx = u32::from_le_bytes(*b"M\0\0\0");
                }
                KVM_CPUID_FEATURES => {
                    // §11 `kvm_ptp_clock`: expose the KVM PTP clock source.
                    if cfg.compute.kvm_ptp_clock {
                        entry.eax |= KVM_FEATURE_PTP_KVM;
                    } else {
                        entry.eax &= !KVM_FEATURE_PTP_KVM;
                    }
                }
                _ => {}
            }
        }

        vcpu.set_cpuid2(&cpuid).map_err(|e| KvmError::VcpuState {
            index,
            detail: format!("set_cpuid2: {e}"),
        })?;
        Ok(())
    }

    /// Point the vCPU at the OVMF reset vector in 16-bit real mode, exactly as
    /// a physical CPU comes out of reset.
    impl crate::pvh::GuestWrite for Machine {
        fn write_gpa(&self, gpa: u64, data: &[u8]) -> VmmResult<()> {
            let mapping = self
                .mapping_for(gpa)
                .ok_or_else(|| KvmError::FirmwareLoad {
                    path: "<pvh kernel>".to_string(),
                    detail: format!("nothing is mapped at {gpa:#x}"),
                })?;
            mapping.write_gpa(gpa, data)
        }
    }

    /// Delivers MSI-X messages with `KVM_SIGNAL_MSI`.
    ///
    /// `signal_msi` takes the message directly, so this needs neither a GSI
    /// nor an entry in the interrupt routing table — which is what makes it
    /// usable before `KVM_SET_GSI_ROUTING` is implemented here. An irqfd is
    /// the better long-term answer for a datapath device, because an
    /// eventfd can be handed to a vhost backend and never enter this
    /// process at all; for a display posting thirty frames a second, one
    /// ioctl per frame is not worth the machinery.
    pub struct KvmMsiSender {
        vm: std::sync::Arc<VmFd>,
    }

    impl KvmMsiSender {
        pub fn new(vm: std::sync::Arc<VmFd>) -> Self {
            KvmMsiSender { vm }
        }
    }

    impl crate::devices::MsiSender for KvmMsiSender {
        fn signal(&self, address: u64, data: u32) {
            let msi = kvm_bindings::kvm_msi {
                address_lo: address as u32,
                address_hi: (address >> 32) as u32,
                data,
                flags: 0,
                devid: 0,
                pad: [0; 12],
            };
            if let Err(e) = self.vm.signal_msi(msi) {
                log::warn!("signal_msi({address:#x}, {data:#x}) failed: {e}");
            }
        }
    }

    /// Publishes a device's framebuffer to the guest as a KVM memory slot.
    ///
    /// See [`crate::devices::GuestRamMapper`] for why a framebuffer BAR
    /// cannot be an ordinary trapping MMIO region.
    pub struct KvmRamMapper {
        vm: std::sync::Arc<VmFd>,
    }

    impl KvmRamMapper {
        pub fn new(vm: std::sync::Arc<VmFd>) -> Self {
            KvmRamMapper { vm }
        }

        fn set(&self, slot: u32, gpa: u64, host: u64, len: u64) -> VmmResult<()> {
            let region = kvm_userspace_memory_region {
                slot,
                guest_phys_addr: gpa,
                memory_size: len,
                userspace_addr: host,
                flags: 0,
            };
            // SAFETY: `host` is the base of a live mapping of at least
            // `len` bytes owned by the caller, which keeps it alive for as
            // long as the slot is registered. A `memory_size` of zero
            // deletes the slot, and KVM ignores `userspace_addr` then.
            unsafe { self.vm.set_user_memory_region(region) }.map_err(|e| {
                KvmError::SetMemRegion {
                    slot,
                    detail: e.to_string(),
                }
                .into()
            })
        }
    }

    impl crate::devices::GuestRamMapper for KvmRamMapper {
        fn remap(&self, slot: u32, gpa: u64, host: u64, len: u64) -> VmmResult<()> {
            self.set(slot, gpa, host, len)
        }

        /// A zero `memory_size` is how KVM is told to drop a slot — and
        /// `__kvm_set_memory_region` answers `EINVAL` if the slot was not
        /// registered in the first place, so the caller has to know whether
        /// it published one. [`crate::display::BochsDisplay`] does.
        fn unmap(&self, slot: u32) -> VmmResult<()> {
            self.set(slot, 0, 0, 0)
        }
    }

    /// Put a vCPU into the state the PVH ABI requires.
    ///
    /// 32-bit flat protected mode, paging off, `%ebx` pointing at
    /// `hvm_start_info`. Note what is deliberately *not* set: `rsp`. The ABI
    /// says "the OS is in charge of setting up it's own stack, GDT and IDT",
    /// and Linux's entry code depends on that — it uses `%ebx + 4` as a
    /// one-slot scratch stack before it has a real one.
    pub fn configure_pvh_entry(
        vcpu: &VcpuFd,
        index: u32,
        state: &crate::pvh::EntryState,
    ) -> VmmResult<()> {
        use crate::pvh::{selector, GDT_START, IDT_START};

        let mut sregs = vcpu.get_sregs().map_err(|e| KvmError::VcpuState {
            index,
            detail: format!("get_sregs: {e}"),
        })?;

        // 32-bit read/execute code, base 0, limit 4 GiB. `db = 1` and
        // `l = 0` are what make it 32-bit rather than 64; `g = 1` makes the
        // limit page-granular, so 0xFFFFFFFF really is the whole address
        // space rather than 4 GiB of pages beyond it.
        sregs.cs.base = 0;
        sregs.cs.limit = 0xFFFF_FFFF;
        sregs.cs.selector = selector::CODE;
        sregs.cs.type_ = 0x0B; // execute/read, accessed
        sregs.cs.present = 1;
        sregs.cs.dpl = 0;
        sregs.cs.db = 1;
        sregs.cs.s = 1;
        sregs.cs.l = 0;
        sregs.cs.g = 1;
        sregs.cs.avl = 0;

        for seg in [
            &mut sregs.ds,
            &mut sregs.es,
            &mut sregs.fs,
            &mut sregs.gs,
            &mut sregs.ss,
        ] {
            seg.base = 0;
            seg.limit = 0xFFFF_FFFF;
            seg.selector = selector::DATA;
            seg.type_ = 0x03; // read/write, accessed
            seg.present = 1;
            seg.dpl = 0;
            seg.db = 1;
            seg.s = 1;
            seg.l = 0;
            seg.g = 1;
            seg.avl = 0;
        }

        // A 32-bit TSS, marked busy (type 0xB), base 0, limit 0x67. The ABI
        // requires it to be *active*; VMX will not enter a guest whose TR is
        // unusable, which is why this is not simply left alone.
        sregs.tr.base = 0;
        sregs.tr.limit = 0x67;
        sregs.tr.selector = selector::TSS;
        sregs.tr.type_ = 0x0B;
        sregs.tr.present = 1;
        sregs.tr.dpl = 0;
        sregs.tr.db = 0;
        sregs.tr.s = 0;
        sregs.tr.l = 0;
        sregs.tr.g = 0;

        sregs.gdt.base = GDT_START;
        sregs.gdt.limit = (crate::pvh::boot_gdt().len() * 8 - 1) as u16;
        sregs.idt.base = IDT_START;
        sregs.idt.limit = 7;

        // PE enters protected mode; ET is architecturally hardwired to 1 on
        // everything since the 486. Every other writeable bit clear, and in
        // particular PG clear — the kernel builds its own page tables.
        sregs.cr0 = 0x11; // PE | ET
        sregs.cr3 = 0;
        sregs.cr4 = 0;
        sregs.efer = 0;

        vcpu.set_sregs(&sregs).map_err(|e| KvmError::VcpuState {
            index,
            detail: format!("set_sregs: {e}"),
        })?;

        let mut regs = vcpu.get_regs().map_err(|e| KvmError::VcpuState {
            index,
            detail: format!("get_regs: {e}"),
        })?;
        regs.rip = state.entry;
        regs.rbx = state.start_info;
        // Bit 1 is reserved and always set. The ABI additionally requires
        // VM, IF and TF clear, which this satisfies.
        regs.rflags = 0x0000_0002;
        regs.rsp = 0;
        vcpu.set_regs(&regs).map_err(|e| KvmError::VcpuState {
            index,
            detail: format!("set_regs: {e}"),
        })?;

        // KVM's reset FPU state is usable, but the kernel touches SSE early
        // and an unset MXCSR is the kind of thing that fails much later.
        let mut fpu = vcpu.get_fpu().map_err(|e| KvmError::VcpuState {
            index,
            detail: format!("get_fpu: {e}"),
        })?;
        fpu.fcw = 0x037F;
        fpu.mxcsr = 0x1F80;
        vcpu.set_fpu(&fpu).map_err(|e| KvmError::VcpuState {
            index,
            detail: format!("set_fpu: {e}"),
        })?;

        Ok(())
    }

    fn configure_registers(vcpu: &VcpuFd, index: u32) -> VmmResult<()> {
        let mut sregs = vcpu.get_sregs().map_err(|e| KvmError::VcpuState {
            index,
            detail: format!("get_sregs: {e}"),
        })?;

        // CS base 0xFFFF_0000 with selector 0xF000 puts the reset vector at
        // 0xFFFF_FFF0 while RIP is only 0xFFF0 — the architectural reset state.
        sregs.cs.base = 0xFFFF_0000;
        sregs.cs.limit = 0xFFFF;
        sregs.cs.selector = 0xF000;
        sregs.cs.present = 1;
        sregs.cs.type_ = 0x0B; // execute/read, accessed
        sregs.cs.s = 1;
        sregs.cs.dpl = 0;
        sregs.cs.db = 0;
        sregs.cs.g = 0;

        for seg in [
            &mut sregs.ds,
            &mut sregs.es,
            &mut sregs.fs,
            &mut sregs.gs,
            &mut sregs.ss,
        ] {
            seg.base = 0;
            seg.limit = 0xFFFF;
            seg.selector = 0;
            seg.present = 1;
            seg.type_ = 0x03; // read/write, accessed
            seg.s = 1;
            seg.g = 0;
            seg.db = 0;
        }

        sregs.cr0 = 0x6000_0010; // CD | NW | ET, protected mode off
        sregs.cr3 = 0;
        sregs.cr4 = 0;

        vcpu.set_sregs(&sregs).map_err(|e| KvmError::VcpuState {
            index,
            detail: format!("set_sregs: {e}"),
        })?;

        let mut regs = vcpu.get_regs().map_err(|e| KvmError::VcpuState {
            index,
            detail: format!("get_regs: {e}"),
        })?;
        regs.rip = 0xFFF0;
        regs.rflags = 0x0000_0002; // bit 1 is reserved and always set
        regs.rsp = 0;
        vcpu.set_regs(&regs).map_err(|e| KvmError::VcpuState {
            index,
            detail: format!("set_regs: {e}"),
        })?;
        Ok(())
    }

    /// The host wall clock, read through `KVM_GET_CLOCK`.
    ///
    /// This is deliberately the *same* kernel call path the guest's own
    /// `ptp_kvm` driver ends up on. `KVM_HC_CLOCK_PAIRING`, which is what
    /// `ptp_kvm` issues, and `KVM_GET_CLOCK` with `KVM_CLOCK_REALTIME` both
    /// resolve to `ktime_get_snapshot()` — one host-realtime reading taken
    /// against one TSC reading. Asking through this interface rather than
    /// `gettimeofday` is what makes the RTC agree with the guest's PTP
    /// clock instead of merely being close to it.
    ///
    /// `KVM_CLOCK_REALTIME` arrived in Linux 5.16. On anything older the
    /// kernel leaves the flag and the field clear, so the fallback below is
    /// a genuine possibility rather than defensive padding — and it is
    /// silent, because an RTC that is right to the second on an old kernel
    /// is not a fault worth logging once per read.
    pub struct KvmWallClock {
        vm: std::sync::Arc<VmFd>,
        /// Which source the last read came from, so a change is announced
        /// once instead of every time.
        reported: std::sync::atomic::AtomicU8,
    }

    const SOURCE_UNKNOWN: u8 = 0;
    const SOURCE_KVM: u8 = 1;
    const SOURCE_HOST: u8 = 2;

    impl KvmWallClock {
        /// The host realtime KVM reports, or `None` where the kernel does
        /// not fill it in.
        pub fn kvm_realtime(&self) -> Option<u64> {
            let clock = self.vm.get_clock().ok()?;
            if clock.flags & kvm_bindings::KVM_CLOCK_REALTIME != 0 && clock.realtime != 0 {
                Some(clock.realtime)
            } else {
                None
            }
        }
    }

    impl crate::devices::WallClock for KvmWallClock {
        fn realtime_nanos(&self) -> u128 {
            match self.kvm_realtime() {
                Some(ns) => {
                    self.announce(SOURCE_KVM);
                    u128::from(ns)
                }
                None => {
                    self.announce(SOURCE_HOST);
                    crate::devices::SystemWallClock.realtime_nanos()
                }
            }
        }
    }

    impl KvmWallClock {
        /// Say which clock is answering, the first time and on any change.
        ///
        /// It changes in practice, and the reason is worth knowing: KVM only
        /// publishes a realtime pairing once its master clock is up, which
        /// needs a vCPU to have enabled kvmclock and the host clocksource to
        /// be the TSC. Probing at bring-up therefore always reports the
        /// fallback, because no vCPU has run yet. Announcing on change
        /// rather than once at construction is what makes the log say what
        /// is actually true during the run.
        fn announce(&self, source: u8) {
            use std::sync::atomic::Ordering;
            if self.reported.swap(source, Ordering::Relaxed) == source {
                return;
            }
            match source {
                SOURCE_KVM => log::info!(
                    "RTC clock source: KVM_GET_CLOCK realtime — the same \
                     ktime_get_snapshot() pairing the guest's ptp_kvm reads"
                ),
                _ => log::info!(
                    "RTC clock source: host CLOCK_REALTIME — KVM is not \
                     reporting KVM_CLOCK_REALTIME yet (it needs the master \
                     clock up: a vCPU running kvmclock, and a TSC host \
                     clocksource), so the RTC and the guest's paravirtual \
                     clock are separate samples of the same epoch"
                ),
            }
        }
    }
}

#[cfg(target_os = "linux")]
pub use live::{configure_pvh_entry, KvmMsiSender, KvmRamMapper, KvmWallClock, Machine};

#[cfg(test)]
mod tests {
    use super::*;

    fn reference_cfg() -> libvmm_config::MachineConfig {
        libvmm_config::MachineConfig::from_toml_str(include_str!(
            "../../../config/reference-vm.toml"
        ))
        .unwrap()
    }

    #[test]
    fn bringup_follows_the_spec_1_4_order() {
        let cfg = reference_cfg();
        let map = GuestMemoryMap::new(&cfg.memory).unwrap();
        let plan = bringup_plan(&cfg, &map, 12);

        assert_eq!(plan[0], BringupStep::OpenKvm);
        assert_eq!(plan[1], BringupStep::CreateVm);

        // Memory regions come next, in slot order, with OVMF read-only.
        assert!(matches!(
            plan[2],
            BringupStep::SetMemoryRegion {
                slot: 0,
                gpa: 0,
                read_only: false,
                ..
            }
        ));
        assert!(matches!(
            plan[3],
            BringupStep::SetMemoryRegion {
                slot: 1,
                gpa: 0x1_0000_0000,
                read_only: false,
                ..
            }
        ));
        assert!(matches!(
            plan[4],
            BringupStep::SetMemoryRegion {
                slot: 2,
                gpa: 0xFFC0_0000,
                read_only: true,
                ..
            }
        ));

        // Split irqchip before any vCPU exists.
        let irqchip_at = plan
            .iter()
            .position(|s| matches!(s, BringupStep::EnableSplitIrqchip { .. }))
            .unwrap();
        let first_vcpu_at = plan
            .iter()
            .position(|s| matches!(s, BringupStep::CreateVcpu { .. }))
            .unwrap();
        assert!(
            irqchip_at < first_vcpu_at,
            "split irqchip must precede vCPU creation"
        );
        assert_eq!(
            plan[irqchip_at],
            BringupStep::EnableSplitIrqchip { gsi_count: 24 }
        );

        // GSI routing is last, after every vCPU.
        assert_eq!(
            plan.last(),
            Some(&BringupStep::SetGsiRouting { msi_routes: 12 })
        );
    }

    #[test]
    fn every_vcpu_starts_at_the_ovmf_reset_vector() {
        let cfg = reference_cfg();
        let map = GuestMemoryMap::new(&cfg.memory).unwrap();
        let rips: Vec<u64> = bringup_plan(&cfg, &map, 0)
            .into_iter()
            .filter_map(|s| match s {
                BringupStep::SetRegs { rip, .. } => Some(rip),
                _ => None,
            })
            .collect();
        assert_eq!(rips.len(), cfg.compute.vcpus as usize);
        assert!(rips.iter().all(|r| *r == 0xFFFF_FFF0));
    }
}
