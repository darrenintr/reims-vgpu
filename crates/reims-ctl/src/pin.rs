//! Pinning a VM's vCPU threads to host CPUs (`[cpu] pin` in `vm.toml`), on a Linux host.
//!
//! QEMU has no option for it: once QMP answers, `start` asks it for each vCPU's thread
//! (`query-cpus-fast`) and sets that thread's affinity to one host CPU. Only the vCPUs are
//! pinned. QEMU's other threads, the library's among them (a FIFO's thread, a window's render
//! thread, the display's queue, the driver's), stay free to run anywhere, and with the vCPUs
//! kept off them the host CPUs left over are theirs: on a 6-core, 12-thread host with an
//! 8-vCPU guest, `auto` gives the guest cores 2 to 5 and both threads of each, and leaves cores
//! 0 and 1 to the library and the host's desktop, so a busy guest never takes the CPU from the
//! threads that turn its frames into GPU work.
//!
//! The CPUs are ordered by host core, each core's threads together ([`order`]): with
//! `threads_per_core = 2` guest core `k` (vCPUs `2k` and `2k + 1`) runs on the two threads of one
//! host core, as the guest's scheduler expects of SMT siblings; with 1, each vCPU takes the
//! first thread of a core of its own if that leaves at least one whole core for the host.
//! Larger guests keep the compact allocation, using SMT threads to leave host cores free.
//! On a 6-core, 12-thread host, a 4-vCPU guest with 1 thread per core therefore uses four
//! physical cores rather than sharing two cores' execution resources.
//!
//! A VM started from a script `reims-ctl command` printed is not pinned.
//!
//! **`confine = "llc"`** (`[cpu] confine` in `vm.toml`, off by default) keeps the whole QEMU
//! process within the last-level caches its vCPUs use. With `pin = "auto"` the vCPUs are first
//! chosen from one L3 domain alone (an AMD CCX), the last that holds them and still leaves one of
//! its cores free, as `auto` leaves one of the host's; then every other thread of QEMU (the main
//! loop, the library's FIFO, display, window and runtime threads, the driver's) is given the
//! CPUs of the domains the vCPUs use. The library starts its threads with the main thread's
//! affinity, so threads made later stay there too. Measured on the Ryzen 5 3600 with a 12-job
//! build on the host (docs/qemu.md, 2026-10-13): QEMU alone on one CCX held 74 to 78 presents a
//! second, where pinned vCPUs with QEMU's other threads anywhere held 27 to 39; the build's
//! sharing of cores and caches cost the guest, not CPU time. It confines QEMU, not the rest of
//! the host: work the host runs on the same domain still shares it. Where the vCPUs span every
//! domain, nothing changes, and the log says so.
//!
//! **`auto` is the default on Linux** (`vm::DEFAULT_PIN`). Measured on a Ryzen 5 3600 with an
//! RX 7600 (RADV, Linux 7.3) and an 8-vCPU macOS 26 guest at 100 Hz, while a 12-job release
//! build ran on the host, the vCPUs alternately unpinned and pinned in one boot, two rounds each
//! (2026-10-13): unpinned, the guest presented 4 to 46 frames a second, with stalls of 420 to
//! 870 ms several times a run; pinned, 27 to 39, none longer than 252 ms. With the host idle both
//! present at the screen's rate. A VM that names `pin = "off"` keeps it.

use crate::qmp;
use anyhow::{Context, Result, bail};
use std::collections::BTreeMap;
use std::path::Path;

/// The `pin` setting.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Pin {
    Off,
    /// Host cores from the last, spreading single-thread guest cores when there is room,
    /// otherwise using SMT capacity, and always leaving at least one whole host core.
    Auto,
    /// These host CPUs, one per vCPU.
    List(Vec<u32>),
}

impl Pin {
    pub fn parse(s: &str) -> Result<Pin> {
        let s = s.trim();
        match s {
            "" | "off" => Ok(Pin::Off),
            "auto" => Ok(Pin::Auto),
            _ => {
                let cpus = parse_list(s)
                    .with_context(|| format!("pin: off, auto or a CPU list, not {s:?}"))?;
                let mut seen = cpus.clone();
                seen.sort_unstable();
                seen.dedup();
                if seen.len() != cpus.len() {
                    bail!("pin: {s} names a CPU twice");
                }
                Ok(Pin::List(cpus))
            }
        }
    }
}

/// A CPU list as the kernel writes one: `0-3,8,10-11`.
pub fn parse_list(s: &str) -> Result<Vec<u32>> {
    let mut out = Vec::new();
    for part in s.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        match part.split_once('-') {
            Some((a, b)) => {
                let (a, b): (u32, u32) = (a.trim().parse()?, b.trim().parse()?);
                if a > b {
                    bail!("{part}: the range runs backwards");
                }
                out.extend(a..=b);
            }
            None => out.push(part.parse()?),
        }
    }
    if out.is_empty() {
        bail!("no CPUs");
    }
    Ok(out)
}

/// The `confine` setting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Confine {
    Off,
    /// QEMU's threads within the L3 domains of its vCPUs (module doc).
    Llc,
}

impl Confine {
    pub fn parse(s: &str) -> Result<Confine> {
        match s.trim() {
            "" | "off" => Ok(Confine::Off),
            "llc" => Ok(Confine::Llc),
            other => bail!("confine: off or llc, not {other:?}"),
        }
    }
}

/// The host's last-level cache domains: the CPUs sharing each L3, from sysfs, in the order of
/// their first CPU; empty where the host shows none.
pub fn llc_domains() -> Vec<Vec<u32>> {
    llc_domains_in(Path::new("/sys/devices/system/cpu"))
}

fn llc_domains_in(root: &Path) -> Vec<Vec<u32>> {
    let mut domains: Vec<Vec<u32>> = Vec::new();
    let cpus = std::fs::read_dir(root).into_iter().flatten().flatten();
    for e in cpus {
        let Some(name) = e.file_name().into_string().ok() else {
            continue;
        };
        if name
            .strip_prefix("cpu")
            .and_then(|n| n.parse::<u32>().ok())
            .is_none()
        {
            continue;
        }
        let caches = std::fs::read_dir(e.path().join("cache"))
            .into_iter()
            .flatten()
            .flatten();
        for c in caches {
            let level = std::fs::read_to_string(c.path().join("level")).ok();
            if level.as_deref().map(str::trim) != Some("3") {
                continue;
            }
            let Some(list) = std::fs::read_to_string(c.path().join("shared_cpu_list")).ok() else {
                continue;
            };
            let Ok(mut shared) = parse_list(&list) else {
                continue;
            };
            shared.sort_unstable();
            if !domains.contains(&shared) {
                domains.push(shared);
            }
        }
    }
    domains.sort_by_key(|d| d[0]);
    domains
}

/// The vCPUs' CPUs as `auto` chooses them within one of `domains`: the last domain that holds
/// `vcpus` and still leaves one of its cores free. `None` where none does.
fn auto_within_llc(
    topology: &[HostCpu],
    domains: &[Vec<u32>],
    vcpus: u32,
    threads_per_core: u32,
) -> Option<Vec<u32>> {
    domains.iter().rev().find_map(|domain| {
        let mine: Vec<HostCpu> = (topology.iter())
            .filter(|h| domain.contains(&h.cpu))
            .copied()
            .collect();
        choose(&Pin::Auto, &mine, vcpus, threads_per_core).ok()
    })
}

/// The CPUs of every domain that holds one of `chosen`, in order; empty where no domain does.
fn confined(chosen: &[u32], domains: &[Vec<u32>]) -> Vec<u32> {
    let mut out: Vec<u32> = (domains.iter())
        .filter(|d| chosen.iter().any(|c| d.contains(c)))
        .flatten()
        .copied()
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}

/// One online host CPU, and the core it is a thread of.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HostCpu {
    pub cpu: u32,
    /// The core: its package, die and core ids.
    pub core: (u32, u32, u32),
}

/// The host's online CPUs, from sysfs; empty where it shows none.
pub fn host_topology() -> Vec<HostCpu> {
    topology_in(Path::new("/sys/devices/system/cpu"))
}

fn topology_in(root: &Path) -> Vec<HostCpu> {
    let read = |p: &Path| -> Option<u32> { std::fs::read_to_string(p).ok()?.trim().parse().ok() };
    let mut out: Vec<HostCpu> = std::fs::read_dir(root)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let cpu: u32 = name.strip_prefix("cpu")?.parse().ok()?;
            let dir = e.path();
            // cpu0 often has no `online`: it cannot go offline.
            if read(&dir.join("online")) == Some(0) {
                return None;
            }
            let t = dir.join("topology");
            let core = (
                read(&t.join("physical_package_id")).unwrap_or(0),
                read(&t.join("die_id")).unwrap_or(0),
                read(&t.join("core_id"))?,
            );
            Some(HostCpu { cpu, core })
        })
        .collect();
    out.sort_by_key(|c| c.cpu);
    out
}

/// The host's cores, each its threads' CPUs in order, the cores in the order of their first CPU.
fn cores(topology: &[HostCpu]) -> Vec<Vec<u32>> {
    let mut by_core: BTreeMap<(u32, u32, u32), Vec<u32>> = BTreeMap::new();
    for c in topology {
        by_core.entry(c.core).or_default().push(c.cpu);
    }
    let mut cores: Vec<Vec<u32>> = by_core.into_values().collect();
    for c in &mut cores {
        c.sort_unstable();
    }
    cores.sort_by_key(|c| c[0]);
    cores
}

/// `cores`' CPUs in the order vCPUs take them (module doc): each core's threads together where
/// the guest's cores have 2, else the cores' first threads, then their second, and so on.
fn order(cores: &[Vec<u32>], threads_per_core: u32) -> Vec<u32> {
    if threads_per_core > 1 {
        return cores.iter().flatten().copied().collect();
    }
    let most = cores.iter().map(Vec::len).max().unwrap_or(0);
    (0..most)
        .flat_map(|i| cores.iter().filter_map(move |c| c.get(i).copied()))
        .collect()
}

/// The host CPU of each vCPU, by index; empty for [`Pin::Off`].
pub fn choose(
    pin: &Pin,
    topology: &[HostCpu],
    vcpus: u32,
    threads_per_core: u32,
) -> Result<Vec<u32>> {
    let n = vcpus as usize;
    match pin {
        Pin::Off => Ok(Vec::new()),
        Pin::Auto => {
            let all = cores(topology);
            if all.is_empty() {
                bail!("pin auto: the host shows no CPU topology; give a CPU list");
            }
            // A small non-SMT guest should get independent execution resources. Counting
            // both host threads here would fit four vCPUs onto only two physical cores.
            // Keep the compact SMT fallback when spreading would take every host core.
            let spread = threads_per_core == 1 && n < all.len();
            let mut taken = Vec::new();
            let mut held = 0;
            for core in all.iter().rev() {
                if held >= n {
                    break;
                }
                held += if spread { 1 } else { core.len() };
                taken.push(core.clone());
            }
            if held < n || taken.len() == all.len() {
                bail!(
                    "pin auto: {vcpus} vCPUs leave none of this host's {} cores ({} CPUs) to \
                     QEMU's own threads; give fewer vCPUs or a CPU list",
                    all.len(),
                    topology.len()
                );
            }
            taken.reverse();
            let mut out = order(&taken, threads_per_core);
            out.truncate(n);
            Ok(out)
        }
        Pin::List(cpus) => {
            if cpus.len() != n {
                bail!(
                    "pin: {} CPUs for {vcpus} vCPUs; name one per vCPU",
                    cpus.len()
                );
            }
            if topology.is_empty() {
                return Ok(cpus.clone());
            }
            if let Some(c) = cpus.iter().find(|c| !topology.iter().any(|h| h.cpu == **c)) {
                bail!("pin: this host has no online CPU {c}");
            }
            let mine: Vec<HostCpu> = topology
                .iter()
                .filter(|h| cpus.contains(&h.cpu))
                .copied()
                .collect();
            Ok(order(&cores(&mine), threads_per_core))
        }
    }
}

/// Pins the running VM's vCPU threads as `cpu` says, over its QMP socket, and with `confine =
/// "llc"` QEMU's other threads (those of process `pid`) to the vCPUs' L3 domains: what was done,
/// for QEMU's log.
pub fn apply(socket: &Path, cpu: &crate::vm::Cpu, vcpus: u32, pid: Option<i32>) -> Result<String> {
    let pin = Pin::parse(&cpu.pin)?;
    if pin == Pin::Off {
        return Ok("vCPUs not pinned".into());
    }
    if !cfg!(target_os = "linux") {
        bail!("vCPU pinning is for Linux hosts");
    }
    let confine = Confine::parse(&cpu.confine)?;
    let topology = host_topology();
    let domains = match confine {
        Confine::Llc => llc_domains(),
        Confine::Off => Vec::new(),
    };
    let within = match (&pin, confine) {
        (Pin::Auto, Confine::Llc) => {
            auto_within_llc(&topology, &domains, vcpus, cpu.threads_per_core)
        }
        _ => None,
    };
    let chosen = match within {
        Some(chosen) => chosen,
        None => choose(&pin, &topology, vcpus, cpu.threads_per_core)?,
    };
    let threads = qmp::run(socket, "query-cpus-fast", None)?;
    let threads = threads
        .as_array()
        .context("query-cpus-fast: not a list")?
        .iter()
        .filter_map(|c| Some((c.get("cpu-index")?.as_u64()?, c.get("thread-id")?.as_i64()?)))
        .collect::<Vec<_>>();
    let mut done = Vec::new();
    let mut vcpu_tids = Vec::new();
    for (index, tid) in threads {
        let Some(&host) = chosen.get(index as usize) else {
            continue;
        };
        set_affinity(tid as i32, &[host])
            .with_context(|| format!("pin vCPU {index} (thread {tid}) to CPU {host}"))?;
        vcpu_tids.push(tid);
        done.push(format!("{index}->{host}"));
    }
    let mut line = format!("vCPUs pinned to host CPUs: {}", done.join(" "));
    if confine == Confine::Llc {
        line.push_str(&confine_others(
            pid, &vcpu_tids, &chosen, &domains, &topology,
        ));
    }
    Ok(line)
}

/// Gives every thread of QEMU's process `pid` but its vCPUs the CPUs of the vCPUs' L3 domains
/// (module doc): what was done, for the log line.
fn confine_others(
    pid: Option<i32>,
    vcpu_tids: &[i64],
    chosen: &[u32],
    domains: &[Vec<u32>],
    topology: &[HostCpu],
) -> String {
    let Some(pid) = pid else {
        return "; QEMU's other threads not confined: no pid".into();
    };
    let set = confined(chosen, domains);
    if set.is_empty() {
        return "; QEMU's other threads not confined: the host shows no L3 domains".into();
    }
    if set.len() >= topology.len() {
        return "; QEMU's other threads not confined: the vCPUs use every L3 domain".into();
    }
    let tasks = std::fs::read_dir(format!("/proc/{pid}/task"));
    let tids = tasks.into_iter().flatten().flatten().filter_map(|e| {
        let tid: i64 = e.file_name().into_string().ok()?.parse().ok()?;
        (!vcpu_tids.contains(&tid)).then_some(tid)
    });
    let (mut moved, mut failed) = (0, 0);
    for tid in tids {
        match set_affinity(tid as i32, &set) {
            Ok(()) => moved += 1,
            Err(_) => failed += 1,
        }
    }
    let list = set.iter().map(u32::to_string).collect::<Vec<_>>().join(",");
    format!("; {moved} other threads confined to CPUs {list} ({failed} not)")
}

#[cfg(target_os = "linux")]
fn set_affinity(tid: i32, cpus: &[u32]) -> Result<()> {
    // SAFETY: a zeroed cpu_set_t is the empty set; CPU_SET writes within it for any CPU below
    // CPU_SETSIZE, which the check keeps to; sched_setaffinity only reads it.
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        for &cpu in cpus {
            if cpu as usize >= libc::CPU_SETSIZE as usize {
                bail!("CPU {cpu} is past CPU_SETSIZE");
            }
            libc::CPU_SET(cpu as usize, &mut set);
        }
        if libc::sched_setaffinity(tid, std::mem::size_of::<libc::cpu_set_t>(), &set) != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn set_affinity(_tid: i32, _cpus: &[u32]) -> Result<()> {
    bail!("vCPU pinning is for Linux hosts")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A Ryzen 5 3600 as Linux numbers it: cores 0 to 5 are CPUs 0 to 5, their second threads
    /// 6 to 11.
    fn ryzen_3600() -> Vec<HostCpu> {
        (0..12)
            .map(|cpu| HostCpu {
                cpu,
                core: (0, 0, cpu % 6),
            })
            .collect()
    }

    #[test]
    fn parses_settings() {
        assert_eq!(Pin::parse("").unwrap(), Pin::Off);
        assert_eq!(Pin::parse(" off ").unwrap(), Pin::Off);
        assert_eq!(Pin::parse("auto").unwrap(), Pin::Auto);
        assert_eq!(
            Pin::parse("2-5, 8-11").unwrap(),
            Pin::List(vec![2, 3, 4, 5, 8, 9, 10, 11])
        );
        for bad in ["on", "5-2", "1,1", "a-b", ","] {
            assert!(Pin::parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn auto_gives_smt_guests_whole_host_cores_from_the_last() {
        let got = choose(&Pin::Auto, &ryzen_3600(), 8, 2).unwrap();
        assert_eq!(got, vec![2, 8, 3, 9, 4, 10, 5, 11]);
    }

    #[test]
    fn auto_spreads_one_thread_guests_over_cores_first() {
        let got = choose(&Pin::Auto, &ryzen_3600(), 8, 1).unwrap();
        assert_eq!(got, vec![2, 3, 4, 5, 8, 9, 10, 11]);
        assert_eq!(
            choose(&Pin::Auto, &ryzen_3600(), 3, 1).unwrap(),
            vec![3, 4, 5]
        );
    }

    #[test]
    fn four_vcpus_get_four_physical_cores_unless_the_guest_requests_smt() {
        let topology = ryzen_3600();
        assert_eq!(
            choose(&Pin::Auto, &topology, 4, 1).unwrap(),
            vec![2, 3, 4, 5]
        );
        assert_eq!(
            choose(&Pin::Auto, &topology, 4, 2).unwrap(),
            vec![4, 10, 5, 11]
        );
    }

    #[test]
    fn spreading_keeps_a_whole_core_for_the_host_and_falls_back_to_smt() {
        let topology = ryzen_3600();
        assert_eq!(
            choose(&Pin::Auto, &topology, 5, 1).unwrap(),
            vec![1, 2, 3, 4, 5]
        );
        assert_eq!(
            choose(&Pin::Auto, &topology, 6, 1).unwrap(),
            vec![3, 4, 5, 9, 10, 11]
        );
        assert_eq!(
            choose(&Pin::Auto, &topology, 10, 1).unwrap(),
            vec![1, 2, 3, 4, 5, 7, 8, 9, 10, 11]
        );
    }

    #[test]
    fn spreading_uses_topology_with_offline_siblings_and_sparse_cpu_ids() {
        let topology: Vec<HostCpu> = ryzen_3600()
            .into_iter()
            .filter(|c| !matches!(c.cpu, 1 | 7 | 10))
            .collect();
        assert_eq!(
            choose(&Pin::Auto, &topology, 4, 1).unwrap(),
            vec![2, 3, 4, 5]
        );
        let no_smt: Vec<HostCpu> = ryzen_3600().into_iter().filter(|c| c.cpu < 6).collect();
        assert_eq!(choose(&Pin::Auto, &no_smt, 4, 1).unwrap(), vec![2, 3, 4, 5]);
        assert!(choose(&Pin::Auto, &no_smt, 6, 1).is_err());
    }

    #[test]
    fn auto_leaves_the_host_a_core() {
        assert!(choose(&Pin::Auto, &ryzen_3600(), 11, 1).is_err());
        assert_eq!(choose(&Pin::Auto, &ryzen_3600(), 10, 2).unwrap().len(), 10);
        assert!(choose(&Pin::Auto, &[], 2, 1).is_err());
    }

    /// The 3600's two CCXs: cores 0 to 2 and their threads, then cores 3 to 5 and theirs.
    fn ccxs() -> Vec<Vec<u32>> {
        vec![vec![0, 1, 2, 6, 7, 8], vec![3, 4, 5, 9, 10, 11]]
    }

    #[test]
    fn parses_confine() {
        assert_eq!(Confine::parse("").unwrap(), Confine::Off);
        assert_eq!(Confine::parse(" llc ").unwrap(), Confine::Llc);
        assert!(Confine::parse("ccx").is_err());
    }

    /// `auto` within an L3 takes the last CCX that holds the vCPUs and leaves one of its cores
    #[test]
    fn auto_within_llc_keeps_the_vcpus_in_one_ccx() {
        let t = ryzen_3600();
        assert_eq!(auto_within_llc(&t, &ccxs(), 4, 2), Some(vec![4, 10, 5, 11]));
        assert_eq!(auto_within_llc(&t, &ccxs(), 2, 1), Some(vec![4, 5]));
        // Six vCPUs would take a whole CCX: none fits.
        assert_eq!(auto_within_llc(&t, &ccxs(), 6, 2), None);
        assert_eq!(auto_within_llc(&t, &[], 2, 1), None);
    }

    #[test]
    fn confining_takes_every_domain_the_vcpus_touch() {
        assert_eq!(confined(&[4, 10], &ccxs()), vec![3, 4, 5, 9, 10, 11]);
        assert_eq!(confined(&[2, 3], &ccxs()).len(), 12);
        assert!(confined(&[2], &[]).is_empty());
    }

    #[test]
    fn reads_sysfs_llc_domains() {
        let dir = std::env::temp_dir().join(format!("reims-ctl-llc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for (cpu, l3) in [(0, "0-1"), (1, "0-1"), (2, "2-3"), (3, "2-3")] {
            for (index, level, list) in [(0, "1", format!("{cpu}")), (3, "3", l3.to_owned())] {
                let c = dir.join(format!("cpu{cpu}/cache/index{index}"));
                std::fs::create_dir_all(&c).unwrap();
                std::fs::write(c.join("level"), format!("{level}\n")).unwrap();
                std::fs::write(c.join("shared_cpu_list"), format!("{list}\n")).unwrap();
            }
        }
        std::fs::create_dir_all(dir.join("cpufreq")).unwrap();
        let got = llc_domains_in(&dir);
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(got, vec![vec![0, 1], vec![2, 3]]);
    }

    #[test]
    fn a_list_is_ordered_by_core() {
        let list = Pin::parse("2-5,8-11").unwrap();
        assert_eq!(
            choose(&list, &ryzen_3600(), 8, 2).unwrap(),
            vec![2, 8, 3, 9, 4, 10, 5, 11]
        );
        assert!(choose(&list, &ryzen_3600(), 4, 2).is_err(), "one per vCPU");
        assert!(choose(&Pin::parse("2,40").unwrap(), &ryzen_3600(), 2, 1).is_err());
        assert_eq!(
            choose(&Pin::parse("7,3").unwrap(), &[], 2, 1).unwrap(),
            vec![7, 3],
            "without a topology the list's order stands"
        );
    }

    /// A thread's affinity is set to the one CPU, as `apply` sets a vCPU's
    #[cfg(target_os = "linux")]
    #[test]
    fn sets_a_threads_affinity() {
        std::thread::spawn(|| {
            // SAFETY: gettid has no preconditions; the set is read back into a zeroed one.
            let tid = unsafe { libc::gettid() };
            let mut set: libc::cpu_set_t = unsafe { std::mem::zeroed() };
            let size = std::mem::size_of::<libc::cpu_set_t>();
            assert_eq!(unsafe { libc::sched_getaffinity(tid, size, &mut set) }, 0);
            let allowed = (0..libc::CPU_SETSIZE as usize)
                .find(|c| unsafe { libc::CPU_ISSET(*c, &set) })
                .unwrap();
            set_affinity(tid, &[allowed as u32]).unwrap();
            assert_eq!(unsafe { libc::sched_getaffinity(tid, size, &mut set) }, 0);
            assert_eq!(unsafe { libc::CPU_COUNT(&set) }, 1);
            assert!(unsafe { libc::CPU_ISSET(allowed, &set) });
        })
        .join()
        .unwrap();
    }

    #[test]
    fn reads_sysfs_topology() {
        let dir = std::env::temp_dir().join(format!("reims-ctl-pin-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for (cpu, core, online) in [(0, 0, None), (1, 1, Some("1")), (2, 0, Some("0"))] {
            let t = dir.join(format!("cpu{cpu}/topology"));
            std::fs::create_dir_all(&t).unwrap();
            std::fs::write(t.join("core_id"), format!("{core}\n")).unwrap();
            std::fs::write(t.join("physical_package_id"), "0\n").unwrap();
            if let Some(o) = online {
                std::fs::write(dir.join(format!("cpu{cpu}/online")), o).unwrap();
            }
        }
        std::fs::create_dir_all(dir.join("cpufreq")).unwrap();
        let got = topology_in(&dir);
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(
            got,
            vec![
                HostCpu {
                    cpu: 0,
                    core: (0, 0, 0)
                },
                HostCpu {
                    cpu: 1,
                    core: (0, 0, 1)
                },
            ]
        );
    }
}
