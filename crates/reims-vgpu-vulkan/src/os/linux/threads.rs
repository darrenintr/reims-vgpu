//! `Threads` on Linux: `std::thread`s, detached, with the stack and name core asks for.
//!
//! No Objective-C runs on Linux, so the reason macOS starts plain pthreads (D35: no unwind
//! handler at a thread's base for a foreign exception's search phase to find) does not apply. A
//! panic on one of these threads is the library's, and its panic hook aborts the process before
//! anything unwinds. A thread's name is a diagnostic; Linux keeps 15 bytes of it, so a longer one
//! is cut there, on a character boundary.
//!
//! **CPU affinity.** A new thread takes its creator's CPU affinity, and the library's threads are
//! often made on a host's vCPU thread, inside an MMIO access: the FIFO threads, the display's
//! thread, and the runtime's threads each of those makes in turn. A host that pins each vCPU
//! thread to one CPU (`reims-ctl`'s `--pin auto`) thereby put all of them on that vCPU's CPU,
//! beside the vCPU. Measured on a Ryzen 5 3600 and an RX 7600 with the tahoe guest at 8 pinned
//! vCPUs (2026-10-14): the six FIFO threads, the display's thread and about 30 runtime threads were
//! all bound to CPU 4, vCPU 4's, while the GPU stayed 17% busy at the median; unbinding them in
//! the running VM took the 90th percentile of the gaps between the guest's frames from 87 ms to
//! 41 ms, and the 95th from 130 ms to 51 ms. So each thread here starts with the affinity of the
//! process's main thread, which a host that pins only its vCPUs leaves unpinned, and which a
//! host that confines the whole process (`taskset`) sets for every thread.
//!
//! **A FIFO thread's submissions go together.** The guest rings a FIFO's doorbell for several
//! command buffers at a time (about 11 a wake on the tahoe guest, 2026-10-14), and on RADV each
//! submission's ioctl walks every buffer object the device has (`amdgpu_cs_ioctl`: the list's
//! sort, each object's lock and reservation sync), which the scheduler and the FIFO threads spent
//! most of their CPU time in. So a FIFO thread defers its commits' submissions
//! (`metal2vulkan_runtime::defer_submissions`) and submits them together each time before it blocks
//! (`reims_vgpu_core::rt::thread::set_before_block`): for ring bytes, where a burst ends, for room
//! among its stamps, or for other channels' stamps. The runtime submits them sooner wherever
//! something waits on them, and within a millisecond in any case.

use std::sync::Arc;

use metal2vulkan_runtime as rt;
use reims_vgpu_core::platform::os::{Threads, TimerClock};
use reims_vgpu_core::raise::death::{DeathId, DeathKind, fatal};

use super::clock::LinuxClock;

/// The FIFO threads' name (`reims_vgpu_core::fifo`).
const FIFO_THREAD: &str = "reims-vgpu.fifo";

/// The bytes of a thread's name Linux keeps (`TASK_COMM_LEN` less its NUL).
const NAME_BYTES: usize = 15;

/// `name`, cut to the bytes Linux keeps.
fn kept_name(name: &str) -> &str {
    let mut end = name.len().min(NAME_BYTES);
    while !name.is_char_boundary(end) {
        end -= 1;
    }
    &name[..end]
}

/// A detached thread named `name` with `stack_size` bytes of stack that runs `body`. A refused
/// thread ends the process.
fn spawn_thread(name: &'static str, stack_size: usize, body: Box<dyn FnOnce() + Send>) {
    let made = std::thread::Builder::new()
        .name(kept_name(name).to_owned())
        .stack_size(stack_size)
        .spawn(move || {
            take_process_affinity();
            // A FIFO thread holds back the GPU work it commits until it would block, and then
            // submits it together (module docs).
            let fifo = name == FIFO_THREAD;
            if fifo {
                reims_vgpu_core::rt::thread::set_before_block(rt::flush_deferred_submissions);
                rt::defer_submissions(true);
            }
            body();
            if fifo {
                rt::defer_submissions(false);
            }
        });
    if made.is_err() {
        fatal(DeathId::Platform {
            kind: DeathKind::Abort,
            call: "pthread_create",
        })
    }
}

/// Gives the calling thread the CPU affinity of the process's main thread (module docs). Where
/// either call is refused, the thread keeps the affinity it was made with.
fn take_process_affinity() {
    let mut set = std::mem::MaybeUninit::<libc::cpu_set_t>::zeroed();
    let size = std::mem::size_of::<libc::cpu_set_t>();
    // SAFETY: the main thread's (the process ID's) mask, written into a set of the size given.
    if unsafe { libc::sched_getaffinity(libc::getpid(), size, set.as_mut_ptr()) } != 0 {
        return;
    }
    // SAFETY: the calling thread's mask, from the set read above.
    unsafe { libc::sched_setaffinity(0, size, set.as_ptr()) };
}

/// The process's threads, and its clock for timers.
#[derive(Debug, Default)]
pub struct LinuxThreads;

impl LinuxThreads {
    /// The process's Threads.
    pub fn new() -> LinuxThreads {
        LinuxThreads
    }
}

impl Threads for LinuxThreads {
    fn spawn(&self, name: &'static str, stack_size: usize, body: Box<dyn FnOnce() + Send>) {
        spawn_thread(name, stack_size, body);
    }

    /// A worker named after its queue, with a GCD worker's 512 KiB of stack, as on macOS.
    fn spawn_worker(&self, label: &'static str, body: Box<dyn FnOnce() + Send>) {
        spawn_thread(label, reims_vgpu_core::rt::queue::WORKER_STACK, body);
    }

    fn handle(&self) -> Arc<dyn Threads> {
        Arc::new(LinuxThreads)
    }

    fn timer_clock(&self) -> Option<Arc<dyn TimerClock>> {
        Some(Arc::new(LinuxClock::new()))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::time::Duration;

    use super::*;

    const WAIT: Duration = Duration::from_secs(10);

    /// The running thread's name, as Linux keeps it.
    fn current_name() -> String {
        let mut buf = [0 as libc::c_char; 64];
        // SAFETY: a live buffer of the length given.
        let rc =
            unsafe { libc::pthread_getname_np(libc::pthread_self(), buf.as_mut_ptr(), buf.len()) };
        assert_eq!(rc, 0);
        // SAFETY: pthread_getname_np NUL-terminates within the buffer.
        unsafe { std::ffi::CStr::from_ptr(buf.as_ptr()) }
            .to_string_lossy()
            .into_owned()
    }

    /// A spawned thread is named, cut to the 15 bytes Linux keeps, and has at least the stack
    /// asked for
    #[test]
    fn a_thread_is_named_and_sized() {
        let (tx, rx) = mpsc::channel();
        LinuxThreads::new().spawn(
            "reims-vgpu.fifo",
            512 * 1024,
            Box::new(move || {
                let mut attr = std::mem::MaybeUninit::<libc::pthread_attr_t>::uninit();
                let mut size = 0usize;
                // SAFETY: the running thread's attributes, read and destroyed here.
                unsafe {
                    libc::pthread_getattr_np(libc::pthread_self(), attr.as_mut_ptr());
                    libc::pthread_attr_getstacksize(attr.as_ptr(), &mut size);
                    libc::pthread_attr_destroy(attr.as_mut_ptr());
                }
                tx.send((current_name(), size)).unwrap();
            }),
        );
        let (name, size) = rx.recv_timeout(WAIT).unwrap();
        assert_eq!(name, "reims-vgpu.fifo");
        assert!(size >= 512 * 1024, "{size}");
        assert_eq!(kept_name("reims-vgpu.display-present"), "reims-vgpu.disp");
        assert_eq!(kept_name("reims-vgpu.é1234"), "reims-vgpu.é12");
        assert_eq!(kept_name("reims-vgpu.ééé"), "reims-vgpu.éé");
    }

    /// The CPUs `tid`'s affinity holds (0: the calling thread; the process ID: the main thread).
    fn affinity(tid: libc::pid_t) -> Vec<usize> {
        let mut set = std::mem::MaybeUninit::<libc::cpu_set_t>::zeroed();
        let size = std::mem::size_of::<libc::cpu_set_t>();
        // SAFETY: the thread's mask, written into a set of the size given.
        assert_eq!(
            unsafe { libc::sched_getaffinity(tid, size, set.as_mut_ptr()) },
            0
        );
        // SAFETY: written above.
        let set = unsafe { set.assume_init() };
        let cpus = 0..usize::try_from(libc::CPU_SETSIZE).unwrap();
        // SAFETY: CPU_ISSET reads the set within its size.
        cpus.filter(|&c| unsafe { libc::CPU_ISSET(c, &set) })
            .collect()
    }

    /// A thread made on a thread pinned to one CPU, as a host pins a vCPU's thread, takes the
    /// process's main thread's affinity, not its creator's one CPU
    #[test]
    fn a_thread_takes_the_process_affinity_not_its_creators() {
        let process = affinity(std::process::id().try_into().unwrap());
        let first = process[0];
        let (tx, rx) = mpsc::channel();
        #[allow(clippy::disallowed_methods)]
        // the pinned creator stands in for a host's vCPU thread
        std::thread::spawn(move || {
            let mut one = std::mem::MaybeUninit::<libc::cpu_set_t>::zeroed();
            let size = std::mem::size_of::<libc::cpu_set_t>();
            // SAFETY: the zeroed set gets one CPU, and the calling thread is pinned to it.
            unsafe {
                libc::CPU_SET(first, one.assume_init_mut());
                assert_eq!(libc::sched_setaffinity(0, size, one.as_ptr()), 0);
            }
            assert_eq!(affinity(0), [first]);
            LinuxThreads::new().spawn(
                "reims-vgpu.fifo",
                512 * 1024,
                Box::new(move || tx.send(affinity(0)).unwrap()),
            );
        });
        assert_eq!(rx.recv_timeout(WAIT).unwrap(), process);
    }

    /// A queue's worker is named after the queue, cut to the bytes Linux keeps
    #[test]
    fn a_worker_is_named_after_its_queue() {
        let (tx, rx) = mpsc::channel();
        LinuxThreads::new().spawn_worker(
            "reims-vgpu.ui",
            Box::new(move || tx.send(current_name()).unwrap()),
        );
        assert_eq!(rx.recv_timeout(WAIT).unwrap(), "reims-vgpu.ui");
    }
}
