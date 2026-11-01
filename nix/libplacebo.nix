# libplacebo from git master (API 375). ffmpeg 9 creates its Vulkan queues with
# VK_DEVICE_QUEUE_CREATE_INTERNALLY_SYNCHRONIZED_BIT_KHR on NVIDIA, and those
# queue flags can only be handed to libplacebo with API >= 365
# (pl_vulkan_queue.flags); the released v7.360.1 is too old. Same pin as the
# Python prototype (gpu_grade.py) that ffcine replaces.

{ libplacebo, fetchFromGitLab }:
libplacebo.overrideAttrs (_: {
  version = "7.375.0-git-804dc71";
  src = fetchFromGitLab {
    domain = "code.videolan.org";
    owner = "videolan";
    repo = "libplacebo";
    rev = "804dc71cd38d708816a146041699d8f687dd637c";
    hash = "sha256-CuA6vlFB7oJ0SCtD1ij0uKp52xp8njYbG37Smx3YzuY=";
  };
})
