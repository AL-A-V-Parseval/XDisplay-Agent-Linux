/* Lab-only numeric measurements, no image or file output. */
#ifndef CURSOR_STAGE_PROBE_H
#define CURSOR_STAGE_PROBE_H
#include <stdint.h>
#include <stddef.h>

#define CURSOR_STAGE_WORDS 22
#define STAGE_ROI_X 448u
#define STAGE_ROI_Y 960u
#define STAGE_ROI_W 512u
#define STAGE_ROI_H 256u

struct stage_segment { uint32_t x, y, width, contrast; };
struct stage_roi_result {
    uint32_t after_count, mixed_count;
    struct stage_segment present_before, new_after;
};
struct cursor_stage {
    int32_t pre[2];
    uint32_t planes;
    int32_t left, top, right, bottom; /* clipped union bbox; empty when no intersection */
    uint32_t overlap; /* bit per measured triplet, for actual per-plane rectangles */
};

static inline int32_t stage_luma(const unsigned char *p) {
    return (29 * p[0] + 150 * p[1] + 77 * p[2] + 128) >> 8;
}

static inline void stage_measure(const unsigned char *pixels, int32_t out[2]) {
    const uint32_t xs[] = {491, 880}, ys[] = {978, 1031}, lengths[] = {5, 16};
    for (unsigned i = 0; i < 2; ++i) {
        int32_t minimum = 255;
        for (uint32_t x = xs[i]; x < xs[i] + lengths[i]; ++x) {
            const unsigned char *p = pixels + ((size_t)ys[i] * 2360 + x) * 4;
            int32_t a = stage_luma(p - 2360 * 4), b = stage_luma(p + 2360 * 4);
            int32_t contrast = stage_luma(p) - (a > b ? a : b);
            if (contrast < minimum) minimum = contrast;
        }
        out[i] = minimum;
    }
}

/* Same coordinates and rule on both sides. A segment is a maximal run of
 * 5–45 pixels on a dark background, >=40 brighter than both adjacent rows.
 * This is a candidate detector, not a semantic classifier for UI pixels. */
static inline uint8_t stage_roi_contrast(const unsigned char *p) {
    int32_t a = stage_luma(p - 2360 * 4), b = stage_luma(p + 2360 * 4);
    int32_t dark = a > b ? a : b;
    if (dark > 45) return 0;
    int32_t contrast = stage_luma(p) - dark;
    return contrast >= 40 ? (uint8_t)contrast : 0;
}

static inline void stage_roi_before(const unsigned char *pixels, uint8_t *before) {
    for (uint32_t y = 0; y < STAGE_ROI_H; ++y)
        for (uint32_t x = 0; x < STAGE_ROI_W; ++x)
            before[y * STAGE_ROI_W + x] = stage_roi_contrast(
                pixels + ((size_t)(STAGE_ROI_Y + y) * 2360 + STAGE_ROI_X + x) * 4);
}

static inline struct stage_roi_result stage_roi_after(const unsigned char *pixels, const uint8_t *before) {
    struct stage_roi_result result = {0};
    for (uint32_t y = 0; y < STAGE_ROI_H; ++y) {
        const unsigned char *row = pixels + ((size_t)(STAGE_ROI_Y + y) * 2360 + STAGE_ROI_X) * 4;
        uint32_t start = 0, count = 0, overlap = 0, minimum = 255;
        for (uint32_t x = 0; x <= STAGE_ROI_W; ++x) {
            uint32_t contrast = x == STAGE_ROI_W ? 0 : stage_roi_contrast(row + x * 4);
            if (contrast) {
                if (!count) start = x;
                ++count;
                overlap += before[y * STAGE_ROI_W + x] != 0;
                if (contrast < minimum) minimum = contrast;
                continue;
            }
            if (count >= 5 && count <= 45 && start > 0 && x < STAGE_ROI_W) {
                ++result.after_count;
                struct stage_segment *best = overlap == count ? &result.present_before :
                    overlap == 0 ? &result.new_after : NULL;
                if (!best) ++result.mixed_count;
                else if (minimum > best->contrast)
                    *best = (struct stage_segment){STAGE_ROI_X + start, STAGE_ROI_Y + y, count, minimum};
            }
            count = overlap = 0;
            minimum = 255;
        }
    }
    return result;
}

static inline void stage_cursor(struct cursor_stage *s, int32_t x, int32_t y,
                                uint32_t w, uint32_t h) {
    s->planes++;
    int64_t r = (int64_t)x + w, b = (int64_t)y + h;
    int32_t l = x < 0 ? 0 : x, t = y < 0 ? 0 : y;
    if (r > 2360) r = 2360;
    if (b > 1640) b = 1640;
    if (l >= r || t >= b) return;
    if (!s->right || l < s->left) s->left = l;
    if (!s->bottom || t < s->top) s->top = t;
    if (r > s->right) s->right = (int32_t)r;
    if (b > s->bottom) s->bottom = (int32_t)b;
    if (l < 496 && r > 491 && t < 980 && b > 977) s->overlap |= 1;
    if (l < 896 && r > 880 && t < 1033 && b > 1030) s->overlap |= 2;
}
#endif
