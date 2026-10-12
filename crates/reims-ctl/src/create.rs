//! Making a VM: its directory, disks, firmware and `vm.toml`, then its registry entry.
//!
//! An x86-64 VM (`q35-x86_64`) boots OpenCore from its own copy of an OpenCore image, with its
//! own copies of OVMF's halves, and a new disk or an existing one (copied, reflinked where the
//! file system can; a qcow2 overlay over it; or used in place). An existing OSX-KVM or ULTMOS
//! launch script can supply what it names: the disk, OpenCore image, firmware, installer, CPU
//! model and flags, and MAC. An installer is used in place, except a DMG, which qemu-img
//! converts to a raw image in the VM's directory ([`installer_media`]).
//!
//! An arm64 VM (`vmapple-arm64`) is one Virtualization.framework made: installed here from an
//! IPSW (vz.rs), or imported from macosvm's or tart's directory (an APFS clone of its disk and
//! auxiliary storage, or used in place).

use crate::host::{Host, Profile};
use crate::install;
use crate::paths;
use crate::vm::*;
use anyhow::{Context, Result, bail};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImportMode {
    /// A copy in the VM's directory: reflinked (btrfs, XFS) or cloned (APFS) where it can be.
    Copy,
    /// A qcow2 overlay over the image, which must then stay.
    Overlay,
    /// The image itself.
    InPlace,
}

impl ImportMode {
    pub fn parse(s: &str) -> Result<ImportMode> {
        Ok(match s {
            "copy" | "clone" => ImportMode::Copy,
            "overlay" => ImportMode::Overlay,
            "in-place" | "inplace" | "link" => ImportMode::InPlace,
            _ => bail!("import mode is copy, overlay or in-place, not {s:?}"),
        })
    }
}

#[derive(Clone, Debug)]
pub enum DiskSource {
    New { size_mib: u64 },
    Existing { path: PathBuf, mode: ImportMode },
}

#[derive(Clone, Debug)]
pub struct Spec {
    pub name: String,
    pub dir: Option<PathBuf>,
    pub profile: Option<Profile>,
    pub cpus: Option<u32>,
    pub memory_mib: Option<u64>,
    pub disk: Option<DiskSource>,
    pub display: Display,
    pub gpu: Gpu,
    pub logs: bool,
    pub audio: bool,
    pub ssh_port: Option<u16>,
    pub mac: Option<String>,
    // q35
    pub from_script: Option<PathBuf>,
    pub opencore: Option<PathBuf>,
    pub installer: Option<PathBuf>,
    pub firmware_code: Option<PathBuf>,
    pub firmware_vars: Option<PathBuf>,
    pub cpu_model: Option<String>,
    pub cpu_flags: Option<String>,
    // vmapple
    pub ipsw: Option<String>,
    pub bundle: Option<PathBuf>,
    pub bundle_mode: ImportMode,
    pub ecid: Option<u64>,
}

impl Spec {
    pub fn new(name: &str) -> Spec {
        Spec {
            name: name.into(),
            dir: None,
            profile: None,
            cpus: None,
            memory_mib: None,
            disk: None,
            display: Display::default(),
            gpu: Gpu::default(),
            logs: false,
            audio: true,
            ssh_port: None,
            mac: None,
            from_script: None,
            opencore: None,
            installer: None,
            firmware_code: None,
            firmware_vars: None,
            cpu_model: None,
            cpu_flags: None,
            ipsw: None,
            bundle: None,
            bundle_mode: ImportMode::Copy,
            ecid: None,
        }
    }
}

/// The host's logical CPUs.
pub fn host_cpus() -> u32 {
    std::thread::available_parallelism()
        .map(|n| n.get() as u32)
        .unwrap_or(4)
}

/// The host's RAM in MiB.
pub fn host_memory_mib() -> u64 {
    #[cfg(target_os = "macos")]
    {
        let mut v: u64 = 0;
        let mut size = std::mem::size_of::<u64>();
        let name = c"hw.memsize";
        // SAFETY: hw.memsize is a u64.
        if unsafe {
            libc::sysctlbyname(
                name.as_ptr(),
                (&mut v as *mut u64).cast(),
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        } == 0
        {
            return v / (1024 * 1024);
        }
        16 * 1024
    }
    #[cfg(not(target_os = "macos"))]
    {
        std::fs::read_to_string("/proc/meminfo")
            .ok()
            .and_then(|m| {
                m.lines()
                    .find(|l| l.starts_with("MemTotal:"))
                    .and_then(|l| l.split_whitespace().nth(1)?.parse::<u64>().ok())
            })
            .map(|kib| kib / 1024)
            .unwrap_or(16 * 1024)
    }
}

pub fn default_cpus() -> u32 {
    (host_cpus() / 2).clamp(2, 8)
}

pub fn default_memory_mib() -> u64 {
    ((host_memory_mib() / 2) / 1024 * 1024).clamp(4096, 16384)
}

pub fn default_dir(name: &str) -> PathBuf {
    paths::default_vms_dir().join(name)
}

/// Makes the VM, calling `progress` with each step, and registers it.
pub fn create(spec: Spec, host: Host, progress: &mut dyn FnMut(&str)) -> Result<Vm> {
    check_name(&spec.name)?;
    let mut reg = Registry::load()?;
    if reg.find(&spec.name).is_some() {
        bail!("a VM named {} already exists", spec.name);
    }
    let profile = match spec.profile {
        Some(p) => p,
        None => host
            .default_profile()
            .with_context(|| format!("reims-ctl runs no VMs on {host} yet"))?,
    };
    if !Profile::supported_on(host).contains(&profile) {
        bail!("{host} cannot run {profile} VMs");
    }
    let dir = spec.dir.clone().unwrap_or_else(|| default_dir(&spec.name));
    let made = !dir.exists();
    if !made && std::fs::read_dir(&dir)?.next().is_some() {
        bail!(
            "{} is not empty: choose a new or empty directory",
            dir.display()
        );
    }
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let result = match profile {
        Profile::Q35X86_64 => make_q35(&spec, host, &dir, progress),
        Profile::VmappleArm64 => make_vmapple(&spec, host, &dir, progress),
    };
    let vm = match result {
        Ok(vm) => vm,
        Err(e) => {
            if made {
                let _ = std::fs::remove_dir_all(&dir);
            } else if let Ok(rd) = std::fs::read_dir(&dir) {
                for e in rd.flatten() {
                    let _ = if e.path().is_dir() {
                        std::fs::remove_dir_all(e.path())
                    } else {
                        std::fs::remove_file(e.path())
                    };
                }
            }
            return Err(e);
        }
    };
    vm.save()?;
    reg.add(&spec.name, &dir)?;
    reg.save()?;
    progress("done");
    Ok(vm)
}

fn base_config(spec: &Spec, profile: Profile, disk: PathBuf, disk_format: ImageFormat) -> VmConfig {
    VmConfig {
        name: spec.name.clone(),
        profile,
        cpus: spec.cpus.unwrap_or_else(default_cpus),
        memory_mib: spec.memory_mib.unwrap_or_else(default_memory_mib),
        cpu: Default::default(),
        disk,
        disk_format,
        stop_timeout_secs: 90,
        extra_args: vec![],
        display: spec.display.clone(),
        gpu: spec.gpu.clone(),
        audio: Audio {
            enabled: spec.audio,
            backend: None,
        },
        network: Network {
            enabled: true,
            mac: spec.mac.clone().unwrap_or_else(random_mac),
            ssh_port: spec.ssh_port,
        },
        logs: Logs {
            enabled: spec.logs,
            ..Logs::default()
        },
        q35: None,
        vmapple: None,
        extra_env: BTreeMap::new(),
    }
}

fn file_name(p: &Path) -> String {
    p.file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Copies `src` to `dst`: a clone on APFS, a reflink where Linux's file system can, else a
/// sparse copy.
pub fn copy_file(src: &Path, dst: &Path) -> Result<()> {
    let ok = if cfg!(target_os = "macos") {
        Command::new("cp")
            .arg("-c")
            .arg(src)
            .arg(dst)
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
            || Command::new("cp")
                .arg(src)
                .arg(dst)
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
    } else {
        Command::new("cp")
            .args(["--reflink=auto", "--sparse=always"])
            .arg(src)
            .arg(dst)
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    };
    if !ok {
        bail!("could not copy {} to {}", src.display(), dst.display());
    }
    Ok(())
}

fn qemu_img(profile: Profile, args: &[&std::ffi::OsStr]) -> Result<()> {
    let img = install::qemu_img(profile)?;
    let out = Command::new(&img)
        .args(args)
        .output()
        .with_context(|| format!("run {}", img.display()))?;
    if !out.status.success() {
        bail!(
            "qemu-img failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

fn make_q35(spec: &Spec, _host: Host, dir: &Path, progress: &mut dyn FnMut(&str)) -> Result<Vm> {
    let script = match &spec.from_script {
        Some(p) => Some(LaunchScript::read(p)?),
        None => None,
    };
    let s = script.clone().unwrap_or_default();

    // The disk.
    let disk_source = spec.disk.clone().or_else(|| {
        s.disk.clone().map(|path| DiskSource::Existing {
            path,
            mode: ImportMode::Copy,
        })
    });
    let (disk, disk_format) = match disk_source {
        Some(DiskSource::New { size_mib }) => {
            progress("creating the disk");
            let d = dir.join("disk.qcow2");
            qemu_img(
                Profile::Q35X86_64,
                &[
                    "create".as_ref(),
                    "-q".as_ref(),
                    "-f".as_ref(),
                    "qcow2".as_ref(),
                    d.as_os_str(),
                    format!("{size_mib}M").as_ref(),
                ],
            )?;
            (PathBuf::from("disk.qcow2"), ImageFormat::Qcow2)
        }
        Some(DiskSource::Existing { path, mode }) => {
            import_disk(&path, mode, dir, "disk", progress)?
        }
        None => bail!("give the VM a disk: a new one's size, or an existing image"),
    };

    // OpenCore.
    let opencore_src = spec.opencore.clone().or(s.opencore.clone()).context(
        "an x86-64 VM boots through an OpenCore image (OpenCore.qcow2, from OSX-KVM or ULTMOS): give its path",
    )?;
    progress("copying the OpenCore image");
    let oc_format = ImageFormat::detect(&opencore_src)?;
    let oc_name = format!(
        "opencore.{}",
        if oc_format == ImageFormat::Qcow2 {
            "qcow2"
        } else {
            "img"
        }
    );
    copy_file(&opencore_src, &dir.join(&oc_name))?;

    // Firmware.
    let (code, vars) = match (
        spec.firmware_code.clone().or(s.firmware_code.clone()),
        spec.firmware_vars.clone().or(s.firmware_vars.clone()),
    ) {
        (Some(c), Some(v)) => (c, v),
        (c, v) => {
            let (fc, fv) = find_firmware(&opencore_src).context(
                "no OVMF firmware found beside the OpenCore image, in QEMU's install or in the \
                 system's OVMF package: install it (Ubuntu's ovmf, Arch's edk2-ovmf), or give \
                 --firmware-code and --firmware-vars",
            )?;
            (c.unwrap_or(fc), v.unwrap_or(fv))
        }
    };
    progress("copying the firmware");
    copy_file(&code, &dir.join("OVMF_CODE.fd"))?;
    copy_file(&vars, &dir.join("OVMF_VARS.fd"))?;
    // A read-only source's copy is read-only too; the variables are written.
    let _ = make_writable(&dir.join("OVMF_VARS.fd"));

    let installer = match spec.installer.clone().or(s.installer.clone()) {
        Some(i) if !i.exists() => bail!("the installer {} does not exist", i.display()),
        Some(i) => Some(installer_media(&i, dir, progress)?),
        None => None,
    };
    let mut c = base_config(spec, Profile::Q35X86_64, disk, disk_format);
    if spec.cpus.is_none()
        && let Some(n) = s.cpus
    {
        c.cpus = n;
    }
    if spec.memory_mib.is_none()
        && let Some(m) = s.memory_mib
    {
        c.memory_mib = m;
    }
    if spec.mac.is_none()
        && let Some(m) = &s.mac
    {
        c.network.mac = m.clone();
    }
    c.q35 = Some(Q35 {
        opencore: oc_name.into(),
        opencore_format: oc_format,
        firmware_code: "OVMF_CODE.fd".into(),
        firmware_vars: "OVMF_VARS.fd".into(),
        installer_kind: installer.as_deref().map(MediaKind::for_path),
        installer,
        cpu_model: spec
            .cpu_model
            .clone()
            .or(s.cpu_model.clone())
            .unwrap_or_else(default_cpu_model),
        cpu_flags: spec
            .cpu_flags
            .clone()
            .or(s.cpu_flags.clone())
            .unwrap_or_else(default_cpu_flags),
        disk_port: s.disk_port.unwrap_or(3),
    });
    Ok(Vm {
        dir: dir.to_path_buf(),
        config: c,
    })
}

fn make_writable(p: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perm = std::fs::metadata(p)?.permissions();
    perm.set_mode(perm.mode() | 0o600);
    std::fs::set_permissions(p, perm)
}

fn import_disk(
    src: &Path,
    mode: ImportMode,
    dir: &Path,
    stem: &str,
    progress: &mut dyn FnMut(&str),
) -> Result<(PathBuf, ImageFormat)> {
    if !src.is_file() {
        bail!("the disk {} does not exist", src.display());
    }
    let src = std::fs::canonicalize(src)?;
    let format = ImageFormat::detect(&src)?;
    Ok(match mode {
        ImportMode::InPlace => (src, format),
        ImportMode::Copy => {
            progress(&format!(
                "copying {} (a reflink or clone where the file system can)",
                file_name(&src)
            ));
            let ext = if format == ImageFormat::Qcow2 {
                "qcow2"
            } else {
                "img"
            };
            let name = format!("{stem}.{ext}");
            copy_file(&src, &dir.join(&name))?;
            let _ = make_writable(&dir.join(&name));
            (PathBuf::from(name), format)
        }
        ImportMode::Overlay => {
            progress(&format!("creating an overlay over {}", file_name(&src)));
            let name = format!("{stem}.qcow2");
            let d = dir.join(&name);
            qemu_img(
                Profile::Q35X86_64,
                &[
                    "create".as_ref(),
                    "-q".as_ref(),
                    "-f".as_ref(),
                    "qcow2".as_ref(),
                    "-b".as_ref(),
                    src.as_os_str(),
                    "-F".as_ref(),
                    format.as_str().as_ref(),
                    d.as_os_str(),
                ],
            )?;
            (PathBuf::from(name), ImageFormat::Qcow2)
        }
    })
}

/// The VM's raw copy of an installer DMG, in its directory.
pub const INSTALLER_COPY: &str = "installer.img";

/// Whether `p` is a DMG QEMU reads only through its dmg driver: a UDIF image, which ends in a
/// 512-byte trailer starting `koly`. A read/write DMG (UDRW) has none: it is a raw image.
pub fn is_dmg(p: &Path) -> Result<bool> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(p).with_context(|| format!("open {}", p.display()))?;
    let len = f.metadata()?.len();
    if len < 512 {
        return Ok(false);
    }
    f.seek(SeekFrom::Start(len - 512))?;
    let mut magic = [0u8; 4];
    f.read_exact(&mut magic)?;
    Ok(magic == *b"koly")
}

/// The installer a VM in `dir` attaches for `src`: `src` itself, used in place, or, for a DMG
/// (the BaseSystem.dmg OSX-KVM's fetch-macOS-v2.py downloads), a raw copy qemu-img writes into
/// `dir`, named relative to it. QEMU could read the DMG itself, but decompresses a whole chunk
/// for each read outside the last one: 4 KiB reads 1 MiB apart took 2.5 ms each from a 2.5 GiB
/// recovery image against 0.05 ms from its raw copy, which took 10 s to make (measured on a
/// Ryzen 7 7840HS).
///
/// The installed QEMU reads zlib DMGs (the recovery image fetch-macOS-v2.py downloaded is one);
/// bzip2 and LZFSE ones only where it was built with their libraries, and LZMA and ADC ones never.
pub fn installer_media(src: &Path, dir: &Path, progress: &mut dyn FnMut(&str)) -> Result<PathBuf> {
    if !is_dmg(src)? {
        return Ok(src.to_path_buf());
    }
    progress(&format!("converting {} to a raw image", file_name(src)));
    // Written beside the copy and renamed over it: a running VM may have it open.
    let part = dir.join(format!("{INSTALLER_COPY}.part"));
    let converted = qemu_img(
        Profile::Q35X86_64,
        &[
            "convert".as_ref(),
            "-q".as_ref(),
            "-f".as_ref(),
            "dmg".as_ref(),
            "-O".as_ref(),
            "raw".as_ref(),
            src.as_os_str(),
            part.as_os_str(),
        ],
    );
    if let Err(e) = converted {
        let _ = std::fs::remove_file(&part);
        let e = format!("{e:#}");
        // qemu-img's warning names the compression: "dmg-lzfse module is missing", "chunks
        // of unknown type".
        if e.contains("module is missing") || e.contains("unknown type") {
            bail!(
                "{e}\n{} is compressed in a way this QEMU does not read: on a Mac, `hdiutil \
                 convert {} -format UDZO -o <new>.dmg` makes a zlib copy, which it does",
                file_name(src),
                crate::qemu::quote(&src.to_string_lossy())
            );
        }
        bail!("{e}");
    }
    std::fs::rename(&part, dir.join(INSTALLER_COPY))
        .with_context(|| format!("rename {}", part.display()))?;
    Ok(PathBuf::from(INSTALLER_COPY))
}

/// OVMF's code and variables: beside the OpenCore image (OSX-KVM keeps them in its root, ULTMOS
/// in `ovmf/`), else QEMU's own edk2 build, else the system's edk2/OVMF package.
pub fn find_firmware(opencore: &Path) -> Option<(PathBuf, PathBuf)> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(d) = opencore.parent() {
        for base in [Some(d), d.parent()].into_iter().flatten() {
            dirs.push(base.to_path_buf());
            dirs.push(base.join("ovmf"));
            dirs.push(base.join("OVMF"));
        }
    }
    for d in &dirs {
        let code = ["OVMF_CODE.fd", "OVMF_CODE_4M.fd", "OVMF_CODE.4m.fd"]
            .iter()
            .map(|n| d.join(n))
            .find(|p| p.is_file());
        let vars = [
            "OVMF_VARS.fd",
            "OVMF_VARS-1920x1080.fd",
            "OVMF_VARS_4M.fd",
            "OVMF_VARS.4m.fd",
        ]
        .iter()
        .map(|n| d.join(n))
        .find(|p| p.is_file());
        if let (Some(c), Some(v)) = (code, vars) {
            return Some((c, v));
        }
    }
    if let Some(share) = install::qemu_binary(Profile::Q35X86_64)
        .ok()
        .and_then(|q| install::qemu_data_dir(&q))
    {
        let (c, v) = (
            share.join("edk2-x86_64-code.fd"),
            share.join("edk2-i386-vars.fd"),
        );
        if c.is_file() && v.is_file() {
            return Some((c, v));
        }
    }
    for (c, v) in [
        (
            "/usr/share/OVMF/OVMF_CODE_4M.fd",
            "/usr/share/OVMF/OVMF_VARS_4M.fd",
        ),
        (
            "/usr/share/edk2/x64/OVMF_CODE.4m.fd",
            "/usr/share/edk2/x64/OVMF_VARS.4m.fd",
        ),
        (
            "/usr/share/edk2/ovmf/OVMF_CODE.fd",
            "/usr/share/edk2/ovmf/OVMF_VARS.fd",
        ),
        (
            "/usr/share/OVMF/OVMF_CODE.fd",
            "/usr/share/OVMF/OVMF_VARS.fd",
        ),
    ] {
        if Path::new(c).is_file() && Path::new(v).is_file() {
            return Some((c.into(), v.into()));
        }
    }
    None
}

fn make_vmapple(spec: &Spec, host: Host, dir: &Path, progress: &mut dyn FnMut(&str)) -> Result<Vm> {
    if let Some(bundle) = &spec.bundle {
        let b = Bundle::read(bundle)?;
        let ecid = spec
            .ecid
            .or(b.ecid)
            .context("the bundle's machine identifier was not found: give its ECID (--ecid)")?;
        let (disk, aux) = match spec.bundle_mode {
            ImportMode::InPlace => (b.disk.clone(), b.aux.clone()),
            ImportMode::Copy => {
                progress(
                    "cloning the disk and auxiliary storage (APFS clones where the volume allows)",
                );
                copy_file(&b.disk, &dir.join("disk.img"))?;
                copy_file(&b.aux, &dir.join("aux.img"))?;
                (PathBuf::from("disk.img"), PathBuf::from("aux.img"))
            }
            ImportMode::Overlay => {
                bail!("a vmapple VM's disk is raw: import it as a copy or in place")
            }
        };
        let mut c = base_config(spec, Profile::VmappleArm64, disk, ImageFormat::Raw);
        if spec.cpus.is_none()
            && let Some(n) = b.cpus
        {
            c.cpus = n;
        }
        if spec.memory_mib.is_none()
            && let Some(m) = b.memory_mib
        {
            c.memory_mib = m;
        }
        c.vmapple = Some(Vmapple { aux, ecid });
        let vm = Vm {
            dir: dir.to_path_buf(),
            config: c,
        };
        crate::runtime::ensure_aux_trimmed(&vm)?;
        return Ok(vm);
    }
    if let Some(ipsw) = &spec.ipsw {
        let size = match &spec.disk {
            Some(DiskSource::New { size_mib }) => *size_mib,
            None => 64 * 1024,
            Some(DiskSource::Existing { .. }) => {
                bail!("an IPSW installs onto a new disk: give its size, not an image")
            }
        };
        let mut c = base_config(
            spec,
            Profile::VmappleArm64,
            "disk.img".into(),
            ImageFormat::Raw,
        );
        let ecid = crate::vz::install(ipsw, dir, size, c.cpus, c.memory_mib, progress)?;
        c.vmapple = Some(Vmapple {
            aux: "aux.img".into(),
            ecid,
        });
        let vm = Vm {
            dir: dir.to_path_buf(),
            config: c,
        };
        crate::runtime::ensure_aux_trimmed(&vm)?;
        let _ = host;
        return Ok(vm);
    }
    bail!(
        "an arm64 VM is installed from an IPSW (--ipsw PATH, or --ipsw latest), or imported from a Virtualization.framework VM's directory (--bundle)"
    )
}

// ---------------------------------------------------------------- launch scripts

/// `p`, a path a launch script names, or where it went: a missing path is looked up in the
/// script's directory `dir` by its last components, the longest that exist first. ULTMOS writes
/// the absolute directory it was set up in into its script, which is wrong once the VM is
/// copied or moved.
fn relocated(p: PathBuf, dir: &Path) -> PathBuf {
    if p.exists() {
        return p;
    }
    let parts: Vec<_> = p
        .components()
        .filter(|c| matches!(c, std::path::Component::Normal(_)))
        .collect();
    (1..=parts.len())
        .rev()
        .map(|k| dir.join(parts[parts.len() - k..].iter().collect::<PathBuf>()))
        .find(|c| c.exists())
        .unwrap_or(p)
}

/// What an OSX-KVM or ULTMOS launch script names.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LaunchScript {
    pub disk: Option<PathBuf>,
    pub disk_port: Option<u32>,
    pub opencore: Option<PathBuf>,
    pub firmware_code: Option<PathBuf>,
    pub firmware_vars: Option<PathBuf>,
    pub installer: Option<PathBuf>,
    pub cpu_model: Option<String>,
    pub cpu_flags: Option<String>,
    pub mac: Option<String>,
    pub cpus: Option<u32>,
    pub memory_mib: Option<u64>,
}

impl LaunchScript {
    pub fn read(path: &Path) -> Result<LaunchScript> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        let dir = std::fs::canonicalize(path)?
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default();
        let mut s = LaunchScript::parse(&text, &dir);
        for p in [
            &mut s.disk,
            &mut s.opencore,
            &mut s.firmware_code,
            &mut s.firmware_vars,
            &mut s.installer,
        ] {
            *p = p.take().map(|p| relocated(p, &dir));
        }
        Ok(s)
    }

    pub fn parse(text: &str, dir: &Path) -> LaunchScript {
        let mut vars: BTreeMap<String, String> = BTreeMap::new();
        let lines: Vec<&str> = text
            .lines()
            .map(str::trim)
            .filter(|l| !l.starts_with('#'))
            .collect();
        for l in &lines {
            if let Some((k, v)) = l.split_once('=')
                && !k.is_empty()
                && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                && !k.starts_with(|c: char| c.is_ascii_digit())
            {
                let v = v.split(" #").next().unwrap_or(v).trim();
                let v = v.trim_matches('"').trim_matches('\'');
                let v = expand_vars(v, &vars);
                vars.insert(k.to_string(), v);
            }
        }
        let body: String = lines
            .iter()
            .map(|l| expand_vars(l, &vars))
            .collect::<Vec<_>>()
            .join("\n");
        let resolve = |p: &str| -> PathBuf {
            let p = paths::expand(p.trim_matches('"').trim_matches('\''));
            if p.is_absolute() { p } else { dir.join(p) }
        };
        let mut s = LaunchScript::default();
        // -cpu MODEL,opts.
        if let Some(i) = body.find("-cpu ") {
            let rest = body[i + 5..].trim_start();
            let tok: String = rest.chars().take_while(|c| !c.is_whitespace()).collect();
            let tok = tok.replace('"', "");
            let mut parts = tok.split(',');
            if let Some(model) = parts
                .next()
                .filter(|m| !m.is_empty() && !m.starts_with('$'))
            {
                s.cpu_model = Some(model.to_string());
            }
            let own = [
                "kvm=on",
                "vendor=GenuineIntel",
                "+invtsc",
                "vmware-cpuid-freq=on",
            ];
            let flags: Vec<&str> = parts
                .filter(|p| !p.is_empty() && !own.contains(p))
                .collect();
            if !flags.is_empty() {
                s.cpu_flags = Some(flags.join(","));
            }
        }
        // Drives: each file= path, by what it names; the disk's AHCI port from its ide-hd.
        let mut drive_ids: BTreeMap<String, PathBuf> = BTreeMap::new();
        for line in body.lines() {
            let Some(i) = line.find("file=") else {
                continue;
            };
            let rest = &line[i + 5..];
            let rest = rest.trim_start_matches('"');
            let end = rest.find(['"', ',', ' ']).unwrap_or(rest.len());
            let file = &rest[..end];
            if file.is_empty() {
                continue;
            }
            let p = resolve(file);
            let low = file.to_ascii_lowercase();
            let id = line
                .split(['-', ' ', ','])
                .find_map(|t| t.strip_prefix("id="))
                .map(|t| t.trim_matches('"').to_string());
            if low.contains("ovmf_code") || low.contains("ovmf-code") {
                s.firmware_code = Some(p);
            } else if low.contains("ovmf_vars") || low.contains("ovmf-vars") {
                s.firmware_vars = Some(p);
            } else if low.contains("opencore") {
                s.opencore = Some(p);
            } else if low.contains("basesystem") || low.ends_with(".iso") || low.contains("install")
            {
                s.installer = Some(p);
            } else if (low.ends_with(".qcow2") || low.ends_with(".img") || low.ends_with(".raw"))
                && s.disk.is_none()
            {
                s.disk = Some(p.clone());
                if let Some(id) = &id {
                    drive_ids.insert(id.clone(), p);
                }
            }
        }
        if let Some(disk) = &s.disk {
            for line in body.lines() {
                if let Some(id) = drive_ids.iter().find(|(_, p)| *p == disk).map(|(id, _)| id)
                    && line.contains("ide-hd")
                    && line.contains(&format!("drive={id}"))
                    && let Some(port) = line
                        .split(',')
                        .find_map(|t| t.trim().strip_prefix("bus=sata."))
                        .and_then(|n| n.trim_matches('"').parse().ok())
                {
                    s.disk_port = Some(port);
                }
            }
        }
        s.mac = vars
            .get("MAC_ADDRESS")
            .cloned()
            .filter(|m| m.len() == 17)
            .or_else(|| {
                let i = body.find("mac=")?;
                let m: String = body[i + 4..].chars().take(17).collect();
                (m.len() == 17 && m.chars().filter(|c| *c == ':').count() == 5).then_some(m)
            });
        let num = |k: &str| vars.get(k).and_then(|v| v.parse::<u32>().ok());
        if let Some(cores) = num("CPU_CORES") {
            s.cpus =
                Some(cores * num("CPU_THREADS").unwrap_or(1) * num("CPU_SOCKETS").unwrap_or(1));
        }
        if let Some(ram) = vars.get("ALLOCATED_RAM") {
            // OSX-KVM gives MiB as a bare number; ULTMOS "12G".
            s.memory_mib = if ram.chars().all(|c| c.is_ascii_digit()) {
                ram.parse().ok()
            } else {
                parse_size_mib(ram).ok()
            };
        }
        s
    }
}

/// `$VAR` and `${VAR}` replaced from `vars`; others left as they are.
fn expand_vars(s: &str, vars: &BTreeMap<String, String>) -> String {
    let mut out = String::new();
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'$' {
            let (name, next) = if b.get(i + 1) == Some(&b'{') {
                match s[i + 2..].find('}') {
                    Some(e) => (&s[i + 2..i + 2 + e], i + 3 + e),
                    None => ("", i + 1),
                }
            } else {
                let e = s[i + 1..]
                    .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                    .map(|e| i + 1 + e)
                    .unwrap_or(s.len());
                (&s[i + 1..e], e)
            };
            if let Some(v) = vars.get(name).filter(|_| !name.is_empty()) {
                out.push_str(v);
                i = next;
                continue;
            }
        }
        let ch = s[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

// ---------------------------------------------------------------- Virtualization.framework VMs

/// A Virtualization.framework VM's files, as macosvm or tart keep them.
#[derive(Clone, Debug)]
pub struct Bundle {
    pub disk: PathBuf,
    pub aux: PathBuf,
    pub ecid: Option<u64>,
    pub cpus: Option<u32>,
    pub memory_mib: Option<u64>,
}

impl Bundle {
    pub fn read(dir: &Path) -> Result<Bundle> {
        let dir = std::fs::canonicalize(dir)
            .with_context(|| format!("{} does not exist", dir.display()))?;
        let disk = dir.join("disk.img");
        if !disk.is_file() {
            bail!(
                "{} has no disk.img: not a macosvm or tart VM",
                dir.display()
            );
        }
        // macosvm: vm.json, aux.img; its machineId is the identifier's data, in base64.
        let vm_json = dir.join("vm.json");
        if vm_json.is_file() && dir.join("aux.img").is_file() {
            let j: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&vm_json)?)?;
            let ecid = j
                .get("machineId")
                .and_then(|v| v.as_str())
                .and_then(|b| ecid_from_identifier_base64(b).ok());
            let cpus = j.get("cpus").and_then(|v| v.as_u64()).map(|n| n as u32);
            let memory_mib = j
                .get("ram")
                .and_then(|v| v.as_u64())
                .map(|b| b / (1024 * 1024));
            return Ok(Bundle {
                disk,
                aux: dir.join("aux.img"),
                ecid,
                cpus,
                memory_mib,
            });
        }
        // tart: config.json with "ecid" (the identifier's data, in base64), nvram.bin.
        let config = dir.join("config.json");
        if config.is_file() && dir.join("nvram.bin").is_file() {
            let j: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&config)?)?;
            let ecid = j
                .get("ecid")
                .and_then(|v| v.as_str())
                .and_then(|b| ecid_from_identifier_base64(b).ok());
            let cpus = j.get("cpuCount").and_then(|v| v.as_u64()).map(|n| n as u32);
            let memory_mib = j
                .get("memorySize")
                .and_then(|v| v.as_u64())
                .map(|b| b / (1024 * 1024));
            return Ok(Bundle {
                disk,
                aux: dir.join("nvram.bin"),
                ecid,
                cpus,
                memory_mib,
            });
        }
        if dir.join("aux.img").is_file() {
            return Ok(Bundle {
                disk,
                aux: dir.join("aux.img"),
                ecid: None,
                cpus: None,
                memory_mib: None,
            });
        }
        bail!(
            "{} has a disk.img but no aux.img (macosvm) or nvram.bin (tart)",
            dir.display()
        )
    }
}

/// The ECID in a `VZMacMachineIdentifier`'s data representation (a property list), given in
/// base64.
pub fn ecid_from_identifier_base64(b64: &str) -> Result<u64> {
    let data = base64_decode(b64)?;
    ecid_from_identifier(&data)
}

pub fn ecid_from_identifier(data: &[u8]) -> Result<u64> {
    let mut child = Command::new("plutil")
        .args(["-extract", "ECID", "raw", "-o", "-", "-"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .context("run plutil")?;
    use std::io::Write;
    child.stdin.take().unwrap().write_all(data)?;
    let out = child.wait_with_output()?;
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    s.parse()
        .with_context(|| format!("no ECID in the machine identifier ({s:?})"))
}

pub fn base64_decode(s: &str) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut acc: u32 = 0;
    let mut bits = 0;
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            b'=' | b'\n' | b'\r' | b' ' => continue,
            _ => bail!("not base64"),
        } as u32;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relocated_finds_a_moved_bundles_files() {
        let d = std::env::temp_dir().join(format!("reims-ctl-relocated-{}", std::process::id()));
        std::fs::create_dir_all(d.join("boot")).unwrap();
        std::fs::write(d.join("boot/OpenCore.qcow2"), b"").unwrap();
        std::fs::write(d.join("OpenCore.qcow2"), b"").unwrap();
        let gone = Path::new("/nonexistent/vm/macos-26");
        // The longest suffix that exists wins over a same-named file higher up.
        assert_eq!(
            relocated(gone.join("boot/OpenCore.qcow2"), &d),
            d.join("boot/OpenCore.qcow2")
        );
        assert_eq!(
            relocated(gone.join("HDD.qcow2"), &d),
            gone.join("HDD.qcow2")
        );
        assert_eq!(
            relocated(d.join("OpenCore.qcow2"), &d),
            d.join("OpenCore.qcow2")
        );
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn a_dmg_is_known_by_its_udif_trailer() {
        let d = std::env::temp_dir().join(format!("reims-ctl-dmg-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let mut udif = vec![0u8; 4096];
        udif[4096 - 512..4096 - 508].copy_from_slice(b"koly");
        std::fs::write(d.join("udzo.dmg"), &udif).unwrap();
        // A read/write DMG is a raw image, which ends in the backup GPT header.
        let mut raw = vec![0u8; 4096];
        raw[4096 - 512..4096 - 504].copy_from_slice(b"EFI PART");
        std::fs::write(d.join("udrw.dmg"), &raw).unwrap();
        std::fs::write(d.join("short.dmg"), b"koly").unwrap();
        assert!(is_dmg(&d.join("udzo.dmg")).unwrap());
        assert!(!is_dmg(&d.join("udrw.dmg")).unwrap());
        assert!(!is_dmg(&d.join("short.dmg")).unwrap());
        // One that is not is used in place, without qemu-img.
        assert_eq!(
            installer_media(&d.join("udrw.dmg"), &d, &mut |_| {}).unwrap(),
            d.join("udrw.dmg")
        );
        std::fs::remove_dir_all(&d).unwrap();
    }

    const ULTMOS: &str = r#"
ALLOCATED_RAM="12G"
CPU_SOCKETS="1"
CPU_CORES="8"
CPU_THREADS="2"
CPU_MODEL="Haswell-noTSX"
CPU_FEATURE_ARGS="+ssse3,+sse4.2,+popcnt,+avx,+aes,+xsave,+xsaveopt,check"
VM_PATH="/vm/macos-26"
OVMF_DIR="/vm/macos-26/ovmf"
MAC_ADDRESS="00:16:cb:00:21:09"
HDD_PATH="$VM_PATH/HDD.qcow2"
args=(
-enable-kvm -m "$ALLOCATED_RAM" -cpu "$CPU_MODEL",kvm=on,vendor=GenuineIntel,+invtsc,vmware-cpuid-freq=on,"$CPU_FEATURE_ARGS"
-device isa-applesmc,osk="the-key"
-drive if=pflash,format=raw,readonly=on,file="$OVMF_DIR/OVMF_CODE.fd"
-drive if=pflash,format=raw,file="$OVMF_DIR/OVMF_VARS.fd"
-drive id=OpenCore,if=none,format=qcow2,file="$VM_PATH/boot/OpenCore.qcow2"
-drive id=HDD,if=none,file="$HDD_PATH",format=qcow2
-device ide-hd,bus=sata.2,drive=OpenCore,bootindex=1
-device ide-hd,bus=sata.3,drive=HDD,rotation_rate=1
-drive id=BaseSystem,if=none,file="$VM_PATH/BaseSystem.img",format=raw
#-drive id=Commented,if=none,file="/nope.img"
)
"#;

    #[test]
    fn reads_an_ultmos_script() {
        let s = LaunchScript::parse(ULTMOS, Path::new("/x"));
        assert_eq!(s.disk.as_deref(), Some(Path::new("/vm/macos-26/HDD.qcow2")));
        assert_eq!(s.disk_port, Some(3));
        assert_eq!(
            s.opencore.as_deref(),
            Some(Path::new("/vm/macos-26/boot/OpenCore.qcow2"))
        );
        assert_eq!(
            s.firmware_code.as_deref(),
            Some(Path::new("/vm/macos-26/ovmf/OVMF_CODE.fd"))
        );
        assert_eq!(
            s.firmware_vars.as_deref(),
            Some(Path::new("/vm/macos-26/ovmf/OVMF_VARS.fd"))
        );
        assert_eq!(
            s.installer.as_deref(),
            Some(Path::new("/vm/macos-26/BaseSystem.img"))
        );
        assert_eq!(s.cpu_model.as_deref(), Some("Haswell-noTSX"));
        assert_eq!(
            s.cpu_flags.as_deref(),
            Some("+ssse3,+sse4.2,+popcnt,+avx,+aes,+xsave,+xsaveopt,check")
        );
        assert_eq!(s.mac.as_deref(), Some("00:16:cb:00:21:09"));
        assert_eq!(s.cpus, Some(16));
        assert_eq!(s.memory_mib, Some(12 * 1024));
    }

    #[test]
    fn reads_an_osx_kvm_script() {
        let text = r#"
MY_OPTIONS="+ssse3,+sse4.2,+popcnt,+avx,+aes,+xsave,+xsaveopt,check"
ALLOCATED_RAM="4096" # MiB
CPU_SOCKETS="1"
CPU_CORES="2"
CPU_THREADS="4"
REPO_PATH="."
OVMF_DIR="."
args=(
  -enable-kvm -m "$ALLOCATED_RAM" -cpu Penryn,kvm=on,vendor=GenuineIntel,+invtsc,vmware-cpuid-freq=on,"$MY_OPTIONS"
  -device isa-applesmc,osk="k2"
  -drive if=pflash,format=raw,readonly=on,file="$REPO_PATH/$OVMF_DIR/OVMF_CODE.fd"
  -drive if=pflash,format=raw,file="$REPO_PATH/$OVMF_DIR/OVMF_VARS-1920x1080.fd"
  -drive id=OpenCoreBoot,if=none,snapshot=on,format=qcow2,file="$REPO_PATH/OpenCore/OpenCore.qcow2"
  -device ide-hd,bus=sata.2,drive=OpenCoreBoot
  -device ide-hd,bus=sata.3,drive=InstallMedia
  -drive id=InstallMedia,if=none,file="$REPO_PATH/BaseSystem.img",format=raw
  -drive id=MacHDD,if=none,file="$REPO_PATH/mac_hdd_ng.img",format=qcow2
  -device ide-hd,bus=sata.4,drive=MacHDD
)
"#;
        let s = LaunchScript::parse(text, Path::new("/osx"));
        assert_eq!(s.disk.as_deref(), Some(Path::new("/osx/./mac_hdd_ng.img")));
        assert_eq!(s.disk_port, Some(4));
        assert_eq!(s.cpu_model.as_deref(), Some("Penryn"));
        assert_eq!(s.memory_mib, Some(4096));
        assert_eq!(s.cpus, Some(8));
        assert_eq!(
            s.installer.as_deref(),
            Some(Path::new("/osx/./BaseSystem.img"))
        );
        assert!(s.firmware_vars.unwrap().ends_with("OVMF_VARS-1920x1080.fd"));
    }

    #[test]
    fn base64() {
        assert_eq!(base64_decode("aGVsbG8=").unwrap(), b"hello");
        assert_eq!(base64_decode("aGVsbG8gd29ybGQ").unwrap(), b"hello world");
    }
}
