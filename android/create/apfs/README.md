# vphone-apfs

Userspace APFS for on-device VM creation: read any container, write unsealed unencrypted volumes,
with no mount and no root. Built as `libvphone_apfs.so` by `android/scripts/build-apfs.sh`. Verify
with `tests/verify.sh`. The interface is documented in `android/build/COORDINATION-create.md`.

| Part | Source | Why |
|---|---|---|
| Container, omap, B-tree and catalog reads, snapshots, sealed `fext` trees, IM4P/GPT location | this crate | Needed for sealed cryptexes, snapshots and role selection. |
| LZBITMAP (decmpfs 13/14) | this crate, from the MIT libzbitmap format notes | `cmpfs` reports it unsupported; iOS cryptexes use it. |
| Other decmpfs types | `cmpfs` =0.1.0 (MIT) | Handles real images correctly. |
| UDIF | `udif` =0.4.2 (MIT) | Handles real LZFSE DMGs. |
| Allocation, B-tree CoW, free queues, checkpoint commit | `apfs-write` / `apfs-core`, enesilhaydin/apfs-explorer at the commit pinned in `build-apfs.sh` (GPL-3.0-only) + `patches/apfs-explorer/` | The tested write engine; see the patch for the three local changes. |

Because it links `apfs-write`, the resulting binary and this crate are GPL-3.0-only.
