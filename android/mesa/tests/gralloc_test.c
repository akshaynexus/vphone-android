/* Host test of how the Turnip build reads a gralloc buffer's layout
 * (android/patches/mesa/0007, 0008): the IMapper 5 metadata backend against a
 * mapper made here, and the handle reader behind it.
 * Run by android/scripts/test-turnip-gralloc.sh.
 */
#include <assert.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>

/* The backend, with its statics. */
#include "u_gralloc_aimapper.c"

#include <hardware/hardware.h>

int
hw_get_module(const char *id, const struct hw_module_t **module)
{
   return -1;
}

#define RGBA_8888 1
#define UBWC_FLAG 0x08000000
#define WIDTH_PX 896

/* A native_handle_t with room for SnapAlloc's ints. */
struct handle {
   native_handle_t h;
   int data[2 + 40];
};

static struct u_gralloc_buffer_handle
buffer(struct handle *h, int num_fds, int num_ints)
{
   h->h.version = sizeof(native_handle_t);
   h->h.numFds = num_fds;
   h->h.numInts = num_ints;
   h->h.data[0] = 7;
   return (struct u_gralloc_buffer_handle){
      .handle = &h->h,
      .hal_format = RGBA_8888,
      .pixel_stride = WIDTH_PX,
   };
}

/* SnapAlloc's handle, as Mesa MR 44464 gives it for an Adreno 840. */
static struct u_gralloc_buffer_handle
snapalloc(struct handle *h, int flags, int aligned_px)
{
   memset(h, 0, sizeof(*h));
   int *ints = &h->h.data[2];
   ints[1] = 1; /* view */
   ints[2] = flags;
   ints[3] = aligned_px * 4;
   ints[4] = aligned_px;
   return buffer(h, 2, 34);
}

/* ---- a mapper ---- */

static struct {
   bool fails;
   uint64_t modifier;
   bool ubwc_planes;
   const char *compression_name;
   int64_t compression;
} mapper_says;

static uint8_t *
put(uint8_t *p, const void *v, size_t n)
{
   memcpy(p, v, n);
   return p + n;
}

static uint8_t *
put_i64(uint8_t *p, int64_t v)
{
   return put(p, &v, sizeof(v));
}

static uint8_t *
put_str(uint8_t *p, const char *s)
{
   p = put_i64(p, strlen(s));
   return put(p, s, strlen(s));
}

static uint8_t *
put_plane(uint8_t *p, int64_t offset, int64_t stride)
{
   p = put_i64(p, 1); /* components */
   p = put_str(p, "android.hardware.graphics.common.PlaneLayoutComponentType");
   p = put_i64(p, 1 << 10);
   p = put_i64(p, 0);
   p = put_i64(p, 8);
   p = put_i64(p, offset);
   p = put_i64(p, 32);
   p = put_i64(p, stride);
   for (int i = 0; i < 5; i++)
      p = put_i64(p, 1);
   return p;
}

static int32_t
get_standard_metadata(const native_handle_t *buffer, int64_t type, void *dest,
                      size_t dest_size)
{
   uint8_t value[1024];
   uint8_t *p = value;

   if (mapper_says.fails)
      return -1;

   p = put_str(p, "android.hardware.graphics.common.StandardMetadataType");
   p = put_i64(p, type);
   switch (type) {
   case SMT_PIXEL_FORMAT_FOURCC: {
      uint32_t fourcc = 0; /* as QTI's mapper answers */
      p = put(p, &fourcc, sizeof(fourcc));
      break;
   }
   case SMT_PIXEL_FORMAT_MODIFIER:
      p = put(p, &mapper_says.modifier, sizeof(uint64_t));
      break;
   case SMT_COMPRESSION:
      if (!mapper_says.compression_name)
         return -1;
      p = put_str(p, mapper_says.compression_name);
      p = put_i64(p, mapper_says.compression);
      break;
   case SMT_PLANE_LAYOUTS:
      if (mapper_says.ubwc_planes) {
         /* The planes WinNative's backend records for a UBWC buffer. */
         p = put_i64(p, 2);
         p = put_plane(p, 86016, WIDTH_PX * 4);
         p = put_plane(p, 0, 128);
      } else {
         p = put_i64(p, 1);
         p = put_plane(p, 0, WIDTH_PX * 4);
      }
      break;
   default:
      return -1;
   }

   int32_t size = p - value;
   if ((size_t)size <= dest_size)
      memcpy(dest, value, size);
   return size;
}

static int checks;

static void
expect(const char *what, struct u_gralloc *gralloc,
       struct u_gralloc_buffer_handle hnd, uint64_t modifier)
{
   struct u_gralloc_buffer_basic_info info;

   memset(&info, 0, sizeof(info));
   int ret = gralloc->ops.get_buffer_basic_info(gralloc, &hnd, &info);
   if (ret != 0 || info.modifier != modifier || info.num_planes != 1 ||
       info.offsets[0] != 0 || info.strides[0] != WIDTH_PX * 4 ||
       info.fds[0] != 7) {
      fprintf(stderr,
              "FAIL %s: ret %d modifier 0x%llx (want 0x%llx) planes %d "
              "offset %d stride %d fd %d\n",
              what, ret, (unsigned long long)info.modifier,
              (unsigned long long)modifier, info.num_planes, info.offsets[0],
              info.strides[0], info.fds[0]);
      exit(1);
   }
   checks++;
}

int
main(void)
{
   struct handle h;
   const int gmsm = ('g' << 24) | ('m' << 16) | ('s' << 8) | 'm';

   /* The handle reader. */
   struct u_gralloc *handles = u_gralloc_fallback_create();
   expect("SnapAlloc linear", handles, snapalloc(&h, 0x200, WIDTH_PX),
          DRM_FORMAT_MOD_LINEAR);
   expect("SnapAlloc UBWC", handles,
          snapalloc(&h, 0x200 | UBWC_FLAG, WIDTH_PX),
          DRM_FORMAT_MOD_QCOM_COMPRESSED);
   /* Properties that are not this buffer's say nothing. */
   expect("SnapAlloc of another width", handles,
          snapalloc(&h, 0x200 | UBWC_FLAG, WIDTH_PX + 64),
          DRM_FORMAT_MOD_INVALID);
   memset(&h, 0, sizeof(h));
   h.h.data[2] = gmsm;
   h.h.data[3] = UBWC_FLAG;
   expect("legacy UBWC", handles, buffer(&h, 2, 20),
          DRM_FORMAT_MOD_QCOM_COMPRESSED);
   h.h.data[3] = 0x200;
   expect("legacy linear", handles, buffer(&h, 2, 20), DRM_FORMAT_MOD_LINEAR);
   /* The old driver read this word as flags whatever came before it. */
   memset(&h, 0, sizeof(h));
   h.h.data[3] = UBWC_FLAG;
   expect("unknown handle", handles, buffer(&h, 2, 20),
          DRM_FORMAT_MOD_INVALID);

   /* The metadata backend over them. */
   AIMapper mapper = {.version = AIMAPPER_VERSION_5};
   mapper.v5.getStandardMetadata = get_standard_metadata;
   struct aimapper_gralloc gr = {.mapper = &mapper, .fallback_gralloc = handles};
   gr.base.ops.get_buffer_basic_info =
      aimapper_get_buffer_basic_info_or_fallback;

   mapper_says.compression_name = "android.hardware.graphics.common.Compression";
   expect("mapper linear", &gr.base, snapalloc(&h, 0x200, WIDTH_PX),
          DRM_FORMAT_MOD_LINEAR);
   mapper_says.compression = 1;
   expect("mapper COMPRESSION", &gr.base, snapalloc(&h, 0x200, WIDTH_PX),
          DRM_FORMAT_MOD_QCOM_COMPRESSED);
   mapper_says.compression = 0;
   mapper_says.compression_name = "vendor.qti.hardware.display.common.Compression";
   expect("mapper vendor compression", &gr.base,
          snapalloc(&h, 0x200, WIDTH_PX), DRM_FORMAT_MOD_QCOM_COMPRESSED);
   /* No COMPRESSION answer, and the planes of a UBWC buffer. */
   mapper_says.compression_name = NULL;
   mapper_says.ubwc_planes = true;
   expect("mapper UBWC planes", &gr.base, snapalloc(&h, 0x200, WIDTH_PX),
          DRM_FORMAT_MOD_QCOM_COMPRESSED);
   /* A mapper that answers nothing leaves the handle. */
   mapper_says.fails = true;
   expect("mapper fails, linear handle", &gr.base,
          snapalloc(&h, 0x200, WIDTH_PX), DRM_FORMAT_MOD_LINEAR);
   expect("mapper fails, UBWC handle", &gr.base,
          snapalloc(&h, 0x200 | UBWC_FLAG, WIDTH_PX),
          DRM_FORMAT_MOD_QCOM_COMPRESSED);

   printf("gralloc_test: %d checks passed\n", checks);
   return 0;
}
