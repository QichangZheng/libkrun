//! Free page reclaim test. macOS/HVF only.
//!
//! On macOS, madvise() doesn't release guest RAM that the guest wrote through
//! stage-2, so with reclaim off the process keeps it even after the guest frees
//! it. This boots a guest with `BalloonDevice::set_free_page_reclaim(true)` and:
//!   - has the guest fill FILL_MIB, measures the VMM's phys_footprint, then has
//!     the guest free that memory and checks that at least RELEASED_PCT of it
//!     leaves the footprint once free page reporting has run;
//!   - checks that data the guest kept alive while the rest was reclaimed is
//!     intact, and that the reclaimed memory can be written and read back.
//!
//! The VMM runs in this process, so the host side measures its own footprint.
//! Host and guest step each other through marker files in the shared rootfs.

use macros::{guest, host};

pub struct TestBalloonFreePageReclaim;

const FILL_MIB: u64 = 512;

#[host]
mod host {
    use super::*;

    use std::fs;
    use std::mem::MaybeUninit;
    use std::os::fd::AsFd;
    use std::path::Path;
    use std::thread;
    use std::time::{Duration, Instant};

    use nix::libc;

    use crate::common::{build_init_config, init_krun, setup_rootfs};
    use crate::{ShouldRun, Test, TestOutcome, TestSetup};

    const RAM_MIB: u32 = 1024;
    const RELEASED_PCT: u64 = 75;
    const SETTLE: Duration = Duration::from_secs(2);
    const RECLAIM_DEADLINE: Duration = Duration::from_secs(20);
    const MIB: u64 = 1024 * 1024;

    #[cfg(feature = "dynamic-linking")]
    fn require_symbols() -> Result<(), libloading::Error> {
        crate::common::require_vm_symbols()?;
        krun::require(None, &[krun::Symbol::KrunBalloonDeviceSetFreePageReclaim])
    }

    fn phys_footprint() -> u64 {
        let mut info = MaybeUninit::<libc::rusage_info_v4>::zeroed();
        let ret = unsafe {
            libc::proc_pid_rusage(
                libc::getpid(),
                libc::RUSAGE_INFO_V4,
                info.as_mut_ptr() as *mut libc::rusage_info_t,
            )
        };
        assert_eq!(ret, 0, "proc_pid_rusage failed");
        unsafe { info.assume_init() }.ri_phys_footprint
    }

    fn wait_for(path: &Path) {
        while !path.exists() {
            thread::sleep(Duration::from_millis(100));
        }
    }

    impl Test for TestBalloonFreePageReclaim {
        fn should_run(&self) -> ShouldRun {
            if !cfg!(target_os = "macos") {
                return ShouldRun::No("free page reclaim is macOS only");
            }
            #[cfg(feature = "dynamic-linking")]
            if require_symbols().is_err() {
                return ShouldRun::No("free page reclaim not available in this libkrun build");
            }
            ShouldRun::Yes
        }

        fn timeout_secs(&self) -> u64 {
            90
        }

        fn start_vm(self: Box<Self>, test_setup: TestSetup) -> anyhow::Result<()> {
            init_krun()?;
            #[cfg(feature = "dynamic-linking")]
            require_symbols().unwrap();

            let root_dir = setup_rootfs(&test_setup)?;
            let init_config = build_init_config(&test_setup.test_case, &[]);

            let mut rootfs = krun::FsDevice::new("/dev/root", root_dir.to_str().unwrap())
                .map_err(|e| anyhow::anyhow!("FsDevice::new: {e:?}"))?;
            let mut payload =
                krun::Payload::load_krunfw().map_err(|e| anyhow::anyhow!("load_krunfw: {e:?}"))?;
            let mut overlay = krun::FsOverlay::new();
            init_config
                .apply(&mut overlay, &mut payload)
                .map_err(|e| anyhow::anyhow!("Config::apply: {e}"))?;
            rootfs.set_overlay(overlay);

            let stdin = std::io::stdin();
            let stdout = std::io::stdout();
            let stderr = std::io::stderr();
            let mut console_builder = krun::ConsoleDevice::builder();
            console_builder
                .add_default_console(
                    Some(stdin.as_fd()),
                    Some(stdout.as_fd()),
                    Some(stderr.as_fd()),
                )
                .map_err(|e| anyhow::anyhow!("add_default_console: {e:?}"))?;
            let console = console_builder
                .build()
                .map_err(|e| anyhow::anyhow!("ConsoleDevice::build: {e:?}"))?;

            let mut balloon =
                krun::BalloonDevice::new().map_err(|e| anyhow::anyhow!("BalloonDevice: {e:?}"))?;
            balloon.set_free_page_reclaim(true);

            let mut devices = krun::MmioDeviceManager::new();
            devices.add(rootfs);
            devices.add(console);
            devices.add(balloon);

            let vmm = krun::VmmBuilder::new()
                .vcpus(2)
                .map_err(|e| anyhow::anyhow!("vcpus: {e:?}"))?
                .ram_mib(RAM_MIB)
                .map_err(|e| anyhow::anyhow!("ram_mib: {e:?}"))?
                .payload(payload)
                .devices(devices)
                .build()
                .map_err(|e| anyhow::anyhow!("VmmBuilder::build: {e:?}"))?;

            let tmp_dir = test_setup.tmp_dir.clone();
            thread::spawn(move || {
                wait_for(&root_dir.join("filled"));
                thread::sleep(SETTLE);
                let before = phys_footprint();
                fs::write(root_dir.join("go-free"), "").unwrap();

                wait_for(&root_dir.join("freed"));
                let start = Instant::now();
                let mut after = phys_footprint();
                while before.saturating_sub(after) < FILL_MIB * MIB * RELEASED_PCT / 100
                    && start.elapsed() < RECLAIM_DEADLINE
                {
                    thread::sleep(Duration::from_millis(250));
                    after = after.min(phys_footprint());
                }
                fs::write(tmp_dir.join("footprint"), format!("{before} {after}")).unwrap();
                fs::write(root_dir.join("go-verify"), "").unwrap();
            });

            vmm.run();
            unreachable!()
        }

        fn check(self: Box<Self>, stdout: Vec<u8>, test_setup: TestSetup) -> TestOutcome {
            let out = String::from_utf8_lossy(&stdout);
            for marker in ["LIVE OK", "REUSE OK"] {
                if !out.lines().any(|l| l.trim() == marker) {
                    return TestOutcome::Fail(format!("guest did not report {marker:?}: {out:?}"));
                }
            }

            let Ok(footprint) = fs::read_to_string(test_setup.tmp_dir.join("footprint")) else {
                return TestOutcome::Fail("host side never measured the footprint".into());
            };
            let mut values = footprint
                .split_whitespace()
                .map(|v| v.parse::<u64>().unwrap());
            let (before, after) = (values.next().unwrap(), values.next().unwrap());
            let released = before.saturating_sub(after);
            if released < FILL_MIB * MIB * RELEASED_PCT / 100 {
                return TestOutcome::Fail(format!(
                    "guest freed {FILL_MIB} MiB but phys_footprint only went from {} to {} MiB",
                    before / MIB,
                    after / MIB
                ));
            }
            TestOutcome::Pass
        }
    }
}

#[guest]
mod guest {
    use super::*;
    use crate::Test;

    use std::fs;
    use std::path::Path;
    use std::ptr;
    use std::slice;
    use std::thread;
    use std::time::Duration;

    use nix::libc;

    const PAGE: usize = 4096;
    const LIVE_MIB: u64 = 64;

    fn wait_for(path: &str) {
        while !Path::new(path).exists() {
            thread::sleep(Duration::from_millis(100));
        }
    }

    fn map_anon(len: usize) -> &'static mut [u8] {
        let p = unsafe {
            libc::mmap(
                ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        assert_ne!(p, libc::MAP_FAILED, "mmap failed");
        unsafe { slice::from_raw_parts_mut(p as *mut u8, len) }
    }

    fn fill(buf: &mut [u8], seed: u64) {
        for (i, page) in buf.chunks_mut(PAGE).enumerate() {
            page.fill((i as u64 ^ seed) as u8);
        }
    }

    fn holds(buf: &[u8], seed: u64) -> bool {
        buf.chunks(PAGE)
            .enumerate()
            .all(|(i, page)| page.iter().all(|&b| b == (i as u64 ^ seed) as u8))
    }

    impl Test for TestBalloonFreePageReclaim {
        fn in_guest(self: Box<Self>) {
            let mib = 1024 * 1024;
            let fill_len = (FILL_MIB * mib) as usize;

            let live = map_anon((LIVE_MIB * mib) as usize);
            fill(live, 0x5a);
            let reclaimable = map_anon(fill_len);
            fill(reclaimable, 0xa5);
            fs::write("/filled", "").unwrap();

            wait_for("/go-free");
            unsafe { libc::munmap(reclaimable.as_mut_ptr() as *mut libc::c_void, fill_len) };
            fs::write("/freed", "").unwrap();

            wait_for("/go-verify");
            if holds(live, 0x5a) {
                println!("LIVE OK");
            }
            let reused = map_anon(fill_len);
            fill(reused, 0x3c);
            if holds(reused, 0x3c) {
                println!("REUSE OK");
            }
        }
    }
}
