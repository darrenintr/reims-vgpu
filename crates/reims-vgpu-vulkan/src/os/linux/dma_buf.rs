//! Dma-bufs of the pages behind a range the backing table resolves ([`super::files`]), for a
//! device that imports no shared memory as host memory.
//!
//! RADV imports host memory as an amdgpu userptr, which Mesa asks for with
//! `AMDGPU_GEM_USERPTR_ANONONLY`, so the kernel refuses every page with a file behind it: the
//! guest's RAM (QEMU's `memory-backend-memfd`) and every range the library makes a view of are
//! shared mappings of memfds, and none of them imports. `/dev/udmabuf` makes a dma-buf of those
//! same memfd pages (`UDMABUF_CREATE_LIST`), and RADV imports that
//! (`VK_EXT_external_memory_dma_buf`). Measured on a Radeon 780M (RADV of Mesa 26.0.8, Linux 7.0,
//! 2026-10-11), with no runtime between:
//! - a sealed memfd mapped shared: `vkGetMemoryHostPointerPropertiesEXT` answers
//!   `VK_ERROR_UNKNOWN`; private anonymous memory imports;
//! - a udmabuf of two pieces of that memfd 6 MiB apart imports as types 5 and 10 (host-visible,
//!   coherent and cached); the GPU read the bytes the CPU wrote through the shared mapping, and a
//!   fill across both pieces reached the file's pages, seen through that mapping after the fence,
//!   with no map, flush or invalidate of the import;
//! - a udmabuf of one piece takes about 9 ms per 64 MiB, the kernel pinning its pages; one of
//!   16,201 scattered 4 KiB pieces (with the limits below raised) 19 ms the first time and 3 ms
//!   after, and RADV imports it in about 50 µs; two over the same pages both import.
//!
//! **Chunks.** udmabuf takes at most `list_limit` pieces (1024 by default) and `size_limit_mb` MiB
//! (64) in one dma-buf, parameters of the `udmabuf` module only root can raise, and the library
//! asks no one to. An import of a macOS 26 guest's boot took up to 16,201 runs of file pages
//! (`docs/qemu.md`, "Linux hosts: AMD GPUs and udmabuf"), so a window is cut into chunks, each a
//! dma-buf within the limits the host has now ([`limits`]), cut only where the runtime can bind
//! the next ([`chunks`]); the runtime binds them in order behind one buffer.
//!
//! **Costs** (measured on the same host, 2026-10-12): a udmabuf pins every page it takes,
//! allocating those never touched (64 MiB of untouched memory in 18 ms), and keeps showing a page
//! the file later drops. Each import the device holds adds about 0.55 µs to every submission
//! (RADV passes each in a list on every submit): 1,024 live imports cost 642 µs a submit, 4,096
//! about 2.5 ms. So the fewest chunks are made, and an import goes with its buffer.
//!
//! What udmabuf takes besides ([`Refusal`]): memfds sealed against shrinking (`F_SEAL_SHRINK`,
//! which QEMU's `seal=on`, its default, sets, and [`super::vm::memfd`] sets on the library's own),
//! at page offsets. The device node is the `kvm` group's on Ubuntu.
//!
//! Pieces of the block of zeros read-only aliases share ([`super::import::allocate_zeros`]) are
//! made of a file of zeros of the import's own instead, as an import's twin maps private zeros
//! there: the GPU's writes stay in it and never reach the block.
//!
//! **One dma-buf for the same pages** ([`Leases`]). A chunk of exactly the runs of file pages a
//! dma-buf already made is of (the same files, offsets and lengths, in order), while some source
//! still holds that dma-buf, is answered with that dma-buf again (a new descriptor of it) rather
//! than a new one: the runtime then imports it once for every buffer over those pages
//! (metal2vulkan's shared imports), where each import would add its cost to every submission
//! above, and udmabuf pins the pages once. A source holds each dma-buf it was answered with until
//! the runtime drops the source, which it does once every import of the source's went; the last
//! holder to go lets the dma-buf go. The pages are the same either way: a file page a dma-buf
//! pins stays what both show, as no file of the library's has pages dropped while it is mapped.
//!
//! **Windows** (`REIMS_VGPU_DMA_BUF_WINDOWS`, off by default). Each import adds its cost to every
//! submission (above), and the shared dma-bufs serve only a chunk of exactly the same pages. With
//! windows on, a stretch of a request that one run of a file's pages covers, from a granule
//! boundary of the request, is answered as an offset into a dma-buf of the whole aligned window of
//! that file it lies in ([`windowed`]): a window of 64 MiB, or udmabuf's size limit where that is
//! lower ([`WINDOW_SPAN`]), cut at multiples of that size from the file's start, clipped to the
//! file's end. Every buffer
//! over pages of one window then shares one import of it (metal2vulkan's chunks at offsets,
//! `DmaBufRequest::offset_alignment`), so an 8 GiB guest's RAM is at most 128 imports, whatever
//! the guest holds. The cost: udmabuf pins every page of a window, allocating those the guest
//! never touched, for as long as any buffer over the window lives, and each window's import takes
//! its whole span of the GPU's memory for system pages (GTT). So the windows held span at most a
//! budget together, 4 GiB unless `REIMS_VGPU_DMA_BUF_WINDOW_BUDGET_MB` names another
//! ([`within_budget`]); past it, stretches are cut into pieces as without windows. Without the
//! cap, Linux 7.3's unbounded `size_limit_mb` made one window of a 12 GiB guest's whole RAM, and
//! with an RX 7600's 15 GiB of GTT every submission then failed for memory (`Not enough memory
//! for command submission`, 2026-10-15) and the device was lost. A stretch whose offset in its
//! window misses the request's offset alignment, or a request that takes no offsets (a
//! texture's), is cut and made as before. The setting is read once, at the first request.
//!
//! **Concurrent misses.** `REIMS_VGPU_DMA_BUF_RACE_FIX` defaults to on. With `0`, `false`, `no`
//! or `off` (case and surrounding spaces ignored), a creator that finds another creator's
//! winning entry after its kernel call keeps its own dma-buf, as before the race fix. Ordinary
//! cache hits still share. The setting is read once, at the first request, and requires a VM
//! process restart to change.

use std::collections::BTreeMap;
use std::fmt;
use std::io;
use std::os::fd::{AsFd as _, AsRawFd as _, FromRawFd as _, OwnedFd};
use std::sync::{Arc, OnceLock};

use metal2vulkan_runtime::DmaBufChunk;
use reims_vgpu_core::sync::Mutex;

use super::files::{self, Backing};
use super::vm;

/// `UDMABUF_FLAGS_CLOEXEC`.
const FLAGS_CLOEXEC: u32 = 0x01;

/// `UDMABUF_CREATE_LIST`, `_IOW('u', 0x43, struct udmabuf_create_list)`: the header's 8 bytes.
const CREATE_LIST: u64 = (1 << 30) | (8 << 16) | ((b'u' as u64) << 8) | 0x43;

/// udmabuf's defaults, taken where the system shows no parameters.
const DEFAULT_LIST_LIMIT: u64 = 1024;
const DEFAULT_SIZE_LIMIT_MB: u64 = 64;

/// The most bytes a window spans (module docs), udmabuf's default size limit, whatever limit the
/// host's module has: a kernel whose limit is unbounded (`size_limit_mb` 2147483647, as on Linux
/// 7.3's Ubuntu build, measured 2026-10-15) would otherwise make one window of a whole file.
const WINDOW_SPAN: u64 = DEFAULT_SIZE_LIMIT_MB << 20;

/// The most bytes the windows some source holds may span together (module docs), unless
/// `REIMS_VGPU_DMA_BUF_WINDOW_BUDGET_MB` says otherwise.
const DEFAULT_WINDOW_BUDGET_MB: u64 = 4096;

/// [`DEFAULT_WINDOW_BUDGET_MB`], or the MiB `REIMS_VGPU_DMA_BUF_WINDOW_BUDGET_MB` names, in
/// bytes, read once per process.
fn window_budget() -> u64 {
    static BUDGET: OnceLock<u64> = OnceLock::new();
    *BUDGET.get_or_init(|| {
        window_budget_value(
            std::env::var("REIMS_VGPU_DMA_BUF_WINDOW_BUDGET_MB")
                .ok()
                .as_deref(),
        )
    })
}

/// The budget a value of `REIMS_VGPU_DMA_BUF_WINDOW_BUDGET_MB` sets: a number of MiB (surrounding
/// spaces ignored), else the default.
fn window_budget_value(value: Option<&str>) -> u64 {
    value
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_WINDOW_BUDGET_MB)
        .saturating_mul(1 << 20)
}

/// Whether concurrent creators join the cache's winner, read once per process.
fn race_fix() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        race_fix_value(std::env::var("REIMS_VGPU_DMA_BUF_RACE_FIX").ok().as_deref())
    })
}

/// Whether requests are answered from windows of their files (module docs), read once per
/// process: off unless `REIMS_VGPU_DMA_BUF_WINDOWS` is `1`, `true`, `yes` or `on`.
fn windows() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED
        .get_or_init(|| windows_value(std::env::var("REIMS_VGPU_DMA_BUF_WINDOWS").ok().as_deref()))
}

/// The default-off switch of [`windows`] (case and surrounding spaces ignored).
fn windows_value(value: Option<&str>) -> bool {
    matches!(
        value.map(str::trim).map(str::to_ascii_lowercase).as_deref(),
        Some("1" | "true" | "yes" | "on")
    )
}

/// The default-on switch, using the same false values as `REIMS_VGPU_RDNA3`.
fn race_fix_value(value: Option<&str>) -> bool {
    !matches!(
        value.map(str::trim).map(str::to_ascii_lowercase).as_deref(),
        Some("0" | "false" | "no" | "off")
    )
}

/// Why no dma-bufs were made of a range.
#[derive(Debug)]
pub(crate) enum Refusal {
    /// A byte of the range has no file behind it in the backing table.
    Unresolved,
    /// `/dev/udmabuf` would not open: absent (the module not loaded), or not the process's to
    /// open (Ubuntu gives it to the `kvm` group).
    Device(Arc<io::Error>),
    /// The range cannot be cut into chunks within udmabuf's limits at the runtime's granule: a
    /// granule holds more pieces or bytes than one dma-buf may, or the runtime binds no chunks
    /// (its granule is the whole window) and the window is past the limits.
    Limit {
        pieces: u64,
        bytes: u64,
        granule: u64,
        limits: Limits,
    },
    /// The kernel refused the call otherwise (a file it takes no pages of, unsealed or not a
    /// memfd, or no memory to pin the pages).
    Kernel(io::Error),
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Refusal::Unresolved => write!(f, "a byte of the range has no file behind it"),
            Refusal::Device(e) => write!(
                f,
                "/dev/udmabuf would not open ({e}): load the udmabuf module, and put the user in \
                 the group that owns the node (kvm on Ubuntu)"
            ),
            Refusal::Limit {
                pieces,
                bytes,
                granule,
                limits,
            } => write!(
                f,
                "{pieces} pieces of {bytes:#x} bytes cut at {granule:#x} do not fit udmabuf's \
                 list_limit {} and size_limit_mb {}",
                limits.pieces,
                limits.bytes >> 20,
            ),
            Refusal::Kernel(e) => write!(f, "UDMABUF_CREATE_LIST failed: {e}"),
        }
    }
}

/// What one dma-buf may hold: pieces, and bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Limits {
    pub(crate) pieces: u64,
    pub(crate) bytes: u64,
}

/// udmabuf's limits as the host has them now, its defaults where it shows none.
fn limits() -> Limits {
    let parameter = |name: &str, default: u64| {
        let path = format!("/sys/module/udmabuf/parameters/{name}");
        (std::fs::read_to_string(path).ok())
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(default)
    };
    Limits {
        pieces: parameter("list_limit", DEFAULT_LIST_LIMIT),
        bytes: parameter("size_limit_mb", DEFAULT_SIZE_LIMIT_MB) << 20,
    }
}

/// `/dev/udmabuf`, opened once for the process.
fn device() -> Result<&'static OwnedFd, Refusal> {
    static DEVICE: OnceLock<Result<OwnedFd, Arc<io::Error>>> = OnceLock::new();
    let opened = DEVICE.get_or_init(|| {
        // SAFETY: a NUL-terminated path.
        let raw = unsafe { libc::open(c"/dev/udmabuf".as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
        if raw < 0 {
            return Err(Arc::new(io::Error::last_os_error()));
        }
        // SAFETY: a descriptor just opened, owned from here on.
        Ok(unsafe { OwnedFd::from_raw_fd(raw) })
    });
    opened.as_ref().map_err(|e| Refusal::Device(Arc::clone(e)))
}

/// What a dma-buf is of ([`MADE`]): each run of its pages, as its file's identity (the address of
/// the backing table's shared descriptor, which [`Made::_files`] keeps alive, so no other file
/// takes it), offset and length, in order.
type Pages = Vec<(usize, u64, u64)>;

/// A dma-buf some source holds ([`Leases`]), by the pages it is of.
struct Made {
    /// A descriptor of the dma-buf, from which each source answered with it gets its own.
    fd: OwnedFd,
    /// The files of its runs, kept so their identities stay theirs.
    _files: Vec<Arc<OwnedFd>>,
    /// The sources holding it.
    holders: usize,
    /// Whether it is a window's, counted in [`WINDOW_BYTES`] then.
    window: bool,
}

/// The bytes the windows in [`MADE`] span together, which [`window_budget`] bounds.
static WINDOW_BYTES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Every dma-buf some source holds, by the pages it is of (module docs).
static MADE: Mutex<BTreeMap<Pages, Made>> = Mutex::new(BTreeMap::new());

/// The dma-bufs one source was answered with, which it holds until it drops (module docs): each
/// [`MADE`] entry's holder count falls with it, and an entry no source holds any more goes, its
/// descriptor closed.
#[derive(Default)]
pub(crate) struct Leases(Vec<Pages>);

impl Drop for Leases {
    fn drop(&mut self) {
        let mut retired = Vec::new();
        let mut made = MADE.lock();
        for pages in self.0.drain(..) {
            let Some(entry) = made.get_mut(&pages) else {
                continue;
            };
            entry.holders -= 1;
            if entry.holders == 0 {
                if entry.window {
                    let bytes = pages.iter().map(|&(_, _, length)| length).sum();
                    WINDOW_BYTES.fetch_sub(bytes, std::sync::atomic::Ordering::Relaxed);
                }
                retired.push(made.remove(&pages));
            }
        }
        // Closing a dma-buf can unpin its pages. Keep that work outside the shared table lock.
        drop(made);
        drop(retired);
    }
}

impl Leases {
    /// Move `other`'s dma-bufs into these, held as long as these are.
    pub(crate) fn extend(&mut self, mut other: Leases) {
        self.0.append(&mut other.0);
    }
}

/// The pages a chunk of `runs` is of ([`Pages`]).
fn pages_of(runs: &[Run<Arc<OwnedFd>>]) -> Pages {
    (runs.iter())
        .map(|r| (Arc::as_ptr(&r.file).addr(), r.offset, r.length))
        .collect()
}

/// A run of file pages: `length` bytes of `file` from `offset`.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Run<F> {
    file: F,
    offset: u64,
    length: u64,
}

/// `runs`, the window's pages in order, cut into chunks: each at most `limits.pieces` runs and
/// `limits.bytes` bytes, and each but the last a multiple of `granule` long, so every cut lies a
/// multiple of `granule` from the window's start. A chunk takes as much as fits, so the fewest
/// chunks are made; a run is cut where a chunk ends inside it. `None` where a granule does not fit
/// one chunk.
fn chunks<F: Clone>(runs: &[Run<F>], granule: u64, limits: Limits) -> Option<Vec<Vec<Run<F>>>> {
    let length: u64 = runs.iter().map(|r| r.length).sum();
    let mut made = Vec::new();
    // The next byte to place, as a run and an offset into it, and its offset in the window.
    let (mut run, mut into, mut at) = (0, 0, 0);
    while at < length {
        // The farthest the limits reach from `at`.
        let room = limits.bytes.min(length - at);
        let (mut reach, mut pieces, mut r, mut i) = (0, 0, run, into);
        while reach < room && pieces < limits.pieces {
            let take = (runs[r].length - i).min(room - reach);
            reach += take;
            pieces += 1;
            (r, i) = if i + take == runs[r].length {
                (r + 1, 0)
            } else {
                (r, i + take)
            };
        }
        // Cut there, or at the granule before it unless the window ends there.
        let size = if at + reach == length {
            reach
        } else {
            reach - reach % granule
        };
        if size == 0 {
            return None;
        }
        let mut chunk = Vec::new();
        let mut left = size;
        while left > 0 {
            let take = (runs[run].length - into).min(left);
            chunk.push(Run {
                file: runs[run].file.clone(),
                offset: runs[run].offset + into,
                length: take,
            });
            left -= take;
            (run, into) = if into + take == runs[run].length {
                (run + 1, 0)
            } else {
                (run, into + take)
            };
        }
        made.push(chunk);
        at += size;
    }
    Some(made)
}

/// [`chunks`] of `runs`, padded where they are more than one: then they come to the window's
/// length rounded up to `granule`, as the runtime binds each chunk at a multiple of it, and the
/// last ends with a run `pad` makes of the bytes past the window, a file of the import's own, so a
/// GPU write past the window's end reaches nothing else.
fn padded<F: Clone>(
    runs: &mut Vec<Run<F>>,
    granule: u64,
    limits: Limits,
    pad: impl FnOnce(u64) -> Run<F>,
) -> Option<Vec<Vec<Run<F>>>> {
    let cut = chunks(runs, granule, limits)?;
    let length: u64 = runs.iter().map(|r| r.length).sum();
    let past = length.next_multiple_of(granule) - length;
    if cut.len() == 1 || past == 0 {
        return Some(cut);
    }
    runs.push(pad(past));
    chunks(runs, granule, limits)
}

/// One chunk of a request as [`windowed`] plans it.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Planned<F> {
    /// The runs of a dma-buf of their own, in order, as [`chunks`] cuts them.
    Pieces(Vec<Run<F>>),
    /// `length` bytes `offset` bytes into a dma-buf of the whole of `window`.
    Window {
        window: Run<F>,
        offset: u64,
        length: u64,
    },
}

impl<F> Planned<F> {
    fn length(&self) -> u64 {
        match self {
            Planned::Pieces(runs) => runs.iter().map(|r| r.length).sum(),
            Planned::Window { length, .. } => *length,
        }
    }
}

/// `runs`, the request's pages in order, planned as chunks at multiples of `granule` from its
/// start, as [`chunks`] cuts them, but where a stretch from a granule boundary lies in one run
/// and in the window `window_of` gives for its first file byte, at an offset into the window that
/// is a multiple of `alignment`: that stretch is a [`Planned::Window`] chunk, as long as the run
/// and the window allow, cut to a multiple of `granule` unless it ends the request. The rest goes
/// through [`chunks`], granule by granule between windows. `None` where [`chunks`] refuses a
/// stretch.
///
/// `window_of(file, offset)` answers the window of `file` that holds byte `offset`, as its start
/// and length in the file, or `None` where the file takes none.
fn windowed<F: Clone>(
    runs: &[Run<F>],
    granule: u64,
    alignment: u64,
    limits: Limits,
    window_of: impl Fn(&F, u64) -> Option<(u64, u64)>,
) -> Option<Vec<Planned<F>>> {
    let total: u64 = runs.iter().map(|r| r.length).sum();
    let mut planned = Vec::new();
    let mut pending: Vec<Run<F>> = Vec::new();
    // The run the last pending piece came from: the next bytes of the same run join it.
    let mut pending_from = None;
    let flush = |pending: &mut Vec<Run<F>>, planned: &mut Vec<Planned<F>>| -> Option<()> {
        if pending.is_empty() {
            return Some(());
        }
        let cut = chunks(pending, granule, limits)?;
        planned.extend(cut.into_iter().map(Planned::Pieces));
        pending.clear();
        Some(())
    };
    // The next byte to place, as a run and an offset into it, and its offset in the request.
    let (mut r, mut i, mut at) = (0, 0, 0);
    while at < total {
        let run = &runs[r];
        let from = run.offset + i;
        if alignment != 0 && at.is_multiple_of(granule) {
            let window = window_of(&run.file, from).filter(|&(start, length)| {
                start <= from && from < start + length && (from - start).is_multiple_of(alignment)
            });
            if let Some((start, length)) = window {
                let reach = (run.length - i).min(start + length - from);
                let take = if at + reach == total {
                    reach
                } else {
                    reach - reach % granule
                };
                if take > 0 {
                    flush(&mut pending, &mut planned)?;
                    planned.push(Planned::Window {
                        window: Run {
                            file: run.file.clone(),
                            offset: start,
                            length,
                        },
                        offset: from - start,
                        length: take,
                    });
                    at += take;
                    (r, i) = if i + take == run.length {
                        (r + 1, 0)
                    } else {
                        (r, i + take)
                    };
                    continue;
                }
            }
        }
        // Up to the next granule boundary (or the request's end) goes with the pieces.
        let mut step = (granule - at % granule).min(total - at);
        while step > 0 {
            let run = &runs[r];
            let take = (run.length - i).min(step);
            match pending.last_mut() {
                Some(last)
                    if pending_from == Some(r) && last.offset + last.length == run.offset + i =>
                {
                    last.length += take;
                }
                _ => pending.push(Run {
                    file: run.file.clone(),
                    offset: run.offset + i,
                    length: take,
                }),
            }
            pending_from = Some(r);
            step -= take;
            at += take;
            (r, i) = if i + take == run.length {
                (r + 1, 0)
            } else {
                (r, i + take)
            };
        }
    }
    flush(&mut pending, &mut planned)?;
    Some(planned)
}

/// [`windowed`] of `runs`, padded as [`padded`] pads them where it plans several chunks.
fn windowed_padded<F: Clone>(
    runs: &mut Vec<Run<F>>,
    granule: u64,
    alignment: u64,
    limits: Limits,
    window_of: impl Fn(&F, u64) -> Option<(u64, u64)>,
    pad: impl FnOnce(u64) -> Run<F>,
) -> Option<Vec<Planned<F>>> {
    let plan = windowed(runs, granule, alignment, limits, &window_of)?;
    let length: u64 = runs.iter().map(|r| r.length).sum();
    let past = length.next_multiple_of(granule) - length;
    if plan.len() == 1 || past == 0 {
        return Some(plan);
    }
    runs.push(pad(past));
    windowed(runs, granule, alignment, limits, &window_of)
}

/// The window of a file of `size` bytes that holds byte `offset` (module docs): from the multiple
/// of `span` at or below it, `span` bytes or to the file's end, whole pages of `page` bytes.
/// `None` past the file's end.
fn window_in(offset: u64, size: u64, span: u64, page: u64) -> Option<(u64, u64)> {
    let span = span - span % page;
    if span == 0 || offset >= size {
        return None;
    }
    let start = offset - offset % span;
    let end = (start + span).min(size - size % page);
    (end > offset).then(|| (start, end - start))
}

/// The bytes of the file `fd`, where it answers.
fn file_size(fd: &OwnedFd) -> Option<u64> {
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: the descriptor is open for the call, and `fstat` writes the structure.
    if unsafe { libc::fstat(fd.as_raw_fd(), stat.as_mut_ptr()) } != 0 {
        return None;
    }
    // SAFETY: the call succeeded, so it wrote the structure.
    let stat = unsafe { stat.assume_init() };
    u64::try_from(stat.st_size).ok()
}

/// Dma-bufs of the `length` bytes from `first`, whole pages, each resolved through the backing
/// table, which in order are those bytes: one piece for each run of a file's pages that follow on,
/// the block of zeros' pieces from a file of zeros of the import's own, cut into chunks within
/// udmabuf's limits at multiples of `granule` from `first` (module docs). A chunk of the same pages
/// as a dma-buf some source holds is that dma-buf again; `leases` holds each one answered.
///
/// With windows on ([`windows`]) and an `offset_alignment` other than 0, a stretch one run of a
/// file covers is an offset into a dma-buf of its window instead ([`windowed`]).
pub(crate) fn of_pages(
    first: u64,
    length: u64,
    granule: u64,
    offset_alignment: u64,
    leases: &mut Leases,
) -> Result<Vec<DmaBufChunk>, Refusal> {
    let page = vm::page_size();
    debug_assert!(first.is_multiple_of(page) && length.is_multiple_of(page));
    let resolved = files::pieces(first, length).ok_or(Refusal::Unresolved)?;
    let zero_bytes: u64 = (resolved.iter())
        .filter(|p| super::import::zeros(&p.backing.fd))
        .map(|p| p.length)
        .sum();
    let zeros = (zero_bytes > 0).then(|| vm::memfd(zero_bytes));
    let mut zeros_at = 0;
    let mut runs: Vec<Run<Arc<OwnedFd>>> = Vec::new();
    for piece in resolved {
        let backing = match &zeros {
            Some(zeros) if super::import::zeros(&piece.backing.fd) => {
                let backing = Backing {
                    fd: Arc::clone(zeros),
                    offset: zeros_at,
                };
                zeros_at += piece.length;
                backing
            }
            _ => piece.backing,
        };
        match runs.last_mut() {
            Some(last)
                if Arc::ptr_eq(&last.file, &backing.fd)
                    && last.offset + last.length == backing.offset =>
            {
                last.length += piece.length;
            }
            _ => runs.push(Run {
                file: backing.fd,
                offset: backing.offset,
                length: piece.length,
            }),
        }
    }
    let device = device()?;
    let limits = limits();
    let granule = granule.max(page);
    let refused = |runs: &[Run<Arc<OwnedFd>>]| Refusal::Limit {
        pieces: runs.len() as u64,
        bytes: length,
        granule,
        limits,
    };
    let pad = |pad| Run {
        file: vm::memfd(pad),
        offset: 0,
        length: pad,
    };
    let plan = if windows() && offset_alignment != 0 {
        let span = limits.bytes.min(WINDOW_SPAN);
        let window_of = |file: &Arc<OwnedFd>, offset| {
            let (start, length) = window_in(offset, file_size(file)?, span, page)?;
            within_budget(file, start, length, window_budget()).then_some((start, length))
        };
        windowed_padded(&mut runs, granule, offset_alignment, limits, window_of, pad)
    } else {
        padded(&mut runs, granule, limits, pad)
            .map(|cut| cut.into_iter().map(Planned::Pieces).collect())
    };
    let plan = plan.ok_or_else(|| refused(&runs))?;
    let mut answered = Leases::default();
    let made = plan.iter().map(|chunk| {
        let length = chunk.length();
        match chunk {
            Planned::Pieces(runs) => Ok(DmaBufChunk {
                fd: held(device, runs, &mut answered)?,
                offset: 0,
                length,
            }),
            Planned::Window { window, offset, .. } => {
                let fd = held(device, std::slice::from_ref(window), &mut answered)?;
                count_window(window);
                Ok(DmaBufChunk {
                    fd,
                    offset: *offset,
                    length,
                })
            }
        }
    });
    let made = made.collect::<Result<Vec<_>, Refusal>>()?;
    leases.extend(answered);
    Ok(made)
}

/// Whether a window of `length` bytes of `file` from `start` may be answered: a dma-buf of it is
/// already held, or the windows held, with it, span no more than `budget` bytes. Past the budget a
/// request's stretch is cut into pieces as without windows, so the pages windows pin (and the GPU
/// memory their imports take) stay bounded. Creators racing for new windows can each pass the
/// check before either is counted, past the budget by at most a window each.
fn within_budget(file: &Arc<OwnedFd>, start: u64, length: u64, budget: u64) -> bool {
    let pages = vec![(Arc::as_ptr(file).addr(), start, length)];
    MADE.lock().contains_key(&pages)
        || WINDOW_BYTES.load(std::sync::atomic::Ordering::Relaxed) + length <= budget
}

/// Count the dma-buf of `window` that [`MADE`] holds in [`WINDOW_BYTES`], once.
fn count_window(window: &Run<Arc<OwnedFd>>) {
    let mut made = MADE.lock();
    if let Some(entry) = made.get_mut(&pages_of(std::slice::from_ref(window)))
        && !entry.window
    {
        entry.window = true;
        WINDOW_BYTES.fetch_add(window.length, std::sync::atomic::Ordering::Relaxed);
    }
}

/// A descriptor of a dma-buf of `runs`: of the one some source holds for those pages ([`MADE`]),
/// else of a new one, kept there; `leases` holds it either way (module docs).
fn held(
    device: &OwnedFd,
    runs: &[Run<Arc<OwnedFd>>],
    leases: &mut Leases,
) -> Result<OwnedFd, Refusal> {
    held_with(runs, leases, race_fix(), || create(device, runs))
}

/// [`held`], with the kernel call supplied separately so its concurrent miss can be tested
/// without a udmabuf device. With `join_races`, a racing creator joins the winner and drops its
/// redundant dma-buf outside the table lock: pinning and unpinning pages must not block
/// unrelated lookups.
fn held_with(
    runs: &[Run<Arc<OwnedFd>>],
    leases: &mut Leases,
    join_races: bool,
    create: impl FnOnce() -> Result<OwnedFd, Refusal>,
) -> Result<OwnedFd, Refusal> {
    let pages = pages_of(runs);
    let mut made = MADE.lock();
    if let Some(entry) = made.get_mut(&pages) {
        let fd = (entry.fd.as_fd())
            .try_clone_to_owned()
            .map_err(Refusal::Kernel)?;
        entry.holders += 1;
        leases.0.push(pages);
        return Ok(fd);
    }
    // udmabuf pins pages inside the call. Let unrelated lookups continue while it runs, then
    // recheck for another creator of these pages before publishing the result.
    drop(made);
    let fd = create()?;
    let kept = fd.as_fd().try_clone_to_owned().map_err(Refusal::Kernel)?;
    let mut made = MADE.lock();
    match made.get_mut(&pages) {
        // Another thread made one of the same pages meanwhile: share its dma-buf too, so the
        // runtime sees one identity and imports it once even during concurrent creation.
        Some(entry) if join_races => {
            let shared = (entry.fd.as_fd())
                .try_clone_to_owned()
                .map_err(Refusal::Kernel)?;
            entry.holders += 1;
            leases.0.push(pages);
            drop(made);
            return Ok(shared);
        }
        // Explicit comparison mode: retain the old behavior for concurrent cache misses.
        Some(_) => return Ok(fd),
        None => {
            made.insert(
                pages.clone(),
                Made {
                    fd: kept,
                    _files: runs.iter().map(|r| Arc::clone(&r.file)).collect(),
                    holders: 1,
                    window: false,
                },
            );
        }
    }
    leases.0.push(pages);
    Ok(fd)
}

/// One dma-buf of `runs`, in order (`UDMABUF_CREATE_LIST`).
fn create(device: &OwnedFd, runs: &[Run<Arc<OwnedFd>>]) -> Result<OwnedFd, Refusal> {
    // `struct udmabuf_create_list` of Linux's `include/uapi/linux/udmabuf.h` as little-endian
    // words: `flags` and `count`, then each `struct udmabuf_create_item`'s `memfd` and its
    // padding, `offset` and `size`.
    let count = u32::try_from(runs.len()).unwrap_or(u32::MAX);
    let mut call = vec![u64::from(FLAGS_CLOEXEC) | (u64::from(count) << 32)];
    for run in runs {
        let memfd = u32::try_from(run.file.as_raw_fd()).unwrap_or(u32::MAX);
        call.extend([u64::from(memfd), run.offset, run.length]);
    }
    // SAFETY: the list describes open memfds the backing table and the import's zeros hold
    // through the call; the kernel reads the header and its items.
    let made = unsafe { libc::ioctl(device.as_raw_fd(), CREATE_LIST as _, call.as_ptr()) };
    if made < 0 {
        return Err(Refusal::Kernel(io::Error::last_os_error()));
    }
    // SAFETY: the dma-buf the call just made, owned from here on.
    Ok(unsafe { OwnedFd::from_raw_fd(made) })
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::MetadataExt as _;
    use std::sync::Barrier;

    use reims_vgpu_core::num::{ToU64 as _, ToUsize as _};
    use reims_vgpu_core::platform::os::vm::{HostAddr, HostVm as _};

    use super::*;

    const PAGE: u64 = 0x1000;

    fn identity(fd: &OwnedFd) -> (u64, u64) {
        let file = std::fs::File::from(fd.as_fd().try_clone_to_owned().unwrap());
        let metadata = file.metadata().unwrap();
        (metadata.dev(), metadata.ino())
    }

    #[test]
    fn windows_are_default_off_and_take_the_documented_true_values() {
        for off in [
            None,
            Some(""),
            Some("0"),
            Some("off"),
            Some("auto"),
            Some("2"),
        ] {
            assert!(!windows_value(off), "{off:?}");
        }
        for on in ["1", "true", "yes", "on", " ON ", "True"] {
            assert!(windows_value(Some(on)), "{on:?}");
        }
    }

    #[test]
    fn the_window_budget_is_4_gib_unless_a_number_of_mib_says_otherwise() {
        for default in [None, Some(""), Some("lots"), Some("-1")] {
            assert_eq!(window_budget_value(default), 4 << 30, "{default:?}");
        }
        assert_eq!(window_budget_value(Some(" 512 ")), 512 << 20);
        assert_eq!(window_budget_value(Some("0")), 0);
    }

    /// A new window is answered only while the windows held span no more than the budget with
    /// it; one already held is answered whatever the budget. Its bytes leave the count with its
    /// last holder.
    #[test]
    fn windows_past_the_budget_are_refused_and_their_bytes_freed_with_them() {
        let file = vm::memfd(4 * vm::page_size());
        let page = vm::page_size();
        let held: Pages = vec![(Arc::as_ptr(&file).addr(), 0, 2 * page)];
        MADE.lock().insert(
            held.clone(),
            Made {
                fd: file.as_fd().try_clone_to_owned().unwrap(),
                _files: vec![Arc::clone(&file)],
                holders: 1,
                window: false,
            },
        );
        let window = Run {
            file: Arc::clone(&file),
            offset: 0,
            length: 2 * page,
        };
        count_window(&window);
        count_window(&window);
        let counted = WINDOW_BYTES.load(std::sync::atomic::Ordering::Relaxed);
        assert!(counted >= 2 * page);
        // Held: answered with no room left.
        assert!(within_budget(&file, 0, 2 * page, 0));
        // New: answered only where the budget has room for it beside what is counted.
        assert!(!within_budget(&file, 2 * page, 2 * page, counted + page));
        assert!(within_budget(&file, 2 * page, 2 * page, counted + 2 * page));
        drop(Leases(vec![held]));
        assert!(WINDOW_BYTES.load(std::sync::atomic::Ordering::Relaxed) <= counted - 2 * page);
    }

    /// A window is the multiple of the span at or below the byte, clipped to the file's whole
    /// pages; none past the file's end.
    #[test]
    fn a_window_is_the_aligned_span_holding_the_byte() {
        const SPAN: u64 = 64 << 20;
        assert_eq!(window_in(0, 8 << 30, SPAN, PAGE), Some((0, SPAN)));
        assert_eq!(
            window_in(SPAN + 5 * PAGE, 8 << 30, SPAN, PAGE),
            Some((SPAN, SPAN))
        );
        // The last window of a file whose size is no multiple of the span.
        assert_eq!(
            window_in(SPAN + PAGE, SPAN + 3 * PAGE, SPAN, PAGE),
            Some((SPAN, 3 * PAGE))
        );
        assert_eq!(
            window_in(SPAN + 3 * PAGE, SPAN + 3 * PAGE, SPAN, PAGE),
            None
        );
        // A span that is no multiple of the page is cut to one.
        assert_eq!(
            window_in(0, 4 * PAGE, 2 * PAGE + 1, PAGE),
            Some((0, 2 * PAGE))
        );
    }

    /// Runs of files named by numbers, `(file, offset, length)` in pages.
    fn runs(of: &[(u32, u64, u64)]) -> Vec<Run<u32>> {
        (of.iter())
            .map(|&(file, offset, length)| Run {
                file,
                offset: offset * PAGE,
                length: length * PAGE,
            })
            .collect()
    }

    const LIMITS: Limits = Limits {
        pieces: 1024,
        bytes: 64 << 20,
    };

    /// Windows of 16 pages in every file but file 9.
    fn sixteen(file: &u32, offset: u64) -> Option<(u64, u64)> {
        (*file != 9).then(|| window_in(offset, 1 << 40, 16 * PAGE, PAGE))?
    }

    fn lengths(plan: &[Planned<u32>]) -> Vec<u64> {
        plan.iter().map(|c| c.length() / PAGE).collect()
    }

    /// One run in one window is one chunk at its offset into the window, whatever the granule.
    #[test]
    fn a_run_in_one_window_is_an_offset_into_it() {
        let plan = windowed(&runs(&[(1, 20, 3)]), 3 * PAGE, PAGE, LIMITS, sixteen).unwrap();
        assert_eq!(
            plan,
            vec![Planned::Window {
                window: Run {
                    file: 1,
                    offset: 16 * PAGE,
                    length: 16 * PAGE
                },
                offset: 4 * PAGE,
                length: 3 * PAGE,
            }]
        );
    }

    /// Without windows (an alignment of 0, or no file taking one) the plan is `chunks`', piece
    /// for piece.
    #[test]
    fn without_windows_the_plan_is_the_pieces_cut() {
        let of = runs(&[(1, 0, 3), (2, 7, 2), (1, 40, 5)]);
        let cut = chunks(&of, 2 * PAGE, LIMITS).unwrap();
        let pieces: Vec<Planned<u32>> = cut.into_iter().map(Planned::Pieces).collect();
        assert_eq!(windowed(&of, 2 * PAGE, 0, LIMITS, sixteen).unwrap(), pieces);
        let none = |_: &u32, _| None;
        assert_eq!(windowed(&of, 2 * PAGE, PAGE, LIMITS, none).unwrap(), pieces);
        let small = Limits {
            pieces: 2,
            bytes: 4 * PAGE,
        };
        let cut = chunks(&of, 2 * PAGE, small).unwrap();
        let pieces: Vec<Planned<u32>> = cut.into_iter().map(Planned::Pieces).collect();
        assert_eq!(windowed(&of, 2 * PAGE, PAGE, small, none).unwrap(), pieces);
    }

    /// Every cut lies a multiple of the granule from the request's start, a window's chunk ends
    /// at the run's or the window's end cut back to the granule, and what no window takes goes
    /// in pieces of its own.
    #[test]
    fn windows_and_pieces_meet_at_granules() {
        // 2 pages of file 9 (no window), 5 of file 1 from page 14 (crossing a window at 16),
        // then 3 of file 2.
        let of = runs(&[(9, 0, 2), (1, 14, 5), (2, 3, 3)]);
        let plan = windowed(&of, 2 * PAGE, PAGE, LIMITS, sixteen).unwrap();
        assert_eq!(lengths(&plan), vec![2, 2, 2, 2, 2]);
        let mut at = 0;
        for chunk in &plan[..plan.len() - 1] {
            at += chunk.length();
            assert!(at.is_multiple_of(2 * PAGE));
        }
        assert!(matches!(plan[0], Planned::Pieces(_)));
        assert_eq!(
            plan[1],
            Planned::Window {
                window: Run {
                    file: 1,
                    offset: 0,
                    length: 16 * PAGE
                },
                offset: 14 * PAGE,
                length: 2 * PAGE,
            }
        );
        assert_eq!(
            plan[2],
            Planned::Window {
                window: Run {
                    file: 1,
                    offset: 16 * PAGE,
                    length: 16 * PAGE
                },
                offset: 0,
                length: 2 * PAGE,
            }
        );
        // File 1's page 18 lies past a granule boundary only one page before its run ends: it
        // and file 2's first page go in pieces, and file 2's last two pages in its window.
        assert!(matches!(plan[3], Planned::Pieces(_)));
        assert!(matches!(plan[4], Planned::Window { offset, .. } if offset == 4 * PAGE));
        // With an alignment of 4 pages, only offsets into a window at multiples of it are taken.
        let plan = windowed(&of, 2 * PAGE, 4 * PAGE, LIMITS, sixteen).unwrap();
        assert!(plan.iter().all(|c| match c {
            Planned::Window { offset, .. } => offset.is_multiple_of(4 * PAGE),
            Planned::Pieces(_) => true,
        }));
        let total: u64 = plan.iter().map(Planned::length).sum();
        assert_eq!(total, 10 * PAGE);
    }

    /// Several chunks are padded to the granule, the pad in the last chunk's pieces.
    #[test]
    fn several_windowed_chunks_are_padded() {
        let mut of = runs(&[(1, 0, 4), (9, 0, 1)]);
        let plan = windowed_padded(&mut of, 2 * PAGE, PAGE, LIMITS, sixteen, |past| Run {
            file: 7,
            offset: 0,
            length: past,
        })
        .unwrap();
        assert_eq!(lengths(&plan), vec![4, 2]);
        let Planned::Pieces(last) = &plan[1] else {
            panic!("{plan:?}");
        };
        assert_eq!(last.last().unwrap().file, 7);
    }

    #[test]
    fn race_fix_is_default_on_and_accepts_the_documented_false_values() {
        for on in [
            None,
            Some(""),
            Some("1"),
            Some("true"),
            Some("on"),
            Some("auto"),
        ] {
            assert!(race_fix_value(on), "{on:?}");
        }
        for off in ["0", "false", "no", "off", " OFF ", "False", " No "] {
            assert!(!race_fix_value(Some(off)), "{off:?}");
        }
    }

    /// Both creators miss before either can publish. The switch chooses one identity or the
    /// old separate identities, while ordinary cache hits and lease lifetimes stay shared.
    /// Memfds stand in for the kernel's dma-bufs, without requiring a udmabuf device.
    #[test]
    fn concurrent_creators_obey_the_race_fix_switch() {
        for join_races in [false, true] {
            let runs = [Run {
                file: vm::memfd(PAGE),
                offset: 0,
                length: PAGE,
            }];
            let pages = pages_of(&runs);
            let barrier = Barrier::new(2);
            let ((a, a_lease), (b, b_lease)) = std::thread::scope(|scope| {
                let make = || {
                    let mut lease = Leases::default();
                    let fd = held_with(&runs, &mut lease, join_races, || {
                        let fd = vm::memfd(PAGE).as_fd().try_clone_to_owned().unwrap();
                        barrier.wait();
                        Ok(fd)
                    })
                    .unwrap();
                    (fd, lease)
                };
                let (a, b) = (scope.spawn(make), scope.spawn(make));
                (a.join().unwrap(), b.join().unwrap())
            });
            assert_eq!(identity(&a) == identity(&b), join_races);
            let holders = if join_races { 2 } else { 1 };
            assert_eq!(MADE.lock().get(&pages).unwrap().holders, holders);
            let winning_identity = identity(&MADE.lock().get(&pages).unwrap().fd);
            let mut c_lease = Leases::default();
            let c = held_with(&runs, &mut c_lease, join_races, || {
                panic!("a live lease must be reused even with the race fix disabled")
            })
            .unwrap();
            assert_eq!(winning_identity, identity(&c));
            let remaining = if join_races || !b_lease.0.is_empty() {
                2
            } else {
                1
            };
            drop(a);
            drop(a_lease);
            assert_eq!(MADE.lock().get(&pages).unwrap().holders, remaining);
            drop(b);
            drop(b_lease);
            assert_eq!(MADE.lock().get(&pages).unwrap().holders, 1);
            drop(c);
            drop(c_lease);
            assert!(!MADE.lock().contains_key(&pages));
        }
    }

    #[test]
    fn refused_creation_holds_no_lease_and_can_be_retried() {
        let runs = [Run {
            file: vm::memfd(PAGE),
            offset: 0,
            length: PAGE,
        }];
        let pages = pages_of(&runs);
        let mut lease = Leases::default();
        let refused = held_with(&runs, &mut lease, true, || {
            Err(Refusal::Kernel(io::Error::from_raw_os_error(libc::ENOMEM)))
        });
        assert!(matches!(refused, Err(Refusal::Kernel(_))));
        assert!(lease.0.is_empty());
        assert!(!MADE.lock().contains_key(&pages));
        let fd = held_with(&runs, &mut lease, true, || {
            vm::memfd(PAGE)
                .as_fd()
                .try_clone_to_owned()
                .map_err(Refusal::Kernel)
        })
        .unwrap();
        assert_eq!(MADE.lock().get(&pages).unwrap().holders, 1);
        drop(fd);
        drop(lease);
        assert!(!MADE.lock().contains_key(&pages));
    }

    /// Whether this host has a udmabuf device this process opens: where it has none, the tests
    /// say so and check nothing.
    fn udmabuf() -> bool {
        match device() {
            Ok(_) => true,
            Err(refusal) => {
                eprintln!("skipped: {refusal}");
                false
            }
        }
    }

    /// The dma-buf's byte at `offset`, read through a mapping of it.
    fn read_through(chunk: &DmaBufChunk, offset: u64) -> u8 {
        let length = usize::try_from(chunk.length).unwrap();
        // SAFETY: a fresh read-only mapping of the dma-buf, read once and unmapped.
        unsafe {
            let at = libc::mmap(
                std::ptr::null_mut(),
                length,
                libc::PROT_READ,
                libc::MAP_SHARED,
                chunk.fd.as_raw_fd(),
                0,
            );
            assert_ne!(at, libc::MAP_FAILED);
            let byte = (at.cast::<u8>())
                .add(usize::try_from(offset).unwrap())
                .read_volatile();
            libc::munmap(at, length);
            byte
        }
    }

    fn run(file: u32, offset: u64, length: u64) -> Run<u32> {
        Run {
            file,
            offset,
            length,
        }
    }

    /// Runs within the limits make one chunk; past them, chunks as full as the limits let, cut
    /// at granules from the window's start, a run cut where a chunk ends in it; a granule past
    /// the limits makes none
    #[test]
    fn a_window_is_cut_into_chunks_within_the_limits_at_granules() {
        let limits = Limits {
            pieces: 3,
            bytes: 0x10000,
        };
        let one = [run(1, 0, 0x1000), run(2, 0x5000, 0x2000)];
        assert_eq!(
            chunks(&one, 0x1000, limits),
            Some(vec![vec![run(1, 0, 0x1000), run(2, 0x5000, 0x2000)]])
        );
        // Five one-page runs, three a chunk.
        let scattered: Vec<_> = (0..5u32)
            .map(|i| run(i, u64::from(i) * 0x3000, 0x1000))
            .collect();
        assert_eq!(
            chunks(&scattered, 0x1000, limits),
            Some(vec![
                vec![
                    run(0, 0, 0x1000),
                    run(1, 0x3000, 0x1000),
                    run(2, 0x6000, 0x1000)
                ],
                vec![run(3, 0x9000, 0x1000), run(4, 0xc000, 0x1000)],
            ])
        );
        // At a granule of two pages the first chunk ends at two pages; the last ends with the
        // window, at no granule.
        assert_eq!(
            chunks(&scattered, 0x2000, limits),
            Some(vec![
                vec![run(0, 0, 0x1000), run(1, 0x3000, 0x1000)],
                vec![
                    run(2, 0x6000, 0x1000),
                    run(3, 0x9000, 0x1000),
                    run(4, 0xc000, 0x1000)
                ],
            ])
        );
        // A run longer than a chunk's bytes is cut where each chunk ends.
        let long = [run(7, 0x1000, 0x25000)];
        assert_eq!(
            chunks(&long, 0x1000, limits),
            Some(vec![
                vec![run(7, 0x1000, 0x10000)],
                vec![run(7, 0x11000, 0x10000)],
                vec![run(7, 0x21000, 0x5000)],
            ])
        );
        // A run cut at a granule inside it: three runs reach 0x5000, the granule 0x2000 cuts
        // the third.
        let mixed = [
            run(1, 0, 0x1000),
            run(2, 0, 0x2000),
            run(3, 0, 0x2000),
            run(4, 0, 0x1000),
        ];
        assert_eq!(
            chunks(&mixed, 0x2000, limits),
            Some(vec![
                vec![run(1, 0, 0x1000), run(2, 0, 0x2000), run(3, 0, 0x1000)],
                vec![run(3, 0x1000, 0x1000), run(4, 0, 0x1000)],
            ])
        );
        // A granule of four single pages past three pieces fits no chunk; the whole window as
        // its granule fits only within the limits.
        assert_eq!(chunks(&scattered, 0x4000, limits), None);
        assert_eq!(chunks(&scattered, 0x5000, limits), None);
        assert!(chunks(&one, 0x3000, limits).is_some());
        // The most runs one import of a macOS 26 guest's boot took, at udmabuf's defaults.
        let boot: Vec<_> = (0..16_201u32)
            .map(|i| run(i, u64::from(i) * 0x2000, 0x1000))
            .collect();
        let defaults = Limits {
            pieces: DEFAULT_LIST_LIMIT,
            bytes: DEFAULT_SIZE_LIMIT_MB << 20,
        };
        let cut = chunks(&boot, 0x1000, defaults).unwrap();
        assert_eq!(cut.len(), 16);
        assert!(cut.iter().all(|c| c.len() as u64 <= DEFAULT_LIST_LIMIT));
        assert_eq!(cut.iter().map(Vec::len).sum::<usize>(), 16_201);
    }

    /// Chunks past one come to the window's length rounded up to the granule, the last ending
    /// with the pad's run; one chunk, or a window of whole granules, takes no pad
    #[test]
    fn several_chunks_are_padded_to_the_granule() {
        let limits = Limits {
            pieces: 2,
            bytes: 0x10000,
        };
        let pad = |length| run(99, 0, length);
        let mut one = vec![run(1, 0, 0x1000)];
        assert_eq!(
            padded(&mut one, 0x4000, limits, pad),
            Some(vec![vec![run(1, 0, 0x1000)]])
        );
        let mut whole: Vec<_> = (0..4u32).map(|i| run(i, 0, 0x2000)).collect();
        assert_eq!(padded(&mut whole, 0x4000, limits, pad).unwrap().len(), 2);
        assert_eq!(whole.len(), 4, "no pad");
        // Three single pages at a granule of two: two chunks, the last padded by a page.
        let mut three: Vec<_> = (0..3u32).map(|i| run(i, 0, 0x1000)).collect();
        assert_eq!(
            padded(&mut three, 0x2000, limits, pad),
            Some(vec![
                vec![run(0, 0, 0x1000), run(1, 0, 0x1000)],
                vec![run(2, 0, 0x1000), run(99, 0, 0x1000)],
            ])
        );
        // Five pages at two a chunk: the pad joins the last page's chunk.
        let mut five: Vec<_> = (0..5u32).map(|i| run(i, 0, 0x1000)).collect();
        let cut = padded(&mut five, 0x2000, limits, pad).unwrap();
        assert_eq!(cut.iter().flatten().map(|r| r.length).sum::<u64>(), 0x6000);
        assert_eq!(cut.last().unwrap().last().unwrap().file, 99);
    }

    /// The dma-bufs of a view show the pages the view shows, in the view's order, and a range
    /// with no file behind it is refused
    #[test]
    fn a_dma_buf_shows_the_pages_of_its_range() {
        if !udmabuf() {
            return;
        }
        let page = vm::page_size();
        let at = vm::allocate_aliasable(4 * page).unwrap().addr().to_u64();
        for i in 0..4 {
            // SAFETY: the range this test just allocated, read and write.
            unsafe {
                vm::ptr(at + i * page)
                    .cast::<u8>()
                    .write_volatile(0x10 + i as u8)
            };
        }
        let chunks = of_pages(at + page, 2 * page, page, 0, &mut Leases::default()).unwrap();
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].length, 2 * page);
        assert_eq!(read_through(&chunks[0], 0), 0x11);
        assert_eq!(read_through(&chunks[0], page), 0x12);
        // SAFETY: as above.
        unsafe { vm::ptr(at + page).cast::<u8>().write_volatile(0x5a) };
        assert_eq!(
            read_through(&chunks[0], 0),
            0x5a,
            "the same pages, not a copy"
        );
        let private = vm::anonymous(None, page, "a test's private page").unwrap();
        assert!(matches!(
            of_pages(private, page, page, 0, &mut Leases::default()),
            Err(Refusal::Unresolved)
        ));
    }

    /// A source's leases hold each dma-buf it was answered with: one held by two sources goes
    /// once both let it go, and the entries of others stay
    #[test]
    fn a_dma_buf_goes_once_every_source_holding_it_went() {
        let file = vm::memfd(vm::page_size());
        let pages: Pages = vec![(Arc::as_ptr(&file).addr(), 0, vm::page_size())];
        let other: Pages = vec![(Arc::as_ptr(&file).addr(), 0, 2 * vm::page_size())];
        for key in [&pages, &other] {
            MADE.lock().insert(
                key.clone(),
                Made {
                    fd: file.as_fd().try_clone_to_owned().unwrap(),
                    _files: vec![Arc::clone(&file)],
                    holders: 0,
                    window: false,
                },
            );
        }
        let lease = || {
            MADE.lock().get_mut(&pages).unwrap().holders += 1;
            Leases(vec![pages.clone()])
        };
        let (first, mut second) = (lease(), lease());
        second.extend(Leases::default());
        drop(first);
        assert!(
            MADE.lock().contains_key(&pages),
            "the second source holds it"
        );
        drop(second);
        assert!(!MADE.lock().contains_key(&pages));
        assert!(MADE.lock().contains_key(&other), "another dma-buf stays");
        MADE.lock().remove(&other);
    }

    /// Two windows of the same pages, while a source holds the first's dma-buf, are answered with
    /// that dma-buf again; once no source holds it, a window of them is a dma-buf of its own
    #[test]
    fn the_same_pages_are_one_dma_buf_while_a_source_holds_it() {
        if !udmabuf() {
            return;
        }
        let identity = |chunk: &DmaBufChunk| {
            let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
            // SAFETY: an open descriptor; `fstat` writes the structure.
            assert_eq!(
                unsafe { libc::fstat(chunk.fd.as_raw_fd(), stat.as_mut_ptr()) },
                0
            );
            // SAFETY: written above.
            let stat = unsafe { stat.assume_init() };
            (stat.st_dev, stat.st_ino)
        };
        let page = vm::page_size();
        let at = vm::allocate_aliasable(2 * page).unwrap().addr().to_u64();
        let mut first = Leases::default();
        let a = of_pages(at, 2 * page, 2 * page, 0, &mut first).unwrap();
        let mut second = Leases::default();
        let b = of_pages(at, 2 * page, 2 * page, 0, &mut second).unwrap();
        assert_eq!(identity(&a[0]), identity(&b[0]));
        let mut third = Leases::default();
        let part = of_pages(at, page, page, 0, &mut third).unwrap();
        assert_ne!(identity(&a[0]), identity(&part[0]), "other pages");
        drop((first, second, third));
        let c = of_pages(at, 2 * page, 2 * page, 0, &mut Leases::default()).unwrap();
        assert_ne!(identity(&a[0]), identity(&c[0]));
    }

    /// A view of more scattered pages than one dma-buf may hold is made of several, in order,
    /// each within the host's limits
    #[test]
    fn a_view_past_the_limits_is_made_of_chunks() {
        if !udmabuf() {
            return;
        }
        let page = vm::page_size();
        let limits = limits();
        // Every other page of a range, so no two pieces follow on in the file.
        let count = limits.pieces + 2;
        let source = vm::allocate_aliasable(2 * count * page).unwrap();
        let view = vm::allocate_aliasable(count * page).unwrap();
        let host = super::super::LinuxVm::new();
        for i in 0..count {
            let from = source.addr().to_u64() + 2 * i * page;
            // SAFETY: the source range this test allocated, read and write.
            unsafe { vm::ptr(from).cast::<u8>().write_volatile((i % 251) as u8) };
            let to = view.addr().to_u64() + i * page;
            let (to, from) = (
                HostAddr::from_addr(to.to_usize()),
                HostAddr::from_addr(from.to_usize()),
            );
            assert!(host.remap_shared_overwrite(to, from, page).is_ok());
        }
        let chunks = of_pages(
            view.addr().to_u64(),
            count * page,
            page,
            0,
            &mut Leases::default(),
        )
        .unwrap();
        assert!(chunks.len() >= 2);
        assert_eq!(chunks.iter().map(|c| c.length).sum::<u64>(), count * page);
        let mut at = 0;
        for chunk in &chunks {
            assert!(chunk.length <= limits.bytes);
            assert!(chunk.length / page <= limits.pieces);
            for offset in (0..chunk.length).step_by(usize::try_from(page).unwrap()) {
                let i = at + offset / page;
                assert_eq!(read_through(chunk, offset), (i % 251) as u8);
            }
            at += chunk.length / page;
        }
    }
}
