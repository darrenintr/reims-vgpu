#!/usr/bin/env python3
import argparse, json, pathlib, re

def files_under(root, suffixes):
    if not root.exists():
        return []
    return [p for p in root.rglob("*") if p.is_file() and p.suffix in suffixes]

def scan_text(paths, patterns):
    out = []
    for p in paths:
        try:
            text = p.read_text(errors="ignore")
        except Exception:
            continue
        for kind, rx in patterns:
            for m in rx.finditer(text):
                out.append({"kind":kind,"file":str(p),"match":m.group(0)[:240]})
    return out

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--root", type=pathlib.Path, required=True)
    ap.add_argument("--output", type=pathlib.Path, required=True)
    ns = ap.parse_args()

    lagfx = ns.root / "libapplegfx-vulkan"
    mosq = ns.root / "mos-qemu"
    mosp = ns.root / "mos-patcher"

    source_files = (
        files_under(lagfx, {".c",".h",".md"}) +
        files_under(mosq, {".c",".h",".md"}) +
        files_under(mosp, {".c",".cpp",".h",".md"})
    )

    patterns = [
        ("pci_vendor", re.compile(r"0x106[bB]")),
        ("pci_device", re.compile(r"0x[eE]{4}")),
        ("bar0_offset", re.compile(r"BAR0\s*\+\s*0x[0-9a-fA-F]+")),
        ("opcode_symbol", re.compile(r"\b(?:LAGFX_OP_[A-Z0-9_]+|Cmd[A-Z][A-Za-z0-9_]+)\b")),
        ("appleparavirt_class", re.compile(r"\bAppleParavirt[A-Za-z0-9_]+\b")),
        ("pvg_api", re.compile(r"\b(?:PGDevice|PGDisplay|PGShellCallbacks|PGDeviceDescriptor)\b")),
    ]
    observations = scan_text(source_files, patterns)

    dedup = []
    seen = set()
    for o in observations:
        key = (o["kind"], o["match"])
        if key in seen:
            continue
        seen.add(key)
        dedup.append(o)

    out = {
        "schema": 1,
        "note": "Cross-source observations only. Reims contract changes still require owner-appropriate verification.",
        "observations": dedup,
    }
    ns.output.parent.mkdir(parents=True, exist_ok=True)
    ns.output.write_text(json.dumps(out, indent=2, sort_keys=True) + "\n")
    print(f"wrote {ns.output} with {len(dedup)} unique observations")

if __name__ == "__main__":
    main()
