#include <assert.h>
#include <stdlib.h>
#include <string.h>
#include "cursor_stage_probe.h"
#include "cursor_compose.h"

int main(void) {
    unsigned char *frame = malloc(2360u * 1640u * 4u);
    assert(frame);
    memset(frame, 30, 2360u * 1640u * 4u);
    // Simulate one short line already in the primary snapshot.
    memset(frame + (978u * 2360 + 491) * 4, 200, 5 * 4);
    memset(frame + (1013u * 2360 + 528) * 4, 220, 16 * 4);
    memset(frame + (1100u * 2360 + 520) * 4, 165, 6 * 4);
    uint8_t before[STAGE_ROI_W * STAGE_ROI_H];
    stage_roi_before(frame, before);
    struct cursor_stage s = {0};
    int32_t after[2];
    stage_measure(frame, s.pre);
    assert(s.pre[0] == 170 && s.pre[1] == 0);
    unsigned char cursor[16 * 4];
    memset(cursor, 255, sizeof(cursor));
    compose_cursor(frame, 2360, 1640, cursor, 16, 1, sizeof(cursor), 880, 1031);
    stage_cursor(&s, 880, 1031, 16, 1);
    stage_measure(frame, after);
    assert(after[0] == 170 && after[1] == 225 && s.overlap == 2);
    assert(s.planes == 1 && s.left == 880 && s.top == 1031 && s.right == 896 && s.bottom == 1032);
    stage_cursor(&s, INT32_MAX, INT32_MIN, 16, 16);
    assert(s.planes == 2 && s.overlap == 2 && s.right == 896);
    stage_cursor(&s, -5, -2, 16, 16);
    assert(s.planes == 3 && s.overlap == 2 && s.left == 0 && s.top == 0);
    memset(frame + (1100u * 2360 + 526) * 4, 165, 6 * 4);
    memset(frame + (1120u * 2360 + 448) * 4, 250, 16 * 4); // clipped edge
    struct stage_roi_result roi = stage_roi_after(frame, before);
    assert(roi.after_count == 4 && roi.mixed_count == 1);
    assert(roi.present_before.x == 528 && roi.present_before.y == 1013);
    assert(roi.present_before.width == 16 && roi.present_before.contrast == 190);
    assert(roi.new_after.x == 880 && roi.new_after.y == 1031);
    assert(roi.new_after.width == 16 && roi.new_after.contrast == 225);
    free(frame);
    return 0;
}
