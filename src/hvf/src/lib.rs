// Copyright 2021 Red Hat, Inc.
// SPDX-License-Identifier: Apache-2.0

#![cfg(target_os = "macos")]

#[allow(non_camel_case_types)]
#[allow(improper_ctypes)]
#[allow(dead_code)]
#[allow(non_snake_case)]
#[allow(non_upper_case_globals)]
#[allow(deref_nullptr)]
pub mod bindings;

#[macro_use]
extern crate log;

use bindings::*;

#[cfg(target_arch = "aarch64")]
use std::arch::asm;
use std::cell::Cell;

use std::convert::TryInto;
use std::fmt::{Display, Formatter};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use arch::ArchMemoryInfo;
#[cfg(all(target_arch = "aarch64", target_os = "macos"))]
use arch::aarch64::sysreg::{SYSREG_MASK, sys_reg_name};
use log::debug;

unsafe extern "C" {
    pub fn mach_absolute_time() -> u64;
}

const HV_EXIT_REASON_CANCELED: hv_exit_reason_t = 0;
const HV_EXIT_REASON_EXCEPTION: hv_exit_reason_t = 1;
const HV_EXIT_REASON_VTIMER_ACTIVATED: hv_exit_reason_t = 2;

const TMR_CTL_ENABLE: u64 = 1 << 0;
const TMR_CTL_IMASK: u64 = 1 << 1;
const TMR_CTL_ISTATUS: u64 = 1 << 2;

const PSR_MODE_EL1H: u64 = 0x0000_0005;
const PSR_MODE_EL2H: u64 = 0x0000_0009;
const PSR_F_BIT: u64 = 0x0000_0040;
const PSR_I_BIT: u64 = 0x0000_0080;
const PSR_A_BIT: u64 = 0x0000_0100;
const PSR_D_BIT: u64 = 0x0000_0200;
const PSTATE_EL1_FAULT_BITS_64: u64 = PSR_MODE_EL1H | PSR_A_BIT | PSR_F_BIT | PSR_I_BIT | PSR_D_BIT;
const PSTATE_EL2_FAULT_BITS_64: u64 = PSR_MODE_EL2H | PSR_A_BIT | PSR_F_BIT | PSR_I_BIT | PSR_D_BIT;

const HCR_TLOR: u64 = 1 << 35;
const HCR_RW: u64 = 1 << 31;
const HCR_TSW: u64 = 1 << 22;
const HCR_TACR: u64 = 1 << 21;
const HCR_TIDCP: u64 = 1 << 20;
const HCR_TSC: u64 = 1 << 19;
const HCR_TID3: u64 = 1 << 18;
const HCR_TWE: u64 = 1 << 14;
const HCR_TWI: u64 = 1 << 13;
const HCR_BSU_IS: u64 = 1 << 10;
const HCR_FB: u64 = 1 << 9;
const HCR_AMO: u64 = 1 << 5;
const HCR_IMO: u64 = 1 << 4;
const HCR_FMO: u64 = 1 << 3;
const HCR_PTW: u64 = 1 << 2;
const HCR_SWIO: u64 = 1 << 1;
const HCR_VM: u64 = 1 << 0;
// Use the same bits as KVM uses in vcpu reset.
const HCR_EL2_BITS: u64 = HCR_TSC
    | HCR_TSW
    | HCR_TWE
    | HCR_TWI
    | HCR_VM
    | HCR_BSU_IS
    | HCR_FB
    | HCR_TACR
    | HCR_AMO
    | HCR_SWIO
    | HCR_TIDCP
    | HCR_RW
    | HCR_TLOR
    | HCR_FMO
    | HCR_IMO
    | HCR_PTW
    | HCR_TID3;

const CNTHCTL_EL0VCTEN: u64 = 1 << 1;
const CNTHCTL_EL0PCTEN: u64 = 1 << 0;
// Trap accesses to both virtual and physical counter registers.
const CNTHCTL_EL2_BITS: u64 = CNTHCTL_EL0VCTEN | CNTHCTL_EL0PCTEN;

const AA64PFR0_EL1_EL2EN: u64 = 1 << 8;
const AA64PFR0_EL1_GIC3EN: u64 = 1 << 24;
const AA64PFR1_EL1_SMEMASK: u64 = 3 << 24;

const EC_WFX_TRAP: u64 = 0x1;
const EC_AA64_HVC: u64 = 0x16;
const EC_AA64_SMC: u64 = 0x17;
#[cfg(all(target_arch = "aarch64", target_os = "macos"))]
const EC_SYSTEMREGISTERTRAP: u64 = 0x18;
const EC_INSTRABORT: u64 = 0x20;
const EC_DATAABORT: u64 = 0x24;
const EC_AA64_BKPT: u64 = 0x3c;

#[derive(Debug)]
pub enum Error {
    EnableEL2,
    FindSymbol(libloading::Error),
    IpaGetDefault,
    IpaGetMaximum,
    IpaSet,
    IpaTooLow,
    MemoryMap,
    MemoryUnmap,
    NestedCheck,
    VcpuCreate,
    VcpuInitialRegisters,
    VcpuReadRegister,
    VcpuReadSystemRegister,
    VcpuRequestExit,
    VcpuRun,
    VcpuSetPendingIrq,
    VcpuSetRegister,
    VcpuSetSystemRegister(u16, u64),
    VcpuSetVtimerMask,
    VmCreate,
}

impl Display for Error {
    fn fmt(&self, f: &mut Formatter) -> std::fmt::Result {
        use self::Error::*;

        match self {
            EnableEL2 => write!(f, "Error enabling EL2 mode in HVF"),
            FindSymbol(err) => write!(f, "Couldn't find symbol in HVF library: {err}"),
            IpaGetDefault => write!(f, "Error getting IPA default value"),
            IpaGetMaximum => write!(f, "Error getting IPA maximum value"),
            IpaSet => write!(f, "Error setting IPA value"),
            IpaTooLow => write!(
                f,
                "The VM requires an IPA larger than the maximum supported in this system"
            ),
            MemoryMap => write!(f, "Error registering memory region in HVF"),
            MemoryUnmap => write!(f, "Error unregistering memory region in HVF"),
            NestedCheck => write!(
                f,
                "Nested virtualization was requested but it's not support in this system"
            ),
            VcpuCreate => write!(f, "Error creating HVF vCPU instance"),
            VcpuInitialRegisters => write!(f, "Error setting up initial HVF vCPU registers"),
            VcpuReadRegister => write!(f, "Error reading HVF vCPU register"),
            VcpuReadSystemRegister => write!(f, "Error reading HVF vCPU system register"),
            VcpuRequestExit => write!(f, "Error requesting HVF vCPU exit"),
            VcpuRun => write!(f, "Error running HVF vCPU"),
            VcpuSetPendingIrq => write!(f, "Error setting HVF vCPU pending irq"),
            VcpuSetRegister => write!(f, "Error setting HVF vCPU register"),
            VcpuSetSystemRegister(reg, val) => write!(
                f,
                "Error setting HVF vCPU system register 0x{reg:#x} to 0x{val:#x}"
            ),
            VcpuSetVtimerMask => write!(f, "Error setting HVF vCPU vtimer mask"),
            VmCreate => write!(f, "Error creating HVF VM instance"),
        }
    }
}

pub enum InterruptType {
    Irq,
    Fiq,
}

pub trait Vcpus {
    fn set_vtimer_irq(&self, vcpuid: u64);
    fn should_wait(&self, vcpuid: u64) -> bool;
    fn has_pending_irq(&self, vcpuid: u64) -> bool;
    fn get_pending_irq(&self, vcpuid: u64) -> u32;
    fn handle_sysreg_read(&self, vcpuid: u64, reg: u32) -> Option<u64>;
    fn handle_sysreg_write(&self, vcpuid: u64, reg: u32, val: u64) -> bool;
}

pub fn vcpu_request_exit(vcpuid: u64) -> Result<(), Error> {
    let mut vcpu: u64 = vcpuid;
    let ret = unsafe { hv_vcpus_exit(&mut vcpu, 1) };

    if ret != HV_SUCCESS {
        Err(Error::VcpuRequestExit)
    } else {
        Ok(())
    }
}

pub fn vcpu_set_pending_irq(
    vcpuid: u64,
    irq_type: InterruptType,
    pending: bool,
) -> Result<(), Error> {
    let _type = match irq_type {
        InterruptType::Irq => hv_interrupt_type_t_HV_INTERRUPT_TYPE_IRQ,
        InterruptType::Fiq => hv_interrupt_type_t_HV_INTERRUPT_TYPE_FIQ,
    };

    let ret = unsafe { hv_vcpu_set_pending_interrupt(vcpuid, _type, pending) };

    if ret != HV_SUCCESS {
        Err(Error::VcpuSetPendingIrq)
    } else {
        Ok(())
    }
}

pub fn vcpu_set_vtimer_mask(vcpuid: u64, masked: bool) -> Result<(), Error> {
    let ret = unsafe { hv_vcpu_set_vtimer_mask(vcpuid, masked) };

    if ret != HV_SUCCESS {
        Err(Error::VcpuSetVtimerMask)
    } else {
        Ok(())
    }
}

/// Checks if Nested Virtualization is supported on the current system. Only
/// M3 or newer chips on macOS 15+ will satisfy the requirements.
pub fn check_nested_virt() -> Result<bool, Error> {
    type GetEL2Supported =
        libloading::Symbol<'static, unsafe extern "C" fn(*mut bool) -> hv_return_t>;

    let get_el2_supported: Result<GetEL2Supported, libloading::Error> =
        unsafe { HVF.get(b"hv_vm_config_get_el2_supported") };
    if get_el2_supported.is_err() {
        info!("cannot find hv_vm_config_get_el2_supported symbol");
        return Ok(false);
    }

    let mut el2_supported: bool = false;
    let ret = unsafe { (get_el2_supported.unwrap())(&mut el2_supported) };
    if ret != HV_SUCCESS {
        error!("hv_vm_config_get_el2_supported failed: {ret:?}");
        return Err(Error::NestedCheck);
    }

    Ok(el2_supported)
}

pub struct HvfVm {}

static HVF: LazyLock<libloading::Library> = LazyLock::new(|| unsafe {
    libloading::Library::new(
        "/System/Library/Frameworks/Hypervisor.framework/Versions/A/Hypervisor",
    )
    .unwrap()
});

impl HvfVm {
    pub fn new(arch_mem_info: &mut ArchMemoryInfo, nested_enabled: bool) -> Result<Self, Error> {
        let config = unsafe { hv_vm_config_create() };
        if nested_enabled {
            let set_el2_enabled: libloading::Symbol<
                'static,
                unsafe extern "C" fn(hv_vm_config_t, bool) -> hv_return_t,
            > = unsafe {
                HVF.get(b"hv_vm_config_set_el2_enabled")
                    .map_err(Error::FindSymbol)?
            };

            let ret = unsafe { (set_el2_enabled)(config, true) };
            if ret != HV_SUCCESS {
                return Err(Error::EnableEL2);
            }
        }

        let mut ipa_size_default: u32 = 0;
        let ret = unsafe { hv_vm_config_get_default_ipa_size(&mut ipa_size_default as *mut _) };
        if ret != HV_SUCCESS {
            return Err(Error::IpaGetDefault);
        }

        let mut ipa_size_max: u32 = 0;
        let ret = unsafe { hv_vm_config_get_max_ipa_size(&mut ipa_size_max as *mut _) };
        if ret != HV_SUCCESS {
            return Err(Error::IpaGetMaximum);
        }

        let mut ipa_size = ipa_size_default;
        // If the guest's last address is beyond the 64GB line, we need a 40-bit (or larger) IPA.
        if arch_mem_info.guest_last_addr > 64 * 1024 * 1024 * 1024 {
            if ipa_size_max < 40 {
                return Err(Error::IpaTooLow);
            }
            ipa_size = ipa_size_max;
        }

        let ret = unsafe { hv_vm_config_set_ipa_size(config, ipa_size) };
        if ret != HV_SUCCESS {
            return Err(Error::IpaSet);
        }
        #[cfg(target_os = "macos")]
        {
            arch_mem_info.ipa_size = ipa_size;
        }

        let ret = unsafe { hv_vm_create(config) };

        if ret != HV_SUCCESS {
            Err(Error::VmCreate)
        } else {
            set_reclaim_ram_range(0, 0);
            Ok(Self {})
        }
    }

    pub fn map_memory(
        &self,
        host_start_addr: u64,
        guest_start_addr: u64,
        size: u64,
    ) -> Result<(), Error> {
        let ret = unsafe {
            hv_vm_map(
                host_start_addr as *mut core::ffi::c_void,
                guest_start_addr,
                size.try_into().unwrap(),
                guest_memory_flags(),
            )
        };
        if ret != HV_SUCCESS {
            Err(Error::MemoryMap)
        } else {
            Ok(())
        }
    }

    pub fn unmap_memory(&self, guest_start_addr: u64, size: u64) -> Result<(), Error> {
        let ret = unsafe { hv_vm_unmap(guest_start_addr, size.try_into().unwrap()) };
        if ret != HV_SUCCESS {
            Err(Error::MemoryUnmap)
        } else {
            Ok(())
        }
    }
}

static RECLAIM_RAM_START: AtomicU64 = AtomicU64::new(0);
static RECLAIM_RAM_END: AtomicU64 = AtomicU64::new(0);

/// Declares the guest RAM range [start, end) that [`reclaim_guest_ram`] may be
/// used on. Must be called before the vCPUs start.
///
/// Guest RAM is otherwise always mapped, so a stage-2 abort in this range means
/// that the guest accessed memory while it was being reclaimed. The vCPU then
/// stops the VM instead of emulating the access as MMIO, which would silently
/// corrupt guest memory.
pub fn set_reclaim_ram_range(start: u64, end: u64) {
    RECLAIM_RAM_START.store(start, Ordering::Relaxed);
    RECLAIM_RAM_END.store(end, Ordering::Release);
}

fn in_reclaim_ram(pa: u64) -> bool {
    pa < RECLAIM_RAM_END.load(Ordering::Acquire) && pa >= RECLAIM_RAM_START.load(Ordering::Relaxed)
}

fn guest_memory_flags() -> hv_memory_flags_t {
    (HV_MEMORY_READ | HV_MEMORY_WRITE | HV_MEMORY_EXEC).into()
}

/// Why [`reclaim_guest_ram`] left a range untouched (still mapped, with its
/// contents unchanged).
#[derive(Debug)]
pub enum ReclaimError {
    /// Less than one host page is left after aligning the range inward.
    TooSmall,
    /// The host and guest addresses are not congruent modulo the host page
    /// size.
    Misaligned,
    /// hv_vm_unmap() failed.
    Unmap(hv_return_t),
}

/// Returns the host pages backing a range of guest RAM to macOS.
///
/// madvise(MADV_FREE) and madvise(MADV_FREE_REUSABLE) on guest RAM have no
/// effect on pages that only the guest has written to (through stage-2): the
/// calls succeed, but neither the process' physical footprint nor the system's
/// free page count change. Replacing the backing pages does release them:
///
/// 1. hv_vm_unmap() the range.
/// 2. mmap(MAP_FIXED) fresh anonymous memory over it, freeing the old pages.
/// 3. hv_vm_map() it back with the same permissions.
///
/// The range then reads as zeroes and is populated again on first access, as
/// after MADV_DONTNEED on Linux. It is shrunk inward to host page boundaries.
/// Returns the number of bytes released, which is 0 if mmap() kept failing and
/// the original pages were mapped back instead.
///
/// Once step 1 has succeeded, this does not return before the range is mapped
/// again: steps 2 and 3 are retried, with a growing delay, up to once per
/// second. It can therefore block for a long time and must not be called from
/// the VMM event loop.
///
/// # Safety
///
/// `host_addr` must be the host address of `guest_addr`, and the whole range
/// must be ordinary guest RAM: anonymous private memory mapped into the guest
/// by [`HvfVm::map_memory`], not the firmware region or a DAX/GPU window, whose
/// contents would be lost. Neither the guest nor the VMM may access the range
/// until this returns.
pub unsafe fn reclaim_guest_ram(
    host_addr: u64,
    guest_addr: u64,
    len: u64,
) -> Result<u64, ReclaimError> {
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as u64;
    let mask = page - 1;
    if host_addr & mask != guest_addr & mask {
        return Err(ReclaimError::Misaligned);
    }
    let start = guest_addr.checked_add(mask).ok_or(ReclaimError::TooSmall)? & !mask;
    let end = guest_addr.saturating_add(len) & !mask;
    if end <= start {
        return Err(ReclaimError::TooSmall);
    }
    let size = (end - start) as usize;
    let hva = (host_addr + (start - guest_addr)) as *mut core::ffi::c_void;

    let ret = unsafe { hv_vm_unmap(start, size) };
    if ret != HV_SUCCESS {
        return Err(ReclaimError::Unmap(ret));
    }

    // Same protection and flags vm-memory uses for guest RAM.
    let fresh_anon = || {
        let addr = unsafe {
            libc::mmap(
                hva,
                size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_FIXED | libc::MAP_PRIVATE | libc::MAP_ANON | libc::MAP_NORESERVE,
                -1,
                0,
            )
        };
        addr != libc::MAP_FAILED
    };
    let mut fresh = false;
    for delay in [0, 1, 10] {
        std::thread::sleep(Duration::from_millis(delay));
        if fresh_anon() {
            fresh = true;
            break;
        }
        warn!(
            "reclaim: mmap(MAP_FIXED, host={hva:p}, len={size}) failed: {}",
            std::io::Error::last_os_error()
        );
    }

    // hv_vm_map() succeeded for this range at boot, so it can only fail
    // transiently, or because a failed mmap() left nothing mapped at hva.
    // Returning with the range unmapped would leave a hole in guest RAM.
    const BACKOFF_MS: [u64; 5] = [0, 1, 10, 100, 1000];
    let mut attempt: u32 = 0;
    loop {
        let ret = unsafe { hv_vm_map(hva, start, size, guest_memory_flags()) };
        if ret == HV_SUCCESS {
            break;
        }
        attempt += 1;
        if attempt <= 5 || attempt.is_multiple_of(60) {
            error!(
                "reclaim: hv_vm_map(host={hva:p}, guest=0x{start:x}, len={size}) failed: \
                 0x{ret:x} (attempt {attempt}), retrying"
            );
        }
        let delay = BACKOFF_MS[(attempt as usize).min(BACKOFF_MS.len() - 1)];
        std::thread::sleep(Duration::from_millis(delay));
        if !fresh && fresh_anon() {
            fresh = true;
        }
    }
    if attempt > 0 {
        warn!("reclaim: guest=0x{start:x} len={size} mapped again after {attempt} failure(s)");
    }
    Ok(if fresh { size as u64 } else { 0 })
}

#[derive(Debug)]
pub enum VcpuExit<'a> {
    Breakpoint,
    Canceled,
    CpuOn(u64, u64, u64),
    HypervisorCall,
    MmioRead(u64, &'a mut [u8]),
    MmioWrite(u64, &'a [u8]),
    PsciHandled,
    SecureMonitorCall,
    Shutdown,
    SystemRegister,
    VtimerActivated,
    WaitForEvent,
    WaitForEventExpired,
    WaitForEventTimeout(Duration),
}

struct MmioRead {
    addr: u64,
    len: usize,
    srt: u32,
}

pub struct HvfVcpu<'a> {
    vcpuid: hv_vcpu_t,
    vcpu_exit: &'a hv_vcpu_exit_t,
    cntfrq: u64,
    mmio_buf: [u8; 8],
    pending_mmio_read: Option<MmioRead>,
    pending_advance_pc: bool,
    vtimer_masked: bool,
    nested_enabled: bool,
    // Cached copy of the HVF vtimer offset (0 until a live pause advances it),
    // so the WFE deadline check doesn't issue a syscall on every wait.
    vtimer_offset: Cell<u64>,
}

impl HvfVcpu<'_> {
    pub fn new(mpidr: u64, nested_enabled: bool) -> Result<Self, Error> {
        let mut vcpuid: hv_vcpu_t = 0;
        let mut vcpu_exit_ptr: *mut hv_vcpu_exit_t = std::ptr::null_mut();

        #[cfg(target_arch = "aarch64")]
        let cntfrq = {
            let cntfrq: u64;
            unsafe { asm!("mrs {}, cntfrq_el0", out(reg) cntfrq) };
            cntfrq
        };
        #[cfg(target_arch = "x86_64")]
        let cntfrq = 0u64;
        #[cfg(target_arch = "riscv64")]
        let cntfrq = 0u64;

        let ret = unsafe {
            hv_vcpu_create(
                &mut vcpuid,
                &mut vcpu_exit_ptr as *mut *mut _,
                std::ptr::null_mut(),
            )
        };
        if ret != HV_SUCCESS {
            return Err(Error::VcpuCreate);
        }

        // We write vcpuid to Aff1 as otherwise it won't match the redistributor ID
        // when using HVF in-kernel GICv3.
        let ret = unsafe { hv_vcpu_set_sys_reg(vcpuid, hv_sys_reg_t_HV_SYS_REG_MPIDR_EL1, mpidr) };
        if ret != HV_SUCCESS {
            return Err(Error::VcpuCreate);
        }

        let vcpu_exit: &hv_vcpu_exit_t = unsafe { vcpu_exit_ptr.as_mut().unwrap() };

        Ok(Self {
            vcpuid,
            vcpu_exit,
            cntfrq,
            mmio_buf: [0; 8],
            pending_mmio_read: None,
            pending_advance_pc: false,
            vtimer_masked: false,
            nested_enabled,
            vtimer_offset: Cell::new(0),
        })
    }

    pub fn set_initial_state(
        &self,
        ipa_size: u32,
        entry_addr: u64,
        fdt_addr: u64,
    ) -> Result<(), Error> {
        // Set IPA size on MMFR0
        let ipa_index: u8 = match ipa_size {
            36 => 1,
            40 => 2,
            42 => 3,
            44 => 4,
            48 => 5,
            52 => 6,
            56 => 7,
            _ => return Err(Error::VcpuInitialRegisters),
        };
        let mut val: u64 = 0;
        let ret = unsafe {
            hv_vcpu_get_sys_reg(
                self.vcpuid,
                hv_sys_reg_t_HV_SYS_REG_ID_AA64MMFR0_EL1,
                &mut val as *mut _,
            )
        };
        if ret != HV_SUCCESS {
            return Err(Error::VcpuInitialRegisters);
        }
        let ret = unsafe {
            hv_vcpu_set_sys_reg(
                self.vcpuid,
                hv_sys_reg_t_HV_SYS_REG_ID_AA64MMFR0_EL1,
                (val & !0xf) | (ipa_index as u64),
            )
        };
        if ret != HV_SUCCESS {
            return Err(Error::VcpuInitialRegisters);
        }

        if self.nested_enabled {
            let ret = unsafe {
                hv_vcpu_set_reg(self.vcpuid, hv_reg_t_HV_REG_CPSR, PSTATE_EL2_FAULT_BITS_64)
            };
            if ret != HV_SUCCESS {
                return Err(Error::VcpuInitialRegisters);
            }

            let ret = unsafe {
                hv_vcpu_set_sys_reg(self.vcpuid, hv_sys_reg_t_HV_SYS_REG_HCR_EL2, HCR_EL2_BITS)
            };
            if ret != HV_SUCCESS {
                return Err(Error::VcpuInitialRegisters);
            }

            let ret = unsafe {
                hv_vcpu_set_sys_reg(
                    self.vcpuid,
                    hv_sys_reg_t_HV_SYS_REG_CNTHCTL_EL2,
                    CNTHCTL_EL2_BITS,
                )
            };
            if ret != HV_SUCCESS {
                return Err(Error::VcpuInitialRegisters);
            }

            // Enable EL2 and GICv3 in ID_AA64PFR0_EL1
            let mut val: u64 = 0;
            let ret = unsafe {
                hv_vcpu_get_sys_reg(
                    self.vcpuid,
                    hv_sys_reg_t_HV_SYS_REG_ID_AA64PFR0_EL1,
                    &mut val as *mut _,
                )
            };
            if ret != HV_SUCCESS {
                return Err(Error::VcpuInitialRegisters);
            }
            let ret = unsafe {
                hv_vcpu_set_sys_reg(
                    self.vcpuid,
                    hv_sys_reg_t_HV_SYS_REG_ID_AA64PFR0_EL1,
                    val | AA64PFR0_EL1_EL2EN | AA64PFR0_EL1_GIC3EN,
                )
            };
            if ret != HV_SUCCESS {
                return Err(Error::VcpuInitialRegisters);
            }

            // If SME is enabled in ID_AA64PFR1_EL1 in the VM, the guest will
            // break after enabling the MMU. Mask it out.
            let mut val: u64 = 0;
            let ret = unsafe {
                hv_vcpu_get_sys_reg(
                    self.vcpuid,
                    hv_sys_reg_t_HV_SYS_REG_ID_AA64PFR1_EL1,
                    &mut val as *mut _,
                )
            };
            if ret != HV_SUCCESS {
                return Err(Error::VcpuInitialRegisters);
            }
            let ret = unsafe {
                hv_vcpu_set_sys_reg(
                    self.vcpuid,
                    hv_sys_reg_t_HV_SYS_REG_ID_AA64PFR1_EL1,
                    val & !AA64PFR1_EL1_SMEMASK,
                )
            };
            if ret != HV_SUCCESS {
                return Err(Error::VcpuInitialRegisters);
            }
        } else {
            let ret = unsafe {
                hv_vcpu_set_reg(self.vcpuid, hv_reg_t_HV_REG_CPSR, PSTATE_EL1_FAULT_BITS_64)
            };
            if ret != HV_SUCCESS {
                return Err(Error::VcpuInitialRegisters);
            }
        }

        let ret = unsafe { hv_vcpu_set_reg(self.vcpuid, hv_reg_t_HV_REG_PC, entry_addr) };
        if ret != HV_SUCCESS {
            return Err(Error::VcpuInitialRegisters);
        }

        let ret = unsafe { hv_vcpu_set_reg(self.vcpuid, hv_reg_t_HV_REG_X0, fdt_addr) };
        if ret != HV_SUCCESS {
            return Err(Error::VcpuInitialRegisters);
        }

        Ok(())
    }

    pub fn id(&self) -> u64 {
        self.vcpuid
    }

    fn read_reg(&self, reg: u32) -> Result<u64, Error> {
        let mut val: u64 = 0;
        let ret = unsafe { hv_vcpu_get_reg(self.vcpuid, reg, &mut val as *mut _) };
        if ret != HV_SUCCESS {
            Err(Error::VcpuReadRegister)
        } else {
            Ok(val)
        }
    }

    pub fn write_reg(&self, rt: u32, val: u64) -> Result<(), Error> {
        let ret = unsafe { hv_vcpu_set_reg(self.vcpuid, rt, val) };
        if ret != HV_SUCCESS {
            Err(Error::VcpuSetRegister)
        } else {
            Ok(())
        }
    }

    fn read_sys_reg(&self, reg: u16) -> Result<u64, Error> {
        let mut val: u64 = 0;
        let ret = unsafe { hv_vcpu_get_sys_reg(self.vcpuid, reg, &mut val as *mut _) };
        if ret != HV_SUCCESS {
            Err(Error::VcpuReadSystemRegister)
        } else {
            Ok(val)
        }
    }

    /// Freeze the guest's CNTVCT across a live pause. The guest virtual counter
    /// is `mach_absolute_time() - vtimer_offset`; while paused the host counter
    /// keeps advancing, so on resume we add the ticks spent paused to the offset.
    /// The guest's counter then resumes where it stopped and armed deadlines stay
    /// in the near future instead of firing en masse to catch up the gap.
    #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
    pub fn advance_vtimer_offset(&self, paused_ticks: u64) -> Result<(), Error> {
        let offset = self.vtimer_offset.get().wrapping_add(paused_ticks);
        let ret = unsafe { hv_vcpu_set_vtimer_offset(self.vcpuid, offset) };
        if ret != HV_SUCCESS {
            return Err(Error::VcpuSetRegister);
        }
        self.vtimer_offset.set(offset);
        Ok(())
    }

    fn hvf_sync_vtimer(&mut self, vcpu_list: Arc<dyn Vcpus>) {
        if !self.vtimer_masked {
            return;
        }

        let ctl = self
            .read_sys_reg(hv_sys_reg_t_HV_SYS_REG_CNTV_CTL_EL0)
            .unwrap();
        let irq_state = (ctl & (TMR_CTL_ENABLE | TMR_CTL_IMASK | TMR_CTL_ISTATUS))
            == (TMR_CTL_ENABLE | TMR_CTL_ISTATUS);
        vcpu_list.set_vtimer_irq(self.vcpuid);
        if !irq_state {
            vcpu_set_vtimer_mask(self.vcpuid, false).unwrap();
            self.vtimer_masked = false;
        }
    }

    fn handle_psci_request(&self) -> Result<VcpuExit<'_>, Error> {
        match self.read_reg(hv_reg_t_HV_REG_X0)? {
            0x8400_0000 /* QEMU_PSCI_0_2_FN_PSCI_VERSION */ => {
                self.write_reg(hv_reg_t_HV_REG_X0, 2)?;
                Ok(VcpuExit::PsciHandled)
            },
            0x8400_0006 /* QEMU_PSCI_0_2_FN_MIGRATE_INFO_TYPE */ => {
                self.write_reg(hv_reg_t_HV_REG_X0, 2)?;
                Ok(VcpuExit::PsciHandled)
            },
            0x8400_0008 /* QEMU_PSCI_0_2_FN_SYSTEM_OFF */ => {
                Ok(VcpuExit::Shutdown)
            },
            0x8400_0009 /* QEMU_PSCI_0_2_FN_SYSTEM_RESET */ => {
                Ok(VcpuExit::Shutdown)
            },
            0xc400_0003 /* QEMU_PSCI_0_2_FN64_CPU_ON */ => {
                let mpidr = self.read_reg(hv_reg_t_HV_REG_X1)?;
                let entry = self.read_reg(hv_reg_t_HV_REG_X2)?;
                let context_id = self.read_reg(hv_reg_t_HV_REG_X3)?;
                self.write_reg(hv_reg_t_HV_REG_X0, 0)?;
                Ok(VcpuExit::CpuOn(mpidr, entry, context_id))
            }
            val => panic!("Unexpected val={val}")
        }
    }

    pub fn run(&mut self, vcpu_list: Arc<dyn Vcpus>) -> Result<VcpuExit<'_>, Error> {
        let pending_irq = vcpu_list.has_pending_irq(self.vcpuid);

        if let Some(mmio_read) = self.pending_mmio_read.take()
            && mmio_read.srt < 31
        {
            let val = match mmio_read.len {
                1 => u8::from_le_bytes(self.mmio_buf[0..1].try_into().unwrap()) as u64,
                2 => u16::from_le_bytes(self.mmio_buf[0..2].try_into().unwrap()) as u64,
                4 => u32::from_le_bytes(self.mmio_buf[0..4].try_into().unwrap()) as u64,
                8 => u64::from_le_bytes(self.mmio_buf[0..8].try_into().unwrap()),
                _ => panic!(
                    "unsupported mmio pa={} len={}",
                    mmio_read.addr, mmio_read.len
                ),
            };

            self.write_reg(mmio_read.srt, val)?;
        }

        if self.pending_advance_pc {
            let pc = self.read_reg(hv_reg_t_HV_REG_PC)?;
            self.write_reg(hv_reg_t_HV_REG_PC, pc + 4)?;
            self.pending_advance_pc = false;
        }

        if pending_irq {
            vcpu_set_pending_irq(self.vcpuid, InterruptType::Irq, true)?;
        }

        let ret = unsafe { hv_vcpu_run(self.vcpuid) };
        if ret != HV_SUCCESS {
            return Err(Error::VcpuRun);
        }

        match self.vcpu_exit.reason {
            HV_EXIT_REASON_EXCEPTION => { /* This is the main one, handle below. */ }
            HV_EXIT_REASON_VTIMER_ACTIVATED => {
                self.vtimer_masked = true;
                return Ok(VcpuExit::VtimerActivated);
            }
            HV_EXIT_REASON_CANCELED => return Ok(VcpuExit::Canceled),
            _ => {
                let pc = self.read_reg(hv_reg_t_HV_REG_PC)?;
                panic!(
                    "unexpected exit reason: vcpuid={} 0x{:x} at pc=0x{:x}",
                    self.id(),
                    self.vcpu_exit.reason,
                    pc
                );
            }
        }

        self.hvf_sync_vtimer(vcpu_list.clone());

        let syndrome = self.vcpu_exit.exception.syndrome;
        let ec = (syndrome >> 26) & 0x3f;
        match ec {
            EC_AA64_BKPT => {
                debug!("vcpu[{}]: BRK exit", self.vcpuid);
                Ok(VcpuExit::Breakpoint)
            }
            EC_DATAABORT => {
                let isv: bool = (syndrome & (1 << 24)) != 0;
                let iswrite: bool = ((syndrome >> 6) & 1) != 0;
                let s1ptw: bool = ((syndrome >> 7) & 1) != 0;
                let sas: u32 = ((syndrome >> 22) & 3) as u32;
                let len: usize = (1 << sas) as usize;
                let srt: u32 = ((syndrome >> 16) & 0x1f) as u32;
                let cm: u32 = ((syndrome >> 8) & 0x1) as u32;

                debug!(
                    "EC_DATAABORT {} {} {} {} {} {} {} {}",
                    syndrome, isv as u8, iswrite as u8, s1ptw as u8, sas, len, srt, cm
                );

                let pa = self.vcpu_exit.exception.physical_address;
                if in_reclaim_ram(pa) {
                    error!(
                        "vcpu[{}]: stage-2 data abort on guest RAM 0x{pa:x} while it was being \
                         reclaimed, stopping the VM",
                        self.vcpuid
                    );
                    return Ok(VcpuExit::Shutdown);
                }
                self.pending_advance_pc = true;

                if iswrite {
                    let val = if srt < 31 {
                        self.read_reg(hv_reg_t_HV_REG_X0 + srt)?
                    } else {
                        0
                    };

                    match len {
                        1 => self.mmio_buf[0..1].copy_from_slice(&(val as u8).to_le_bytes()),
                        4 => self.mmio_buf[0..4].copy_from_slice(&(val as u32).to_le_bytes()),
                        8 => self.mmio_buf[0..8].copy_from_slice(&val.to_le_bytes()),
                        _ => panic!("unsupported mmio len={len}"),
                    };

                    Ok(VcpuExit::MmioWrite(pa, &self.mmio_buf[0..len]))
                } else {
                    self.pending_mmio_read = Some(MmioRead { addr: pa, srt, len });
                    Ok(VcpuExit::MmioRead(pa, &mut self.mmio_buf[0..len]))
                }
            }
            #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
            EC_SYSTEMREGISTERTRAP => {
                let isread: bool = (syndrome & 1) != 0;
                let rt: u32 = ((syndrome >> 5) & 0x1f) as u32;
                let reg: u32 = syndrome as u32 & SYSREG_MASK;
                debug!(
                    "EC_SYSTEMREGISTERTRAP isread={}, syndrome={}, rt={}, reg={}, reg_name={}",
                    isread as u32,
                    syndrome,
                    rt,
                    reg,
                    sys_reg_name(reg).unwrap_or("unknown sysreg")
                );

                self.pending_advance_pc = true;

                if isread {
                    assert!(rt < 32);

                    // See https://developer.arm.com/documentation/dui0801/l/Overview-of-AArch64-state/Registers-in-AArch64-state
                    if rt == 31 {
                        return Ok(VcpuExit::SystemRegister);
                    }

                    match vcpu_list.handle_sysreg_read(self.vcpuid, reg) {
                        Some(val) => {
                            self.write_reg(rt, val)?;
                            Ok(VcpuExit::SystemRegister)
                        }
                        None => panic!(
                            "UNKNOWN rt={}, reg={} name={}",
                            rt,
                            reg,
                            sys_reg_name(reg).unwrap_or("unknown sysreg")
                        ),
                    }
                } else {
                    assert!(rt < 32);

                    // See https://developer.arm.com/documentation/dui0801/l/Overview-of-AArch64-state/Registers-in-AArch64-state
                    let val = if rt == 31 { 0u64 } else { self.read_reg(rt)? };

                    if vcpu_list.handle_sysreg_write(self.vcpuid, reg, val) {
                        Ok(VcpuExit::SystemRegister)
                    } else {
                        panic!(
                            "unexpected write: {} name={}",
                            reg,
                            sys_reg_name(reg).unwrap_or("unknown sysreg")
                        );
                    }
                }
            }
            EC_WFX_TRAP => {
                let ctl = self.read_sys_reg(hv_sys_reg_t_HV_SYS_REG_CNTV_CTL_EL0)?;

                self.pending_advance_pc = true;
                if ((ctl & 1) == 0) || (ctl & 2) != 0 {
                    return Ok(VcpuExit::WaitForEvent);
                }

                // Also CNTV_CVAL & CNTV_CVAL_EL0
                let cval = self.read_sys_reg(hv_sys_reg_t_HV_SYS_REG_CNTV_CVAL_EL0)?;
                // The guest's deadline `cval` is in the CNTVCT domain, where
                // CNTVCT = mach_absolute_time() - vtimer_offset. The offset is 0
                // on a fresh boot but nonzero after a live pause (it keeps CNTVCT
                // continuous across the paused interval). Compare against the
                // corrected counter, not raw mach time — otherwise after a pause
                // `now` runs ahead of every armed deadline and the vCPU busy-loops
                // on WaitForEventExpired instead of parking, so the guest's virtual
                // timer never fires and timed sleeps hang.
                let now = unsafe { mach_absolute_time() }.saturating_sub(self.vtimer_offset.get());
                if now > cval {
                    return Ok(VcpuExit::WaitForEventExpired);
                }

                let timeout = Duration::from_nanos((cval - now) * (1_000_000_000 / self.cntfrq));
                Ok(VcpuExit::WaitForEventTimeout(timeout))
            }
            EC_AA64_HVC => self.handle_psci_request(),
            EC_AA64_SMC => {
                self.pending_advance_pc = true;
                self.handle_psci_request()
            }
            EC_INSTRABORT if in_reclaim_ram(self.vcpu_exit.exception.physical_address) => {
                error!(
                    "vcpu[{}]: stage-2 instruction abort on guest RAM 0x{:x} while it was being \
                     reclaimed, stopping the VM",
                    self.vcpuid, self.vcpu_exit.exception.physical_address
                );
                Ok(VcpuExit::Shutdown)
            }
            _ => panic!("unexpected exception: 0x{ec:x}"),
        }
    }
}
