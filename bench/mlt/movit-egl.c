// Headless Movit render for MLT: melt only gets an OpenGL context for Movit from the Qt
// `qglsl` consumer (needs X11/Wayland) or the X11 `xgl` display consumer. This creates an
// EGL device context on the GPU instead, fires "init glsl" in MLT's render thread, and
// drives an avformat consumer.
//
//   movit-egl <profile> <producer> <out> [consumer key=value ...]
//
// <producer> is anything mlt_factory_producer accepts (a .mlt XML file, a media file).
#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <framework/mlt.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

static mlt_filter glsl;

static EGLContext make_context(void)
{
    PFNEGLQUERYDEVICESEXTPROC query_devices
        = (PFNEGLQUERYDEVICESEXTPROC) eglGetProcAddress("eglQueryDevicesEXT");
    PFNEGLGETPLATFORMDISPLAYEXTPROC get_display
        = (PFNEGLGETPLATFORMDISPLAYEXTPROC) eglGetProcAddress("eglGetPlatformDisplayEXT");
    EGLDeviceEXT devs[8];
    EGLint n = 0;
    if (!query_devices || !get_display || !query_devices(8, devs, &n) || n < 1) {
        fprintf(stderr, "movit-egl: no EGL devices\n");
        return EGL_NO_CONTEXT;
    }
    EGLDisplay dpy = get_display(EGL_PLATFORM_DEVICE_EXT, devs[0], NULL);
    EGLint major, minor;
    if (!eglInitialize(dpy, &major, &minor) || !eglBindAPI(EGL_OPENGL_API)) {
        fprintf(stderr, "movit-egl: eglInitialize/eglBindAPI failed\n");
        return EGL_NO_CONTEXT;
    }
    const EGLint cfg_attr[] = {EGL_SURFACE_TYPE, EGL_PBUFFER_BIT, EGL_RENDERABLE_TYPE,
                               EGL_OPENGL_BIT, EGL_NONE};
    EGLConfig cfg;
    EGLint ncfg = 0;
    if (!eglChooseConfig(dpy, cfg_attr, &cfg, 1, &ncfg) || ncfg < 1) {
        fprintf(stderr, "movit-egl: no EGL config\n");
        return EGL_NO_CONTEXT;
    }
    const EGLint ctx_attr[] = {EGL_CONTEXT_MAJOR_VERSION, 3, EGL_CONTEXT_MINOR_VERSION, 2,
                               EGL_CONTEXT_OPENGL_PROFILE_MASK,
                               EGL_CONTEXT_OPENGL_CORE_PROFILE_BIT, EGL_NONE};
    EGLContext ctx = eglCreateContext(dpy, cfg, EGL_NO_CONTEXT, ctx_attr);
    if (ctx == EGL_NO_CONTEXT || !eglMakeCurrent(dpy, EGL_NO_SURFACE, EGL_NO_SURFACE, ctx)) {
        fprintf(stderr, "movit-egl: context creation failed (0x%x)\n", eglGetError());
        return EGL_NO_CONTEXT;
    }
    fprintf(stderr, "movit-egl: EGL %d.%d %s\n", major, minor, eglQueryString(dpy, EGL_VENDOR));
    return ctx;
}

// Fired from inside each MLT render thread, so the context is current where Movit runs.
static void on_thread_started(mlt_properties owner, mlt_consumer consumer)
{
    (void) owner;
    if (make_context() == EGL_NO_CONTEXT)
        exit(2);
    mlt_events_fire(MLT_FILTER_PROPERTIES(glsl), "init glsl", mlt_event_data_none());
    if (!mlt_properties_get_int(MLT_FILTER_PROPERTIES(glsl), "glsl_supported")) {
        fprintf(stderr, "movit-egl: Movit init failed\n");
        exit(2);
    }
}

static void on_thread_stopped(mlt_properties owner, mlt_consumer consumer)
{
    (void) owner;
    (void) consumer;
    mlt_events_fire(MLT_FILTER_PROPERTIES(glsl), "close glsl", mlt_event_data_none());
}

int main(int argc, char **argv)
{
    if (argc < 4) {
        fprintf(stderr, "usage: %s <profile> <producer> <out> [key=value ...]\n", argv[0]);
        return 1;
    }
    mlt_repository repo = mlt_factory_init(NULL);
    if (!repo)
        return 1;
    if (getenv("MOVIT_EGL_LOG")) // e.g. 40 = MLT_LOG_VERBOSE, 48 = MLT_LOG_DEBUG
        mlt_log_set_level(atoi(getenv("MOVIT_EGL_LOG")));
    mlt_profile profile = mlt_profile_init(argv[1]);
    glsl = mlt_factory_filter(profile, "glsl.manager", NULL);
    if (!glsl) {
        fprintf(stderr, "movit-egl: no glsl.manager (movit module missing)\n");
        return 1;
    }
    mlt_producer producer = mlt_factory_producer(profile, NULL, argv[2]);
    if (!producer) {
        fprintf(stderr, "movit-egl: cannot open %s\n", argv[2]);
        return 1;
    }
    mlt_consumer consumer = mlt_factory_consumer(profile, "avformat", argv[3]);
    mlt_properties cp = MLT_CONSUMER_PROPERTIES(consumer);
    mlt_properties_set_int(cp, "terminate_on_pause", 1);
    mlt_properties_set_int(cp, "real_time", -1);
    for (int i = 4; i < argc; i++)
        mlt_properties_parse(cp, argv[i]);
    mlt_events_listen(cp, consumer, "consumer-thread-started", (mlt_listener) on_thread_started);
    mlt_events_listen(cp, consumer, "consumer-thread-stopped", (mlt_listener) on_thread_stopped);

    mlt_consumer_connect(consumer, MLT_PRODUCER_SERVICE(producer));
    mlt_consumer_start(consumer);
    while (!mlt_consumer_is_stopped(consumer))
        usleep(100000);
    mlt_consumer_stop(consumer);
    mlt_consumer_close(consumer);
    mlt_producer_close(producer);
    mlt_filter_close(glsl);
    mlt_profile_close(profile);
    mlt_factory_close();
    return 0;
}
