/*
 * present-shim: when did each commit of any Wayland app reach the screen?
 *
 * LD_PRELOAD it into a player (gst-launch, ffplay, mpv, the PoC). Before
 * every wl_surface.commit it asks wp_presentation for feedback on that
 * commit, on a private queue served by its own thread, and logs one line
 * per commit to $PRESENT_SHIM_LOG:
 *
 *   surface_id commit_mono_ns presented_mono_ns refresh_ns flags
 *   surface_id commit_mono_ns discarded
 *
 * Players that ask for no feedback themselves (EGL apps, waylandsink) get
 * an honest "presented" time this way. The compositor must announce
 * CLOCK_MONOTONIC (clock id 1); the shim logs the id it got.
 *
 * Only x86-64: requests are forwarded by re-passing eight integer-class
 * varargs, which holds because every Wayland argument is an int or a
 * pointer (wl_fixed_t is an int).
 *
 * Build: see build-present-shim.sh next to this file.
 */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <pthread.h>
#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <wayland-client.h>

#include "presentation-time-client-protocol.h"

#define WL_SURFACE_COMMIT 6

typedef struct wl_proxy *(*marshal_flags_fn)(struct wl_proxy *, uint32_t, const struct wl_interface *, uint32_t,
                                             uint32_t, ...);

static marshal_flags_fn real_marshal_flags;
static pthread_mutex_t lock = PTHREAD_MUTEX_INITIALIZER;
static __thread int in_shim; /* our own requests pass straight through */
static int state;            /* 0 not tried, 1 ready, -1 gave up */
static struct wl_display *display;
static struct wl_event_queue *queue;
static struct wp_presentation *presentation;
static FILE *out;

struct pending {
	uint32_t surface;
	uint64_t commit_ns;
};

static uint64_t now_ns(void)
{
	struct timespec ts;
	clock_gettime(CLOCK_MONOTONIC, &ts);
	return (uint64_t)ts.tv_sec * 1000000000u + (uint64_t)ts.tv_nsec;
}

static void clock_id(void *data, struct wp_presentation *p, uint32_t clk)
{
	(void)data;
	(void)p;
	fprintf(out, "# clock_id %u (CLOCK_MONOTONIC is %d)\n", clk, CLOCK_MONOTONIC);
	fflush(out);
}

static const struct wp_presentation_listener presentation_listener = {.clock_id = clock_id};

static void fb_sync_output(void *data, struct wp_presentation_feedback *fb, struct wl_output *o)
{
	(void)data;
	(void)fb;
	(void)o;
}

static void fb_presented(void *data, struct wp_presentation_feedback *fb, uint32_t sec_hi, uint32_t sec_lo,
                         uint32_t nsec, uint32_t refresh, uint32_t seq_hi, uint32_t seq_lo, uint32_t flags)
{
	struct pending *p = data;
	uint64_t at = ((((uint64_t)sec_hi << 32) | sec_lo) * 1000000000u) + nsec;
	(void)seq_hi;
	(void)seq_lo;
	fprintf(out, "%u %llu %llu %u %u\n", p->surface, (unsigned long long)p->commit_ns, (unsigned long long)at,
	        refresh, flags);
	free(p);
	in_shim = 1;
	wp_presentation_feedback_destroy(fb);
	in_shim = 0;
}

static void fb_discarded(void *data, struct wp_presentation_feedback *fb)
{
	struct pending *p = data;
	fprintf(out, "%u %llu discarded\n", p->surface, (unsigned long long)p->commit_ns);
	free(p);
	in_shim = 1;
	wp_presentation_feedback_destroy(fb);
	in_shim = 0;
}

static const struct wp_presentation_feedback_listener feedback_listener = {
	.sync_output = fb_sync_output,
	.presented = fb_presented,
	.discarded = fb_discarded,
};

static void global(void *data, struct wl_registry *r, uint32_t name, const char *iface, uint32_t version)
{
	(void)data;
	(void)version;
	if (strcmp(iface, wp_presentation_interface.name) == 0) {
		presentation = wl_registry_bind(r, name, &wp_presentation_interface, 1);
		wp_presentation_add_listener(presentation, &presentation_listener, NULL);
	}
}

static void global_remove(void *data, struct wl_registry *r, uint32_t name)
{
	(void)data;
	(void)r;
	(void)name;
}

static const struct wl_registry_listener registry_listener = {.global = global, .global_remove = global_remove};

static void *serve(void *arg)
{
	(void)arg;
	in_shim = 1;
	while (wl_display_dispatch_queue(display, queue) >= 0)
		fflush(out);
	return NULL;
}

/* Called with `lock` held and in_shim set. */
static void setup(struct wl_proxy *surface)
{
	const char *path = getenv("PRESENT_SHIM_LOG");
	struct wl_display *wrapper;
	struct wl_registry *registry;
	pthread_t thread;

	state = -1;
	if (!path || !(out = fopen(path, "w")))
		return;
	display = wl_proxy_get_display(surface);
	queue = wl_display_create_queue(display);
	wrapper = wl_proxy_create_wrapper(display);
	wl_proxy_set_queue((struct wl_proxy *)wrapper, queue);
	registry = wl_display_get_registry(wrapper);
	wl_proxy_wrapper_destroy(wrapper);
	wl_registry_add_listener(registry, &registry_listener, NULL);
	wl_display_roundtrip_queue(display, queue);
	wl_display_roundtrip_queue(display, queue); /* clock_id */
	if (!presentation) {
		fprintf(out, "# the compositor offers no wp_presentation\n");
		fflush(out);
		return;
	}
	fprintf(out, "# surface_id commit_mono_ns presented_mono_ns refresh_ns flags\n");
	fflush(out);
	if (pthread_create(&thread, NULL, serve, NULL) == 0) {
		pthread_detach(thread);
		state = 1;
	}
}

struct wl_proxy *wl_proxy_marshal_flags(struct wl_proxy *proxy, uint32_t opcode, const struct wl_interface *iface,
                                        uint32_t version, uint32_t flags, ...)
{
	uintptr_t a[8];
	va_list ap;
	int i;

	if (!real_marshal_flags)
		real_marshal_flags = (marshal_flags_fn)dlsym(RTLD_NEXT, "wl_proxy_marshal_flags");
	va_start(ap, flags);
	for (i = 0; i < 8; i++)
		a[i] = va_arg(ap, uintptr_t);
	va_end(ap);

	if (!in_shim && opcode == WL_SURFACE_COMMIT && strcmp(wl_proxy_get_class(proxy), "wl_surface") == 0) {
		in_shim = 1;
		pthread_mutex_lock(&lock);
		if (state == 0)
			setup(proxy);
		if (state == 1) {
			struct pending *p = malloc(sizeof *p);
			struct wp_presentation_feedback *fb;
			p->surface = wl_proxy_get_id(proxy);
			p->commit_ns = now_ns();
			/* A new object takes the queue of the proxy it is made on: our
			 * presentation proxy, so the feedback lands on our queue. */
			fb = wp_presentation_feedback(presentation, (struct wl_surface *)proxy);
			wp_presentation_feedback_add_listener(fb, &feedback_listener, p);
		}
		pthread_mutex_unlock(&lock);
		in_shim = 0;
	}
	return real_marshal_flags(proxy, opcode, iface, version, flags, a[0], a[1], a[2], a[3], a[4], a[5], a[6],
	                          a[7]);
}
