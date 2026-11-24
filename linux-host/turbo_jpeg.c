/* Unprivileged pixel-only worker. No DRM, device files, sockets or input APIs. */
#define _POSIX_C_SOURCE 200809L
#include <turbojpeg.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <poll.h>
#include <time.h>
#include <errno.h>
#include <fcntl.h>
#include <sys/mman.h>
#include <sys/stat.h>

#define MAX_PIXELS 8388608u
#define MAX_JPEG (8u * 1024u * 1024u)
/* TurboJPEG reserves worst-case coefficient output, larger than typical JPEG.
 * Private allocation cap is distinct from the application's 8MiB wire bound. */
#define MAX_JPEG_CAPACITY (64u * 1024u * 1024u)
static int64_t now_ms(void) {
    struct timespec t;
    if (clock_gettime(CLOCK_MONOTONIC, &t)) return -1;
    return (int64_t)t.tv_sec * 1000 + t.tv_nsec / 1000000;
}
static int io_exact(int fd, unsigned char *p, size_t n, int output, int64_t end) {
    while (n) {
        int64_t left = end - now_ms();
        if (left <= 0) return -1;
        struct pollfd f = {fd, output ? POLLOUT : POLLIN, 0};
        int r = poll(&f, 1, (int)left);
        if (r < 0 && errno == EINTR) continue;
        if (r <= 0) return -1;
        ssize_t k = output ? write(fd, p, n) : read(fd, p, n);
        if (k < 0 && (errno == EINTR || errno == EAGAIN)) continue;
        if (k <= 0) return -1;
        p += k; n -= (size_t)k;
    }
    return 0;
}
static uint32_t le32(const unsigned char *p) {
    return (uint32_t)p[0] | (uint32_t)p[1]<<8 | (uint32_t)p[2]<<16 | (uint32_t)p[3]<<24;
}
static void put32(unsigned char *p, uint32_t v) {
    for (int i = 0; i < 4; i++) p[i] = (unsigned char)(v >> (8*i));
}
int main(int argc, char **argv) {
    int continuous = argc > 1 && !strcmp(argv[argc-1], "--continuous");
    if (continuous) argc--;
    int quality = 75;
    if (argc >= 4 && !strcmp(argv[argc-2], "--quality")) {
        char *end = NULL;
        errno = 0;
        long value = strtol(argv[argc-1], &end, 10);
        if (errno || !argv[argc-1][0] || *end || value < 30 || value > 95) return 2;
        quality = (int)value;
        argc -= 2;
    }
    int subsampling = TJSAMP_420;
    if (argc >= 4 && !strcmp(argv[argc-2], "--subsampling")) {
        if (!strcmp(argv[argc-1], "420")) subsampling = TJSAMP_420;
        else if (!strcmp(argv[argc-1], "422")) subsampling = TJSAMP_422;
        else if (!strcmp(argv[argc-1], "444")) subsampling = TJSAMP_444;
        else return 2;
        argc -= 2;
    }
    int output_fd = -1;
    unsigned char *output_shared = MAP_FAILED;
    if (argc >= 3 && !strcmp(argv[argc-2], "--shared-output-fd")) {
        char *end = NULL;
        errno = 0;
        long fd = strtol(argv[argc-1], &end, 10);
        struct stat st;
        if (errno || !argv[argc-1][0] || *end || fd < 3 || fd > 1048576 ||
            fstat((int)fd, &st) || !S_ISREG(st.st_mode) ||
            st.st_size != MAX_JPEG_CAPACITY) return 2;
        output_fd = (int)fd;
        output_shared = mmap(NULL, MAX_JPEG_CAPACITY, PROT_READ | PROT_WRITE,
                             MAP_SHARED, output_fd, 0);
        if (output_shared == MAP_FAILED) return 1;
        argc -= 2;
    }
    int shared_fd = -1;
    size_t shared_size = 0;
    unsigned char *shared = MAP_FAILED;
    if (argc == 3 && !strcmp(argv[1], "--shared-fd")) {
        char *end = NULL;
        long fd = strtol(argv[2], &end, 10);
        struct stat st;
        if (!argv[2][0] || *end || fd < 3 || fd > 1048576 ||
            fstat((int)fd, &st) || !S_ISREG(st.st_mode) || st.st_size <= 0 ||
            (uint64_t)st.st_size > (uint64_t)MAX_PIXELS * 4) return 2;
        shared_fd = (int)fd; shared_size = (size_t)st.st_size;
        shared = mmap(NULL, shared_size, PROT_READ, MAP_SHARED, shared_fd, 0);
        if (shared == MAP_FAILED) return 1;
    } else if (argc != 2 || strcmp(argv[1], "--worker")) {
        fputs("pixel-only worker: --worker OR --shared-fd N\n", stderr); return 2;
    }
    if (output_fd >= 0 && (shared_fd < 0 || output_fd == shared_fd)) return 2;
    if (continuous && output_fd < 0) return 2;
    for (int fd = 0; fd <= 1; fd++) {
        int flags = fcntl(fd, F_GETFL);
        if (flags < 0 || fcntl(fd, F_SETFL, flags | O_NONBLOCK) < 0) return 1;
    }
    tjhandle enc = tjInitCompress();
    if (!enc) return 1;
    unsigned char *raw = NULL, *jpeg = NULL;
    size_t raw_capacity = 0;
    unsigned long jpeg_capacity = 0;
    uint32_t fixed_w = 0, fixed_h = 0;
    int status = 0;
    int64_t life = now_ms() + 120000;
    for (int frame = 0; continuous || frame < 300; frame += !continuous) {
        unsigned char h[16];
        int64_t deadline = now_ms() + 15000;
        if (!continuous && deadline > life) deadline = life;
        /* Parent closes/kills on normal completion; no data is retained on disk. */
        if (io_exact(0, h, sizeof h, 0, deadline)) break;
        uint32_t w = le32(h+4), ht = le32(h+8), bytes = le32(h+12);
        uint64_t pixels = (uint64_t)w * ht;
        if (memcmp(h, "BGR1", 4) || w < 16 || ht < 16 || w > 4096 || ht > 4096 ||
            pixels > MAX_PIXELS || bytes != pixels * 4 ||
            (fixed_w && (w != fixed_w || ht != fixed_h))) { status = 2; break; }
        if (!raw) {
            fixed_w = w; fixed_h = ht;
            raw_capacity = bytes;
            jpeg_capacity = tjBufSize((int)w, (int)ht, subsampling);
            if (jpeg_capacity == (unsigned long)-1 || jpeg_capacity > MAX_JPEG_CAPACITY) {
                status = 2; break;
            }
            if (shared_fd >= 0 && raw_capacity != shared_size) { status = 2; break; }
            raw = shared_fd >= 0 ? shared : malloc(raw_capacity);
            jpeg = output_fd >= 0 ? output_shared : tjAlloc((int)jpeg_capacity);
            if (!raw || !jpeg) { status = 1; break; }
        }
        if (shared_fd < 0 && io_exact(0, raw, raw_capacity, 0, deadline)) { status = 1; break; }
        unsigned long size = jpeg_capacity;
        if (tjCompress2(enc, raw, (int)w, (int)w*4, (int)ht, TJPF_BGRX,
                        &jpeg, &size, subsampling, quality, TJFLAG_FASTDCT | TJFLAG_NOREALLOC) ||
            size < 4 || size > MAX_JPEG || jpeg[0] != 255 || jpeg[1] != 216 ||
            jpeg[size-2] != 255 || jpeg[size-1] != 217) { status = 1; break; }
        unsigned char out[8] = {'J','P','G','1',0,0,0,0};
        if (output_fd >= 0) out[2] = 'S';
        put32(out+4, (uint32_t)size);
        /* Publish completion only after all shared JPEG bytes have been written.
         * No further encode occurs until the parent supplies its next request. */
        if (io_exact(1, out, sizeof out, 1, deadline) ||
            (output_fd < 0 && io_exact(1, jpeg, size, 1, deadline))) { status = 1; break; }
    }
    if (shared_fd >= 0) { munmap(shared, shared_size); close(shared_fd); }
    else free(raw);
    if (output_fd >= 0) {
        munmap(output_shared, MAX_JPEG_CAPACITY);
        close(output_fd);
    } else if (jpeg) tjFree(jpeg);
    tjDestroy(enc);
    if (status) fputs("bounded pixel encoder rejected/failed frame\n", stderr);
    return status;
}
