#include <linux/dma-mapping.h>
static int start(struct video_device *vdev) {
    return VIDIOC_STREAMON;
}
void *buf = dma_alloc_coherent(dev, size, &addr, GFP_KERNEL);
/* empty-arg spelling: dma_alloc_coherent() */
/* 100% 覆盖 a_b */
