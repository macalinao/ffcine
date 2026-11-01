// libplacebo's libav helpers (`libplacebo/utils/libav.h`) are header-only: `static inline` unless
// one translation unit defines PL_LIBAV_IMPLEMENTATION. This is that unit, so Rust can link
// pl_map_avframe_ex() & co. Vulkan first so AVVkFrame (hwdec) mapping is compiled in.
#include <vulkan/vulkan.h>
#define PL_LIBAV_IMPLEMENTATION 1
#include <libplacebo/utils/libav.h>
