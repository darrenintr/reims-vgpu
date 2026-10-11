//! Parity of the metal2vk compute route against metal2vulkan's emitter.
//!
//! On this branch every kernel is translated by `runtime::m2v_clspv` alone,
//! module and reflection both. metal2vulkan appears here only as an oracle:
//! each kernel is translated by both, each module runs through the shipped
//! compute entry with its *own* reflection's dispatch plan and the same
//! buffers, and the two must write the same bytes. Where the answer is exact it
//! is asserted too.
//!
//! Needs a Vulkan ICD and the patched clspv named by `REIMS_VGPU_CLSPV`; skips
//! by name without either. **Serial:** one process-global engine.

#![cfg(feature = "backend-vulkan")]

use metal2vulkan::passes::{Stage, TransformOptions};
use metal2vulkan::reflect::{KernelDispatch, ShaderReflection};
use reims_vgpu::backend::vulkan::engine::{
    self, ComputeBufferResource, ComputeDispatch, ComputeDispatchRegion, ComputeRequest,
};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

fn device() -> &'static reims_vgpu::model::DeviceState {
    static DEVICE: OnceLock<reims_vgpu::model::DeviceState> = OnceLock::new();
    DEVICE.get_or_init(|| {
        reims_vgpu::model::DeviceState::new(
            reims_vgpu::model::DeviceId(1),
            reims_vgpu_protocol::gva::PAGE_SHIFT_X86,
        )
    })
}

fn session() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    let guard = LOCK
        .get_or_init(|| {
            reims_vgpu::observe::redirect_logs_for_tests();
            Mutex::new(())
        })
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    engine::test_reset_engine(device());
    guard
}

fn words(bytes: &[u8]) -> Vec<u32> {
    bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|c| u32::from_le_bytes(*c))
        .collect()
}

/// Both modules for one kernel, with the reflection they answer to; `None`
/// (with a line saying why) when this host cannot run the comparison.
struct Pair {
    m2v_reflection: ShaderReflection,
    metal2vulkan: Vec<u32>,
    clspv_reflection: ShaderReflection,
    clspv: Vec<u32>,
}

fn translate(path: &str, local_size: [u32; 3]) -> Option<Pair> {
    let Some(clspv) = reims_vgpu_config::clspv_path() else {
        eprintln!("SKIP {path}: REIMS_VGPU_CLSPV names no clspv binary");
        return None;
    };
    let air = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(path);
    let tmp = std::env::temp_dir().join(format!(
        "vk_kernel_clspv_{}_{}",
        path.replace('/', "_"),
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("scratch");
    let opts = TransformOptions {
        kernel_local_size: local_size,
        ..Default::default()
    };
    let (m2v, reflection) = metal2vulkan::translate_reflected_with_options(
        air.to_str().unwrap(),
        Stage::Kernel,
        &tmp,
        opts,
    )
    .unwrap_or_else(|e| panic!("metal2vulkan {path}: {e}"));
    let (clspv_spirv, clspv_reflection) =
        reims_vgpu::runtime::m2v_clspv::compile_kernel(&clspv, &air, &tmp, local_size)
            .unwrap_or_else(|r| panic!("clspv route {path}: {r:?}"));
    let _ = std::fs::remove_dir_all(&tmp);
    Some(Pair {
        m2v_reflection: reflection,
        metal2vulkan: words(&m2v),
        clspv_reflection,
        clspv: words(&clspv_spirv),
    })
}

/// Run one module over `buffers` (binding, bytes) on a `threads` grid.
fn run(
    label: &str,
    spirv: Vec<u32>,
    reflection: &ShaderReflection,
    local_size: [u32; 3],
    threads: [u32; 3],
    buffers: &[(u32, Vec<u8>)],
) -> Option<Vec<Vec<u8>>> {
    let dispatch = reflection
        .kernel_dispatch
        .unwrap_or(KernelDispatch::safe_default());
    let push_offset = match dispatch {
        KernelDispatch::ThreadsDynamic { offset } => offset,
        other => panic!("{label}: dispatch contract {other:?}"),
    };
    let plan = dispatch.plan(local_size, Some(threads)).expect("plan");
    let regions = plan
        .regions
        .iter()
        .map(|r| ComputeDispatchRegion {
            local_size: r.local_size,
            group_count: r.group_count,
            push_constants: plan.push_constants(*r),
        })
        .collect();
    let req = ComputeRequest {
        spirv,
        entry: "main".into(),
        dispatch: ComputeDispatch::Regions {
            push_offset,
            threadgroups_per_grid: plan.threadgroups_per_grid,
            regions,
        },
        storage_buffers: buffers
            .iter()
            .map(|(binding, bytes)| ComputeBufferResource {
                binding: *binding,
                bytes: bytes.clone(),
                writable: true,
            })
            .collect(),
        sampled_images: vec![],
        samplers: vec![],
        storage_images: vec![],
    };
    match engine::execute_compute_request(device(), &req) {
        Ok(out) => Some(out.buffers.into_iter().map(|b| b.bytes).collect()),
        Err(e) => {
            let s = e.to_string();
            let lower = s.to_ascii_lowercase();
            if lower.contains("no vulkan") || lower.contains("vk_engine_init") {
                eprintln!("SKIP {label}: no GPU ({s})");
                None
            } else {
                panic!("{label}: {s}");
            }
        }
    }
}

/// The writable buffers one run read back, in request order.
type Buffers = Vec<Vec<u8>>;

/// Both modules over the same input: `(metal2vulkan's buffers, clspv's)`.
fn both(
    path: &str,
    local_size: [u32; 3],
    threads: [u32; 3],
    buffers: &[(u32, Vec<u8>)],
) -> Option<(Buffers, Buffers)> {
    let _g = session();
    let pair = translate(path, local_size)?;
    let m2v = run(
        &format!("{path} metal2vulkan"),
        pair.metal2vulkan,
        &pair.m2v_reflection,
        local_size,
        threads,
        buffers,
    )?;
    let clspv = run(
        &format!("{path} clspv"),
        pair.clspv,
        &pair.clspv_reflection,
        local_size,
        threads,
        buffers,
    )?;
    Some((m2v, clspv))
}

/// [`both`], asserting the two emitters wrote the same bytes; returns them.
fn parity(
    path: &str,
    local_size: [u32; 3],
    threads: [u32; 3],
    buffers: &[(u32, Vec<u8>)],
) -> Option<Vec<Vec<u8>>> {
    let (m2v, clspv) = both(path, local_size, threads, buffers)?;
    assert_eq!(clspv, m2v, "{path}: the two emitters wrote different bytes");
    Some(clspv)
}

/// `x` rounded to the nearest half-precision value, ties to even, as Metal's
/// `half(float)` conversion is specified to round. Finite, normal-range inputs
/// only, which is all the test feeds it.
fn round_to_half(x: f32) -> f32 {
    let bits = x.to_bits();
    let sign = bits & 0x8000_0000;
    let exp = ((bits >> 23) & 0xff) as i32 - 127;
    assert!(
        (-14..=15).contains(&exp),
        "{x} is outside half's normal range"
    );
    // A half keeps 10 of the float's 23 mantissa bits.
    let drop = 13;
    let mant = bits & 0x007f_ffff;
    let keep = mant >> drop;
    let rest = mant & ((1 << drop) - 1);
    let half_way = 1 << (drop - 1);
    let round_up = rest > half_way || (rest == half_way && keep & 1 == 1);
    let rounded = (bits & !((1 << drop) - 1)) + if round_up { 1 << drop } else { 0 };
    f32::from_bits(rounded | sign)
}

fn f32s(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_le_bytes()).collect()
}
fn u32s(v: &[u32]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_le_bytes()).collect()
}
fn read_u32(b: &[u8]) -> Vec<u32> {
    words(b)
}
fn read_f32(b: &[u8]) -> Vec<f32> {
    words(b).into_iter().map(f32::from_bits).collect()
}

#[test]
fn float_mul4_add3_matches() {
    let input = f32s(&[1., 2., 3., 4., 5., 6., 7., 8.]);
    let Some(out) = parity(
        "air/float_mul4_add3.air",
        [64, 1, 1],
        [8, 1, 1],
        &[(0, input)],
    ) else {
        return;
    };
    assert_eq!(
        read_f32(&out[0]),
        vec![7., 11., 15., 19., 23., 27., 31., 35.]
    );
}

/// A 5x3x2 grid over 4x2x1 threadgroups: partial groups on two axes, so the
/// plan issues edge regions whose thread base the route must add.
#[test]
fn grid3d_positions_cross_edge_regions() {
    let grid = [5u32, 3, 2];
    let n = (grid[0] * grid[1] * grid[2]) as usize;
    let Some(out) = parity(
        "clspv/grid3d.air",
        [4, 2, 1],
        grid,
        &[(0, u32s(&vec![u32::MAX; n]))],
    ) else {
        return;
    };
    assert_eq!(read_u32(&out[0]), (0..n as u32).collect::<Vec<_>>());
}

/// Threadgroup memory and barriers: four groups of 64 each sum their slice.
#[test]
fn tg_reduce_sums_each_group() {
    let input: Vec<f32> = (1..=256).map(|i| i as f32).collect();
    let Some(out) = parity(
        "clspv/tg_reduce.air",
        [64, 1, 1],
        [256, 1, 1],
        &[(0, f32s(&input)), (1, f32s(&[0.0; 4]))],
    ) else {
        return;
    };
    let want: Vec<f32> = (0..4)
        .map(|g| input[g * 64..(g + 1) * 64].iter().sum())
        .collect();
    assert_eq!(read_f32(&out[1]), want);
}

/// A constant parameter buffer, float math, half rounding and conversions,
/// against an exact reference for `int4(float4(half4(r)) * 16) + bias`.
///
/// The clspv module must match the reference. metal2vulkan's is reported, not
/// asserted: on lavapipe it rounds two of these values toward zero where
/// Metal's `half(float)` rounds to nearest even (5.562072 to 5.5586 instead
/// of 5.5625), which is its defect and not this route's.
///
/// A value within four float ulps of a half rounding midpoint is skipped: a
/// fast-math `sqrt` is not required to be correctly rounded, so a GPU may land
/// either side of it and both answers are Metal-conformant.
#[test]
fn math_convert_matches_the_reference() {
    let input: Vec<f32> = (0..64).map(|i| (i as f32 - 20.0) * 0.37).collect();
    let mut params = Vec::new();
    params.extend_from_slice(&1.5f32.to_le_bytes());
    params.extend_from_slice(&(-3.0f32).to_le_bytes());
    params.extend_from_slice(&9.0f32.to_le_bytes());
    params.extend_from_slice(&7u32.to_le_bytes());
    let Some((m2v, clspv)) = both(
        "clspv/math_convert.air",
        [8, 1, 1],
        [16, 1, 1],
        &[(0, f32s(&input)), (1, u32s(&[0; 64])), (2, params)],
    ) else {
        return;
    };
    let ints = |b: &[u8]| -> Vec<i32> { words(b).into_iter().map(|w| w as i32).collect() };
    let (got, theirs) = (ints(&clspv[1]), ints(&m2v[1]));
    let mut judged = 0;
    let mut m2v_off = Vec::new();
    for (i, v) in input.iter().enumerate() {
        let r = (v.abs().sqrt() as f64 * 1.5 + 1.0) as f32;
        let r = r.clamp(-3.0, 9.0);
        let near = |d: i32| round_to_half(f32::from_bits((r.to_bits() as i32 + d) as u32));
        if near(-4) != near(4) {
            continue;
        }
        judged += 1;
        let want = (round_to_half(r) * 16.0) as i32 + 7;
        assert_eq!(got[i], want, "clspv element {i}: r={r}");
        if theirs[i] != want {
            m2v_off.push((i, theirs[i], want));
        }
    }
    assert!(judged >= 60, "only {judged} of 64 elements were judged");
    if !m2v_off.is_empty() {
        eprintln!("math_convert: metal2vulkan differs from the reference at (index, got, want) {m2v_off:?}");
    }
}

/// 16-bit builtins: every thread of a 16x8 grid writes its flat index within
/// its 8x4 threadgroup.
#[test]
fn short_ids_write_their_flat_index() {
    let Some(out) = parity(
        "clspv/short_ids.air",
        [8, 4, 1],
        [16, 8, 1],
        &[(0, vec![0xff; 64 * 8 * 2])],
    ) else {
        return;
    };
    let got: Vec<u16> = out[0]
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_le_bytes(*c))
        .collect();
    for y in 0..8usize {
        for x in 0..16usize {
            let want = ((y % 4) * 8 + (x % 8)) as u16;
            assert_eq!(got[y * 64 + x], want, "({x},{y})");
        }
    }
}

/// Relaxed atomics: 256 inputs, half of them odd.
#[test]
fn atomic_count_counts_odd_inputs() {
    let input: Vec<u32> = (0..256).collect();
    let Some(out) = parity(
        "clspv/atomic_count.air",
        [64, 1, 1],
        [256, 1, 1],
        &[(0, u32s(&[0])), (1, u32s(&input))],
    ) else {
        return;
    };
    assert_eq!(read_u32(&out[0])[0], 128);
}

/// The interface check is what makes the clspv module safe to hand the
/// dispatcher: buffers move to whatever descriptor the reflection names, and a
/// reflection that names none for a buffer the kernel uses is refused rather
/// than bound somewhere guessed.
#[test]
fn the_clspv_module_follows_the_reflections_descriptors_or_is_refused() {
    let Some(clspv) = reims_vgpu_config::clspv_path() else {
        eprintln!("SKIP descriptor remap: REIMS_VGPU_CLSPV names no clspv binary");
        return;
    };
    let air =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/air/float_mul4_add3.air");
    let tmp = std::env::temp_dir().join(format!("vk_kernel_clspv_remap_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("scratch");
    let opts = TransformOptions {
        kernel_local_size: [64, 1, 1],
        ..Default::default()
    };
    let (_, reflection) = metal2vulkan::translate_reflected_with_options(
        air.to_str().unwrap(),
        Stage::Kernel,
        &tmp,
        opts,
    )
    .expect("metal2vulkan");

    let mut moved = reflection.clone();
    let buffer = moved
        .bindings
        .iter_mut()
        .find(|b| b.param_index == Some(0))
        .expect("the buffer's binding");
    buffer.descriptor.as_mut().expect("a descriptor").binding = 5;
    let module = words(
        &reims_vgpu::runtime::m2v_clspv::compile(&clspv, &air, &tmp, &moved)
            .expect("remapped module"),
    );
    // OpDecorate (opcode 71, three operands) %target Binding(33) 5.
    let decorated_5 = module
        .windows(4)
        .any(|w| w[0] == (4 << 16 | 71) && w[2] == 33 && w[3] == 5);
    assert!(decorated_5, "the buffer did not move to binding 5");

    let mut missing = reflection;
    missing.bindings.retain(|b| b.param_index != Some(0));
    let refusal = reims_vgpu::runtime::m2v_clspv::compile(&clspv, &air, &tmp, &missing)
        .expect_err("a buffer with no descriptor must be refused");
    assert!(
        matches!(
            refusal,
            reims_vgpu::runtime::m2v_clspv::ClspvRefusal::Interface { .. }
        ),
        "{refusal:?}"
    );
    let _ = std::fs::remove_dir_all(&tmp);
}
