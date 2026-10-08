# Apple ParavirtualizedGraphics reverse-engineering sources

This branch collects the public research surfaces that can help recover the contract spoken by
`AppleParavirtGPU.kext` while keeping Reims' execution architecture intact. The sources are pinned in
`scripts/apple-pvg-re/sources.lock.json`; `sync_sources.py` materializes them under the ignored
`.cache/apple-pvg-re/` directory.

## Highest-value independent implementations

### MattJackson/libapplegfx-vulkan

The strongest independent protocol implementation found. It is a clean-room Linux implementation of
the host-side ParavirtualizedGraphics API and contains protocol dispatchers, opcode catalogs,
`CmdExecIndirect*` handling, task/radix translation, stamps, display/IOSurface handling, AIR-to-SPIR-V
work, and explicit notes derived from AppleParavirtGPU observation/disassembly. Use it primarily as a
second protocol oracle. Do not import its lavapipe-first execution or synchronous fence-wait model
into Reims.

### MattJackson/mos-qemu

A Linux C `apple-gfx-pci` transport using libapplegfx-vulkan instead of Apple's host framework. It is
useful for PCI identity, BAR0/MMIO, MSI, guest-RAM callback semantics, QEMU display integration, and
cross-checking the thin-device boundary. Reims should keep its own QEMU shim and Rust ownership model.

### MattJackson/mos-patcher

Contains kext symbol/vtable routing work and explicit AppleParavirtGPU -> IOAcceleratorFamily2 ->
IOGraphicsFamily inheritance observations. Useful when a guest-version change looks like a changed
class/method contract rather than a command-stream change.

## API and host-framework surfaces

- `qemu/qemu`: upstream `apple-gfx` is the canonical host-framework-backed transport. It does not
  independently decode the proprietary command protocol but accurately records the PGDevice /
  PGShellCallbacks boundary QEMU must satisfy.
- `tmc/apple`: generated Go bindings for ParavirtualizedGraphics. Useful for enumerating PGDevice,
  PGDisplay and descriptor/callback API surface without depending on the framework at build time.
- `madsmtm/objc2-generated`: generated Rust bindings for the same framework. Kept reference-only
  because the generated repository does not declare standalone redistribution terms.

## Modern Apple virtual-device research

- `wh1te4ever/super-tart-vphone` and its writeup: PCC/vPhone work involving
  `AppleParavirtGPUMetalIOGPUFamily` and the compiler-plugin dependency. The project explicitly
  removed the Apple-derived compiler-plugin implementation over copyright concerns; Reims must not
  import or reconstruct that removed material from third-party copies.
- `dr-data/virtualmaconipad`: reconstruction/loading tooling for Apple virtualization/private
  frameworks including ParavirtualizedGraphics. Useful for understanding framework dependencies and
  arm64e runtime shape, not as a guest wire-format oracle.
- `Lakr233/vphone-cli`: current vPhone research and binary-comparison notes around the paravirtual GPU
  userspace bundle.
- `steelbrain/experiment-macOS-arm64-on-asahi-linux-arm64`: evidence for AppleParavirtGPU + Reims on
  arm64 macOS under KVM/Asahi. Treat its observations as pathway-specific.

## Version-diff and historical references

- `blacktop/ipsw-diffs`: symbol/size deltas for ParavirtualizedGraphics across Tahoe builds. This is
  Apple-derived metadata and remains local/reference-only.
- `phracker/MacOSX-SDKs`: historical ParavirtualizedGraphics headers. Apple SDK material; inspect
  locally only and do not vendor it into Reims.
- Nick Botticelli's vma2 boot log gist: useful class-tree observations (`AppleParavirtGPU`, channels,
  tasks, resources, page tables, display/fence classes). URL-only because it is an evidence log, not
  a source tree.

## Integration policy

The purpose of these sources is to find disagreements quickly, not to create a second implementation
inside Reims. Findings should land in the owner that can state them:

| Finding | Reims owner |
| --- | --- |
| Serializer-emitted operation layout | `reims-vgpu-wire` after live-oracle verification |
| Backend-neutral FIFO/device semantic | `reims-vgpu-protocol` |
| Guest VA/page-table geometry | `reims-vgpu-wire` / `reims-vgpu-protocol`, pathway-scoped |
| Dependency/order/lifetime implication | `reims-vgpu-core` |
| Guest RAM provenance/aliasing | `reims-vgpu-memory` |
| Vulkan placement/transfer capability | `reims-vgpu-vulkan` |
| PCI/MMIO/MSI/QEMU glue | `vendor/qemu` thin shim |

Never bring over libapplegfx's serialized `vkQueueSubmit`/`vkWaitForFences` execution model, its
lavapipe preference, or copy-on-map fallback as Reims product policy. Those are implementation
choices for a different backend architecture, not Apple PVG contracts.

## Suggested comparison order

1. PCI identity, BAR0 capability/version negotiation and device-info replies.
2. Root/virtual channel creation, FIFO framing, doorbells, stamps and interrupts.
3. `CmdExecIndirect2/3`, task/radix VA -> GPA translation and resource tables.
4. Display swap, IOSurface mapping and scanout publication.
5. Render/compute/blit opcode coverage and field layouts.
6. Version-specific differences between Ventura, Sequoia and Tahoe.

Any discrepancy should first become an observation/counter or a focused oracle experiment. Only then
should it change execution behavior.
