//! Textures and samplers on the metal2vk route.
//!
//! AIR passes a texture or a sampler as an opaque `ptr addrspace(1|2)` and
//! touches it only through `air.*_texture_2d` functions. clspv reads OpenCL
//! images: a `target("spirv.Image", ...)` handle used through `read_image*`,
//! `write_image*` and `get_image_*`. So a handle parameter is retyped where it
//! is declared and wherever it is passed, each texture function is renamed per
//! image access (a read-only, write-only and read-write image are different
//! OpenCL types), and each renamed function gets a body over the OpenCL
//! built-in.
//!
//! Carried: 2D textures of `float`, `half`, `uint`, `ushort`, `int` and `short`
//! with `read`, `write`, `read_write` and `sample` access; `read` and `write` at
//! level 0; `get_width` and `get_height` at level 0; and `sample` through a
//! sampler argument at explicit level 0 with no offset. Everything else is
//! refused by name: other texture types, a constexpr sampler, a non-zero level
//! or offset, and a handle used any way but as an argument to those functions.

use super::{matching_paren, param_type, split_top, ClspvRefusal, Declarations};

/// The OpenCL image access a Metal texture becomes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ImageAccess {
    /// `read` and `sample`: a sampled image.
    ReadOnly,
    /// `write`: a storage image nothing reads.
    WriteOnly,
    /// `read_write`: a storage image.
    ReadWrite,
}

impl ImageAccess {
    fn qualifier(self) -> u32 {
        match self {
            Self::ReadOnly => 0,
            Self::WriteOnly => 1,
            Self::ReadWrite => 2,
        }
    }

    fn suffix(self) -> &'static str {
        match self {
            Self::ReadOnly => "ro",
            Self::WriteOnly => "wo",
            Self::ReadWrite => "rw",
        }
    }

    /// The Itanium-mangled OpenCL image type.
    fn mangled(self) -> &'static str {
        match self {
            Self::ReadOnly => "14ocl_image2d_ro",
            Self::WriteOnly => "14ocl_image2d_wo",
            Self::ReadWrite => "14ocl_image2d_rw",
        }
    }

    /// The LLVM type clspv reads an image of this access as.
    pub fn image_type(self) -> String {
        format!(
            "target(\"spirv.Image\", void, 1, 0, 0, 0, 0, 0, {})",
            self.qualifier()
        )
    }

    /// Whether the image is a storage image (written).
    pub fn writable(self) -> bool {
        !matches!(self, Self::ReadOnly)
    }
}

/// The LLVM type clspv reads a sampler as.
pub const SAMPLER_TYPE: &str = "target(\"spirv.Sampler\")";

/// A texture's element type, as its Metal type name declares it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Component {
    Float,
    Half,
    Uint,
    Ushort,
    Int,
    Short,
}

/// One texture argument.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KernelTexture {
    pub param: u32,
    pub metal_index: u32,
    pub access: ImageAccess,
    pub component: Component,
}

/// One sampler argument.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KernelSampler {
    pub param: u32,
    pub metal_index: u32,
}

/// Read a texture argument's Metal type name: `texture2d<float, read>`,
/// `texture2d<half, write>`, `texture2d<uint, read_write>`,
/// `texture2d<float, sample>` or `texture2d<float>` (sample).
pub fn parse_texture(
    index: u32,
    type_name: Option<&str>,
) -> Result<(ImageAccess, Component), ClspvRefusal> {
    let refuse = || ClspvRefusal::Argument {
        index,
        kind: type_name.unwrap_or("air.texture").to_owned(),
    };
    let inner = type_name
        .and_then(|n| n.strip_prefix("texture2d<"))
        .and_then(|n| n.strip_suffix('>'))
        .ok_or_else(refuse)?;
    let mut parts = inner.split(',').map(str::trim);
    let component = match parts.next() {
        Some("float") => Component::Float,
        Some("half") => Component::Half,
        Some("uint") => Component::Uint,
        Some("ushort") => Component::Ushort,
        Some("int") => Component::Int,
        Some("short") => Component::Short,
        _ => return Err(refuse()),
    };
    let access = match parts.next() {
        None | Some("sample") | Some("read") => ImageAccess::ReadOnly,
        Some("write") => ImageAccess::WriteOnly,
        Some("read_write") => ImageAccess::ReadWrite,
        Some(_) => return Err(refuse()),
    };
    if parts.next().is_some() {
        return Err(refuse());
    }
    Ok((access, component))
}

/// What a handle parameter is, for retyping it.
#[derive(Clone, Copy, Debug)]
pub enum Handle {
    Image(ImageAccess),
    Sampler,
}

impl Handle {
    pub fn llvm_type(self) -> String {
        match self {
            Self::Image(access) => access.image_type(),
            Self::Sampler => SAMPLER_TYPE.to_owned(),
        }
    }
}

/// The texture functions the body calls, by renamed symbol, with the access
/// they were renamed for.
pub type UsedVariants = std::collections::BTreeMap<String, (String, ImageAccess)>;

/// Whether `symbol` (without `@`) is an AIR texture function this module owns.
pub fn is_texture_function(symbol: &str) -> bool {
    symbol.starts_with("air.") && symbol.contains("_texture_")
}

/// Retype every use of a handle in one body line, and rename the texture
/// function a handle is passed to after its image access.
pub fn rewrite_body_line(
    line: &str,
    handles: &std::collections::HashMap<String, Handle>,
    used: &mut UsedVariants,
) -> Result<String, ClspvRefusal> {
    let mut out = line.to_owned();
    for (name, handle) in handles {
        out = retype_uses(&out, name, *handle)?;
    }
    let touched = out != line;
    // A handle may only reach the texture functions this module defines: any
    // other callee (a helper the AIR compiler did not inline) still takes it
    // as a pointer, and an OpenCL image cannot be one.
    let refuse_other = || ClspvRefusal::Argument {
        index: 0,
        kind: format!("texture or sampler passed to `{}`", line.trim()),
    };
    let Some(at) = out.find("@air.") else {
        return if touched {
            Err(refuse_other())
        } else {
            Ok(out)
        };
    };
    let Some(open) = out[at..].find('(').map(|o| at + o) else {
        return if touched {
            Err(refuse_other())
        } else {
            Ok(out)
        };
    };
    let symbol = out[at + 1..open].to_owned();
    if !is_texture_function(&symbol) {
        return if touched {
            Err(refuse_other())
        } else {
            Ok(out)
        };
    }
    let close = matching_paren(&out, open).ok_or_else(|| super::shape(out.clone()))?;
    let args: Vec<String> = split_top(&out[open + 1..close])
        .into_iter()
        .map(str::to_owned)
        .collect();
    let access = args
        .first()
        .and_then(|a| image_access_of(a))
        .ok_or_else(|| ClspvRefusal::Intrinsic {
            name: format!("{symbol} (texture is not a kernel argument)"),
        })?;
    check_call_constants(&symbol, &args)?;
    let renamed = format!("{symbol}.m2v.{}", access.suffix());
    used.insert(renamed.clone(), (symbol.clone(), access));
    out.replace_range(at + 1..open, &renamed);
    Ok(out)
}

fn image_access_of(arg: &str) -> Option<ImageAccess> {
    [
        ImageAccess::ReadOnly,
        ImageAccess::WriteOnly,
        ImageAccess::ReadWrite,
    ]
    .into_iter()
    .find(|a| arg.trim_start().starts_with(&a.image_type()))
}

/// Replace the `ptr addrspace(N) <attrs>` in front of every use of `name` with
/// the handle's type. Any use that is not such an operand is refused: a handle
/// stored, compared or selected has no OpenCL image equivalent here.
fn retype_uses(line: &str, name: &str, handle: Handle) -> Result<String, ClspvRefusal> {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(at) = find_token(rest, name) {
        let before = &rest[..at];
        let start = operand_start(before);
        let operand = before[start..].trim();
        if !operand.starts_with("ptr addrspace(") {
            return Err(ClspvRefusal::Argument {
                index: 0,
                kind: format!("texture or sampler {name} used as `{}`", line.trim()),
            });
        }
        out.push_str(&before[..start]);
        if start != 0 {
            out.push(' ');
        }
        out.push_str(&handle.llvm_type());
        out.push(' ');
        out.push_str(name);
        rest = &rest[at + name.len()..];
    }
    out.push_str(rest);
    Ok(out)
}

/// The position of `name` as a whole token in `s`.
fn find_token(s: &str, name: &str) -> Option<usize> {
    let mut from = 0;
    while let Some(i) = s[from..].find(name).map(|i| from + i) {
        let end = i + name.len();
        let next = s[end..].chars().next();
        if !next.is_some_and(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_') {
            return Some(i);
        }
        from = end;
    }
    None
}

/// Where the operand ending at the end of `before` starts: just after the
/// previous top-level `,` or `(`.
fn operand_start(before: &str) -> usize {
    let mut depth = 0i32;
    for (i, c) in before.char_indices().rev() {
        match c {
            ')' => depth += 1,
            '(' if depth > 0 => depth -= 1,
            '(' | ',' => return i + 1,
            _ => {}
        }
    }
    0
}

/// The call-site constants a body can only honour at one value.
fn check_call_constants(symbol: &str, args: &[String]) -> Result<(), ClspvRefusal> {
    let base = symbol.trim_start_matches("air.");
    let refuse = |what: &str| ClspvRefusal::Intrinsic {
        name: format!("{symbol} ({what})"),
    };
    let is = |i: usize, want: &str| args.get(i).map(|a| a.trim()) == Some(want);
    if base.starts_with("read_texture_2d") {
        is(2, "i32 0")
            .then_some(())
            .ok_or_else(|| refuse("non-zero level"))
    } else if base.starts_with("write_texture_2d") {
        is(3, "i32 0")
            .then_some(())
            .ok_or_else(|| refuse("non-zero level"))
    } else if base.starts_with("get_width_texture_2d") || base.starts_with("get_height_texture_2d")
    {
        is(1, "i32 0")
            .then_some(())
            .ok_or_else(|| refuse("non-zero level"))
    } else if base.starts_with("sample_texture_2d") {
        if !args
            .get(1)
            .is_some_and(|a| a.trim_start().starts_with(SAMPLER_TYPE))
        {
            return Err(refuse("constexpr sampler"));
        }
        let zero = "float 0.000000e+00";
        (is(4, "<2 x i32> zeroinitializer") && is(6, zero) && is(7, zero))
            .then_some(())
            .ok_or_else(|| refuse("offset or non-zero level"))
    } else {
        Err(refuse("unsupported texture function"))
    }
}

/// The definition of one renamed texture function, from the AIR declaration
/// of the function it renames.
pub fn definition(
    renamed: &str,
    decl: &str,
    access: ImageAccess,
    decls: &mut Declarations<'_>,
) -> Result<String, ClspvRefusal> {
    let at = decl
        .find("@air.")
        .ok_or_else(|| super::shape(decl.to_owned()))?;
    let open = decl[at..]
        .find('(')
        .map(|o| o + at)
        .ok_or_else(|| super::shape(decl.to_owned()))?;
    let close = matching_paren(decl, open).ok_or_else(|| super::shape(decl.to_owned()))?;
    let symbol = &decl[at + 1..open];
    let ret = decl["declare ".len()..at]
        .split_whitespace()
        .filter(|t| !matches!(*t, "noundef" | "zeroext" | "signext"))
        .collect::<Vec<_>>()
        .join(" ");
    let mut params: Vec<String> = split_top(&decl[open + 1..close])
        .into_iter()
        .map(|p| param_type(p).map(str::to_owned))
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| super::shape(decl.to_owned()))?;
    let refuse = || ClspvRefusal::Intrinsic {
        name: symbol.to_owned(),
    };
    let img = access.image_type();
    let m = access.mangled();
    let base = symbol.trim_start_matches("air.");
    let integer = |s: &str| {
        if s.contains(".u.") {
            Some(('u', "ui", "j"))
        } else if s.contains(".s.") {
            Some(('s', "i", "i"))
        } else {
            None
        }
    };
    *params.first_mut().ok_or_else(refuse)? = img.clone();
    let body = if base.starts_with("read_texture_2d") || base.starts_with("sample_texture_2d") {
        let sampled = base.starts_with("sample_texture_2d");
        if sampled {
            *params.get_mut(1).ok_or_else(refuse)? = SAMPLER_TYPE.to_owned();
        }
        // `{ <4 x T>, i8 }`: the texel and a status byte; the texel type
        // decides the built-in.
        let texel = ret
            .strip_prefix("{ ")
            .and_then(|r| r.strip_suffix(", i8 }"))
            .ok_or_else(refuse)?
            .to_owned();
        let (lanes, elem) = texel_shape(&texel).ok_or_else(refuse)?;
        if lanes != 4 {
            return Err(refuse());
        }
        let (fetch_ty, call_name, half) = match (elem, integer(base)) {
            ("float", None) => ("<4 x float>", "f", false),
            ("half", None) => ("<4 x float>", "f", true),
            ("i32", Some((_, n, _))) => ("<4 x i32>", n, false),
            ("i16", Some((_, n, _))) => ("<4 x i32>", n, false),
            _ => return Err(refuse()),
        };
        let mut b = String::new();
        if sampled {
            let builtin = format!(
                "_Z{}read_image{call_name}{m}11ocl_samplerDv2_f",
                10 + call_name.len()
            );
            if params.get(2).map(String::as_str) != Some("<2 x float>") {
                return Err(refuse());
            }
            decls.need(
                &format!("declare {fetch_ty} @{builtin}({img}, {SAMPLER_TYPE}, <2 x float>)"),
                &builtin,
            );
            b.push_str(&format!(
                "  %v = call {fetch_ty} @{builtin}({img} %p0, {SAMPLER_TYPE} %p1, <2 x float> %p2)\n"
            ));
        } else {
            let builtin = format!("_Z{}read_image{call_name}{m}Dv2_i", 10 + call_name.len());
            let coord = coord_i32(&params, 1, &mut b).ok_or_else(refuse)?;
            decls.need(
                &format!("declare {fetch_ty} @{builtin}({img}, <2 x i32>)"),
                &builtin,
            );
            b.push_str(&format!(
                "  %v = call {fetch_ty} @{builtin}({img} %p0, <2 x i32> {coord})\n"
            ));
        }
        let value = match (half, elem) {
            (true, _) => {
                b.push_str("  %t = fptrunc <4 x float> %v to <4 x half>\n");
                "%t"
            }
            (false, "i16") => {
                b.push_str("  %t = trunc <4 x i32> %v to <4 x i16>\n");
                "%t"
            }
            _ => "%v",
        };
        b.push_str(&format!(
            "  %s0 = insertvalue {ret} poison, {texel} {value}, 0\n  %s = insertvalue {ret} %s0, i8 1, 1\n  ret {ret} %s\n"
        ));
        b
    } else if base.starts_with("write_texture_2d") {
        let value_ty = params.get(2).cloned().ok_or_else(refuse)?;
        let (lanes, elem) = texel_shape(&value_ty).ok_or_else(refuse)?;
        if lanes != 4 || ret != "void" {
            return Err(refuse());
        }
        let mut b = String::new();
        let coord = coord_i32(&params, 1, &mut b).ok_or_else(refuse)?;
        let (call_name, code, stored_ty, value) = match (elem, integer(base)) {
            ("float", None) => ("f", "f", "<4 x float>", "%p2"),
            ("half", None) => {
                b.push_str("  %w = fpext <4 x half> %p2 to <4 x float>\n");
                ("f", "f", "<4 x float>", "%w")
            }
            ("i32", Some((_, n, c))) => (n, c, "<4 x i32>", "%p2"),
            ("i16", Some((sign, n, c))) => {
                let ext = if sign == 's' { "sext" } else { "zext" };
                b.push_str(&format!("  %w = {ext} <4 x i16> %p2 to <4 x i32>\n"));
                (n, c, "<4 x i32>", "%w")
            }
            _ => return Err(refuse()),
        };
        let builtin = format!(
            "_Z{}write_image{call_name}{m}Dv2_iDv4_{code}",
            11 + call_name.len()
        );
        decls.need(
            &format!("declare void @{builtin}({img}, <2 x i32>, {stored_ty})"),
            &builtin,
        );
        b.push_str(&format!(
            "  call void @{builtin}({img} %p0, <2 x i32> {coord}, {stored_ty} {value})\n  ret void\n"
        ));
        b
    } else if let Some(dim) = base
        .strip_prefix("get_width_texture_2d")
        .map(|_| "width")
        .or_else(|| base.strip_prefix("get_height_texture_2d").map(|_| "height"))
    {
        if ret != "i32" {
            return Err(refuse());
        }
        let builtin = format!("_Z{}get_image_{dim}{m}", 10 + dim.len());
        decls.need(&format!("declare i32 @{builtin}({img})"), &builtin);
        format!("  %r = call i32 @{builtin}({img} %p0)\n  ret i32 %r\n")
    } else {
        return Err(refuse());
    };
    let sig = params
        .iter()
        .enumerate()
        .map(|(i, t)| format!("{t} %p{i}"))
        .collect::<Vec<_>>()
        .join(", ");
    Ok(format!(
        "define internal {ret} @{renamed}({sig}) alwaysinline {{\nentry:\n{body}}}\n"
    ))
}

/// `(lanes, element)` of `<N x T>`.
fn texel_shape(ty: &str) -> Option<(u32, &str)> {
    let inner = ty.strip_prefix('<')?.strip_suffix('>')?;
    let (n, elem) = inner.split_once(" x ")?;
    Some((n.trim().parse().ok()?, elem.trim()))
}

/// The coordinate parameter `i` as `<2 x i32>`, widening a `ushort2` one.
fn coord_i32(params: &[String], i: usize, body: &mut String) -> Option<String> {
    match params.get(i)?.as_str() {
        "<2 x i32>" => Some(format!("%p{i}")),
        "<2 x i16>" => {
            body.push_str(&format!("  %c = zext <2 x i16> %p{i} to <2 x i32>\n"));
            Some("%c".to_owned())
        }
        _ => None,
    }
}
