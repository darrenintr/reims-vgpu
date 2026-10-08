# Apple PVG reverse-engineering source workspace

This directory makes public Apple ParavirtualizedGraphics / AppleParavirtGPU research reproducible
without vendoring third-party source, Apple binaries, extracted frameworks, disassembly, or captured
command bytes into Reims.

## Fetch

```sh
python3 scripts/apple-pvg-re/sync_sources.py --list
python3 scripts/apple-pvg-re/sync_sources.py
```

The default destination is `.cache/apple-pvg-re/`, already ignored by the repository. Sources are
checked out detached at the exact commits in `sources.lock.json`, with sparse checkout where useful.

Reference-only sources are intentionally skipped by default because they contain or are derived from
Apple material, have unclear redistribution terms, or are research narratives rather than clean-room
implementations:

```sh
python3 scripts/apple-pvg-re/sync_sources.py --include-reference
```

## Normalize comparable anchors

```sh
python3 scripts/apple-pvg-re/extract_contracts.py \
  --root .cache/apple-pvg-re \
  --output .cache/apple-pvg-re/contracts.json
```

The extractor currently records independent opcode names/ordinals, BAR0 offsets mentioned by the
clean-room implementation, PCI identity constants, and AppleParavirt class names. Its output is an
observation aid, not a source of truth.

## Rule for moving findings into Reims

Do not copy a third-party decoder wholesale into Reims and do not commit Apple-derived bytes. A
finding may become product code only after it is expressed as the owning Reims contract and verified
with the evidence required by that owner. In particular, `reims-vgpu-wire` Tier 1 still requires the
live serializer oracle; this workspace can point at a disagreement but cannot waive that requirement.
