/* Metadata only. Never maps or reads framebuffer pixels, sets modes, or takes DRM master. */
#define _POSIX_C_SOURCE 200809L
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>
#include <xf86drm.h>
#include <xf86drmMode.h>

/* Universal-plane visibility is a per-fd query capability, not a modeset. */
static void planes(int fd, unsigned int crtc_id) {
    if (drmSetClientCap(fd, DRM_CLIENT_CAP_UNIVERSAL_PLANES, 1)) return;
    drmModePlaneResPtr r = drmModeGetPlaneResources(fd);
    if (!r) return;
    for (unsigned int i = 0; i < r->count_planes; i++) {
        drmModePlanePtr p = drmModeGetPlane(fd, r->planes[i]);
        if (!p) continue;
        if (p->crtc_id == crtc_id && p->fb_id) {
            printf("{\"virtual_crtc_id\":%u,\"plane_id\":%u,\"fb_id\":%u,\"properties\":{",
                   crtc_id, p->plane_id, p->fb_id);
            drmModeObjectPropertiesPtr props = drmModeObjectGetProperties(fd, p->plane_id, DRM_MODE_OBJECT_PLANE);
            int comma = 0;
            if (props) {
                for (unsigned int j = 0; j < props->count_props; j++) {
                    drmModePropertyPtr prop = drmModeGetProperty(fd, props->props[j]);
                    if (!prop) continue;
                    if (!strcmp(prop->name, "type") || !strcmp(prop->name, "CRTC_X") ||
                        !strcmp(prop->name, "CRTC_Y") || !strcmp(prop->name, "CRTC_W") ||
                        !strcmp(prop->name, "CRTC_H")) {
                        printf("%s\"%s\":%llu", comma ? "," : "", prop->name,
                               (unsigned long long)props->prop_values[j]);
                        comma = 1;
                    }
                    drmModeFreeProperty(prop);
                }
                drmModeFreeObjectProperties(props);
            }
            puts("}}");
        }
        drmModeFreePlane(p);
    }
    drmModeFreePlaneResources(r);
}

int main(int argc, char **argv) {
    if (argc != 2 || strncmp(argv[1], "/dev/dri/card", 13) != 0) {
        fprintf(stderr, "usage: vkms_probe /dev/dri/cardN\n");
        return 2;
    }
    const char *suffix = argv[1] + 13;
    if (!*suffix || strspn(suffix, "0123456789") != strlen(suffix)) return 2;
    int fd = open(argv[1], O_RDWR | O_CLOEXEC | O_NOFOLLOW);
    if (fd < 0) { perror("open DRM node"); return 1; }
    drmVersionPtr version = drmGetVersion(fd);
    if (!version || version->name_len != 4 || memcmp(version->name, "vkms", 4)) {
        fprintf(stderr, "refusing non-vkms driver\n");
        if (version) drmFreeVersion(version);
        close(fd);
        return 1;
    }
    drmFreeVersion(version);
    drmModeResPtr resources = drmModeGetResources(fd);
    if (!resources) { perror("DRM resources"); close(fd); return 1; }
    int found = 0;
    for (int i = 0; i < resources->count_connectors; i++) {
        drmModeConnectorPtr c = drmModeGetConnector(fd, resources->connectors[i]);
        if (!c) continue;
        if (c->connector_type != DRM_MODE_CONNECTOR_VIRTUAL || c->connection != DRM_MODE_CONNECTED) {
            drmModeFreeConnector(c); continue;
        }
        drmModeEncoderPtr enc = drmModeGetEncoder(fd, c->encoder_id);
        drmModeCrtcPtr crtc = enc ? drmModeGetCrtc(fd, enc->crtc_id) : NULL;
        printf("{\"driver\":\"vkms\",\"connector_id\":%u,\"virtual_index\":%u,\"crtc_id\":%u,\"active\":%s",
               c->connector_id, c->connector_type_id, enc ? enc->crtc_id : 0,
               crtc && crtc->mode_valid ? "true" : "false");
        if (crtc && crtc->mode_valid) {
            printf(",\"width\":%u,\"height\":%u,\"fb_id\":%u", crtc->width, crtc->height, crtc->buffer_id);
            drmModeFB2Ptr fb = drmModeGetFB2(fd, crtc->buffer_id);
            if (fb) {
                printf(",\"fourcc\":%u,\"pitch\":%u,\"offset\":%u,\"modifier\":%llu,\"handle_available\":%s",
                       fb->pixel_format, fb->pitches[0], fb->offsets[0],
                       (unsigned long long)fb->modifier, fb->handles[0] ? "true" : "false");
                /* GETFB2 creates GEM handles only for authorized callers. Close owned handles. */
                for (int p = 0; p < 4; p++) {
                    if (!fb->handles[p]) continue;
                    int duplicate = 0;
                    for (int j = 0; j < p; j++) if (fb->handles[j] == fb->handles[p]) duplicate = 1;
                    if (!duplicate) drmCloseBufferHandle(fd, fb->handles[p]);
                }
                drmModeFreeFB2(fb);
            } else {
                printf(",\"getfb2_errno\":%d", errno);
            }
        }
        puts("}");
        if (crtc && crtc->mode_valid) planes(fd, crtc->crtc_id);
        found++;
        if (crtc) drmModeFreeCrtc(crtc);
        if (enc) drmModeFreeEncoder(enc);
        drmModeFreeConnector(c);
    }
    drmModeFreeResources(resources);
    close(fd);
    return found ? 0 : 1;
}
