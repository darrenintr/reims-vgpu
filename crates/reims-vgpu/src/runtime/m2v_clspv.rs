//! The metal2vk compute route: a guest kernel's AIR compiled by clspv instead of
//! by metal2vulkan's own SPIR-V emitter.
//!
//! [metal2vk](https://github.com/joshuaswarren/metal2vk) translates Metal compute
//! kernels by parsing MSL source with clang and lowering the IR through a patched
//! clspv. The guest never hands this device source: it hands compiled AIR, which
//! is LLVM bitcode with Apple's calling conventions. This module is the missing
//! front half: it rewrites the AIR's disassembly into the OpenCL-shaped IR clspv
//! reads, runs metal2vk's patched clspv on it, and makes the result answer to the
//! interface the dispatcher already binds against.
//!
//! # The device describes the kernel itself
//!
//! Compute kernels do not pass through metal2vulkan at all. [`reflection_for`]
//! builds the [`ShaderReflection`] the dispatcher binds against from the AIR's
//! own metadata (each buffer's Metal index, address space, access and, for a
//! `constant T&` object, its size), and the clspv module is made to answer to
//! it:
//!
//! - each Metal buffer `n` at descriptor set 0, binding `n` (the buffer band of
//!   the shared descriptor layout);
//! - the 48-byte dispatch-region push constant at offset 0, read the way
//!   [`metal2vulkan::reflect::KernelDispatchPlan::push_constants`] writes it:
//!   threads per grid, thread base, threadgroup base, threadgroups per grid;
//! - the workgroup size as spec constants 0, 1 and 2, because a dispatch is
//!   split into up to eight exact regions, each specialised to its own size;
//! - one `GLCompute` entry point named `main`.
//!
//! [`conform`] checks each of these against clspv's own reflection and rewrites
//! the bindings. Anything it cannot prove is a [`ClspvRefusal`], and the kernel
//! is declined by name. The reflection *type* is still metal2vulkan's, because
//! it is the vocabulary the whole dispatcher (shared with the graphics path)
//! speaks; nothing of metal2vulkan's translation or analysis runs for a kernel.
//!
//! Buffer footprints are not computed: the dispatcher then stages whole
//! allocations, which costs bytes and is never wrong.
//!
//! # The kernel body is not edited
//!
//! The AIR kernel is kept as an ordinary function and called from a new `main`
//! kernel, which takes the buffers and one by-value region struct and computes
//! each Metal builtin the kernel declared from OpenCL's work-item functions plus
//! the region words. clspv's `--inline-entry-points` flattens the two. Rewriting
//! the original signature in place would have to renumber every unnamed value in
//! the body; the wrapper never touches it.
//!
//! The `air.*` functions the kernel calls are given bodies (math as LLVM
//! intrinsics, conversions as casts, the threadgroup barrier as OpenCL's
//! `barrier`, relaxed read-modify-write atomics as `atomicrmw`). A kernel calling
//! one this module does not know is refused by name, as is any argument kind it
//! does not carry yet: textures, samplers, threadgroup-memory parameters,
//! argument buffers, SIMD-group builtins.

use metal2vulkan::reflect::{ResourceKind, ShaderReflection};

/// Name of the kernel the wrapper declares, and of the entry point the
/// dispatcher looks up.
pub(crate) const ENTRY_POINT: &str = "main";

/// Words in metal2vulkan's dispatch-region push constant.
const REGION_WORDS: u32 = 12;
const REGION_BYTES: u32 = REGION_WORDS * 4;
/// Where each triple sits in the region, as `KernelDispatchPlan::push_constants`
/// writes it.
const REGION_THREADS_PER_GRID: u32 = 0;
const REGION_THREAD_BASE: u32 = 3;
const REGION_THREADGROUP_BASE: u32 = 6;
const REGION_THREADGROUPS_PER_GRID: u32 = 9;

/// Why the metal2vk route did not produce a kernel module. The kernel is then
/// declined by name; there is no other compute translator.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClspvRefusal {
    /// `REIMS_VGPU_CLSPV` names no binary.
    BinaryUnset,
    /// `llvm-dis` could not disassemble the AIR.
    Disassemble { detail: String },
    /// The module names no kernel, or more than one.
    KernelCount { count: usize },
    /// The kernel's metadata or definition did not parse.
    KernelShape { detail: String },
    /// A kernel argument of a kind this route does not carry.
    Argument { index: u32, kind: String },
    /// A builtin argument in a type this route does not build.
    BuiltinType { kind: String, ty: String },
    /// A buffer in an address space other than device or constant.
    AddressSpace { index: u32, space: u32 },
    /// An `air.*` function this route has no body for.
    Intrinsic { name: String },
    /// The module already defines a symbol the wrapper needs.
    NameTaken { name: String },
    /// clspv failed, or reported a pass that gave up.
    Compile { detail: String },
    /// The clspv module does not answer to the kernel's reflected interface.
    Interface { detail: String },
    /// `spirv-val` rejected the clspv module.
    Validate { detail: String },
}

impl crate::observe::decline::Decline for ClspvRefusal {
    fn slug(&self) -> &'static str {
        match self {
            Self::BinaryUnset => "kernel_clspv_binary_unset",
            Self::Disassemble { .. } => "kernel_clspv_disassemble",
            Self::KernelCount { .. } => "kernel_clspv_kernel_count",
            Self::KernelShape { .. } => "kernel_clspv_kernel_shape",
            Self::Argument { .. } => "kernel_clspv_argument",
            Self::BuiltinType { .. } => "kernel_clspv_builtin_type",
            Self::AddressSpace { .. } => "kernel_clspv_address_space",
            Self::Intrinsic { .. } => "kernel_clspv_intrinsic",
            Self::NameTaken { .. } => "kernel_clspv_name_taken",
            Self::Compile { .. } => "kernel_clspv_compile",
            Self::Interface { .. } => "kernel_clspv_interface",
            Self::Validate { .. } => "kernel_clspv_validate",
        }
    }

    fn fields(&self) -> Vec<(&'static str, String)> {
        match self {
            Self::BinaryUnset => Vec::new(),
            Self::Disassemble { detail }
            | Self::KernelShape { detail }
            | Self::Compile { detail }
            | Self::Interface { detail }
            | Self::Validate { detail } => vec![("detail", first_line(detail))],
            Self::KernelCount { count } => vec![("count", count.to_string())],
            Self::Argument { index, kind } => {
                vec![("index", index.to_string()), ("kind", kind.clone())]
            }
            Self::BuiltinType { kind, ty } => vec![("kind", kind.clone()), ("ty", ty.clone())],
            Self::AddressSpace { index, space } => {
                vec![("index", index.to_string()), ("space", space.to_string())]
            }
            Self::Intrinsic { name } => vec![("name", name.clone())],
            Self::NameTaken { name } => vec![("name", name.clone())],
        }
    }
}

fn first_line(detail: &str) -> String {
    detail
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .chars()
        .take(200)
        .collect()
}

/// Translate the kernel AIR at `air_path`: the SPIR-V module and the reflection
/// the dispatcher binds it with, both from metal2vk's route alone.
pub fn compile_kernel(
    clspv: &str,
    air_path: &std::path::Path,
    tmp: &std::path::Path,
    local_size: [u32; 3],
) -> Result<(Vec<u8>, ShaderReflection), ClspvRefusal> {
    let adapted = adapt(&disassemble(air_path, tmp)?)?;
    let reflection = reflection_for(&adapted, local_size)?;
    let spirv = compile_adapted(clspv, &adapted, tmp, &reflection)?;
    Ok((spirv, reflection))
}

/// Compile the AIR at `air_path` into a module that answers to `reflection`,
/// whoever produced it. [`compile_kernel`] is this with the route's own
/// reflection; tests use it to hold the module to a reflection they altered.
pub fn compile(
    clspv: &str,
    air_path: &std::path::Path,
    tmp: &std::path::Path,
    reflection: &ShaderReflection,
) -> Result<Vec<u8>, ClspvRefusal> {
    let adapted = adapt(&disassemble(air_path, tmp)?)?;
    compile_adapted(clspv, &adapted, tmp, reflection)
}

fn disassemble(air_path: &std::path::Path, tmp: &std::path::Path) -> Result<String, ClspvRefusal> {
    let ll_path = tmp.join("k.clspv-src.ll");
    let air = air_path.to_str().unwrap_or("k.air");
    let ll_str = ll_path.to_str().unwrap_or("k.clspv-src.ll");
    tool::run("llvm-dis", &[air, "-o", ll_str])
        .map_err(|detail| ClspvRefusal::Disassemble { detail })?;
    std::fs::read_to_string(&ll_path).map_err(|e| ClspvRefusal::Disassemble {
        detail: e.to_string(),
    })
}

fn compile_adapted(
    clspv: &str,
    adapted: &Adapted,
    tmp: &std::path::Path,
    reflection: &ShaderReflection,
) -> Result<Vec<u8>, ClspvRefusal> {
    let in_path = tmp.join("k.clspv.ll");
    let out_path = tmp.join("k.clspv.spv");
    std::fs::write(&in_path, &adapted.ir).map_err(|e| ClspvRefusal::Compile {
        detail: e.to_string(),
    })?;
    let in_str = in_path.to_str().unwrap_or("k.clspv.ll");
    let out_str = out_path.to_str().unwrap_or("k.clspv.spv");
    let stderr = tool::run(
        clspv,
        &[
            "-x",
            "ir",
            // OpenCL C++ mode turns on module-scope push constants (global
            // offsets, non-uniform regions) that collide with the dispatch
            // region; the IR needs none of what it adds.
            "--cl-std=CL1.2",
            "--inline-entry-points",
            "--spv-version=1.5",
            "-arch=spir64",
            "--pod-pushconstant",
            "--cluster-pod-kernel-args",
            in_str,
            "-o",
            out_str,
        ],
    )
    .map_err(|detail| ClspvRefusal::Compile { detail })?;
    // metal2vk's rule: a pointer pass that gave up leaves a half-rewritten module
    // clspv still emits, so the module is wrong even though the exit was clean.
    if stderr.contains("did not converge") || stderr.contains("M2V-GIVEUP:") {
        return Err(ClspvRefusal::Compile { detail: stderr });
    }
    let spirv = std::fs::read(&out_path).map_err(|e| ClspvRefusal::Compile {
        detail: e.to_string(),
    })?;
    let spirv = conform(&spirv, adapted, reflection)?;
    tool::spirv_val(&spirv, tmp).map_err(|detail| ClspvRefusal::Validate { detail })?;
    Ok(spirv)
}

/// The reflection the dispatcher binds this kernel with, from the AIR alone.
///
/// Buffer `n` sits at binding `n` of set 0, the buffer band of the descriptor
/// layout every translated shader shares. Dispatch is exact-thread
/// (`ThreadsDynamic` at offset 0), which is what the wrapper's region struct and
/// clspv's workgroup-size spec constants implement.
pub fn reflection_for(
    adapted: &Adapted,
    local_size: [u32; 3],
) -> Result<ShaderReflection, ClspvRefusal> {
    use metal2vulkan::reflect::{
        BufferExtent, DescriptorLayout, DescriptorLocation, KernelDispatch, ResourceBinding,
        ShaderStage, DEFAULT_KERNEL_DISPATCH_PUSH_CONSTANT_OFFSET, REFLECTION_VERSION,
    };
    let layout = DescriptorLayout::default();
    let bindings =
        adapted
            .interface
            .iter()
            .map(|b| {
                let binding = layout.buffers.binding(b.metal_index).ok_or_else(|| {
                    ClspvRefusal::Interface {
                        detail: format!("buffer index {} is past the buffer band", b.metal_index),
                    }
                })?;
                Ok(ResourceBinding {
                    kind: ResourceKind::Buffer,
                    metal_index: b.metal_index,
                    descriptor: Some(DescriptorLocation {
                        set: layout.set,
                        binding,
                        count: 1,
                    }),
                    param_index: Some(b.param),
                    stage_input_location: None,
                    address_space: Some(b.address_space),
                    declared_size: b.type_size,
                    // A `constant T&` names its object's size; a pointer names
                    // nothing, so the whole allocation stays reachable.
                    extent: Some(match b.buffer_size {
                        Some(bytes) => BufferExtent::Object { bytes },
                        None => BufferExtent::Unbounded,
                    }),
                    footprint: None,
                    type_layout: None,
                    type_name: None,
                    texture_shape: None,
                    embedded_source: None,
                    access: b.access,
                    static_sampler: None,
                })
            })
            .collect::<Result<Vec<_>, ClspvRefusal>>()?;
    Ok(ShaderReflection {
        reflection_version: REFLECTION_VERSION,
        descriptor_layout: layout,
        stage: ShaderStage::Kernel,
        entry_point: Some(adapted.kernel_name.clone()),
        bindings,
        argument_buffer_fields: vec![],
        vertex_attributes: vec![],
        varyings: vec![],
        render_targets: vec![],
        depth_members: vec![],
        depth_qualifier: None,
        stencil_members: vec![],
        local_size: Some(local_size),
        max_work_group_size: None,
        kernel_dispatch: Some(KernelDispatch::ThreadsDynamic {
            offset: DEFAULT_KERNEL_DISPATCH_PUSH_CONSTANT_OFFSET,
        }),
        vertex_builtins: None,
        tessellation: None,
        imageblock_layouts: vec![],
        implicit_imageblock_attachments: vec![],
        fragment_imageblock: None,
        datalayout: adapted.datalayout.clone(),
        runtime_sampler_specializations: vec![],
        runtime_storage_image_specializations: vec![],
        function_constants: vec![],
    })
}

/// The rewritten module, and which AIR parameter each wrapper argument carries.
#[derive(Debug)]
pub struct Adapted {
    pub ir: String,
    /// `buffers[ordinal]` is the AIR parameter index of the wrapper's argument
    /// `ordinal`. The region struct is the argument after the last of them.
    pub buffers: Vec<u32>,
    /// The kernel's buffers, in wrapper-argument order (`interface[ordinal]`
    /// is the buffer `buffers[ordinal]` names).
    pub interface: Vec<KernelBuffer>,
    /// The kernel's own name, unquoted and without `@`.
    pub kernel_name: String,
    /// The AIR `target datalayout`, kept in the module and reported.
    pub datalayout: Option<String>,
}

impl Adapted {
    fn region_ordinal(&self) -> u32 {
        self.buffers.len() as u32
    }
}

/// One buffer argument, as the AIR metadata and parameter type describe it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KernelBuffer {
    /// The AIR parameter it is.
    pub param: u32,
    /// `[[buffer(n)]]`.
    pub metal_index: u32,
    /// 1 for `device`, 2 for `constant`.
    pub address_space: u32,
    pub access: Option<metal2vulkan::reflect::ResourceAccess>,
    /// The object size a `constant T&` declares (`air.buffer_size`).
    pub buffer_size: Option<u32>,
    /// The pointee type's size (`air.arg_type_size`).
    pub type_size: Option<u32>,
}

/// One kernel argument, as `!air.kernel` describes it.
#[derive(Debug)]
struct AirArg {
    index: u32,
    kind: String,
    address_space: Option<u32>,
    location_index: Option<u32>,
    access: Option<metal2vulkan::reflect::ResourceAccess>,
    buffer_size: Option<u32>,
    type_size: Option<u32>,
}

/// Rewrite AIR disassembly into IR clspv compiles: a `spir_kernel` wrapper named
/// [`ENTRY_POINT`] around the untouched kernel, and bodies for the `air.*`
/// functions it calls.
pub fn adapt(ll: &str) -> Result<Adapted, ClspvRefusal> {
    let meta = metadata_nodes(ll);
    let kernels = named_metadata(ll, "air.kernel")
        .map(|v| split_top(strip_tuple(v)))
        .unwrap_or_default();
    if kernels.len() != 1 {
        return Err(ClspvRefusal::KernelCount {
            count: kernels.len(),
        });
    }
    let kernel_node = resolve(&meta, kernels[0])?;
    let fields = split_top(strip_tuple(kernel_node));
    let name = fields
        .first()
        .and_then(|f| f.trim().strip_prefix("ptr "))
        .map(str::trim)
        .filter(|n| n.starts_with('@'))
        .ok_or_else(|| shape(format!("kernel node names no function: {kernel_node}")))?
        .to_owned();
    let args_node = fields
        .get(2)
        .ok_or_else(|| shape("kernel node has no argument list".to_owned()))?;
    let args = split_top(strip_tuple(resolve(&meta, args_node)?))
        .into_iter()
        .map(|a| air_arg(resolve(&meta, a)?))
        .collect::<Result<Vec<_>, _>>()?;

    relaxed_atomic_calls(ll)?;
    let defined = global_names(ll);
    for taken in [format!("@{ENTRY_POINT}"), body_name(&name)] {
        if defined.contains(&taken) {
            return Err(ClspvRefusal::NameTaken { name: taken });
        }
    }

    let mut out = String::with_capacity(ll.len() + 4096);
    let mut define_params = None;
    let mut air_decls = Vec::new();
    for line in ll.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("target triple") {
            out.push_str("target triple = \"spir64-unknown-unknown\"\n");
            continue;
        }
        if trimmed.starts_with("!air.") {
            continue;
        }
        if trimmed.contains("@llvm.experimental.noalias.scope.decl(")
            && !trimmed.starts_with("declare")
        {
            continue;
        }
        if trimmed.starts_with("declare ") && trimmed.contains("@air.") {
            air_decls.push(trimmed.to_owned());
            continue;
        }
        if trimmed.starts_with("define ") && define_names(trimmed, &name) {
            let (renamed, params) = rename_define(trimmed, &name)?;
            define_params = Some(params);
            out.push_str(&renamed);
            out.push('\n');
            continue;
        }
        if trimmed.starts_with('!') {
            // Metadata that named the kernel (the node `!air.kernel` listed,
            // now orphaned) follows it to its new name, or the module would
            // reference a symbol that no longer exists.
            let body = body_name(&name);
            out.push_str(
                &line
                    .replace(&format!("ptr {name},"), &format!("ptr {body},"))
                    .replace(&format!("ptr {name}}}"), &format!("ptr {body}}}")),
            );
            out.push('\n');
            continue;
        }
        out.push_str(&strip_alias_attachments(line));
        out.push('\n');
    }
    let params = define_params.ok_or_else(|| shape(format!("no definition of {name}")))?;
    if params.len() != args.len() {
        return Err(shape(format!(
            "{name} takes {} parameters and its metadata describes {}",
            params.len(),
            args.len()
        )));
    }

    let mut decls = Declarations::new(&defined);
    for decl in &air_decls {
        out.push_str(&air_definition(decl, &mut decls)?);
    }
    let (wrapper, interface) = wrapper(&name, &params, &args, &mut decls)?;
    let buffers = interface.iter().map(|b| b.param).collect();
    out.push_str(&format!(
        "\n%m2v.region = type {{ [{REGION_WORDS} x i32] }}\n"
    ));
    out.push_str(&wrapper);
    out.push_str(&decls.finish());
    let datalayout = ll.lines().find_map(|l| {
        l.strip_prefix("target datalayout = \"")
            .and_then(|v| v.strip_suffix('"'))
            .map(str::to_owned)
    });
    let kernel_name = name.trim_start_matches('@').trim_matches('"').to_owned();
    Ok(Adapted {
        ir: out,
        buffers,
        interface,
        kernel_name,
        datalayout,
    })
}

fn shape(detail: String) -> ClspvRefusal {
    ClspvRefusal::KernelShape { detail }
}

fn body_name(name: &str) -> String {
    match name.strip_suffix('"') {
        Some(quoted) => format!("{quoted}.m2v.body\""),
        None => format!("{name}.m2v.body"),
    }
}

/// Whether a `define` line defines `name`.
fn define_names(line: &str, name: &str) -> bool {
    line.contains(&format!("{name}("))
}

/// The kernel's definition under its body name, and its parameter types.
fn rename_define(line: &str, name: &str) -> Result<(String, Vec<String>), ClspvRefusal> {
    let at = line
        .find(&format!("{name}("))
        .ok_or_else(|| shape(format!("definition of {name} not found")))?;
    let open = at + name.len();
    let close = matching_paren(line, open)
        .ok_or_else(|| shape(format!("unbalanced parameter list for {name}")))?;
    let params = split_top(&line[open + 1..close])
        .into_iter()
        .map(|p| param_type(p).map(str::to_owned))
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| shape(format!("unparsed parameter list for {name}")))?;
    let renamed = format!("{}{}{}", &line[..at], body_name(name), &line[open..]);
    Ok((renamed, params))
}

/// The type at the front of one parameter: `ptr addrspace(1)`, `<3 x i32>`, `i32`.
fn param_type(param: &str) -> Option<&str> {
    let p = param.trim();
    if p.starts_with('<') {
        return p.find('>').map(|end| &p[..=end]);
    }
    if let Some(rest) = p.strip_prefix("ptr") {
        let rest_trim = rest.trim_start();
        if rest_trim.starts_with("addrspace(") {
            let start = p.len() - rest_trim.len();
            let end = p[start..].find(')')? + start;
            return Some(&p[..=end]);
        }
        return Some("ptr");
    }
    p.split_whitespace().next()
}

fn air_arg(node: &str) -> Result<AirArg, ClspvRefusal> {
    let fields = split_top(strip_tuple(node));
    let index = fields
        .first()
        .and_then(|f| f.trim().strip_prefix("i32 "))
        .and_then(|n| n.trim().parse().ok())
        .ok_or_else(|| shape(format!("argument node without an index: {node}")))?;
    let kind = fields
        .get(1)
        .and_then(|f| metadata_string(f))
        .ok_or_else(|| shape(format!("argument node without a kind: {node}")))?
        .to_owned();
    let int_after = |key: &str| {
        fields
            .iter()
            .position(|f| metadata_string(f) == Some(key))
            .and_then(|at| fields.get(at + 1))
            .and_then(|v| v.trim().strip_prefix("i32 "))
            .and_then(|n| n.trim().parse().ok())
    };
    let has = |key: &str| fields.iter().any(|f| metadata_string(f) == Some(key));
    use metal2vulkan::reflect::ResourceAccess;
    let access = if has("air.read_write") {
        Some(ResourceAccess::ReadWrite)
    } else if has("air.read") {
        Some(ResourceAccess::ReadOnly)
    } else if has("air.write") {
        Some(ResourceAccess::WriteOnly)
    } else {
        None
    };
    Ok(AirArg {
        index,
        kind,
        address_space: int_after("air.address_space"),
        location_index: int_after("air.location_index"),
        access,
        buffer_size: int_after("air.buffer_size"),
        type_size: int_after("air.arg_type_size"),
    })
}

/// A Metal builtin the wrapper computes, and the work-item function and region
/// triple it is computed from.
#[derive(Clone, Copy)]
enum Builtin {
    /// `get_*_id(d)` plus the region's base for that triple.
    WorkItem(&'static str, Option<u32>),
    /// A region triple alone.
    Region(u32),
    /// `get_local_size(d)`, bounded before it is narrowed.
    ///
    /// clspv reads the workgroup size from a `<3 x i32>` spec-constant
    /// variable. When two lanes of it are truncated to 16 bits (a `ushort2`
    /// builtin), InstCombine merges them into one `<6 x i16>` reinterpreting
    /// load of that variable, which clspv's later passes cannot lower and abort
    /// on. A `umin` with 65535 hides the reinterpretation and changes no value:
    /// a workgroup dimension is at most 1024 on any Vulkan device.
    LocalSize,
    /// `get_local_linear_id()`, a scalar.
    LocalLinear,
}

fn builtin(kind: &str) -> Option<Builtin> {
    Some(match kind {
        "air.thread_position_in_grid" => {
            Builtin::WorkItem("_Z13get_global_idj", Some(REGION_THREAD_BASE))
        }
        "air.thread_position_in_threadgroup" => Builtin::WorkItem("_Z12get_local_idj", None),
        "air.threadgroup_position_in_grid" => {
            Builtin::WorkItem("_Z12get_group_idj", Some(REGION_THREADGROUP_BASE))
        }
        "air.threads_per_threadgroup" => Builtin::LocalSize,
        "air.threads_per_grid" => Builtin::Region(REGION_THREADS_PER_GRID),
        "air.threadgroups_per_grid" => Builtin::Region(REGION_THREADGROUPS_PER_GRID),
        "air.thread_index_in_threadgroup" => Builtin::LocalLinear,
        _ => return None,
    })
}

/// `(lanes, element)` of a builtin parameter type: `i32`, `i16`, or a vector of
/// up to three of either.
fn builtin_lanes(ty: &str) -> Option<(u32, &str)> {
    match ty {
        "i32" | "i16" => Some((1, ty)),
        _ => {
            let inner = ty.strip_prefix('<')?.strip_suffix('>')?;
            let (n, elem) = inner.split_once(" x ")?;
            let n: u32 = n.trim().parse().ok()?;
            let elem = elem.trim();
            ((1..=3).contains(&n) && matches!(elem, "i32" | "i16")).then_some((n, elem))
        }
    }
}

fn wrapper(
    name: &str,
    params: &[String],
    args: &[AirArg],
    decls: &mut Declarations<'_>,
) -> Result<(String, Vec<KernelBuffer>), ClspvRefusal> {
    let mut sig = Vec::new();
    let mut body = String::new();
    let mut call_args = Vec::new();
    let mut buffers = Vec::new();
    let mut tmp = 0u32;
    let mut fresh = |stem: &str| {
        tmp += 1;
        format!("%m2v.{stem}{tmp}")
    };
    for (param_ty, arg) in params.iter().zip(args) {
        if arg.kind == "air.buffer" {
            // The parameter's type is the authority; older AIR (Metal 2.2)
            // does not repeat it in the metadata at all. Where both are
            // present they must agree.
            let space = param_ty
                .strip_prefix("ptr addrspace(")
                .and_then(|r| r.strip_suffix(')'))
                .and_then(|n| n.parse().ok())
                .unwrap_or(0);
            if arg.address_space.is_some_and(|stated| stated != space) {
                return Err(shape(format!(
                    "buffer {} is addrspace({space}) and its metadata says {:?}",
                    arg.index, arg.address_space
                )));
            }
            if !matches!(space, 1 | 2) {
                return Err(ClspvRefusal::AddressSpace {
                    index: arg.index,
                    space,
                });
            }
            let value = format!("%m2v.buf{}", arg.index);
            sig.push(format!("{param_ty} {value}"));
            call_args.push(format!("{param_ty} {value}"));
            let metal_index = arg.location_index.ok_or_else(|| {
                shape(format!("buffer {} names no [[buffer(n)]] index", arg.index))
            })?;
            buffers.push(KernelBuffer {
                param: arg.index,
                metal_index,
                address_space: space,
                // `constant` memory is read-only whatever the metadata says.
                access: if space == 2 {
                    Some(metal2vulkan::reflect::ResourceAccess::ReadOnly)
                } else {
                    arg.access
                },
                buffer_size: arg.buffer_size,
                type_size: arg.type_size,
            });
            continue;
        }
        let Some(b) = builtin(&arg.kind) else {
            return Err(ClspvRefusal::Argument {
                index: arg.index,
                kind: arg.kind.clone(),
            });
        };
        let (lanes, elem) = builtin_lanes(param_ty).ok_or_else(|| ClspvRefusal::BuiltinType {
            kind: arg.kind.clone(),
            ty: param_ty.clone(),
        })?;
        let mut lane_values = Vec::new();
        for d in 0..lanes {
            let v32 = match b {
                Builtin::WorkItem(func, base) => {
                    decls.need(&format!("declare i64 @{func}(i32)"), func);
                    let wide = fresh("wi");
                    body.push_str(&format!("  {wide} = call i64 @{func}(i32 {d})\n"));
                    let narrow = fresh("wi");
                    body.push_str(&format!("  {narrow} = trunc i64 {wide} to i32\n"));
                    match base {
                        None => narrow,
                        Some(base) => {
                            let word = region_word(&mut body, &mut fresh, base + d);
                            let sum = fresh("pos");
                            body.push_str(&format!("  {sum} = add i32 {narrow}, {word}\n"));
                            sum
                        }
                    }
                }
                Builtin::Region(base) => region_word(&mut body, &mut fresh, base + d),
                Builtin::LocalSize => {
                    decls.need(
                        "declare i64 @_Z14get_local_sizej(i32)",
                        "_Z14get_local_sizej",
                    );
                    decls.need("declare i32 @llvm.umin.i32(i32, i32)", "llvm.umin.i32");
                    let wide = fresh("ls");
                    body.push_str(&format!(
                        "  {wide} = call i64 @_Z14get_local_sizej(i32 {d})\n"
                    ));
                    let narrow = fresh("ls");
                    body.push_str(&format!("  {narrow} = trunc i64 {wide} to i32\n"));
                    let bounded = fresh("ls");
                    body.push_str(&format!(
                        "  {bounded} = call i32 @llvm.umin.i32(i32 {narrow}, i32 65535)\n"
                    ));
                    bounded
                }
                Builtin::LocalLinear => {
                    decls.need(
                        "declare i64 @_Z19get_local_linear_idv()",
                        "_Z19get_local_linear_idv",
                    );
                    let wide = fresh("lin");
                    body.push_str(&format!(
                        "  {wide} = call i64 @_Z19get_local_linear_idv()\n"
                    ));
                    let narrow = fresh("lin");
                    body.push_str(&format!("  {narrow} = trunc i64 {wide} to i32\n"));
                    narrow
                }
            };
            let v = if elem == "i16" {
                let t = fresh("h");
                body.push_str(&format!("  {t} = trunc i32 {v32} to i16\n"));
                t
            } else {
                v32
            };
            lane_values.push(v);
        }
        let value = if lanes == 1 {
            lane_values.remove(0)
        } else {
            let mut acc = "poison".to_owned();
            for (d, lane) in lane_values.iter().enumerate() {
                let next = fresh("vec");
                body.push_str(&format!(
                    "  {next} = insertelement {param_ty} {acc}, {elem} {lane}, i32 {d}\n"
                ));
                acc = next;
            }
            acc
        };
        call_args.push(format!("{param_ty} {value}"));
    }
    sig.push("ptr byval(%m2v.region) align 4 %m2v.region".to_owned());
    let text = format!(
        "\ndefine spir_kernel void @{ENTRY_POINT}({}) {{\nentry:\n{body}  call void {}({})\n  ret void\n}}\n",
        sig.join(", "),
        body_name(name),
        call_args.join(", ")
    );
    Ok((text, buffers))
}

fn region_word(body: &mut String, fresh: &mut impl FnMut(&str) -> String, word: u32) -> String {
    let ptr = fresh("rp");
    body.push_str(&format!(
        "  {ptr} = getelementptr inbounds %m2v.region, ptr %m2v.region, i32 0, i32 0, i32 {word}\n"
    ));
    let value = fresh("rw");
    body.push_str(&format!("  {value} = load i32, ptr {ptr}, align 4\n"));
    value
}

/// Declarations the rewrite adds, skipping any symbol the module already has.
struct Declarations<'a> {
    existing: &'a std::collections::BTreeSet<String>,
    added: std::collections::BTreeMap<String, String>,
}

impl<'a> Declarations<'a> {
    fn new(existing: &'a std::collections::BTreeSet<String>) -> Self {
        Self {
            existing,
            added: Default::default(),
        }
    }

    fn need(&mut self, decl: &str, symbol: &str) {
        let key = format!("@{symbol}");
        if !self.existing.contains(&key) {
            self.added.entry(key).or_insert_with(|| decl.to_owned());
        }
    }

    fn finish(self) -> String {
        let mut out = String::from("\n");
        for decl in self.added.values() {
            out.push_str(decl);
            out.push('\n');
        }
        out
    }
}

/// A body for one `declare ... @air.NAME(...)` line, or the refusal naming it.
fn air_definition(decl: &str, decls: &mut Declarations<'_>) -> Result<String, ClspvRefusal> {
    let at = decl.find("@air.").ok_or_else(|| shape(decl.to_owned()))?;
    let open = decl[at..]
        .find('(')
        .map(|o| o + at)
        .ok_or_else(|| shape(decl.to_owned()))?;
    let close = matching_paren(decl, open).ok_or_else(|| shape(decl.to_owned()))?;
    let symbol = &decl[at + 1..open];
    let name = &symbol[4..];
    let ret = decl["declare ".len()..at]
        .split_whitespace()
        .filter(|t| !matches!(*t, "noundef" | "zeroext" | "signext"))
        .collect::<Vec<_>>()
        .join(" ");
    let params = split_top(&decl[open + 1..close])
        .into_iter()
        .map(|p| param_type(p).map(str::to_owned))
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| shape(decl.to_owned()))?;
    let refuse = || ClspvRefusal::Intrinsic {
        name: symbol.to_owned(),
    };
    let body = air_body(name, &ret, &params, decls).ok_or_else(refuse)?;
    let sig = params
        .iter()
        .enumerate()
        .map(|(i, t)| format!("{t} %p{i}"))
        .collect::<Vec<_>>()
        .join(", ");
    Ok(format!(
        "define internal {ret} @{symbol}({sig}) alwaysinline {{\nentry:\n{body}}}\n"
    ))
}

/// The LLVM intrinsic suffix for a float type: `f32`, `f16`, `v4f32`.
fn float_suffix(ty: &str) -> Option<String> {
    match ty {
        "float" => Some("f32".into()),
        "half" => Some("f16".into()),
        _ => {
            let inner = ty.strip_prefix('<')?.strip_suffix('>')?;
            let (n, elem) = inner.split_once(" x ")?;
            let e = match elem.trim() {
                "float" => "f32",
                "half" => "f16",
                _ => return None,
            };
            Some(format!("v{}{e}", n.trim()))
        }
    }
}

/// `value` broadcast to `ty`: a scalar literal, or a vector of it.
fn splat(ty: &str, value: &str) -> String {
    match ty.strip_prefix('<').and_then(|t| t.strip_suffix('>')) {
        Some(inner) => {
            let (n, elem) = inner.split_once(" x ").unwrap_or(("1", inner));
            let n: usize = n.trim().parse().unwrap_or(1);
            let lanes = vec![format!("{} {value}", elem.trim()); n];
            format!("<{}>", lanes.join(", "))
        }
        None => value.to_owned(),
    }
}

fn air_body(
    name: &str,
    ret: &str,
    params: &[String],
    decls: &mut Declarations<'_>,
) -> Option<String> {
    let base = name.strip_prefix("fast_").unwrap_or(name);
    // `air.<op>.<suffix>`: the op, and a type suffix the declaration's own
    // types already carry, so the suffix is not parsed for meaning.
    let op = base.split('.').next()?;
    if op == "convert" {
        return convert_body(base, ret, params);
    }
    if op == "atomic" {
        return atomic_body(base, ret, params);
    }
    if base == "wg.barrier" && params.len() == 2 && ret == "void" {
        decls.need("declare void @_Z7barrierj(i32)", "_Z7barrierj");
        return Some(
            "  %tg = and i32 %p0, 2\n  %tgb = icmp ne i32 %tg, 0\n  %local = select i1 %tgb, i32 1, i32 0\n  \
             %dev = and i32 %p0, 1\n  %devb = icmp ne i32 %dev, 0\n  %global = select i1 %devb, i32 2, i32 0\n  \
             %flags = or i32 %local, %global\n  call void @_Z7barrierj(i32 %flags)\n  ret void\n"
                .to_owned(),
        );
    }
    let suffix = float_suffix(ret)?;
    if params.iter().any(|p| p != ret) {
        return None;
    }
    let mut call = |intr: &str, args: &[&str]| -> String {
        let full = format!("llvm.{intr}.{suffix}");
        let tys = vec![ret; args.len()].join(", ");
        decls.need(&format!("declare {ret} @{full}({tys})"), &full);
        let a = args
            .iter()
            .map(|v| format!("{ret} {v}"))
            .collect::<Vec<_>>()
            .join(", ");
        format!("call {ret} @{full}({a})")
    };
    let one = splat(ret, "1.0");
    let zero = splat(ret, "0.0");
    let text = match (op, params.len()) {
        (
            "sqrt" | "fabs" | "floor" | "ceil" | "trunc" | "rint" | "round" | "sin" | "cos" | "exp"
            | "exp2" | "log" | "log2",
            1,
        ) => format!("  %r = {}\n", call(op, &["%p0"])),
        ("rsqrt", 1) => format!(
            "  %s = {}\n  %r = fdiv {ret} {one}, %s\n",
            call("sqrt", &["%p0"])
        ),
        ("saturate", 1) => format!(
            "  %lo = {}\n  %r = {}\n",
            call("maxnum", &["%p0", &zero]),
            call("minnum", &["%lo", &one])
        ),
        ("fmin", 2) => format!("  %r = {}\n", call("minnum", &["%p0", "%p1"])),
        ("fmax", 2) => format!("  %r = {}\n", call("maxnum", &["%p0", "%p1"])),
        ("pow", 2) => format!("  %r = {}\n", call("pow", &["%p0", "%p1"])),
        ("copysign", 2) => format!("  %r = {}\n", call("copysign", &["%p0", "%p1"])),
        ("fma", 3) => format!("  %r = {}\n", call("fma", &["%p0", "%p1", "%p2"])),
        ("clamp", 3) => format!(
            "  %lo = {}\n  %r = {}\n",
            call("maxnum", &["%p0", "%p1"]),
            call("minnum", &["%lo", "%p2"])
        ),
        _ => return None,
    };
    Some(format!("{text}  ret {ret} %r\n"))
}

/// `air.atomic.<global|local>.<op>.<s|u>.i32(ptr, i32 value, i32 order, i32
/// scope, i1 volatile)` as an `atomicrmw`.
///
/// Read-modify-write only, and relaxed only: [`relaxed_atomic_calls`] has
/// already refused any call site whose order is not `memory_order_relaxed`, so
/// `monotonic` is the order every caller asked for.
fn atomic_body(name: &str, ret: &str, params: &[String]) -> Option<String> {
    let parts: Vec<&str> = name.split('.').collect();
    let ["atomic", space, op, sign, "i32"] = parts.as_slice() else {
        return None;
    };
    let ptr = match *space {
        "global" => "ptr addrspace(1)",
        "local" => "ptr addrspace(3)",
        _ => return None,
    };
    if ret != "i32" || params != [ptr, "i32", "i32", "i32", "i1"] {
        return None;
    }
    let rmw = match (*op, *sign) {
        ("add", _) => "add",
        ("sub", _) => "sub",
        ("and", _) => "and",
        ("or", _) => "or",
        ("xor", _) => "xor",
        ("xchg", _) => "xchg",
        ("min", "s") => "min",
        ("max", "s") => "max",
        ("min", "u") => "umin",
        ("max", "u") => "umax",
        _ => return None,
    };
    Some(format!(
        "  %r = atomicrmw {rmw} {ptr} %p0, i32 %p1 monotonic, align 4\n  ret i32 %r\n"
    ))
}

/// Refuse a module that calls an `air.atomic.*` function with any order but
/// relaxed (`i32 0`): [`atomic_body`] lowers every call to `monotonic`.
fn relaxed_atomic_calls(ll: &str) -> Result<(), ClspvRefusal> {
    for line in ll.lines() {
        let Some(at) = line.find("@air.atomic.") else {
            continue;
        };
        if line.trim_start().starts_with("declare") {
            continue;
        }
        let symbol_end = line[at..].find('(').map(|o| at + o);
        let args = symbol_end
            .and_then(|open| matching_paren(line, open).map(|close| &line[open + 1..close]))
            .map(split_top)
            .unwrap_or_default();
        if args.get(2).map(|a| a.trim()) != Some("i32 0") {
            let end = symbol_end.unwrap_or(line.len());
            return Err(ClspvRefusal::Intrinsic {
                name: format!("{} (non-relaxed order)", &line[at + 1..end]),
            });
        }
    }
    Ok(())
}

/// Element kind and width of a scalar or vector type: `('f', 32)` for `float`
/// or `<4 x float>`, `('i', 16)` for `i16`.
fn element(ty: &str) -> Option<(char, u32, u32)> {
    let (lanes, elem) = match ty.strip_prefix('<').and_then(|t| t.strip_suffix('>')) {
        Some(inner) => {
            let (n, e) = inner.split_once(" x ")?;
            (n.trim().parse().ok()?, e.trim())
        }
        None => (1, ty),
    };
    let (kind, width) = match elem {
        "float" => ('f', 32),
        "half" => ('f', 16),
        "double" => ('f', 64),
        _ => ('i', elem.strip_prefix('i')?.parse().ok()?),
    };
    Some((kind, width, lanes))
}

/// `air.convert.<dst kind>.<dst>.<src kind>.<src>`, where a kind is `f`, `s`
/// or `u`. Only the two signedness letters are read from the name; widths come
/// from the declared types.
fn convert_body(name: &str, ret: &str, params: &[String]) -> Option<String> {
    let parts: Vec<&str> = name.split('.').collect();
    let [_, dk, _, sk, _] = parts.as_slice() else {
        return None;
    };
    let [src] = params else { return None };
    let (dkind, dw, dl) = element(ret)?;
    let (skind, sw, sl) = element(src)?;
    if dl != sl {
        return None;
    }
    let cast = match (*dk, *sk) {
        ("f", "f") if dkind == 'f' && skind == 'f' => match dw.cmp(&sw) {
            std::cmp::Ordering::Greater => "fpext",
            std::cmp::Ordering::Less => "fptrunc",
            std::cmp::Ordering::Equal => return Some(format!("  ret {ret} %p0\n")),
        },
        ("f", "s") if dkind == 'f' && skind == 'i' => "sitofp",
        ("f", "u") if dkind == 'f' && skind == 'i' => "uitofp",
        ("s", "f") if dkind == 'i' && skind == 'f' => "fptosi",
        ("u", "f") if dkind == 'i' && skind == 'f' => "fptoui",
        ("s" | "u", "s" | "u") if dkind == 'i' && skind == 'i' => match dw.cmp(&sw) {
            std::cmp::Ordering::Greater if *sk == "s" => "sext",
            std::cmp::Ordering::Greater => "zext",
            std::cmp::Ordering::Less => "trunc",
            std::cmp::Ordering::Equal => return Some(format!("  ret {ret} %p0\n")),
        },
        _ => return None,
    };
    Some(format!(
        "  %r = {cast} {src} %p0 to {ret}\n  ret {ret} %r\n"
    ))
}

/// Drop `!alias.scope` and `!noalias` attachments: metal2vk found AIR's scoped
/// alias metadata in a form the LLVM inside clspv rejects.
fn strip_alias_attachments(line: &str) -> String {
    let mut out = line.to_owned();
    for key in [", !alias.scope !", ", !noalias !"] {
        while let Some(at) = out.find(key) {
            let digits = out[at + key.len()..]
                .chars()
                .take_while(char::is_ascii_digit)
                .count();
            out.replace_range(at..at + key.len() + digits, "");
        }
    }
    out
}

/// Every `@symbol` the module declares or defines, globals included.
fn global_names(ll: &str) -> std::collections::BTreeSet<String> {
    let mut names = std::collections::BTreeSet::new();
    for line in ll.lines() {
        let t = line.trim_start();
        let candidate = if t.starts_with("define ") || t.starts_with("declare ") {
            t.find('@').map(|at| &t[at..])
        } else if t.starts_with('@') {
            Some(t)
        } else {
            None
        };
        if let Some(rest) = candidate {
            let end = rest[1..]
                .find(|c: char| !(c.is_ascii_alphanumeric() || "._$-\"".contains(c)))
                .map_or(rest.len(), |e| e + 1);
            names.insert(rest[..end].to_owned());
        }
    }
    names
}

/// `!N = ...` lines, by `!N`.
fn metadata_nodes(ll: &str) -> std::collections::HashMap<&str, &str> {
    ll.lines()
        .filter_map(|line| {
            let (lhs, rhs) = line.split_once(" = ")?;
            (lhs.starts_with('!') && lhs[1..].chars().all(|c| c.is_ascii_digit()))
                .then(|| (lhs, rhs.trim_start_matches("distinct ").trim()))
        })
        .collect()
}

fn named_metadata<'a>(ll: &'a str, name: &str) -> Option<&'a str> {
    let prefix = format!("!{name} = ");
    ll.lines().find_map(|l| l.strip_prefix(prefix.as_str()))
}

/// The tuple a `!N` reference names, or the tuple itself if given inline.
fn resolve<'a>(
    meta: &std::collections::HashMap<&str, &'a str>,
    reference: &'a str,
) -> Result<&'a str, ClspvRefusal> {
    let r = reference.trim();
    if r.starts_with("!{") {
        return Ok(r);
    }
    meta.get(r)
        .copied()
        .ok_or_else(|| shape(format!("unresolved metadata {r}")))
}

fn strip_tuple(node: &str) -> &str {
    node.trim()
        .strip_prefix("!{")
        .and_then(|n| n.strip_suffix('}'))
        .unwrap_or("")
}

fn metadata_string(field: &str) -> Option<&str> {
    field.trim().strip_prefix("!\"")?.strip_suffix('"')
}

/// Split at commas outside brackets and quotes.
fn split_top(s: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0i32;
    let mut quoted = false;
    let mut start = 0;
    for (i, c) in s.char_indices() {
        match c {
            '"' => quoted = !quoted,
            '(' | '{' | '[' | '<' if !quoted => depth += 1,
            ')' | '}' | ']' | '>' if !quoted => depth -= 1,
            ',' if !quoted && depth == 0 => {
                parts.push(s[start..i].trim());
                start = i + 1;
            }
            _ => {}
        }
    }
    let last = s[start..].trim();
    if !last.is_empty() {
        parts.push(last);
    }
    parts
}

fn matching_paren(s: &str, open: usize) -> Option<usize> {
    let mut depth = 0i32;
    let mut quoted = false;
    for (i, c) in s[open..].char_indices() {
        match c {
            '"' => quoted = !quoted,
            '(' if !quoted => depth += 1,
            ')' if !quoted => {
                depth -= 1;
                if depth == 0 {
                    return Some(open + i);
                }
            }
            _ => {}
        }
    }
    None
}

// ---------------------------------------------------------------------------
// The SPIR-V half: hold clspv's module to metal2vulkan's interface.
// ---------------------------------------------------------------------------

const OP_EXT_INST_IMPORT: u32 = 11;
const OP_EXT_INST: u32 = 12;
const OP_ENTRY_POINT: u32 = 15;
const OP_CONSTANT: u32 = 43;
const OP_DECORATE: u32 = 71;
const DECORATION_BINDING: u32 = 33;
const DECORATION_DESCRIPTOR_SET: u32 = 34;
const EXECUTION_MODEL_GL_COMPUTE: u32 = 5;

/// `NonSemantic.ClspvReflection` instruction numbers this pass reads.
mod reflection_op {
    pub const KERNEL: u32 = 1;
    pub const ARGUMENT_INFO: u32 = 2;
    pub const ARGUMENT_STORAGE_BUFFER: u32 = 3;
    pub const ARGUMENT_POD_PUSH_CONSTANT: u32 = 7;
    pub const SPEC_CONSTANT_WORKGROUP_SIZE: u32 = 12;
    pub const PROPERTY_REQUIRED_WORKGROUP_SIZE: u32 = 24;
    pub const SPEC_CONSTANT_SUBGROUP_MAX_SIZE: u32 = 25;
    pub const WORKGROUP_VARIABLE_SIZE: u32 = 42;
}

/// Check clspv's module against `reflection` and move its buffers to the
/// descriptors metal2vulkan assigned them.
pub fn conform(
    spirv: &[u8],
    adapted: &Adapted,
    reflection: &ShaderReflection,
) -> Result<Vec<u8>, ClspvRefusal> {
    let iface = |detail: String| ClspvRefusal::Interface { detail };
    if !spirv.len().is_multiple_of(4) || spirv.len() < 20 {
        return Err(iface(format!("{} bytes is not a module", spirv.len())));
    }
    let mut words: Vec<u32> = spirv
        .chunks_exact(4)
        .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect();
    if words[0] != 0x0723_0203 {
        return Err(iface("not little-endian SPIR-V".to_owned()));
    }

    let mut sets = std::collections::HashSet::new();
    let mut constants = std::collections::HashMap::new();
    let mut reflections = Vec::new();
    let mut entry_points = Vec::new();
    // target -> (word index of its DescriptorSet literal, of its Binding literal)
    let mut descriptors: std::collections::HashMap<u32, (Option<usize>, Option<usize>)> =
        Default::default();
    let mut at = 5;
    while at < words.len() {
        let count = (words[at] >> 16) as usize;
        let op = words[at] & 0xffff;
        if count == 0 || at + count > words.len() {
            return Err(iface(format!("truncated instruction at word {at}")));
        }
        let ins = &words[at..at + count];
        match op {
            OP_EXT_INST_IMPORT if count >= 3 => {
                if spirv_string(&ins[2..]).starts_with("NonSemantic.ClspvReflection") {
                    sets.insert(ins[1]);
                }
            }
            OP_CONSTANT if count == 4 => {
                constants.insert(ins[2], ins[3]);
            }
            OP_EXT_INST if count >= 5 && sets.contains(&ins[3]) => {
                reflections.push((ins[4], ins[5..].to_vec()));
            }
            OP_ENTRY_POINT if count >= 4 => {
                entry_points.push((ins[1], spirv_string(&ins[3..])));
            }
            OP_DECORATE if count >= 4 => match ins[2] {
                DECORATION_DESCRIPTOR_SET => {
                    descriptors.entry(ins[1]).or_default().0 = Some(at + 3)
                }
                DECORATION_BINDING => descriptors.entry(ins[1]).or_default().1 = Some(at + 3),
                _ => {}
            },
            _ => {}
        }
        at += count;
    }

    if entry_points != [(EXECUTION_MODEL_GL_COMPUTE, ENTRY_POINT.to_owned())] {
        return Err(iface(format!("entry points {entry_points:?}")));
    }
    let value = |id: u32| {
        constants
            .get(&id)
            .copied()
            .ok_or_else(|| iface(format!("reflection operand %{id} is not a constant")))
    };

    let mut remap = std::collections::HashMap::new();
    let mut workgroup_spec = None;
    for (inst, operands) in &reflections {
        use reflection_op::*;
        match *inst {
            KERNEL | ARGUMENT_INFO | SPEC_CONSTANT_SUBGROUP_MAX_SIZE | WORKGROUP_VARIABLE_SIZE => {}
            ARGUMENT_STORAGE_BUFFER if operands.len() >= 4 => {
                let ordinal = value(operands[1])?;
                let from = (value(operands[2])?, value(operands[3])?);
                let param = *adapted
                    .buffers
                    .get(ordinal as usize)
                    .ok_or_else(|| iface(format!("storage buffer at ordinal {ordinal}")))?;
                let to = reflection
                    .bindings
                    .iter()
                    .find(|b| b.kind == ResourceKind::Buffer && b.param_index == Some(param))
                    .and_then(|b| b.descriptor)
                    .map(|d| (d.set, d.binding))
                    .ok_or_else(|| {
                        iface(format!(
                            "metal2vulkan assigns parameter {param} no descriptor"
                        ))
                    })?;
                remap.insert(from, to);
            }
            ARGUMENT_POD_PUSH_CONSTANT if operands.len() >= 4 => {
                let ordinal = value(operands[1])?;
                let (offset, size) = (value(operands[2])?, value(operands[3])?);
                if ordinal != adapted.region_ordinal() || offset != 0 || size != REGION_BYTES {
                    return Err(iface(format!(
                        "push constant ordinal {ordinal} offset {offset} size {size}"
                    )));
                }
            }
            SPEC_CONSTANT_WORKGROUP_SIZE if operands.len() >= 3 => {
                workgroup_spec = Some([
                    value(operands[0])?,
                    value(operands[1])?,
                    value(operands[2])?,
                ]);
            }
            PROPERTY_REQUIRED_WORKGROUP_SIZE => {
                return Err(iface("clspv fixed the workgroup size".to_owned()));
            }
            other => {
                return Err(iface(format!("clspv reflection instruction {other}")));
            }
        }
    }
    if workgroup_spec != Some([0, 1, 2]) {
        return Err(iface(format!("workgroup size spec ids {workgroup_spec:?}")));
    }

    for (target, slots) in &descriptors {
        let (Some(set_at), Some(binding_at)) = *slots else {
            return Err(iface(format!("%{target} has half a descriptor")));
        };
        let from = (words[set_at], words[binding_at]);
        let to = remap
            .get(&from)
            .ok_or_else(|| iface(format!("descriptor {from:?} is not a kernel buffer")))?;
        words[set_at] = to.0;
        words[binding_at] = to.1;
    }
    Ok(words.iter().flat_map(|w| w.to_le_bytes()).collect())
}

fn spirv_string(words: &[u32]) -> String {
    let bytes: Vec<u8> = words
        .iter()
        .flat_map(|w| w.to_le_bytes())
        .take_while(|b| *b != 0)
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

mod tool;

#[cfg(test)]
mod tests;
