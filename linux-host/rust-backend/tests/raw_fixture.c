/* Offline test-only F/VKM1 emitter. Never opens DRM, USB, graphics or network. */
#define _DEFAULT_SOURCE
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
static int put(const void *p, size_t n) {
    const unsigned char *b=p;
    size_t chunk = n >= 1024 * 1024 ? 65533 : 127;
    while(n) { size_t part=n>chunk?chunk:n; ssize_t k=write(1,b,part); if(k<=0)return -1; b+=k;n-=(size_t)k; }
    return 0;
}
int main(int argc,char **argv) {
    int continuous = argc == 5 && !strcmp(argv[4], "--continuous");
    unsigned w=0,h=0; if((argc!=4&&!continuous)||sscanf(argv[1],"%ux%u",&w,&h)!=2||w<16||h<16||w>4096||h>4096)return 2;
    unsigned mode=(unsigned)atoi(argv[2]),count=(unsigned)atoi(argv[3]);
    size_t n=(size_t)w*h*4; unsigned char *raw=malloc(n); if(!raw)return 3;
    for(unsigned frame=0;continuous||frame<count;frame++) {
        unsigned char request; if(read(0,&request,1)!=1||request!='F')break;
        for(size_t i=0;i<n;i++)raw[i]=(unsigned char)((i*17+frame*41)%256);
        uint32_t hdr[4]; memcpy(hdr,"VKM1",4); hdr[1]=w;hdr[2]=h;hdr[3]=(uint32_t)n;
        if(mode==2&&frame==2)hdr[1]=w+1;
        if(put(hdr,sizeof hdr))break;
        if(mode==3&&frame==2) { put(raw,n/2);break; }
        if(mode==4)usleep(250000);
        if(put(raw,n))break;
    }
    free(raw);return 0;
}
