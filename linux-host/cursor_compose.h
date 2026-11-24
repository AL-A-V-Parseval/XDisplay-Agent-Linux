/* Pure packed BGRX + premultiplied BGRA compositor. No device access. */
#ifndef XDISPLAY_CURSOR_COMPOSE_H
#define XDISPLAY_CURSOR_COMPOSE_H
#include <stdint.h>
#include <stddef.h>
static void compose_cursor(unsigned char *dst, uint32_t dw, uint32_t dh,
                           const unsigned char *src, uint32_t sw, uint32_t sh,
                           uint32_t stride, int32_t x, int32_t y) {
    for (uint32_t row = 0; row < sh; row++) {
        int64_t dy = (int64_t)y + row;
        if (dy < 0 || dy >= dh) continue;
        for (uint32_t col = 0; col < sw; col++) {
            int64_t dx = (int64_t)x + col;
            if (dx < 0 || dx >= dw) continue;
            const unsigned char *s = src + (size_t)row * stride + col * 4;
            unsigned char *d = dst + ((size_t)dy * dw + (size_t)dx) * 4;
            for (int c = 0; c < 3; c++) {
                unsigned int v = s[c] + (d[c] * (255u - s[3]) + 127u) / 255u;
                d[c] = v > 255 ? 255 : (unsigned char)v;
            }
        }
    }
}
#endif
