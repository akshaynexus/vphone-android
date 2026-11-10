# Turnip for Android (KGSL)

`android/scripts/build-turnip.sh` fetches Mesa at a pinned commit into `android/build/mesa`, applies
`android/patches/mesa/*.patch` in name order, and builds
`android/build/out/libvulkan_freedreno.so`: Turnip for Android bionic against KGSL, loaded on a
stock device through libadrenotools via the system Vulkan loader's HAL hook (`HMI`), with
`VK_ANDROID_native_buffer` swapchains. Target GPUs: Adreno 750 (S24 Ultra), Adreno 840 (OnePlus 15).

## The Mesa revision

`316009797d8de1efdda53646f527821d2993ce24`, `main` of 2026-11-08. Why this one:

- Adreno 750 is listed with its KGSL chip id (`0x43051401`, `freedreno_devices.py`).
- Adreno 840 is listed in two configurations: `0x44050A31` (3 slices) and `0x44050A21` (2 slices).
  A community Turnip patch reports a retail SM8850 phone as `0x44050A21`; which one the OnePlus 15
  reports has not been checked on a device.
- The newest tag at the time, `mesa-26.3.0-rc3` of 2026-10-30, lists the same two ids, so a tag would identify
  the GPUs too. The pin is `main` because the patches here were made against it, and A8xx on KGSL is
  moving quickly in Turnip. Move the pin deliberately and rebuild the patches when doing so.

## Patches

| Patch | Origin | What |
|---|---|---|
| `0001-tu-kgsl-recycle-retired-command-stream-BOs-on-A840` | Mesa MR !44888 by its author, draft, head `2b6c9163b5`; workaround for issue #16460 | Keeps retired command-stream BOs mapped and reuses them instead of freeing them to KGSL, quirk-gated. Without it, Adreno 840 under KGSL raises CP zero-opcode faults within minutes in the MR's tests. The root cause is unknown (the MR says the blob shows the same signature on the same kernel). Conflict resolved against the pin; the quirk is extended to the 2-slice A840, which the MR did not cover. |
| `0002-tu-kgsl-implement-VK_EXT_external_memory_host` | This repository | The extension, see below. |
| `0003-tu-Expose-VK_EXT_post_depth_coverage` | This repository | Exposes the extension. The hardware bit (`RB_PS_INPUT_CNTL.POSTDEPTHCOVERAGE`) was already set by ir3 for the execution mode. |
| `0004-tu-Expose-storageInputOutput16` | This repository | 16-bit stage IO, see below. |
| `0005-tu-Implement-VK_EXT_fragment_shader_interlock` | This repository | Pixel and sample interlock, see below. |
| `0006-tu-Expose-8-bit-and-16-bit-UBO-and-push-constant-acc` | This repository | 8-bit and 16-bit UBO and push constant reads, widened to dword reads. |
| `0007-u_gralloc-fallback-detect-UBWC-on-SnapAlloc-buffer-ha` | Mesa MR !44464 by its author (Hugo223), open, unchanged: the MR's `.patch` of 2026-09-23 | Reads the UBWC flag from SnapAlloc's buffer handle (Snapdragon 8 Elite generation: no `'gmsm'` magic, the flags one int later), trusted only where the handle has SnapAlloc's shape and its aligned width is the buffer's stride. See below. |
| `0008-u_gralloc-Add-an-IMapper-5-backend-that-needs-no-lib` | WinNative-Emu/Drivers `patches/aimapper/u_gralloc_aimapper.c` and `patches/add_aimapper_gralloc.py` at `7bfb3c433c` (by Leb-Sun, MIT; Droid-Deck's DD-Turnip and Banners-Turnip carry the same file); one change here | A gralloc backend over the vendor's IMapper 5 that needs only `dlopen`, so it exists under `-Dandroid-stub=true`. See below. |
| `0009-vulkan-android-Lay-a-swapchain-s-images-out-as-grall` | This repository; the usage bit's placement from WinNative-Emu/Drivers `patches/add_ubwc_swapchain_usage.py` | A swapchain image gralloc says nothing of is LINEAR, `TU_ANDROID_SWAPCHAIN_LAYOUT`, and the line that says what was chosen. See below. |

## The swapchain's layout on Android

The swapchain's buffers are gralloc's: Android's Vulkan loader allocates them and hands each to the
driver as a `VkNativeBufferANDROID`, and the compositor reads them as gralloc allocated them. The
driver has to lay its image out the same way, which it learns from `u_gralloc`
(`vk_android_get_anb_layout`).

At the pin, a driver built with `-Dandroid-stub=true` has only the fallback backend, which sets a
modifier only for a handle that starts with the `'gmsm'` magic of Qualcomm's older gralloc
(`u_gralloc_fallback.c`). SnapAlloc's handle (SM8750, SM8850) has none, the modifier stays
`DRM_FORMAT_MOD_INVALID`, and `tu_image.cc` then lays the image out tiled, with UBWC where it is
allowed: a picture smeared across every 16 rows on an Adreno 840 (0.5.6, 0.5.7). Mesa `main` has no
fix as of the pin; MR !29785 ("u_gralloc/fallback: Set fd from handle directly", merged 2024-06) is in
the pin and is where the `'gmsm'` check comes from.

What is read, in order:

1. **The mapper's metadata** (`0008`). `u_gralloc_aimapper.c` loads the vendor's mapper with
   libvndksupport's `android_load_sphal_library` and `AIMapper_loadIMapper`, and asks it for the
   buffer's planes, modifier and `StandardMetadataType::COMPRESSION`. A buffer is UBWC where
   COMPRESSION says so, or where its planes are those Qualcomm's mapper gives a UBWC buffer (two, the
   metadata plane at offset 0), which it reports with a LINEAR modifier. The backend is not made where
   libvndksupport or a `mapper.*.so` with `AIMapper_loadIMapper` cannot be loaded.
   The change here: a buffer the mapper does not describe is read from its handle (2), where the
   original failed the image.
2. **The handle** (`0007`), where there is no mapper or it does not answer: the `'gmsm'` layout, then
   SnapAlloc's (34 ints per view, or 33), whose flags word is trusted only where the view is the
   primary one and its aligned width is the stride the loader gave.
3. **LINEAR** (`0009`), where neither says: what Qualcomm's gralloc allocates without its vendor usage
   bit, which Mesa never sets by itself.

The community drivers' other fix, reading `data[numFds + 1]` as the flags of any handle (whitebelyash's
"always use ubwc detection path", Banners-Turnip's `a8xx_gen8.patch`, WinNative's
`fix_gralloc_flushall.py`, Droid-Deck's `gralloc_ubwc_detect.py`, Mesa MR !41330), is not used: on
SnapAlloc that word is the view, so it answers LINEAR for every buffer, a compressed one too.

`TU_ANDROID_SWAPCHAIN_LAYOUT` (`0009`; the app's *Screen buffer layout*): `auto` is the above; `linear`
is LINEAR whatever gralloc says; `ubwc` asks gralloc for compressed buffers with the vendor usage bit
`0x10000000` and lays the image out as gralloc then says, compressed where it says nothing. The bit is
added where the loader asks what to allocate with: `VkAndroidHardwareBufferUsageANDROID` on
`vkGetPhysicalDeviceImageFormatProperties2`, which WinNative's patch reports is what Android 16's loader
uses, and `vkGetSwapchainGrallocUsage*ANDROID`. The community drivers set the bit always; here it is
behind the setting, and the default swapchain is linear, because no compressed swapchain has been
shown on a phone with this driver and its patches.

A single plane's row pitch is the loader's stride times the format's bytes per pixel whatever the
backend read, as it was before these patches: a pitch Turnip has no layout for would fail the swapchain.

The driver prints its choice once per swapchain, to stderr and to logcat: the image, gralloc's format
and stride, the handle's first ints, what gralloc said and through which backend, the setting, and the
layout and row pitch taken.

`android/scripts/test-turnip-gralloc.sh` runs `android/mesa/tests/gralloc_test.c` on the build
machine: the patched `u_gralloc` sources against handles of each kind and a mapper made in the test.
Nothing of 0007 to 0009 has run on a phone.

## `VK_EXT_external_memory_host` on KGSL

Imports go through `IOCTL_KGSL_GPUOBJ_IMPORT` with `KGSL_USER_MEM_TYPE_ADDR`.

What the kernel does, from the KGSL source (`kgsl.c`: `_gpuobj_map_useraddr`, `kgsl_setup_useraddr`,
`check_vma`, `memdesc_sg_virt`, `kgsl_sharedmem.c`: `kgsl_memdesc_init`), checked on the CodeLinaro
`graphics-kernel` branches `gfx-kernel.lnx.15.0.r12-rel` (commit `2974f90e7e20`) and
`gfx-kernel.lnx.16.0.r15-rel` (commit `7f4e7b2763c0`):

- **Shared memfd mappings import.** There is no anonymous-only check: `check_vma` refuses only KGSL's
  own mappings and a cached/uncached mix across the range, and `memdesc_sg_virt` uses plain
  `get_user_pages` with `FOLL_WRITE` (no `FOLL_LONGTERM`). The KGSL source never says `VM_SHARED` or
  tests `vm_file` beyond its own mapping.
- **Address and length are multiples of the kernel's page size**, `-EINVAL` otherwise. Hence
  `minImportedHostPointerAlignment` = `os_page_size`, 4K or 16K.
- **The whole range must resolve writable**: a hole, a read-only or `PROT_NONE` page fails with
  `-EFAULT`, or `-EINVAL` when GUP falls short. Those become `VK_ERROR_INVALID_EXTERNAL_HANDLE`;
  `-ENOMEM` becomes `VK_ERROR_OUT_OF_DEVICE_MEMORY`.
- **Lifetime.** The kernel holds a reference per page until the object is freed
  (`kgsl_destroy_anon`), and it maps those physical pages. Imports therefore pin the range, and the
  GPU keeps the old pages if the mapping or the memfd changes underneath.
- **Coherence.** `kgsl_memdesc_init` strips `IOCOHERENT` from an uncached mode, so it needs a cached
  mode. On kernels with `CONFIG_QCOM_KGSL_IOCOHERENCY_DEFAULT` (set in the `pineapple` and `canoe`
  consolidate configs) it is also forced on. The driver requests `WRITEBACK | IOCOHERENT`, reads the
  flags the kernel returns, and refuses the import if `IOCOHERENT` is not among them.
- **GPU address.** Without `USE_CPU_MAP` KGSL chooses from its own range; imports of 1 MiB and up get
  1 MiB alignment. The GPU address is unrelated to the pointer.

Driver behaviour:

- The extension is exposed only where the backend implements it and the kernel has IO-coherent
  cached memory. Host pointers are offered only the `HOST_VISIBLE | HOST_COHERENT | HOST_CACHED` type.
- Imported memory is not counted in the Vulkan heap, and `vkFreeMemory` frees the kernel object but
  never unmaps: the mapping is the application's. `vkMapMemory` returns the pointer.
- An image created for `HOST_ALLOCATION` no longer reports `requiresDedicatedAllocation`, which host
  memory could never honour.

## Features added for metal2vulkan

All are gated to a7xx and newer (`info->chip >= 7`), since that is what the targets are.

### 16-bit stage IO (`storageInputOutput16`)

metal2vulkan declares a `half` color output as a `half` only with this feature
(`lower/stage_io/plan.rs` `half_variable`), which every `half4` fragment shader of the compositor
returns. Adreno's half-precision paths are used where they exist: `bary.f` into a half register for
smooth inputs, and `SP_PS_OUTPUT_REG.HALF_PRECISION` for float color outputs, the same paths GL
mediump takes in ir3. Everything else (flat inputs through `flat.b`, vertex attributes, integer
color outputs, varyings out of VS/TES/GS, tess and geometry storage) is stored as 32 bits and
converted at the access: a 16-bit component consumes a whole component in Vulkan's location
assignment, so the interface does not move. Transform feedback can only capture 32-bit and 64-bit
components (`VUID-StandaloneSpirv-Offset-04692`), so it is not affected.

### `VK_EXT_fragment_shader_interlock` (pixel and sample, not shading rate)

Without it metal2vulkan refuses raster order groups and imageblocks and has no storage route for
framebuffer fetch (`lower/fragment/{rog,imageblock,snapshot,stencil,depth_test}.rs`).
The hardware has no critical section instruction; `src/freedreno/registers` and the ISA have no
interlock. What it has is `GRAS_SC_CNTL.SINGLE_PRIM_MODE`, whose Mesa documentation says it
"Invalidate[s] UCHE and wait[s] for any pending work to finish if there was possibly an
overlapping primitive prior to the current one", and which Turnip already uses for
rasterization-order attachment access. The Vulkan specification defines every interlock mode
through rasterization order, which is defined per sample location for each separate primitive, in
primitive order (`fragops.adoc`, `primsrast.adoc`), so invocations of one primitive at one pixel
have no ordering requirement. So:

- A pipeline whose fragment shader declares an interlock mode runs under
  `FLUSH_PER_OVERLAP` in GMEM and `FLUSH_PER_OVERLAP_AND_OVERWRITE` in sysmem.
- `OpBegin`/`OpEndInvocationInterlockEXT` become memory barriers (`fence`, and `ccinv` at the
  beginning), which keep the critical section's accesses inside it. That the `fence` at the end
  means the stores have landed when the invocation finishes, which the patch's comments intend, is
  not stated by Mesa's ISA documentation and has not been measured.
- An interlocked draw is preceded by `WAIT_FOR_IDLE` + `CACHE_INVALIDATE`, in the render pass CS
  like any self-dependency barrier, because primitive order also runs across draws and the
  register's documentation does not say the overlap test does.

The cost is real: those draws lose early rejection across primitives and force GMEM in autotune
(`rp_state->sysmem_single_prim_mode`). Only shaders that declare an interlock pay it. The
compositor's framebuffer fetch does not: Turnip keeps rasterization-order attachment access and
metal2vulkan keeps using it (`lower/fragment/fetch.rs`, `inputs_order`).

### What is not there, and why

| Feature | Result | Why |
|---|---|---|
| `VK_KHR_fragment_shader_barycentric` | Not added | The hardware has the weights (`ij`, `load_barycentric_*`) and an evaluation at fixed weights (`bary.f`) would give per-vertex values, but nothing in Mesa's registers, ISA docs or code says which of `i`, `j`, `1-i-j` belongs to which vertex of the primitive, or how `PROVOKING_VTX_LAST` affects it. `BaryCoordKHR` must match vertex order. A guess that is wrong rotates the weights and gives silently wrong output, which needs a device to measure. metal2vulkan's geometry-shader route stays correct and is reached only by shaders with `[[barycentric_coord]]`. |
| `VK_EXT_shader_atomic_float2`, float atomic add | Not added | cat6 has integer atomics only (`instr-a3xx.h`, `OPC_ATOMIC_*`). A driver compare-exchange loop would cost what metal2vulkan already does for every float atomic (`lower/access/atomic.rs`), so advertising it would only hide the same loop. |
| `VK_KHR_shader_maximal_reconvergence` | Not added | A guarantee about control flow that ir3 has no handling for (`shader_info.maximally_reconverges` is read only by RADV/ACO) and that cannot be checked without running shaders on the GPU. |
| `VK_EXT_depth_range_unrestricted` | Not added | Nothing in the registers or Turnip says whether the hardware keeps `D32_SFLOAT` values outside `[0, 1]`. metal2vulkan draws past `[0, 1]` with clip distances and depth clamp, which Turnip has. |
| `VK_EXT_depth_bias_control` | Not added | No consumer in metal2vulkan's runtime (its name appears only in the enables table), and `depthBiasExact`/float representation would claim a scale the driver has no knowledge of: `tu6_emit_depth_bias` writes the factors unchanged. |
| `shaderStorageImageMultisample` | Not added | No sample-addressed image path in the ISA or Turnip. Writable multisampled textures stay refused. |
| `VK_KHR_shader_fma` | Not added | ir3 has `mad.f` (`float_mul_add* = has_fmad`), not a fused correctly rounded operation. |
| `shaderFloat64` | Not added | No 64-bit float hardware. |
| `VK_EXT_mesh_shader`, `VK_KHR_cooperative_matrix` | Not added | No registers or opcodes for either in `src/freedreno`. |
| `VK_KHR_shader_untyped_pointers`, `VK_KHR_shader_bfloat16`, `VK_QCOM_tile_shading` | Not added | metal2vulkan names them in its tables but has no route using them (`bfloat16` only with cooperative matrices). Tile functions run through metal2vulkan's own `render/tile_dispatch.rs`; a `QCOM_tile_shading` implementation has no registers in Mesa to start from. |

Known deviation left in place: on A8xx under KGSL, the alpha of `10_10_10_2_UNORM` is converted at fp16
precision (Mesa MR !45973, RFC, not applied), so alpha values 1/3 and 2/3 read back slightly low.

## Verification status

The aarch64 build is compiled, linked and inspected by the script's `--check`: the exports, the
DT_NEEDED list and the 16K segment alignment. The extension names `--check` looks for are a weak
signal, because Mesa's generated tables hold every known extension name.

Patches 0003 to 0006 were also checked on the build host with `freedreno`'s noop drm-shim, against
a host build of this tree with Mesa's assertions on, for A750 (`0x43051401`), A840 (`0x44050A31`,
added to a local copy of the shim) and A830. The test creates real pipelines from GLSL through
`spirv_to_nir`, ir3 and Turnip's state emission, and records interlocked draws:

- `vulkaninfo` reports the features and extensions on all of them.
- 16-bit IO: VS, TCS, TES, GS and FS, with `R16` vertex formats; the FS disassembly has
  `bary.f (ei)hr...` for smooth inputs, `flat.b` into `r` followed by `cov.f32f16` for flat
  inputs and `hr` color outputs.
- Interlock: the disassembly has `fence`/`ccinv` at begin and `fence` at end around
  `ldib`/`stib`/`atomic.b`; a two-draw render pass records.
- Narrow constants: `ldc` is 32-bit and followed by `shr.b`/`cov`, correct for each field offset.

None of this executes on a GPU. What has not been observed: the order `SINGLE_PRIM_MODE` actually
gives storage writes (the register's documented behaviour is what the implementation relies on),
that a `half` color output converts correctly into every render target format on these GPUs, and
everything KGSL does. No KGSL behaviour above has been observed; it was read from the kernel source
of the named branches. The stock kernels of the S24 Ultra and OnePlus 15 may differ from them.
