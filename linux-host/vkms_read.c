/* Experimental, bounded VKMS-only reader. No DRM master, modeset, network, or files written.
 * Default stdin: one 'F' per frame. stdout: VKM1 + LE32 width,height,byte_count + packed BGRX.
 * Opt-in --shared-slot: anonymous SEQPACKET stdout carries SLO1/read-only FD then SLT1;
 * stdin must ACK each frame with 'A' after encoding, before any further 'F'.
 * Root/CAP_SYS_ADMIN may be required for GETFB2. Never install setuid or file capabilities. */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <linux/dma-buf.h>
#include <poll.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <stddef.h>
#include <unistd.h>
#include <drm_fourcc.h>
#include <xf86drm.h>
#include <xf86drmMode.h>
#include "cursor_compose.h"
#include "raw_slot.h"
#include "cursor_stage_probe.h"

static unsigned int cursor_frames;
static void release_fb(int fd, drmModeFB2Ptr fb) {
    if (!fb) return;
    for (int p = 0; p < 4; p++) {
        if (!fb->handles[p]) continue;
        int duplicate = 0;
        for (int j = 0; j < p; j++) if (fb->handles[j] == fb->handles[p]) duplicate = 1;
        if (!duplicate) drmCloseBufferHandle(fd, fb->handles[p]);
    }
    drmModeFreeFB2(fb);
}

/* Only planes attached to this already-validated virtual CRTC may supply pixels.
 * Reject unsupported composition rather than silently emitting a partial frame.
 * DRM snapshots are not atomic; a cursor/primary can move between these queries. */
static int add_cursor(int fd, uint32_t crtc_id, unsigned char *pixels,
                      uint32_t width, uint32_t height, struct cursor_stage *stage) {
    drmModePlaneResPtr planes = drmModeGetPlaneResources(fd);
    if (!planes) return -1;
    int result = 0;
    for (uint32_t i = 0; i < planes->count_planes; i++) {
        drmModePlanePtr plane = drmModeGetPlane(fd, planes->planes[i]);
        if (!plane) { result = -1; break; }
        if (plane->crtc_id != crtc_id || !plane->fb_id) {
            drmModeFreePlane(plane); continue;
        }
        uint64_t type = UINT64_MAX, x = 0, y = 0, w = 0, h = 0;
        uint64_t sx = 0, sy = 0, sw = 0, sh = 0, rotation = 1, alpha = 65535, blend = 0;
        unsigned int fields = 0;
        drmModeObjectPropertiesPtr props = drmModeObjectGetProperties(fd, plane->plane_id, DRM_MODE_OBJECT_PLANE);
        if (props) {
            for (uint32_t j = 0; j < props->count_props; j++) {
                drmModePropertyPtr p = drmModeGetProperty(fd, props->props[j]);
                if (!p) continue;
                uint64_t v = props->prop_values[j];
                if (!strcmp(p->name, "type")) type = v;
                else if (!strcmp(p->name, "CRTC_X")) { x = v; fields |= 1; }
                else if (!strcmp(p->name, "CRTC_Y")) { y = v; fields |= 2; }
                else if (!strcmp(p->name, "CRTC_W")) { w = v; fields |= 4; }
                else if (!strcmp(p->name, "CRTC_H")) { h = v; fields |= 8; }
                else if (!strcmp(p->name, "SRC_X")) { sx = v; fields |= 16; }
                else if (!strcmp(p->name, "SRC_Y")) { sy = v; fields |= 32; }
                else if (!strcmp(p->name, "SRC_W")) { sw = v; fields |= 64; }
                else if (!strcmp(p->name, "SRC_H")) { sh = v; fields |= 128; }
                else if (!strcmp(p->name, "rotation")) rotation = v;
                else if (!strcmp(p->name, "alpha")) alpha = v;
                else if (!strcmp(p->name, "pixel blend mode")) blend = v;
                drmModeFreeProperty(p);
            }
            drmModeFreeObjectProperties(props);
        }
        uint32_t fb_id = plane->fb_id;
        drmModeFreePlane(plane);
        if (type == DRM_PLANE_TYPE_PRIMARY) continue;
        if (type != DRM_PLANE_TYPE_CURSOR || fields != 255 || !w || !h ||
            w > 512 || h > 512 || sx || sy || sw != (w << 16) || sh != (h << 16) ||
            rotation != DRM_MODE_ROTATE_0 || alpha != 65535 || blend != 0) {
            result = -1; break;
        }
        drmModeFB2Ptr fb = drmModeGetFB2(fd, fb_id);
        int dmafd = -1, synced = 0;
        void *mapping = MAP_FAILED;
        size_t length = 0;
        if (!fb || fb->width != w || fb->height != h || !fb->handles[0] ||
            fb->handles[1] || fb->handles[2] || fb->handles[3] ||
            fb->pixel_format != DRM_FORMAT_ARGB8888 || fb->modifier != DRM_FORMAT_MOD_LINEAR ||
            fb->offsets[0] || fb->pitches[0] < w * 4 || fb->pitches[0] > 8192) goto cursor_error;
        length = (size_t)fb->pitches[0] * fb->height;
        if (drmPrimeHandleToFD(fd, fb->handles[0], DRM_CLOEXEC, &dmafd)) goto cursor_error;
        mapping = mmap(NULL, length, PROT_READ, MAP_SHARED, dmafd, 0);
        if (mapping == MAP_FAILED) goto cursor_error;
        struct dma_buf_sync sync = {.flags = DMA_BUF_SYNC_START | DMA_BUF_SYNC_READ};
        if (ioctl(dmafd, DMA_BUF_IOCTL_SYNC, &sync)) goto cursor_error;
        synced = 1;
        compose_cursor(pixels, width, height, mapping, fb->width, fb->height,
                       fb->pitches[0], (int32_t)(uint32_t)x, (int32_t)(uint32_t)y);
        if (stage) stage_cursor(stage, (int32_t)(uint32_t)x, (int32_t)(uint32_t)y, fb->width, fb->height);
        cursor_frames++;
        goto cursor_cleanup;
    cursor_error:
        result = -1;
    cursor_cleanup:
        if (synced) {
            struct dma_buf_sync end = {.flags = DMA_BUF_SYNC_END | DMA_BUF_SYNC_READ};
            if (ioctl(dmafd, DMA_BUF_IOCTL_SYNC, &end)) result = -1;
        }
        if (mapping != MAP_FAILED) munmap(mapping, length);
        if (dmafd >= 0) close(dmafd);
        release_fb(fd, fb);
        if (result) break;
    }
    drmModeFreePlaneResources(planes);
    return result;
}

static int emit(int output_fd, const void *data, size_t n) {
    const unsigned char *p = data;
    while (n) {
        struct pollfd pollfd = {output_fd, POLLOUT, 0};
        if (poll(&pollfd, 1, 5000) <= 0 || !(pollfd.revents & POLLOUT)) return -1;
        ssize_t wrote = write(output_fd, p, n);
        if (wrote < 0 && (errno == EAGAIN || errno == EINTR)) continue;
        if (wrote <= 0) return -1;
        p += wrote; n -= (size_t)wrote;
    }
    return 0;
}
static void le32(unsigned char *p, uint32_t v) {
    for (int i = 0; i < 4; i++) p[i] = (unsigned char)(v >> (i * 8));
}
struct private_frame {
    unsigned char *pixels;
    size_t capacity;
};

static int get_frame(int fd, uint32_t connector_id, struct private_frame *buffer,
                     struct raw_slot *slot, uint32_t stage_frame, int output_fd) {
    struct cursor_stage stage = {0};
#ifdef ENABLE_CURSOR_STAGE_PROBE
    uint8_t before[STAGE_ROI_W * STAGE_ROI_H];
#endif
    int result = -1, dmafd = -1, synced = 0;
    void *mapping = MAP_FAILED;
    size_t length = 0;
    unsigned char *pixels = NULL;
    drmModeConnectorPtr c = drmModeGetConnector(fd, connector_id);
    drmModeEncoderPtr enc = NULL;
    drmModeCrtcPtr crtc = NULL;
    drmModeFB2Ptr fb = NULL;
    if (!c || c->connector_type != DRM_MODE_CONNECTOR_VIRTUAL ||
        c->connection != DRM_MODE_CONNECTED || !c->encoder_id) goto cleanup;
    enc = drmModeGetEncoder(fd, c->encoder_id);
    crtc = enc ? drmModeGetCrtc(fd, enc->crtc_id) : NULL;
    if (!crtc || !crtc->mode_valid || !crtc->buffer_id) goto cleanup;
    fb = drmModeGetFB2(fd, crtc->buffer_id);
    if (!fb || !fb->handles[0] || fb->handles[1] || fb->handles[2] || fb->handles[3]) goto cleanup;
    if (fb->pixel_format != DRM_FORMAT_XRGB8888 || fb->modifier != DRM_FORMAT_MOD_LINEAR ||
        fb->offsets[0] != 0 || fb->width < 16 || fb->height < 16 ||
        fb->width > 4096 || fb->height > 4096 || (uint64_t)fb->width * fb->height > 8388608 ||
        fb->pitches[0] < fb->width * 4 || fb->pitches[0] > 32768) goto cleanup;
    length = (size_t)fb->pitches[0] * fb->height;
    if (length > 64 * 1024 * 1024) goto cleanup;
    if (drmPrimeHandleToFD(fd, fb->handles[0], DRM_CLOEXEC, &dmafd)) goto cleanup;
    mapping = mmap(NULL, length, PROT_READ, MAP_SHARED, dmafd, 0);
    if (mapping == MAP_FAILED) goto cleanup;
    struct dma_buf_sync sync = {.flags = DMA_BUF_SYNC_START | DMA_BUF_SYNC_READ};
    if (ioctl(dmafd, DMA_BUF_IOCTL_SYNC, &sync)) goto cleanup;
    synced = 1;
    if (slot) {
        if (raw_slot_prepare(slot, STDOUT_FILENO, fb->width, fb->height)) goto cleanup;
        pixels = slot->writer;
    } else {
        size_t required = (size_t)fb->width * fb->height * 4;
        if (required > buffer->capacity) {
            unsigned char *grown = realloc(buffer->pixels, required);
            if (!grown) goto cleanup;
            buffer->pixels = grown;
            buffer->capacity = required;
        }
        pixels = buffer->pixels;
    }
    /* Overwrite every active pixel before cursor composition; never retain old cursor pixels.
     * emit() finishes copying this private snapshot before the next request can reuse it. */
    for (uint32_t y = 0; y < fb->height; y++)
        memcpy(pixels + (size_t)y * fb->width * 4,
               (unsigned char *)mapping + (size_t)y * fb->pitches[0], fb->width * 4);
    struct dma_buf_sync end = {.flags = DMA_BUF_SYNC_END | DMA_BUF_SYNC_READ};
    synced = 0;
    if (ioctl(dmafd, DMA_BUF_IOCTL_SYNC, &end)) goto cleanup;
    if (stage_frame && (fb->width != 2360 || fb->height != 1640)) goto cleanup;
#ifdef ENABLE_CURSOR_STAGE_PROBE
    if (stage_frame) {
        stage_measure(pixels, stage.pre);
        stage_roi_before(pixels, before);
    }
#endif
    if (add_cursor(fd, crtc->crtc_id, pixels, fb->width, fb->height, stage_frame ? &stage : NULL)) goto cleanup;
    unsigned char header[16] = {'V', 'K', 'M', '1'};
    le32(header + 4, fb->width); le32(header + 8, fb->height);
    le32(header + 12, fb->width * fb->height * 4);
    if (slot) {
        if (raw_slot_publish(slot, STDOUT_FILENO)) goto cleanup;
    } else if (emit(output_fd, header, sizeof(header)) ||
               emit(output_fd, pixels, (size_t)fb->width * fb->height * 4)) goto cleanup;
#ifdef ENABLE_CURSOR_STAGE_PROBE
    if (stage_frame) {
        int32_t post[2];
        stage_measure(pixels, post);
        struct stage_roi_result roi = stage_roi_after(pixels, before);
        uint32_t words[CURSOR_STAGE_WORDS] = {
            stage_frame, (uint32_t)stage.pre[0], (uint32_t)stage.pre[1],
            (uint32_t)post[0], (uint32_t)post[1], stage.planes,
            (uint32_t)stage.left, (uint32_t)stage.top,
            (uint32_t)stage.right, (uint32_t)stage.bottom, stage.overlap, crtc->buffer_id,
            roi.after_count, roi.mixed_count,
            roi.present_before.x, roi.present_before.y, roi.present_before.width, roi.present_before.contrast,
            roi.new_after.x, roi.new_after.y, roi.new_after.width, roi.new_after.contrast
        };
        unsigned char record[4 + 4 * CURSOR_STAGE_WORDS] = {'C', 'S', 'T', '2'};
        for (size_t j = 0; j < CURSOR_STAGE_WORDS; j++) le32(record + 4 + j * 4, words[j]);
        if (emit(output_fd, record, sizeof(record))) goto cleanup;
    }
#endif
    result = 0;
cleanup:
    if (synced) {
        struct dma_buf_sync sync = {.flags = DMA_BUF_SYNC_END | DMA_BUF_SYNC_READ};
        if (ioctl(dmafd, DMA_BUF_IOCTL_SYNC, &sync)) result = -1;
    }
    if (mapping != MAP_FAILED) munmap(mapping, length);
    if (dmafd >= 0) close(dmafd);
    release_fb(fd, fb);
    if (crtc) drmModeFreeCrtc(crtc);
    if (enc) drmModeFreeEncoder(enc);
    if (c) drmModeFreeConnector(c);
    return result;
}
static int unnamed_socket(int fd, int expected) {
    int type;
    socklen_t len = sizeof type;
    struct sockaddr_un local, peer;
    socklen_t local_len = sizeof local, peer_len = sizeof peer;
    return !getsockopt(fd, SOL_SOCKET, SO_TYPE, &type, &len) && type == expected &&
        !getsockname(fd, (struct sockaddr *)&local, &local_len) && local.sun_family == AF_UNIX &&
        !getpeername(fd, (struct sockaddr *)&peer, &peer_len) && peer.sun_family == AF_UNIX &&
        local_len == offsetof(struct sockaddr_un, sun_path) &&
        peer_len == offsetof(struct sockaddr_un, sun_path);
}

static int open_vkms(const char *path) {
    int fd = open(path, O_RDWR | O_CLOEXEC | O_NOFOLLOW);
    if (fd < 0) return -1;
    struct stat st;
    drmVersionPtr ver = drmGetVersion(fd);
    int valid = !fstat(fd, &st) && S_ISCHR(st.st_mode) && ver && ver->name_len == 4 && !memcmp(ver->name, "vkms", 4);
    if (ver) drmFreeVersion(ver);
    if (!valid || drmSetClientCap(fd, DRM_CLIENT_CAP_UNIVERSAL_PLANES, 1) ||
        drmSetClientCap(fd, DRM_CLIENT_CAP_ATOMIC, 1)) {
        close(fd);
        return -1;
    }
    return fd;
}

static uint32_t read_le32(const unsigned char *v) {
    return (uint32_t)v[0] | (uint32_t)v[1] << 8 | (uint32_t)v[2] << 16 | (uint32_t)v[3] << 24;
}

/* One inherited control channel; no listener, paths, or executable commands.
 * An individual socket owns all capture resources. Its EOF returns us to idle. */
static int broker(void) {
    const int control = STDOUT_FILENO;
    struct ucred controller;
    socklen_t len = sizeof controller;
    if (!unnamed_socket(control, SOCK_SEQPACKET) ||
        getsockopt(control, SOL_SOCKET, SO_PEERCRED, &controller, &len) || controller.uid == 0) return 2;
    if (signal(SIGPIPE, SIG_IGN) == SIG_ERR || send(control, "R", 1, MSG_NOSIGNAL) != 1) return 2;
    for (;;) {
        struct pollfd idle = {control, POLLIN, 0};
        if (poll(&idle, 1, 600000) <= 0) return 0;
        unsigned char args[8];
        struct iovec iov = {args, sizeof args};
        union { struct cmsghdr align; unsigned char bytes[CMSG_SPACE(sizeof(int))]; } ancillary;
        struct msghdr msg = {.msg_iov = &iov, .msg_iovlen = 1,
                            .msg_control = ancillary.bytes, .msg_controllen = sizeof ancillary.bytes};
        ssize_t n = recvmsg(control, &msg, MSG_CMSG_CLOEXEC);
        if (!n && !CMSG_FIRSTHDR(&msg)) return 0;
        struct cmsghdr *h = CMSG_FIRSTHDR(&msg);
        /* On malformed input exit: the kernel closes every received descriptor. */
        if (n != sizeof args || (msg.msg_flags & (MSG_TRUNC | MSG_CTRUNC)) || !h ||
            h->cmsg_level != SOL_SOCKET || h->cmsg_type != SCM_RIGHTS ||
            h->cmsg_len != CMSG_LEN(sizeof(int)) || CMSG_NXTHDR(&msg, h)) return 2;
        int data;
        memcpy(&data, CMSG_DATA(h), sizeof data);
        struct ucred peer;
        len = sizeof peer;
        if (!unnamed_socket(data, SOCK_STREAM) ||
            getsockopt(data, SOL_SOCKET, SO_PEERCRED, &peer, &len) ||
            peer.uid != controller.uid || peer.pid != controller.pid) return 2;
        uint32_t connector = read_le32(args + 4);
        char path[32];
        snprintf(path, sizeof path, "/dev/dri/card%u", read_le32(args));
        struct private_frame frame = {0};
        alarm(15);
        int fd = connector ? open_vkms(path) : -1;
        if (fd >= 0) {
            int flags = fcntl(data, F_GETFL);
            if (flags >= 0 && !fcntl(data, F_SETFL, flags | O_NONBLOCK)) {
                for (;;) {
                    struct pollfd inputs[] = {{control, POLLIN, 0}, {data, POLLIN, 0}};
                    unsigned char request;
                    alarm(0);
                    if (poll(inputs, 2, 12000) <= 0 || inputs[0].revents ||
                        read(data, &request, 1) != 1 || request != 'F') break;
                    alarm(15);
                    if (get_frame(fd, connector, &frame, NULL, 0, data)) break;
                }
            }
            close(fd);
        }
        alarm(15);
        free(frame.pixels);
        close(data);
        if (send(control, "D", 1, MSG_NOSIGNAL) != 1) return 0;
        alarm(0);
    }
}

static uint32_t number(const char *s, uint32_t maximum) {
    if (!*s || strspn(s, "0123456789") != strlen(s)) return 0;
    errno = 0;
    unsigned long v = strtoul(s, NULL, 10);
    return errno || !v || v > maximum ? 0 : (uint32_t)v;
}
int main(int argc, char **argv) {
    if (argc == 2 && !strcmp(argv[1], "--broker-v1")) return broker();
    int shared = argc == 5 && !strcmp(argv[4], "--shared-slot");
#ifdef ENABLE_CURSOR_STAGE_PROBE
    int stage_probe = argc == 5 && !strcmp(argv[4], "--cursor-stage-probe");
#else
    int stage_probe = 0;
#endif
    int continuous = argc == 5 && !strcmp(argv[4], "--continuous");
    if ((argc != 4 && !shared && !stage_probe && !continuous) || strncmp(argv[1], "/dev/dri/card", 13) ||
        !argv[1][13] || strspn(argv[1] + 13, "0123456789") != strlen(argv[1] + 13)) {
        fprintf(stderr, "usage: vkms-read /dev/dri/cardN CONNECTOR_ID FRAME_LIMIT [--shared-slot]\n"); return 2;
    }
    uint32_t connector = number(argv[2], UINT32_MAX), limit = number(argv[3], 300);
    if (!connector || !limit || (continuous && limit != 300)) return 2;
    if (shared && !raw_slot_channel(STDOUT_FILENO)) {
        fprintf(stderr, "shared-slot requires private SOCK_SEQPACKET stdout\n"); return 2;
    }
    if (continuous && !unnamed_socket(STDOUT_FILENO, SOCK_STREAM)) return 2;
    struct raw_slot slot = RAW_SLOT_INIT;
    int fd = open_vkms(argv[1]);
    if (fd < 0) { fprintf(stderr, "cannot open validated VKMS device\n"); return 1; }
    int flags = fcntl(STDOUT_FILENO, F_GETFL);
    if (flags < 0 || fcntl(STDOUT_FILENO, F_SETFL, flags | O_NONBLOCK)) { close(fd); return 1; }
    if (!continuous) alarm(120); /* Existing bounded invocations retain the lifetime cap. */
    struct private_frame buffer = {0};
    int result = 0;
    for (uint32_t i = 0; continuous || i < limit || (shared && slot.pending_ack);) {
        struct pollfd input = {STDIN_FILENO, POLLIN, 0};
        unsigned char request;
        if (poll(&input, 1, 10000) <= 0) { result = 1; break; }
        ssize_t n = read(STDIN_FILENO, &request, 1);
        if (!n) break;
        if (n != 1) { result = 1; break; }
        if (shared && slot.pending_ack) {
            if (request != 'A' || raw_slot_ack(&slot)) { result = 1; break; }
            continue;
        }
        if (request != 'F') { result = 1; break; }
        uint32_t frame = i + 1;
        if (continuous) alarm(12); /* A stalled request cannot pin the privileged helper. */
        /* Diagnostic mode writes a numeric sidecar, not an extra image copy. */
        if (get_frame(fd, connector, &buffer, shared ? &slot : NULL, stage_probe ? frame : 0, STDOUT_FILENO)) {
            fprintf(stderr, "VKMS read failed: missing active virtual output, access, or unsupported buffer (%s)\n", strerror(errno));
            result = 1; break;
        }
        i++;
    }
    raw_slot_close(&slot);
    free(buffer.pixels);
    fprintf(stderr, "VKMS cursor frames: %u\n", cursor_frames);
    close(fd);
    return result;
}
