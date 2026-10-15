use std::cmp;
#[cfg(target_os = "macos")]
use std::io;
use std::io::Write;
#[cfg(target_os = "macos")]
use std::mem;
#[cfg(target_os = "macos")]
use std::os::fd::{AsRawFd, RawFd};
#[cfg(target_os = "macos")]
use std::sync::{Arc, Mutex, mpsc};
#[cfg(target_os = "macos")]
use std::thread;

use utils::eventfd::EventFd;
use vm_memory::{ByteValued, GuestAddress, GuestMemoryBackend, GuestMemoryMmap, GuestMemoryRegion};

use super::super::{
    ActivateError, ActivateResult, BalloonError, DeviceQueue, DeviceState, QueueConfig,
    VirtioDevice,
};
use super::{defs, defs::uapi};
use crate::virtio::InterruptTransport;
#[cfg(target_os = "windows")]
use windows_sys::Win32::System::Memory::DiscardVirtualMemory;

// Inflate queue.
pub(crate) const IFQ_INDEX: usize = 0;
// Deflate queue.
pub(crate) const DFQ_INDEX: usize = 1;
// Stats queue.
pub(crate) const STQ_INDEX: usize = 2;
// Page-hinting queue.
pub(crate) const PHQ_INDEX: usize = 3;
// Free page reporting queue.
pub(crate) const FRQ_INDEX: usize = 4;

// Supported features.
pub(crate) const AVAIL_FEATURES: u64 = (1 << uapi::VIRTIO_F_VERSION_1 as u64)
    | (1 << uapi::VIRTIO_BALLOON_F_STATS_VQ as u64)
    | (1 << uapi::VIRTIO_BALLOON_F_FREE_PAGE_HINT as u64)
    | (1 << uapi::VIRTIO_BALLOON_F_REPORTING as u64);

#[derive(Copy, Clone, Debug, Default)]
#[repr(C, packed)]
pub struct VirtioBalloonConfig {
    /* Number of pages host wants Guest to give up. */
    num_pages: u32,
    /* Number of pages we've actually got in balloon. */
    actual: u32,
    /* Free page report command id, readonly by guest */
    free_page_report_cmd_id: u32,
    /* Stores PAGE_POISON if page poisoning is in use */
    poison_val: u32,
}

// Safe because it only has data and has no implicit padding.
unsafe impl ByteValued for VirtioBalloonConfig {}

pub struct Balloon {
    pub(crate) queues: Option<Vec<DeviceQueue>>,
    pub(crate) avail_features: u64,
    pub(crate) acked_features: u64,
    pub(crate) activate_evt: EventFd,
    pub(crate) device_state: DeviceState,
    config: VirtioBalloonConfig,
    bad_reports: u64,
    #[cfg(target_os = "macos")]
    reclaimer: Option<Reclaimer>,
}

/// Reports shorter than this are only madvise()d, so that the guest can't make
/// us split the stage-2 mapping of its RAM into many small pieces. Linux
/// reports 2 MiB blocks by default, but page_reporting_order can be lowered.
#[cfg(target_os = "macos")]
const RECLAIM_MIN_LEN: u64 = 2 << 20;

/// Gives free page reports back to the host with hvf::reclaim_guest_ram(), on a
/// worker thread because that may block. A report is added to the used ring
/// only once the worker is done with it: until then the guest doesn't touch
/// the reported pages, so it never finds them unmapped.
#[cfg(target_os = "macos")]
struct Reclaimer {
    /// Guest RAM range [start, end) that may be reclaimed.
    ram: (u64, u64),
    jobs: mpsc::Sender<ReclaimJob>,
    /// Heads of the descriptor chains the worker is done with.
    done: Arc<Mutex<Vec<u16>>>,
    done_evt: EventFd,
}

#[cfg(target_os = "macos")]
struct ReclaimJob {
    head: u16,
    /// (guest address, host address, length)
    ranges: Vec<(u64, u64, u64)>,
}

/// Host address of a free page report, if the whole range [addr, addr + len)
/// lies within one guest memory region. Both values come from the guest.
fn report_host_addr(mem: &GuestMemoryMmap, addr: GuestAddress, len: u64) -> Option<*mut u8> {
    let region = mem.find_region(addr)?;
    let last = addr.0.checked_add(len.checked_sub(1)?)?;
    if last > region.last_addr().0 {
        return None;
    }
    mem.get_host_address(addr).ok()
}

impl Balloon {
    pub fn new() -> super::Result<Balloon> {
        Ok(Balloon {
            queues: None,
            avail_features: AVAIL_FEATURES,
            acked_features: 0,
            activate_evt: EventFd::new(utils::eventfd::EFD_NONBLOCK)
                .map_err(BalloonError::EventFd)?,
            device_state: DeviceState::Inactive,
            config: VirtioBalloonConfig::default(),
            bad_reports: 0,
            #[cfg(target_os = "macos")]
            reclaimer: None,
        })
    }

    /// Give free page reports of at least 2 MiB inside the guest RAM range
    /// [start, end) back to the host by remapping them, instead of
    /// madvise()ing them, which doesn't free any page the guest has written
    /// to. The range must not include the firmware region or SHM windows.
    #[cfg(target_os = "macos")]
    pub fn enable_reclaim(&mut self, start: u64, end: u64) -> super::Result<()> {
        let done_evt = EventFd::new(utils::eventfd::EFD_NONBLOCK).map_err(BalloonError::EventFd)?;
        let worker_evt = done_evt.try_clone().map_err(BalloonError::EventFd)?;
        let (jobs, job_rx) = mpsc::channel();
        let done = Arc::new(Mutex::new(Vec::new()));
        let worker_done = done.clone();
        thread::Builder::new()
            .name("balloon reclaim".into())
            .spawn(move || reclaim_worker(job_rx, worker_done, worker_evt))
            .map_err(BalloonError::Worker)?;

        hvf::set_reclaim_ram_range(start, end);
        self.reclaimer = Some(Reclaimer {
            ram: (start, end),
            jobs,
            done,
            done_evt,
        });
        Ok(())
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn reclaim_done_fd(&self) -> Option<RawFd> {
        self.reclaimer.as_ref().map(|r| r.done_evt.as_raw_fd())
    }

    /// Adds the reports the reclaim worker is done with to the used ring.
    /// Returns true if there were any.
    #[cfg(target_os = "macos")]
    pub(crate) fn complete_reclaimed(&mut self) -> bool {
        let Some(r) = self.reclaimer.as_ref() else {
            return false;
        };
        if let Err(e) = r.done_evt.read()
            && e.kind() != io::ErrorKind::WouldBlock
        {
            error!("balloon: failed to read reclaim event: {e:?}");
        }
        let heads = mem::take(&mut *r.done.lock().unwrap());

        let mem = match self.device_state {
            DeviceState::Activated(ref mem, _) => mem,
            DeviceState::Inactive => unreachable!(),
        };
        let queues = self
            .queues
            .as_mut()
            .expect("queues should exist when activated");
        for &head in &heads {
            if let Err(e) = queues[FRQ_INDEX].queue.add_used(mem, head, 0) {
                error!("failed to add used elements to the queue: {e:?}");
            }
        }
        !heads.is_empty()
    }

    pub fn id(&self) -> &str {
        defs::BALLOON_DEV_ID
    }

    pub fn process_frq(&mut self) -> bool {
        debug!("balloon: process_frq()");
        let mem = match self.device_state {
            DeviceState::Activated(ref mem, _) => mem,
            // This should never happen, it's been already validated in the event handler.
            DeviceState::Inactive => unreachable!(),
        };

        let queues = self
            .queues
            .as_mut()
            .expect("queues should exist when activated");
        let mut have_used = false;

        while let Some(head) = queues[FRQ_INDEX].queue.pop(mem) {
            let index = head.index;
            #[cfg(target_os = "macos")]
            let mut reclaim = Vec::new();
            for desc in head.into_iter() {
                let len = desc.len as u64;
                let Some(host_addr) = report_host_addr(mem, desc.addr, len) else {
                    // Releasing a range that goes past the end of guest memory
                    // would discard whatever the VMM has mapped next to it.
                    self.bad_reports += 1;
                    if self.bad_reports.is_power_of_two() {
                        warn!(
                            "balloon: ignoring free page report outside guest memory: \
                             guest_addr={:?} len={} ({} so far)",
                            desc.addr, desc.len, self.bad_reports
                        );
                    }
                    continue;
                };
                debug!(
                    "balloon: should release guest_addr={:?} host_addr={:p} len={}",
                    desc.addr, host_addr, desc.len
                );
                #[cfg(target_os = "macos")]
                if let Some(r) = &self.reclaimer
                    && len >= RECLAIM_MIN_LEN
                    && desc.addr.0 >= r.ram.0
                    && desc.addr.0 + len <= r.ram.1
                {
                    reclaim.push((desc.addr.0, host_addr as u64, len));
                    continue;
                }
                // SAFETY: the range is guest memory, which the guest no longer uses.
                unsafe { discard(host_addr, desc.len as usize) };
            }

            #[cfg(target_os = "macos")]
            if !reclaim.is_empty()
                && let Some(r) = &self.reclaimer
            {
                match r.jobs.send(ReclaimJob {
                    head: index,
                    ranges: reclaim,
                }) {
                    Ok(()) => continue,
                    Err(mpsc::SendError(job)) => {
                        error!("balloon: reclaim worker is gone, using madvise()");
                        for (_, host_addr, len) in job.ranges {
                            // SAFETY: as above.
                            unsafe { discard(host_addr as *mut u8, len as usize) };
                        }
                    }
                }
            }

            have_used = true;
            if let Err(e) = queues[FRQ_INDEX].queue.add_used(mem, index, 0) {
                error!("failed to add used elements to the queue: {e:?}");
            }
        }

        have_used
    }
}

/// Lets the host drop the pages backing a free page report.
///
/// # Safety
///
/// The range must be guest memory whose contents the guest doesn't need.
unsafe fn discard(host_addr: *mut u8, len: usize) {
    #[cfg(target_os = "linux")]
    let advice = libc::MADV_DONTNEED;
    #[cfg(target_os = "macos")]
    let advice = libc::MADV_FREE;
    #[cfg(unix)]
    unsafe {
        libc::madvise(host_addr as *mut libc::c_void, len, advice)
    };
    #[cfg(target_os = "windows")]
    unsafe {
        DiscardVirtualMemory(host_addr as *mut core::ffi::c_void, len)
    };
}

#[cfg(target_os = "macos")]
fn reclaim_worker(jobs: mpsc::Receiver<ReclaimJob>, done: Arc<Mutex<Vec<u16>>>, done_evt: EventFd) {
    let mut fallbacks: u64 = 0;
    for job in jobs {
        for (guest_addr, host_addr, len) in job.ranges {
            // SAFETY: process_frq() checked that the range is guest RAM, and the
            // guest doesn't touch it until the report is in the used ring.
            let result = unsafe { hvf::reclaim_guest_ram(host_addr, guest_addr, len) };
            if let Ok(released) = result
                && released > 0
            {
                debug!("balloon: reclaimed guest_addr=0x{guest_addr:x} len={released}");
                continue;
            }
            fallbacks += 1;
            if fallbacks.is_power_of_two() {
                warn!(
                    "balloon: could not remap guest_addr=0x{guest_addr:x} len={len} ({result:?}), \
                     using madvise() ({fallbacks} so far)"
                );
            }
            // SAFETY: as above.
            unsafe { discard(host_addr as *mut u8, len as usize) };
        }
        done.lock().unwrap().push(job.head);
        if let Err(e) = done_evt.write(1) {
            error!("balloon: failed to signal reclaim completion: {e:?}");
        }
    }
}

impl VirtioDevice for Balloon {
    fn avail_features(&self) -> u64 {
        self.avail_features
    }

    fn acked_features(&self) -> u64 {
        self.acked_features
    }

    fn set_acked_features(&mut self, acked_features: u64) {
        self.acked_features = acked_features
    }

    fn device_type(&self) -> u32 {
        uapi::VIRTIO_ID_BALLOON
    }

    fn device_name(&self) -> &str {
        "balloon"
    }

    fn queue_config(&self) -> &[QueueConfig] {
        &defs::QUEUE_CONFIG
    }

    fn read_config(&self, offset: u64, mut data: &mut [u8]) {
        let config_slice = self.config.as_slice();
        let config_len = config_slice.len() as u64;
        if offset >= config_len {
            error!("Failed to read config space");
            return;
        }
        if let Some(end) = offset.checked_add(data.len() as u64) {
            // This write can't fail, offset and end are checked against config_len.
            data.write_all(&config_slice[offset as usize..cmp::min(end, config_len) as usize])
                .unwrap();
        }
    }

    fn write_config(&mut self, offset: u64, data: &[u8]) {
        warn!(
            "balloon: guest driver attempted to write device config (offset={:x}, len={:x})",
            offset,
            data.len()
        );
    }

    fn activate(
        &mut self,
        mem: GuestMemoryMmap,
        interrupt: InterruptTransport,
        queues: Vec<DeviceQueue>,
    ) -> ActivateResult {
        if queues.len() != defs::NUM_QUEUES {
            error!(
                "Cannot perform activate. Expected {} queue(s), got {}",
                defs::NUM_QUEUES,
                queues.len()
            );
            return Err(ActivateError::BadActivate);
        }

        if self.activate_evt.write(1).is_err() {
            error!("Cannot write to activate_evt",);
            return Err(ActivateError::BadActivate);
        }

        self.queues = Some(queues);
        self.device_state = DeviceState::Activated(mem, interrupt);

        Ok(())
    }

    fn is_activated(&self) -> bool {
        self.device_state.is_activated()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_report_host_addr() {
        let mem = GuestMemoryMmap::<()>::from_ranges(&[(GuestAddress(0x10000), 0x40000)]).unwrap();
        let base = mem.get_host_address(GuestAddress(0x10000)).unwrap();
        let check = |addr, len| report_host_addr(&mem, GuestAddress(addr), len);

        assert_eq!(check(0x10000, 0x40000), Some(base));
        assert_eq!(check(0x40000, 0x10000), Some(base.wrapping_add(0x30000)));

        assert_eq!(check(0x40000, 0x10001), None);
        assert_eq!(check(0x10000, u32::MAX as u64), None);
        assert_eq!(check(0x0, 0x1000), None);
        assert_eq!(check(0x10000, 0), None);
        assert_eq!(check(u64::MAX, 2), None);
    }
}
