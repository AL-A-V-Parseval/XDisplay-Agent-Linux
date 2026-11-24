/* Session-private raw pixel slot. Define _GNU_SOURCE before system headers.
 * No supplied pathname/DRM handle is accepted or exported by this interface. */
#ifndef XDISPLAY_RAW_SLOT_H
#define XDISPLAY_RAW_SLOT_H
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <unistd.h>

struct raw_slot {
    unsigned char *writer;
    size_t size;
    uint32_t width, height, sequence;
    int pending_ack;
};
#define RAW_SLOT_INIT {.writer=MAP_FAILED}

static void raw_slot_close(struct raw_slot *slot) {
    if (slot->writer != MAP_FAILED) munmap(slot->writer, slot->size);
    *slot = (struct raw_slot)RAW_SLOT_INIT;
}
static void raw_slot_le32(unsigned char *p, uint32_t value) {
    for (int i=0; i<4; i++) p[i]=(unsigned char)(value >> (8*i));
}
static int raw_slot_channel(int channel) {
    int type=0;
    socklen_t size=sizeof type;
    struct sockaddr_storage address;
    socklen_t length=sizeof address;
    if (getsockopt(channel, SOL_SOCKET, SO_TYPE, &type, &size) || type!=SOCK_SEQPACKET ||
            getsockname(channel, (struct sockaddr *)&address, &length) ||
            address.ss_family!=AF_UNIX || length!=sizeof(sa_family_t)) return 0;
    length=sizeof address;
    return !getpeername(channel, (struct sockaddr *)&address, &length) &&
        address.ss_family==AF_UNIX && length==sizeof(sa_family_t);
}
/* Only send small bounded records; a stalled peer cannot grow the queue forever. */
static int raw_slot_send(int channel, const unsigned char header[16], int fd) {
    union { struct cmsghdr alignment; unsigned char bytes[CMSG_SPACE(sizeof(int))]; } control;
    memset(&control, 0, sizeof control);
    struct iovec vector={.iov_base=(void *)header, .iov_len=16};
    struct msghdr msg={.msg_iov=&vector, .msg_iovlen=1};
    if (fd>=0) {
        msg.msg_control=control.bytes; msg.msg_controllen=sizeof control.bytes;
        struct cmsghdr *cmsg=CMSG_FIRSTHDR(&msg);
        cmsg->cmsg_level=SOL_SOCKET; cmsg->cmsg_type=SCM_RIGHTS;
        cmsg->cmsg_len=CMSG_LEN(sizeof(int));
        memcpy(CMSG_DATA(cmsg), &fd, sizeof fd);
    }
    struct pollfd output={.fd=channel, .events=POLLOUT};
    if (poll(&output, 1, 5000)<=0 || !(output.revents&POLLOUT)) return -1;
    return sendmsg(channel, &msg, MSG_NOSIGNAL|MSG_DONTWAIT)==16 ? 0 : -1;
}
static int raw_slot_prepare(struct raw_slot *slot, int channel, uint32_t width, uint32_t height) {
    if (!raw_slot_channel(channel) || slot->pending_ack || width<16 || height<16 ||
            width>4096 || height>4096 || (uint64_t)width*height>8388608) {
        errno=EINVAL; return -1;
    }
    if (slot->writer!=MAP_FAILED) {
        if (slot->width!=width || slot->height!=height) { errno=EINVAL; return -1; }
        return 0;
    }
    size_t size=(size_t)width*height*4;
    int fd=memfd_create("xdisplay-vkms-slot", MFD_CLOEXEC|MFD_ALLOW_SEALING);
    if (fd<0) return -1;
    if (ftruncate(fd, (off_t)size)) { close(fd); return -1; }
    unsigned char *writer=mmap(NULL, size, PROT_READ|PROT_WRITE, MAP_SHARED, fd, 0);
    if (writer==MAP_FAILED) { close(fd); return -1; }
    int seals=F_SEAL_GROW|F_SEAL_SHRINK|F_SEAL_FUTURE_WRITE|F_SEAL_SEAL;
    if (fcntl(fd, F_ADD_SEALS, seals)) { munmap(writer, size); close(fd); return -1; }
    char path[64];
    int count=snprintf(path, sizeof path, "/proc/self/fd/%d", fd);
    int ro=count>0 && (size_t)count<sizeof path ? open(path, O_RDONLY|O_CLOEXEC) : -1;
    close(fd);
    if (ro<0) { munmap(writer, size); return -1; }
    unsigned char header[16]={'S','L','O','1'};
    raw_slot_le32(header+4, width); raw_slot_le32(header+8, height);
    raw_slot_le32(header+12, (uint32_t)size);
    int result=raw_slot_send(channel, header, ro);
    close(ro);
    if (result) { munmap(writer, size); return -1; }
    *slot=(struct raw_slot){.writer=writer, .size=size, .width=width, .height=height};
    return 0;
}
static int raw_slot_publish(struct raw_slot *slot, int channel) {
    if (slot->writer==MAP_FAILED || slot->pending_ack || slot->sequence>=300) {
        errno=EINVAL; return -1;
    }
    unsigned char header[16]={'S','L','T','1'};
    raw_slot_le32(header+4, slot->width); raw_slot_le32(header+8, slot->height);
    raw_slot_le32(header+12, slot->sequence);
    if (raw_slot_send(channel, header, -1)) return -1;
    slot->pending_ack=1; slot->sequence++;
    return 0;
}
static int raw_slot_ack(struct raw_slot *slot) {
    if (!slot->pending_ack) { errno=EINVAL; return -1; }
    slot->pending_ack=0;
    return 0;
}
#endif
