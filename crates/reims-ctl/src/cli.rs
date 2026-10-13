//! The commands. With none, reims-ctl opens its interactive interface (tui.rs).

use crate::create::{self, DiskSource, ImportMode, Spec};
use crate::host::{Host, Profile};
use crate::vm::{self, Registry, Vm, format_mib, parse_size_mib};
use crate::{gpu, install, logs, opencore, paths, qemu, runtime, update, view};
use anyhow::{Context, Result, anyhow, bail};
use clap::{Args, Parser, Subcommand};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Parser)]
#[command(
    name = "reims-ctl",
    version,
    about = "Create, boot and manage macOS VMs on QEMU with reims-vgpu",
    long_about = "Create, boot and manage macOS VMs on QEMU with reims-vgpu.\n\n\
        Run with no command for the interactive interface. Every option takes its value as \
        `--name value` or `--name=value`; yes/no options also take a bare `--name` for yes.",
    after_help = "VMs live in ~/.local/share/reims-ctl/vms unless created elsewhere (--dir); \
        `reims-ctl command NAME` prints the QEMU command a VM runs, for scripts of your own."
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Cmd>,
}

#[derive(Subcommand)]
pub enum Cmd {
    /// List the VMs and whether they run
    List {
        /// Machine-readable output
        #[arg(long)]
        json: bool,
    },
    /// Create a VM
    #[command(after_help = create_examples())]
    Create(Box<CreateArgs>),
    /// Delete a VM (its directory too, unless --keep-files)
    Delete {
        name: String,
        /// Leave the VM's directory where it is
        #[arg(long)]
        keep_files: bool,
        /// Do not ask
        #[arg(long, short)]
        yes: bool,
    },
    /// Start a VM: a window opens for each of its displays
    Start { name: String },
    /// Shut a VM down; if the guest has not shut down when the timeout ends, power it off
    Stop {
        name: String,
        /// Power off at once
        #[arg(long)]
        force: bool,
        /// Seconds to wait for the guest (the VM's own setting by default)
        #[arg(long)]
        timeout: Option<u64>,
    },
    /// Show a VM's settings and state
    Show { name: String },
    /// Change a VM's settings (they apply at its next start)
    Set(Box<SetArgs>),
    /// Show a running VM's screen in this terminal, and use its keyboard and mouse from here
    #[command(after_help = VIEW_HELP)]
    View {
        name: String,
        /// The display to show, from 1
        #[arg(long, default_value_t = 1)]
        display: u32,
        /// The most frames a second
        #[arg(long, default_value_t = 15)]
        fps: u32,
        /// How to draw the screen (by default, the best the terminal answers it can show)
        #[arg(long, value_enum)]
        protocol: Option<view::Protocol>,
    },
    /// Print the QEMU command that runs a VM, as a shell script
    Command {
        name: String,
        /// Write it to this file (executable) instead
        #[arg(long, short)]
        output: Option<PathBuf>,
    },
    /// Put an installer image in a VM (x86-64), into the running VM's CD drive where it can; a
    /// DMG is converted to a raw image in the VM's directory
    Insert { name: String, path: PathBuf },
    /// Take the installer out of a VM (x86-64)
    Eject { name: String },
    /// Replace a VM's OpenCore image (x86-64) with one the installed reims-util makes, presenting
    /// the same Mac
    #[command(name = "update-opencore", after_help = UPDATE_OPENCORE_HELP)]
    UpdateOpencore { name: String },
    /// List the GPUs a VM can be given (Linux)
    Gpus,
    /// Print the end of a VM's QEMU log, or its serial console's
    Logs {
        name: String,
        /// Lines to print
        #[arg(long, short = 'n', default_value_t = 40)]
        lines: usize,
        /// The serial console's log instead
        #[arg(long)]
        serial: bool,
    },
    /// Zip a VM's logs, settings and the host's details into Downloads, for a bug report
    Report { name: String },
    /// Update reims-ctl and its QEMU from the checkout it was installed from
    Update {
        /// Switch the checkout to this channel first; later updates stay on it
        #[arg(long, value_enum)]
        channel: Option<update::Channel>,
    },
    /// Check the host and the install
    Doctor,
    /// The interactive interface (what reims-ctl runs with no command)
    Tui,
    #[command(name = "__vulkan-probe", hide = true)]
    VulkanProbe,
    #[command(name = "__vz-install", hide = true)]
    VzInstall {
        #[arg(long)]
        ipsw: PathBuf,
        #[arg(long)]
        dir: PathBuf,
        #[arg(long)]
        disk_mib: u64,
        #[arg(long)]
        cpus: u32,
        #[arg(long)]
        memory_mib: u64,
    },
    #[command(name = "__vz-latest", hide = true)]
    VzLatest,
    #[command(name = "__record-install", hide = true)]
    RecordInstall {
        #[arg(long)]
        repo: PathBuf,
        #[arg(long)]
        qemu: PathBuf,
        #[arg(long, default_value = "")]
        commit: String,
    },
}

const VIEW_HELP: &str = "\
For a VM on a host with no screen, to set macOS up before it has screen sharing or remote \
login. The screen is drawn with the terminal's graphics (kitty's, iTerm2's or sixel; half-block \
characters where it has none), over SSH as well, and the terminal's keys, mouse and pasted text \
go to the guest as a US keyboard's and a tablet's.

Ctrl-] disconnects; the VM keeps running. Alt+key is Cmd+key. The Cmd, Opt, Ctrl and Shift \
buttons on the top row hold that key for the next key or click.";

const UPDATE_OPENCORE_HELP: &str = "\
An image an older reims-util made lacks what a newer one's carries, such as ReimsVgpu.kext, which \
macOS 26 needs for a display wider than 2048 pixels and for GPU-accelerated Chrome. The new image \
presents the same Mac (its model, serial number, MLB, UUID and ROM, read from the old image's \
config.plist), so macOS sees no new machine, and the VM keeps its OVMF variables, which hold its \
boot entries. Whatever else was changed in the old image is not carried over. The VM must be \
stopped; reims-ctl update installs the reims-util this reims-ctl needs.";

/// `--flag`, `--flag=true`, `--flag false`.
fn bool_arg() -> clap::Arg {
    clap::Arg::new("x")
        .num_args(0..=1)
        .default_missing_value("true")
}

/// `create --help`'s examples, under a header styled as clap styles its own.
fn create_examples() -> String {
    let h = *clap::builder::Styles::default().get_header();
    format!("{h}Examples:{h:#}{CREATE_EXAMPLES}")
}

/// `e`, followed by the examples: for a create that was not given enough to make a VM.
fn with_create_examples(e: impl std::fmt::Display) -> anyhow::Error {
    anyhow!("{e}\n\nExamples:{CREATE_EXAMPLES}")
}

const CREATE_EXAMPLES: &str = r"
  An x86-64 guest on Linux, with OpenCore from OSX-KVM:

    # A disk that already has macOS installed (copied; see --disk-mode)
    reims-ctl create mac --opencore ~/OSX-KVM/OpenCore/OpenCore.qcow2 \
      --disk ~/OSX-KVM/mac_hdd_ng.img

    # A new 128G disk and the macOS installer (the recovery image as downloaded, or
    # converted to BaseSystem.img); once macOS is installed, reims-ctl eject mac
    # takes the installer out
    reims-ctl create mac --opencore ~/OSX-KVM/OpenCore/OpenCore.qcow2 \
      --installer ~/OSX-KVM/BaseSystem.dmg --start

    # Everything an OSX-KVM or ULTMOS launch script names: its disk (copied),
    # OpenCore, firmware, installer, CPU and MAC
    reims-ctl create mac --from-script ~/OSX-KVM/OpenCore-Boot.sh

  An arm64 guest on an Apple silicon Mac:

    # Install the newest macOS this Mac supports, on a new 64G disk
    reims-ctl create mac --ipsw latest --start

    # A macosvm or tart VM that already has macOS installed (an APFS clone)
    reims-ctl create mac --bundle ~/VMs/macos13";

#[derive(Args)]
pub struct CreateArgs {
    /// The VM's name (or --name)
    #[arg(value_name = "NAME")]
    pub name_pos: Option<String>,
    /// The VM's name
    #[arg(long)]
    pub name: Option<String>,
    /// Its directory (default ~/.local/share/reims-ctl/vms/NAME)
    #[arg(long)]
    pub dir: Option<PathBuf>,
    /// vCPUs
    #[arg(long)]
    pub cpus: Option<u32>,
    /// RAM, such as 8G
    #[arg(long)]
    pub memory: Option<String>,
    /// A new disk of this size, such as 128G
    #[arg(long)]
    pub disk_size: Option<String>,
    /// An existing disk image
    #[arg(long, conflicts_with = "disk_size")]
    pub disk: Option<PathBuf>,
    /// How an existing disk is used: copy (a reflink or clone where it can), overlay, in-place
    #[arg(long, default_value = "copy")]
    pub disk_mode: String,
    /// x86-64: the OpenCore image to copy (OpenCore.qcow2, from OSX-KVM or ULTMOS)
    #[arg(long)]
    pub opencore: Option<PathBuf>,
    /// x86-64: the installer image (an .iso goes in a CD drive; BaseSystem.img as a disk, and
    /// BaseSystem.dmg too, converted to a raw image in the VM's directory)
    #[arg(long, alias = "iso", alias = "cdrom")]
    pub installer: Option<PathBuf>,
    /// x86-64: take the disk, OpenCore, firmware, installer, CPU and MAC from an OSX-KVM or
    /// ULTMOS launch script
    #[arg(long)]
    pub from_script: Option<PathBuf>,
    /// x86-64: OVMF's code (default: found beside the OpenCore image, or QEMU's own)
    #[arg(long)]
    pub firmware_code: Option<PathBuf>,
    /// x86-64: OVMF's variables
    #[arg(long)]
    pub firmware_vars: Option<PathBuf>,
    /// x86-64: QEMU's CPU model (default Haswell-noTSX)
    #[arg(long)]
    pub cpu_model: Option<String>,
    /// x86-64: CPU flags after the model's
    #[arg(long)]
    pub cpu_flags: Option<String>,
    /// The network card's MAC
    #[arg(long)]
    pub mac: Option<String>,
    /// arm64: install macOS from this IPSW, or `latest` to download the newest this Mac supports
    #[arg(long)]
    pub ipsw: Option<String>,
    /// arm64: import a Virtualization.framework VM's directory (macosvm's or tart's)
    #[arg(long)]
    pub bundle: Option<PathBuf>,
    /// arm64: copy (an APFS clone) or in-place
    #[arg(long, default_value = "copy")]
    pub bundle_mode: String,
    /// arm64: the machine identifier's ECID, where the bundle does not hold it
    #[arg(long)]
    pub ecid: Option<u64>,
    /// Displays, 1 to 8, each in a window of its own
    #[arg(long)]
    pub displays: Option<u32>,
    /// The guest's first mode: WIDTHxHEIGHT, or host for the host screen's modes
    #[arg(long)]
    pub resolution: Option<String>,
    /// Run the mode HiDPI (3840x2160 looks like 1920x1080)
    #[arg(long, num_args = 0..=1, default_missing_value = "true")]
    pub hidpi: Option<bool>,
    /// Borderless windows that fill their screens
    #[arg(long, num_args = 0..=1, default_missing_value = "true")]
    pub borderless: Option<bool>,
    /// The refresh rate offered to the guest
    #[arg(long)]
    pub refresh: Option<u32>,
    /// The GPU (Linux): auto, a number or name from `reims-ctl gpus`, or a Vulkan ICD JSON file
    #[arg(long)]
    pub gpu: Option<String>,
    /// Keep the library's log and the serial console
    #[arg(long, num_args = 0..=1, default_missing_value = "true")]
    pub logs: Option<bool>,
    /// A USB audio device on the host's sound
    #[arg(long, num_args = 0..=1, default_missing_value = "true")]
    pub audio: Option<bool>,
    /// Forward this port on 127.0.0.1 to the guest's SSH port
    #[arg(long)]
    pub ssh_port: Option<u16>,
    /// Start it once made
    #[arg(long)]
    pub start: bool,
}

#[derive(Args)]
pub struct SetArgs {
    pub name: String,
    #[arg(long)]
    pub cpus: Option<u32>,
    /// RAM, such as 8G
    #[arg(long)]
    pub memory: Option<String>,
    /// x86-64: threads per core the guest sees, 1 or 2 (8 vCPUs at 2 are 4 cores)
    #[arg(long)]
    pub threads_per_core: Option<u32>,
    /// Linux: pin each vCPU to a host CPU at start: off, auto (the default), or a list such as
    /// 2-5,8-11
    #[arg(long)]
    pub pin: Option<String>,
    /// Linux, with pinned vCPUs: off (the default), or llc to keep QEMU's other threads in the
    /// vCPUs' L3 caches (an AMD CCX) and auto's vCPUs in one where they fit
    #[arg(long)]
    pub confine: Option<String>,
    #[arg(long)]
    pub displays: Option<u32>,
    /// WIDTHxHEIGHT, or host
    #[arg(long)]
    pub resolution: Option<String>,
    #[arg(long, num_args = 0..=1, default_missing_value = "true")]
    pub hidpi: Option<bool>,
    #[arg(long, num_args = 0..=1, default_missing_value = "true")]
    pub borderless: Option<bool>,
    /// The refresh rate offered to the guest; 0 for the host screen's
    #[arg(long)]
    pub refresh: Option<u32>,
    /// auto, a number or name from `reims-ctl gpus`, or a Vulkan ICD JSON file
    #[arg(long)]
    pub gpu: Option<String>,
    #[arg(long, num_args = 0..=1, default_missing_value = "true")]
    pub logs: Option<bool>,
    /// REIMS_VGPU_LOG's filter, such as info or warn,reims_vgpu_core=debug
    #[arg(long)]
    pub log_level: Option<String>,
    #[arg(long, num_args = 0..=1, default_missing_value = "true")]
    pub audio: Option<bool>,
    /// coreaudio, pipewire, pa, alsa, none, or auto
    #[arg(long)]
    pub audio_backend: Option<String>,
    #[arg(long, num_args = 0..=1, default_missing_value = "true")]
    pub network: Option<bool>,
    /// 0 for none
    #[arg(long)]
    pub ssh_port: Option<u16>,
    #[arg(long)]
    pub mac: Option<String>,
    /// Seconds stop waits for the guest before it powers the VM off
    #[arg(long)]
    pub stop_timeout: Option<u64>,
    #[arg(long)]
    pub cpu_model: Option<String>,
    #[arg(long)]
    pub cpu_flags: Option<String>,
    /// More QEMU arguments, as one shell-quoted string; empty for none
    #[arg(long, allow_hyphen_values = true)]
    pub extra_args: Option<String>,
    /// KEY=VALUE for QEMU's environment; KEY= removes it (repeatable)
    #[arg(long = "env")]
    pub env: Vec<String>,
}

pub fn main(cli: Cli) -> Result<i32> {
    let host = Host::current();
    let cmd = match cli.command {
        None | Some(Cmd::Tui) => return crate::tui::run().map(|_| 0),
        Some(c) => c,
    };
    match cmd {
        Cmd::List { json } => list(json)?,
        Cmd::Create(a) => create_cmd(*a, host)?,
        Cmd::Delete {
            name,
            keep_files,
            yes,
        } => delete(&name, keep_files, yes)?,
        Cmd::Start { name } => {
            let vm = Registry::load()?.open(&name)?;
            for w in runtime::warnings(&vm, host) {
                eprintln!("warning: {w}");
            }
            let pid = runtime::start(&vm, host)?;
            println!("{name}: running, pid {pid}");
        }
        Cmd::Stop {
            name,
            force,
            timeout,
        } => {
            let vm = Registry::load()?.open(&name)?;
            let t = Duration::from_secs(timeout.unwrap_or(vm.config.stop_timeout_secs));
            runtime::stop(&vm, force, t, |s| println!("{name}: {s}"))?;
        }
        Cmd::Show { name } => show(&Registry::load()?.open(&name)?, host),
        Cmd::Set(a) => set(*a, host)?,
        Cmd::View {
            name,
            display,
            fps,
            protocol,
        } => {
            let vm = Registry::load()?.open(&name)?;
            let opts = view::Options {
                display,
                fps,
                protocol,
            };
            if let Some(m) = view::run(&vm, &opts)? {
                println!("{m}");
            }
        }
        Cmd::Command { name, output } => {
            let vm = Registry::load()?.open(&name)?;
            let script =
                runtime::invocation_for_print(&vm, host)?.shell_script(&script_comment(&vm));
            match output {
                Some(p) => {
                    std::fs::write(&p, script)?;
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755))?;
                    println!("wrote {}", p.display());
                }
                None => print!("{script}"),
            }
        }
        Cmd::Insert { name, path } => {
            let mut vm = Registry::load()?.open(&name)?;
            let path = paths::absolute(&path.to_string_lossy());
            println!(
                "{}",
                set_installer(&mut vm, Some(path), &mut |p| println!("{name}: {p}"))?
            );
        }
        Cmd::Eject { name } => {
            let mut vm = Registry::load()?.open(&name)?;
            println!("{}", set_installer(&mut vm, None, &mut |_| {})?);
        }
        Cmd::UpdateOpencore { name } => {
            let mut vm = Registry::load()?.open(&name)?;
            println!(
                "{}",
                opencore::renew(&mut vm, &mut |p| println!("{name}: {p}"))?
            );
        }
        Cmd::Gpus => gpus(host),
        Cmd::Logs {
            name,
            lines,
            serial,
        } => {
            let vm = Registry::load()?.open(&name)?;
            let f = if serial {
                vm.serial_log()
            } else {
                vm.qemu_log()
            };
            if !f.exists() {
                bail!("{} does not exist yet", f.display());
            }
            println!("{}", runtime::tail(&f, lines));
        }
        Cmd::Report { name } => {
            let vm = Registry::load()?.open(&name)?;
            let p = logs::bundle(&vm, host)?;
            println!("wrote {}", p.display());
            if !vm.config.logs.enabled {
                println!(
                    "logs were off for {name}: turn them on (reims-ctl set {name} --logs), reproduce, and report again for the library's log"
                );
            }
        }
        Cmd::Update { channel } => update::update(channel)?,
        Cmd::Doctor => return doctor(host),
        Cmd::Tui => unreachable!(),
        Cmd::VulkanProbe => return Ok(gpu::probe_main()),
        Cmd::VzInstall {
            ipsw,
            dir,
            disk_mib,
            cpus,
            memory_mib,
        } => return crate::vz::install_main(&ipsw, &dir, disk_mib, cpus, memory_mib),
        Cmd::VzLatest => return crate::vz::latest_main(),
        Cmd::RecordInstall { repo, qemu, commit } => {
            crate::reims_util::install(&repo)?;
            install::Install {
                repo,
                commit,
                qemu,
                installed_at: runtime::now_stamp("%Y-%m-%d %H:%M:%S %z"),
            }
            .save()?;
        }
    }
    let _ = bool_arg;
    Ok(0)
}

pub fn script_comment(vm: &Vm) -> String {
    format!(
        "{}: the QEMU command reims-ctl {} runs for this VM.\nIts window opens under -display none; QMP is at the -qmp socket.",
        vm.name(),
        env!("CARGO_PKG_VERSION")
    )
}

fn list(json: bool) -> Result<()> {
    let reg = Registry::load()?;
    if json {
        let mut out = Vec::new();
        for e in &reg.vms {
            let (state, pid) = match Vm::load(&e.dir) {
                Ok(vm) => match runtime::pid(&vm) {
                    Some(p) => ("running", Some(p)),
                    None => ("stopped", None),
                },
                Err(_) => ("broken", None),
            };
            out.push(
                serde_json::json!({ "name": e.name, "dir": e.dir, "state": state, "pid": pid }),
            );
        }
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }
    if reg.vms.is_empty() {
        println!("No VMs yet: reims-ctl create, or reims-ctl for the interactive interface.");
        return Ok(());
    }
    println!(
        "{:<20} {:<9} {:>5} {:>7} {:<9} DIR",
        "NAME", "STATE", "CPUS", "MEMORY", "DISPLAYS"
    );
    for e in &reg.vms {
        match Vm::load(&e.dir) {
            Ok(vm) => {
                let c = &vm.config;
                let state = if runtime::running(&vm) {
                    "running"
                } else {
                    "stopped"
                };
                println!(
                    "{:<20} {:<9} {:>5} {:>7} {:<9} {}",
                    e.name,
                    state,
                    c.cpus,
                    format_mib(c.memory_mib),
                    format!("{}x {}", c.display.count, c.display.resolution),
                    paths::tilde(&e.dir)
                );
            }
            Err(err) => println!("{:<20} {:<9} {err:#}", e.name, "broken"),
        }
    }
    Ok(())
}

pub fn resolve_gpu(arg: &str) -> Result<vm::Gpu> {
    let a = arg.trim();
    if a.is_empty() || a == "auto" || a == "default" {
        return Ok(vm::Gpu::default());
    }
    if a.ends_with(".json") && std::path::Path::new(a).is_file() {
        return Ok(vm::Gpu {
            driver_file: Some(paths::absolute(a)),
            device_id: None,
            name: None,
        });
    }
    let list = gpu::list();
    if let Ok(n) = a.parse::<usize>()
        && n >= 1
        && n <= list.len()
    {
        return Ok(list[n - 1].to_config());
    }
    let low = a.to_ascii_lowercase();
    let hits: Vec<&gpu::GpuInfo> = list
        .iter()
        .filter(|g| g.name.to_ascii_lowercase().contains(&low))
        .collect();
    match hits.len() {
        1 => Ok(hits[0].to_config()),
        0 => bail!("no GPU matches {a:?}: reims-ctl gpus lists them"),
        _ => bail!("{a:?} matches several GPUs: give its number from reims-ctl gpus"),
    }
}

fn create_cmd(a: CreateArgs, host: Host) -> Result<()> {
    let name = a
        .name
        .clone()
        .or(a.name_pos.clone())
        .ok_or_else(|| with_create_examples("give the VM a name: reims-ctl create NAME"))?;
    // Nothing a VM is made from: whatever create says is missing, the examples show the ways.
    let source =
        a.opencore.is_some() || a.from_script.is_some() || a.ipsw.is_some() || a.bundle.is_some();
    let mut s = Spec::new(&name);
    s.dir = a
        .dir
        .as_ref()
        .map(|d| paths::absolute(&d.to_string_lossy()));
    s.cpus = a.cpus;
    s.memory_mib = a.memory.as_deref().map(parse_size_mib).transpose()?;
    s.disk = match (&a.disk_size, &a.disk) {
        (Some(sz), _) => Some(DiskSource::New {
            size_mib: parse_size_mib(sz)?,
        }),
        (None, Some(p)) => Some(DiskSource::Existing {
            path: paths::absolute(&p.to_string_lossy()),
            mode: ImportMode::parse(&a.disk_mode)?,
        }),
        (None, None) if a.from_script.is_none() && a.bundle.is_none() => Some(DiskSource::New {
            size_mib: if a.ipsw.is_some() {
                64 * 1024
            } else {
                128 * 1024
            },
        }),
        _ => None,
    };
    let abs = |p: &Option<PathBuf>| p.as_ref().map(|p| paths::absolute(&p.to_string_lossy()));
    s.opencore = abs(&a.opencore);
    s.installer = abs(&a.installer);
    s.from_script = abs(&a.from_script);
    s.firmware_code = abs(&a.firmware_code);
    s.firmware_vars = abs(&a.firmware_vars);
    s.cpu_model = a.cpu_model.clone();
    s.cpu_flags = a.cpu_flags.clone();
    s.mac = a.mac.clone();
    s.ipsw = a.ipsw.clone().map(|i| {
        if i == "latest" {
            i
        } else {
            paths::absolute(&i).to_string_lossy().into_owned()
        }
    });
    s.bundle = abs(&a.bundle);
    s.bundle_mode = ImportMode::parse(&a.bundle_mode)?;
    s.ecid = a.ecid;
    if let Some(n) = a.displays {
        s.display.count = n;
    }
    if let Some(r) = &a.resolution {
        s.display.resolution = r.clone();
    }
    s.display.hidpi = a.hidpi.unwrap_or(false);
    s.display.borderless = a.borderless.unwrap_or(false);
    s.display.refresh = a.refresh;
    s.display.check()?;
    if let Some(g) = &a.gpu {
        s.gpu = resolve_gpu(g)?;
    }
    s.logs = a.logs.unwrap_or(false);
    s.audio = a.audio.unwrap_or(true);
    s.ssh_port = a.ssh_port;
    let vm = create::create(s, host, &mut |p| println!("{name}: {p}")).map_err(|e| {
        if source {
            e
        } else {
            with_create_examples(format!("{e:#}"))
        }
    })?;
    println!("created {name} in {}", paths::tilde(&vm.dir));
    if a.start {
        let pid = runtime::start(&vm, host)?;
        println!("{name}: running, pid {pid}");
    }
    Ok(())
}

fn confirm(question: &str) -> bool {
    print!("{question} [y/N] ");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    std::io::stdin().read_line(&mut line).is_ok() && matches!(line.trim(), "y" | "Y" | "yes")
}

pub fn delete_vm(name: &str, keep_files: bool) -> Result<String> {
    let mut reg = Registry::load()?;
    let entry = reg
        .find(name)
        .cloned()
        .with_context(|| format!("no VM named {name}"))?;
    if let Ok(vm) = Vm::load(&entry.dir)
        && runtime::running(&vm)
    {
        bail!("{name} is running: stop it first");
    }
    reg.remove(name);
    reg.save()?;
    if keep_files {
        return Ok(format!(
            "{name} deleted; its files stay in {}",
            paths::tilde(&entry.dir)
        ));
    }
    if entry.dir.join(vm::CONFIG_FILE).is_file() {
        std::fs::remove_dir_all(&entry.dir)
            .with_context(|| format!("remove {}", entry.dir.display()))?;
        Ok(format!("{name} deleted, with {}", paths::tilde(&entry.dir)))
    } else {
        Ok(format!(
            "{name} deleted; {} holds no vm.toml, so it stays",
            paths::tilde(&entry.dir)
        ))
    }
}

fn delete(name: &str, keep_files: bool, yes: bool) -> Result<()> {
    let reg = Registry::load()?;
    let entry = reg
        .find(name)
        .cloned()
        .with_context(|| format!("no VM named {name}"))?;
    if !yes {
        let what = if keep_files {
            "keeping its files".to_string()
        } else {
            format!("and {} with its disks", paths::tilde(&entry.dir))
        };
        if !confirm(&format!("Delete {name}, {what}?")) {
            println!("nothing deleted");
            return Ok(());
        }
    }
    println!("{}", delete_vm(name, keep_files)?);
    Ok(())
}

/// Lines describing a VM, for `show` and the interface's details.
pub fn describe(vm: &Vm, host: Host) -> Vec<(String, String)> {
    let c = &vm.config;
    let mut v: Vec<(String, String)> = Vec::new();
    let mut add = |k: &str, val: String| v.push((k.into(), val));
    add(
        "state",
        match runtime::pid(vm) {
            Some(p) => format!("running (pid {p})"),
            None => "stopped".into(),
        },
    );
    add("kind", c.profile.describe().into());
    add("directory", paths::tilde(&vm.dir));
    let mut cpus = c.cpus.to_string();
    if c.cpu.threads_per_core > 1 {
        cpus.push_str(&format!(
            " ({} cores of {} threads)",
            c.cpus / c.cpu.threads_per_core,
            c.cpu.threads_per_core
        ));
    }
    if c.cpu.pins() {
        cpus.push_str(&format!(", pinned {}", c.cpu.pin));
        if c.cpu.confine.trim() == "llc" {
            cpus.push_str(", QEMU kept in their L3");
        }
    }
    add("cpus", cpus);
    add("memory", format_mib(c.memory_mib));
    // A disk in the VM's directory by its file name: the directory is the line above.
    let disk = vm.disk();
    let disk_name = match disk.strip_prefix(&vm.dir) {
        Ok(rel) => rel.display().to_string(),
        Err(_) => paths::tilde(&disk),
    };
    add("disk", format!("{disk_name} ({})", c.disk_format.as_str()));
    let mut disp = format!("{} x {}", c.display.count, c.display.resolution);
    if c.display.hidpi {
        disp.push_str(" HiDPI");
    }
    if let Some((w, h)) = c.display.looks_like() {
        disp.push_str(&format!(" (looks like {w}x{h})"));
    }
    if c.display.borderless {
        disp.push_str(", borderless");
    }
    if let Some(r) = c.display.refresh {
        disp.push_str(&format!(", {r} Hz"));
    }
    add("displays", disp);
    if c.profile.gpu_choice(host) {
        add(
            "gpu",
            match (&c.gpu.name, &c.gpu.driver_file) {
                (Some(n), _) => n.clone(),
                (None, Some(f)) => paths::tilde(f),
                _ => "automatic (discrete first)".into(),
            },
        );
    }
    if let Some(q) = &c.q35 {
        add("opencore", paths::tilde(&vm.path(&q.opencore)));
        add(
            "installer",
            match &q.installer {
                Some(i) => format!(
                    "{} ({})",
                    paths::tilde(&vm.path(i)),
                    if q.installer_kind == Some(vm::MediaKind::Disk) {
                        "disk"
                    } else {
                        "CD"
                    }
                ),
                None => "none".into(),
            },
        );
        add("cpu model", q.cpu_model.clone());
    }
    if let Some(a) = &c.vmapple {
        add("ecid", a.ecid.to_string());
    }
    add(
        "audio",
        if c.audio.enabled {
            c.audio.backend.clone().unwrap_or_else(|| "on".into())
        } else {
            "off".into()
        },
    );
    add(
        "network",
        if !c.network.enabled {
            "off".into()
        } else {
            match c.network.ssh_port {
                Some(p) => format!("user, mac {}, ssh on 127.0.0.1:{p}", c.network.mac),
                None => format!("user, mac {}", c.network.mac),
            }
        },
    );
    add(
        "logs",
        if c.logs.enabled {
            format!("on ({})", c.logs.level)
        } else {
            "off".into()
        },
    );
    add("stop timeout", format!("{} s", c.stop_timeout_secs));
    if !c.extra_args.is_empty() {
        add(
            "extra args",
            c.extra_args
                .iter()
                .map(|a| qemu::quote(a))
                .collect::<Vec<_>>()
                .join(" "),
        );
    }
    for (k, val) in &c.extra_env {
        add("env", format!("{k}={val}"));
    }
    v
}

fn show(vm: &Vm, host: Host) {
    println!("{}", vm.name());
    for (k, v) in describe(vm, host) {
        println!("  {k:<13} {v}");
    }
}

/// Sets the installer, and changes the running VM's disc where it can. Returns what happened.
/// A DMG is converted into the VM's directory first (`create::installer_media`), and the VM's
/// copy of the installer it replaces is deleted.
pub fn set_installer(
    vm: &mut Vm,
    path: Option<PathBuf>,
    progress: &mut dyn FnMut(&str),
) -> Result<String> {
    let running = runtime::running(vm);
    let name = vm.name().to_string();
    let dir = vm.dir.clone();
    let q = vm
        .config
        .q35
        .as_mut()
        .context("only x86-64 VMs take installer media")?;
    let old = q.installer.clone();
    let old_kind = old
        .as_ref()
        .map(|_| q.installer_kind.unwrap_or(vm::MediaKind::Cdrom));
    if let Some(p) = &path
        && !p.is_file()
    {
        bail!("{} does not exist", p.display());
    }
    // The VM's copy, named again by its path (the TUI shows it so), stays as it is.
    let copy = old
        .as_ref()
        .filter(|o| o.is_relative())
        .map(|o| dir.join(o));
    let installer = match &path {
        Some(p) if copy.as_deref().is_some_and(|c| same_file(c, p)) => old.clone(),
        Some(p) => Some(create::installer_media(p, &dir, progress)?),
        None => None,
    };
    let new_kind = installer.as_deref().map(vm::MediaKind::for_path);
    q.installer = installer.clone();
    q.installer_kind = new_kind;
    vm.save()?;
    if let Some(c) = &copy
        && installer != old
    {
        let _ = std::fs::remove_file(c);
    }
    if !running {
        return Ok(match &path {
            Some(p) => format!("{name}: {} attached for the next start", paths::tilde(p)),
            None => format!("{name}: installer removed"),
        });
    }
    // Live: only a CD drive's disc changes; a disk installer changes at the next start.
    let live = old_kind != Some(vm::MediaKind::Disk) && new_kind != Some(vm::MediaKind::Disk);
    if live {
        runtime::change_cd(vm, path.as_deref())?;
        Ok(match &path {
            Some(p) => format!("{name}: {} is in the CD drive", paths::tilde(p)),
            None => format!("{name}: CD ejected"),
        })
    } else {
        Ok(format!("{name}: the installer changes at the next start"))
    }
}

/// Whether `a` and `b` name the same existing file.
fn same_file(a: &Path, b: &Path) -> bool {
    matches!(
        (std::fs::canonicalize(a), std::fs::canonicalize(b)),
        (Ok(a), Ok(b)) if a == b
    )
}

fn set(a: SetArgs, host: Host) -> Result<()> {
    let mut vm = Registry::load()?.open(&a.name)?;
    let c = &mut vm.config;
    if let Some(n) = a.cpus {
        c.cpus = n;
    }
    if let Some(m) = &a.memory {
        c.memory_mib = parse_size_mib(m)?;
    }
    if let Some(t) = a.threads_per_core {
        c.cpu.threads_per_core = t;
    }
    if let Some(p) = &a.pin {
        c.cpu.pin = p.trim().to_owned();
    }
    if let Some(v) = &a.confine {
        c.cpu.confine = v.trim().to_owned();
    }
    if let Some(n) = a.displays {
        c.display.count = n;
    }
    if let Some(r) = &a.resolution {
        c.display.resolution = r.clone();
    }
    if let Some(b) = a.hidpi {
        c.display.hidpi = b;
    }
    if let Some(b) = a.borderless {
        c.display.borderless = b;
    }
    if let Some(r) = a.refresh {
        c.display.refresh = (r != 0).then_some(r);
    }
    if let Some(g) = &a.gpu {
        c.gpu = resolve_gpu(g)?;
    }
    if let Some(b) = a.logs {
        c.logs.enabled = b;
    }
    if let Some(l) = &a.log_level {
        c.logs.level = l.clone();
    }
    if let Some(b) = a.audio {
        c.audio.enabled = b;
    }
    if let Some(b) = &a.audio_backend {
        c.audio.backend = (b != "auto" && !b.is_empty()).then(|| b.clone());
    }
    if let Some(b) = a.network {
        c.network.enabled = b;
    }
    if let Some(p) = a.ssh_port {
        c.network.ssh_port = (p != 0).then_some(p);
    }
    if let Some(m) = &a.mac {
        c.network.mac = m.clone();
    }
    if let Some(t) = a.stop_timeout {
        c.stop_timeout_secs = t;
    }
    if a.cpu_model.is_some() || a.cpu_flags.is_some() {
        let q = c
            .q35
            .as_mut()
            .context("--cpu-model and --cpu-flags are x86-64 settings")?;
        if let Some(m) = &a.cpu_model {
            q.cpu_model = m.clone();
        }
        if let Some(f) = &a.cpu_flags {
            q.cpu_flags = f.clone();
        }
    }
    if let Some(x) = &a.extra_args {
        c.extra_args = shlex::split(x).context("--extra-args: unbalanced quotes")?;
    }
    for kv in &a.env {
        let (k, v) = kv
            .split_once('=')
            .with_context(|| format!("--env takes KEY=VALUE, not {kv:?}"))?;
        if v.is_empty() {
            c.extra_env.remove(k);
        } else {
            c.extra_env.insert(k.into(), v.into());
        }
    }
    vm.save()?;
    let note = if runtime::running(&vm) {
        " (at its next start)"
    } else {
        ""
    };
    println!("{}: saved{note}", vm.name());
    show(&vm, host);
    Ok(())
}

fn gpus(host: Host) {
    if host.os != crate::host::Os::Linux {
        println!(
            "The library runs on the system's default Metal device on {}: there is no GPU to choose.",
            qemu::os_word(host)
        );
        return;
    }
    let list = gpu::list();
    if list.is_empty() {
        println!(
            "No Vulkan GPU found (is the Vulkan loader, libvulkan.so.1, installed with your GPU's driver?)"
        );
        return;
    }
    println!("By default the library takes the first discrete GPU, then the first integrated one.");
    for (i, g) in list.iter().enumerate() {
        println!(
            "{:>2}. {}  [{:04x}:{:04x}]  {}",
            i + 1,
            g.name,
            g.vendor_id,
            g.device_id,
            g.driver_file.display()
        );
    }
    println!("Give a VM one with: reims-ctl set NAME --gpu NUMBER");
}

fn doctor(host: Host) -> Result<i32> {
    let mut bad = 0;
    let mut line = |ok: bool, what: &str, detail: String| {
        if !ok {
            bad += 1;
        }
        println!("{} {what}: {detail}", if ok { "ok  " } else { "FAIL" });
    };
    match crate::reims_util::binary() {
        Ok(bin) => line(
            true,
            "reims-util",
            format!(
                "{} ({})",
                bin.display(),
                crate::reims_util::version(&bin).unwrap_or_default()
            ),
        ),
        Err(e) => line(false, "reims-util", format!("{e:#}")),
    }
    let profile = host.default_profile();
    line(
        profile.is_some(),
        "host",
        format!(
            "{host}{}",
            profile
                .map(|p| format!(", runs {p} VMs"))
                .unwrap_or_else(|| ", which reims-ctl does not support yet".into())
        ),
    );
    line(
        true,
        "files",
        format!(
            "settings in {}, VMs and QEMU in {}",
            paths::tilde(&paths::config_dir()),
            paths::tilde(&paths::data_dir())
        ),
    );
    let Some(profile) = profile else { return Ok(1) };
    match install::qemu_binary(profile) {
        Ok(q) => {
            line(
                true,
                "qemu",
                format!(
                    "{} ({})",
                    q.display(),
                    install::qemu_version(&q).unwrap_or_default()
                ),
            );
            let b = install::audio_backends(&q);
            line(
                true,
                "audio backends",
                format!(
                    "{} (default {})",
                    b.join(", "),
                    install::default_audio_backend(&b)
                ),
            );
        }
        Err(e) => line(false, "qemu", format!("{e:#}")),
    }
    match install::Install::load() {
        Some(i) => line(
            true,
            "checkout",
            format!(
                "{} at {}, following {}",
                i.repo.display(),
                i.commit,
                update::following(&i.repo)
            ),
        ),
        None => line(
            false,
            "checkout",
            "no install record: reims-ctl update needs scripts/install-ctl.sh to have run".into(),
        ),
    }
    match host.os {
        crate::host::Os::Linux => {
            let kvm = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open("/dev/kvm");
            line(
                kvm.is_ok(),
                "/dev/kvm",
                match kvm {
                    Ok(_) => "usable".into(),
                    Err(e) => format!(
                        "{e}: add yourself to the kvm group (sudo usermod -aG kvm $USER) and log in again"
                    ),
                },
            );
            if let Some(n) = std::fs::read_to_string("/proc/sys/vm/max_map_count")
                .ok()
                .and_then(|s| s.trim().parse::<u64>().ok())
            {
                line(
                    n >= 1_048_576,
                    "vm.max_map_count",
                    if n >= 1_048_576 {
                        n.to_string()
                    } else {
                        format!(
                            "{n}: sudo sysctl -w vm.max_map_count=1048576 (a 12 GiB guest needs about that)"
                        )
                    },
                );
            }
            let gpus = gpu::list();
            line(
                !gpus.is_empty(),
                "vulkan",
                if gpus.is_empty() {
                    "no GPU found".into()
                } else {
                    gpus.iter()
                        .map(|g| g.label())
                        .collect::<Vec<_>>()
                        .join("; ")
                },
            );
            let session = std::env::var_os("WAYLAND_DISPLAY").is_some()
                || std::env::var_os("DISPLAY").is_some();
            line(
                session,
                "desktop session",
                if session {
                    "found".into()
                } else {
                    "neither WAYLAND_DISPLAY nor DISPLAY is set: a VM's window needs a desktop session".into()
                },
            );
        }
        crate::host::Os::Mac => {
            line(
                std::path::Path::new(qemu::AVPBOOTER).exists(),
                "vmapple boot ROM",
                qemu::AVPBOOTER.into(),
            );
            line(
                crate::vz::entitled(),
                "virtualization entitlement",
                if crate::vz::entitled() {
                    "present (installs from IPSWs)".into()
                } else {
                    "missing: installing from an IPSW needs it; rerun scripts/install-ctl.sh".into()
                },
            );
        }
        crate::host::Os::Other => {}
    }
    if profile == Profile::Q35X86_64 {
        // An OpenCore folder can bring its own; this is what a VM without one boots.
        let fw = create::find_firmware(std::path::Path::new(""));
        line(
            fw.is_some(),
            "OVMF firmware",
            match fw {
                Some((code, _)) => code.display().to_string(),
                None => "not found: install your system's OVMF package (Ubuntu's ovmf, Arch's \
                         edk2-ovmf), or use an OpenCore folder that carries OVMF"
                    .into(),
            },
        );
    }
    Ok(if bad == 0 { 0 } else { 1 })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_change_of_installer_deletes_only_the_vms_own_copy() {
        let d = std::env::temp_dir().join(format!("reims-ctl-installer-{}", std::process::id()));
        std::fs::create_dir_all(d.join("vm")).unwrap();
        let config = r#"
            name = "t"
            profile = "q35-x86_64"
            cpus = 2
            memory_mib = 4096
            disk = "disk.qcow2"
            [q35]
            opencore = "opencore.qcow2"
            firmware_code = "OVMF_CODE.fd"
            firmware_vars = "OVMF_VARS.fd"
            installer = "installer.img"
            installer_kind = "disk"
        "#;
        let mut vm = Vm {
            dir: d.join("vm"),
            config: toml::from_str(config).unwrap(),
        };
        let copy = d.join("vm").join(create::INSTALLER_COPY);
        let user = d.join("BaseSystem.img");
        std::fs::write(&copy, [0u8; 4096]).unwrap();
        std::fs::write(&user, [0u8; 4096]).unwrap();
        let installer = |vm: &Vm| vm.config.q35.as_ref().unwrap().installer.clone();
        let mut quiet = |_: &str| {};
        // The copy, named by its path (as the TUI shows it), stays the VM's copy.
        set_installer(&mut vm, Some(copy.clone()), &mut quiet).unwrap();
        assert_eq!(installer(&vm), Some(create::INSTALLER_COPY.into()));
        assert!(copy.is_file());
        // Another installer replaces it, and the copy goes.
        set_installer(&mut vm, Some(user.clone()), &mut quiet).unwrap();
        assert_eq!(installer(&vm), Some(user.clone()));
        assert!(!copy.exists());
        // An installer used in place is never deleted.
        set_installer(&mut vm, None, &mut quiet).unwrap();
        assert_eq!(installer(&vm), None);
        assert!(user.is_file());
        std::fs::remove_dir_all(&d).unwrap();
    }
}
