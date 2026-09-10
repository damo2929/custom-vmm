/* A flat C interface to libx264's encoder.
 *
 * Why this exists: bindgen cannot generate x264_param_t. x264.h defines
 * x264_zone_t before x264_param_t, and the zone struct holds a
 * `struct x264_param_t *` back-pointer. Reaching that forward declaration
 * first makes bindgen materialise x264_param_t as an opaque one-byte struct,
 * and it never upgrades the item when the complete definition appears later
 * in the same header. Confirmed against bindgen 0.70, 0.71 and 0.72, and not
 * fixable from the Rust side: blocklisting the zone type suppresses emission
 * but not materialisation.
 *
 * The other three codec libraries bind cleanly, so only x264 is wrapped. The
 * ABI below is ours, built from scalars and pointers only, which is what
 * makes it safe to declare by hand on the Rust side.
 *
 * Every entry point returns 0 or a positive count on success and a negative
 * VMM_X264_E_* code on failure.
 */

#ifndef VMM_X264_H
#define VMM_X264_H

#include <stdint.h>

#define VMM_X264_E_ALLOC        -1  /* out of memory */
#define VMM_X264_E_PRESET       -2  /* x264 rejected the preset/tune pair */
#define VMM_X264_E_PROFILE      -3  /* x264 rejected the profile */
#define VMM_X264_E_OPEN         -4  /* x264_encoder_open failed */
#define VMM_X264_E_ENCODE       -5  /* x264_encoder_encode failed */
#define VMM_X264_E_NO_NAL       -6  /* bytes reported but no NAL produced */
#define VMM_X264_E_INVALID      -7  /* caller passed a null or bad argument */

typedef struct vmm_x264_encoder vmm_x264_encoder;

/* Constrained VBR as the design spec §7.1 defines it: target_kbps is the
 * average, max_kbps the ceiling instantaneous output must stay below, and
 * vbv_buffer_bits the HRD buffer that enforces it. */
typedef struct {
    int32_t width;
    int32_t height;
    int32_t framerate;
    int32_t gop_length;
    int32_t target_kbps;
    int32_t max_kbps;
    int32_t vbv_buffer_bits;
} vmm_x264_config;

/* What libx264 chose for itself, so the caller can report it. */
typedef struct {
    uint32_t cpu_flags;   /* X264_CPU_* bitmap libx264 detected */
    int32_t  threads;     /* frame threads it settled on */
} vmm_x264_info;

/* One encoded access unit. `data` points into the encoder's own buffer and
 * stays valid only until the next call on the same encoder. */
typedef struct {
    const uint8_t *data;
    int32_t size;
    int32_t keyframe;
    int64_t pts;
    int64_t dts;
} vmm_x264_output;

int32_t vmm_x264_open(const vmm_x264_config *cfg,
                      vmm_x264_encoder **out,
                      vmm_x264_info *info);

/* Encode one I420 frame. Returns 1 when `out` was filled, 0 when libx264
 * buffered the frame, or a negative error code.
 *
 * A non-zero force_idr codes this frame as an IDR regardless of where the
 * GOP would otherwise have put one, and restarts the GOP counter. The caller
 * uses it to hold the keyframe interval to a wall-clock bound, which a
 * frame-counted GOP cannot do when the guest renders slower than the
 * configured frame rate. */
int32_t vmm_x264_encode(vmm_x264_encoder *enc,
                        const uint8_t *y, const uint8_t *u, const uint8_t *v,
                        int32_t y_stride, int32_t uv_stride,
                        int64_t pts,
                        int32_t force_idr,
                        vmm_x264_output *out);

/* Drain one buffered frame. Same return convention as vmm_x264_encode. */
int32_t vmm_x264_flush(vmm_x264_encoder *enc, vmm_x264_output *out);

int32_t vmm_x264_delayed_frames(vmm_x264_encoder *enc);

void vmm_x264_close(vmm_x264_encoder *enc);

/* X264_BUILD the shim was compiled against. */
int32_t vmm_x264_build(void);

#endif /* VMM_X264_H */
