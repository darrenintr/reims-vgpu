use super::*;

/// The fixture compute kernel's disassembly, read through the same `llvm-dis`
/// the route runs.
fn fixture_ll(name: &str) -> String {
    let air = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/air")
        .join(name);
    let dir = std::env::temp_dir().join(format!("m2v-clspv-test-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let ll = dir.join("k.ll");
    metal2vulkan::tools::run(
        "llvm-dis",
        &[air.to_str().unwrap(), "-o", ll.to_str().unwrap()],
    )
    .expect("llvm-dis reads the fixture");
    let text = std::fs::read_to_string(&ll).expect("disassembly");
    let _ = std::fs::remove_dir_all(&dir);
    text
}

/// A synthetic AIR module: one kernel whose parameters are `params` and whose
/// `!air.kernel` argument nodes are `arg_nodes`, plus extra `declare` lines.
fn module(params: &str, call_args: &str, arg_nodes: &[&str], declares: &str) -> String {
    let refs: Vec<String> = (0..arg_nodes.len())
        .map(|i| format!("!{}", 10 + i))
        .collect();
    let mut nodes = String::new();
    for (i, n) in arg_nodes.iter().enumerate() {
        nodes.push_str(&format!("!{} = !{{{n}}}\n", 10 + i));
    }
    format!(
        "target datalayout = \"e-p:64:64:64-i32:32:32-f32:32:32-v96:128:128-v128:128:128-n8:16:32\"\n\
         target triple = \"air64_v28-apple-macosx26.5.0\"\n\n\
         define void @k({params}) local_unnamed_addr #0 {{\n\
         entry:\n  call void @use({call_args})\n  ret void\n}}\n\n\
         declare void @use({params})\n{declares}\n\
         attributes #0 = {{ nounwind }}\n\n\
         !air.kernel = !{{!1}}\n\
         !1 = !{{ptr @k, !2, !3}}\n!2 = !{{}}\n!3 = !{{{}}}\n{nodes}",
        refs.join(", ")
    )
}

/// The fixture kernel becomes a `main` kernel over its one buffer plus the
/// region struct, the original body survives under its own name, and nothing
/// AIR-specific is left for clspv to trip on.
#[test]
fn the_fixture_kernel_is_wrapped_not_rewritten() {
    let ll = fixture_ll("float_mul4_add3.air");
    let adapted = adapt(&ll).expect("the fixture adapts");

    assert_eq!(adapted.buffers, vec![0], "one buffer, AIR parameter 0");
    assert_eq!(adapted.region_ordinal(), 1);
    let ir = &adapted.ir;
    assert!(
        ir.contains("target triple = \"spir64-unknown-unknown\""),
        "{ir}"
    );
    assert!(
        ir.contains("define spir_kernel void @main(ptr addrspace(1) %m2v.buf0, ptr byval(%m2v.region) align 4 %m2v.region)"),
        "{ir}"
    );
    assert!(
        ir.contains("@k.m2v.body("),
        "the body keeps its code under a new name"
    );
    assert!(
        ir.contains("call i64 @_Z13get_global_idj(i32 0)"),
        "thread_position_in_grid comes from the global id"
    );
    assert!(
        ir.contains("i32 0, i32 0, i32 3"),
        "plus the region's thread base"
    );
    assert!(!ir.contains("!air."), "no AIR named metadata survives");
    assert!(
        !ir.contains("!alias.scope"),
        "scoped alias metadata is dropped"
    );
}

/// A `uint3` position is assembled lane by lane, each lane offset by its own
/// region word, and a `ushort2` threadgroup position is narrowed per lane.
#[test]
fn vector_builtins_are_built_per_lane_from_their_region_triples() {
    let ll = module(
        "ptr addrspace(1) %b, <3 x i32> %gid, <2 x i16> %tg",
        "ptr addrspace(1) %b, <3 x i32> %gid, <2 x i16> %tg",
        &[
            "i32 0, !\"air.buffer\", !\"air.location_index\", i32 0, i32 1, !\"air.read_write\", !\"air.address_space\", i32 1",
            "i32 1, !\"air.thread_position_in_grid\"",
            "i32 2, !\"air.threadgroup_position_in_grid\"",
        ],
        "",
    );
    let ir = adapt(&ll).expect("adapts").ir;
    for word in [3, 4, 5] {
        assert!(
            ir.contains(&format!("i32 0, i32 0, i32 {word}\n")),
            "thread base {word}: {ir}"
        );
    }
    for word in [6, 7] {
        assert!(
            ir.contains(&format!("i32 0, i32 0, i32 {word}\n")),
            "threadgroup base {word}: {ir}"
        );
    }
    assert!(
        !ir.contains("i32 0, i32 0, i32 8\n"),
        "a ushort2 reads two lanes"
    );
    assert!(ir.contains("insertelement <3 x i32>"), "{ir}");
    assert!(ir.contains("trunc i32") && ir.contains("to i16"), "{ir}");
    assert!(
        ir.contains("call void @k.m2v.body(ptr addrspace(1) %m2v.buf0, <3 x i32> %m2v.vec"),
        "{ir}"
    );
}

/// What the route does not carry yet is refused by name, never guessed at.
#[test]
fn unsupported_kernels_are_refused_by_name() {
    let texture = module(
        "ptr addrspace(1) %t",
        "ptr addrspace(1) %t",
        &["i32 0, !\"air.texture\", !\"air.location_index\", i32 0, i32 1"],
        "",
    );
    assert_eq!(
        adapt(&texture).unwrap_err(),
        ClspvRefusal::Argument {
            index: 0,
            kind: "air.texture".into()
        }
    );

    let simd = module(
        "i32 %lane",
        "i32 %lane",
        &["i32 0, !\"air.thread_index_in_simdgroup\""],
        "",
    );
    assert!(matches!(adapt(&simd), Err(ClspvRefusal::Argument { .. })));

    let unknown = module(
        "ptr addrspace(1) %b",
        "ptr addrspace(1) %b",
        &["i32 0, !\"air.buffer\", !\"air.address_space\", i32 1"],
        "declare float @air.simd_sum.f32(float)\n",
    );
    assert_eq!(
        adapt(&unknown).unwrap_err(),
        ClspvRefusal::Intrinsic {
            name: "air.simd_sum.f32".into()
        }
    );

    let threadgroup_buffer = module(
        "ptr addrspace(3) %s",
        "ptr addrspace(3) %s",
        &["i32 0, !\"air.buffer\", !\"air.address_space\", i32 3"],
        "",
    );
    assert_eq!(
        adapt(&threadgroup_buffer).unwrap_err(),
        ClspvRefusal::AddressSpace { index: 0, space: 3 }
    );
}

/// Known `air.*` functions get bodies; conversions read signedness from the
/// name and widths from the declared types.
#[test]
fn known_air_functions_get_bodies() {
    let ll = module(
        "ptr addrspace(1) %b",
        "ptr addrspace(1) %b",
        &["i32 0, !\"air.buffer\", !\"air.address_space\", i32 1"],
        "declare <4 x float> @air.fast_sqrt.v4f32(<4 x float>)\n\
         declare float @air.clamp.f32(float, float, float)\n\
         declare float @air.convert.f.f32.u.i32(i32)\n\
         declare i32 @air.convert.s.i32.s.i16(i16)\n\
         declare void @air.wg.barrier(i32, i32)\n",
    );
    let ir = adapt(&ll).expect("adapts").ir;
    assert!(
        ir.contains("call <4 x float> @llvm.sqrt.v4f32(<4 x float> %p0)"),
        "{ir}"
    );
    assert!(
        ir.contains("declare <4 x float> @llvm.sqrt.v4f32(<4 x float>)"),
        "{ir}"
    );
    assert!(
        ir.contains("@llvm.maxnum.f32") && ir.contains("@llvm.minnum.f32"),
        "{ir}"
    );
    assert!(ir.contains("uitofp i32 %p0 to float"), "{ir}");
    assert!(ir.contains("sext i16 %p0 to i32"), "{ir}");
    assert!(ir.contains("call void @_Z7barrierj(i32 %flags)"), "{ir}");
}

/// Metal 2.2 AIR names a buffer's address space only in its parameter type;
/// the route reads it from there.
#[test]
fn a_buffer_without_metadata_address_space_takes_it_from_its_type() {
    let ll = module(
        "ptr addrspace(1) %b, i32 %gid",
        "ptr addrspace(1) %b, i32 %gid",
        &[
            "i32 0, !\"air.buffer\", !\"air.location_index\", i32 0, i32 1, !\"air.read_write\"",
            "i32 1, !\"air.thread_position_in_grid\"",
        ],
        "",
    );
    assert_eq!(adapt(&ll).expect("adapts").buffers, vec![0]);
}

/// The wrapper never collides with a symbol the guest module already has.
#[test]
fn a_module_that_already_defines_main_is_refused() {
    let ll = module(
        "ptr addrspace(1) %b",
        "ptr addrspace(1) %b",
        &["i32 0, !\"air.buffer\", !\"air.address_space\", i32 1"],
        "declare void @main()\n",
    );
    assert_eq!(
        adapt(&ll).unwrap_err(),
        ClspvRefusal::NameTaken {
            name: "@main".into()
        }
    );
}

/// The route's own kernels, compiled to AIR by Apple's Metal compiler on the
/// macOS runner (`.github/workflows/compile-clspv-fixtures.yml`), each adapt to
/// IR with the buffers their signatures declare, in parameter order.
#[test]
fn the_macos_compiled_kernels_adapt() {
    let clspv = |name: &str| {
        let air = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/clspv")
            .join(name);
        let dir = std::env::temp_dir().join(format!("m2v-clspv-fx-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        let ll = dir.join("k.ll");
        metal2vulkan::tools::run(
            "llvm-dis",
            &[air.to_str().unwrap(), "-o", ll.to_str().unwrap()],
        )
        .expect("llvm-dis reads the fixture");
        let text = std::fs::read_to_string(&ll).expect("disassembly");
        let _ = std::fs::remove_dir_all(&dir);
        adapt(&text)
    };
    for (name, buffers) in [
        ("grid3d.air", vec![0]),
        ("tg_reduce.air", vec![0, 1]),
        ("short_ids.air", vec![0]),
        ("math_convert.air", vec![0, 1, 2]),
        ("atomic_count.air", vec![0, 1]),
    ] {
        let adapted = clspv(name).unwrap_or_else(|r| panic!("{name}: {r:?}"));
        assert_eq!(adapted.buffers, buffers, "{name}");
        assert!(
            adapted.ir.contains("define spir_kernel void @main("),
            "{name}"
        );
    }
    let atomics = clspv("atomic_count.air").expect("relaxed atomics adapt").ir;
    assert!(
        atomics.contains("atomicrmw add ptr addrspace(1) %p0, i32 %p1 monotonic"),
        "{atomics}"
    );
}

/// An atomic call with any order but relaxed is refused: the body lowers every
/// call to `monotonic`, which would weaken a stronger order.
#[test]
fn a_non_relaxed_atomic_is_refused() {
    let ll = module(
        "ptr addrspace(1) %b",
        "ptr addrspace(1) %b",
        &["i32 0, !\"air.buffer\", !\"air.address_space\", i32 1"],
        "declare i32 @air.atomic.global.add.u.i32(ptr addrspace(1), i32, i32, i32, i1)\n\
         define void @helper(ptr addrspace(1) %p) {\n  %x = call i32 @air.atomic.global.add.u.i32(ptr addrspace(1) %p, i32 1, i32 5, i32 2, i1 true)\n  ret void\n}\n",
    );
    assert!(
        matches!(adapt(&ll), Err(ClspvRefusal::Intrinsic { name }) if name.contains("non-relaxed"))
    );
}
