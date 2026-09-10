#include "vmm_x264.h"

#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <x264.h>

struct vmm_x264_encoder {
    x264_t *handle;
};

int32_t vmm_x264_open(const vmm_x264_config *cfg,
                      vmm_x264_encoder **out,
                      vmm_x264_info *info)
{
    if (cfg == NULL || out == NULL || info == NULL) {
        return VMM_X264_E_INVALID;
    }
    *out = NULL;

    x264_param_t param;

    /* "veryfast" is the quality/latency point a live console wants, and
     * "zerolatency" removes the lookahead and B-frames that would otherwise
     * add multi-frame delay to an interactive session. */
    if (x264_param_default_preset(&param, "veryfast", "zerolatency") < 0) {
        return VMM_X264_E_PRESET;
    }

    /* param.cpu is left exactly as x264_param_default detected it. Zeroing
     * it would disable every hand-written SIMD kernel, which costs roughly
     * an order of magnitude of encode throughput. */
    info->cpu_flags = param.cpu;

    param.i_csp        = X264_CSP_I420;
    param.i_width      = cfg->width;
    param.i_height     = cfg->height;
    param.i_bitdepth   = 8;
    param.i_fps_num    = (uint32_t)cfg->framerate;
    param.i_fps_den    = 1;

    /* One IDR every gop_length frames, with scene-cut promotion off, so a
     * client joining late waits a bounded and predictable time. */
    param.i_keyint_max          = cfg->gop_length;
    param.i_keyint_min          = cfg->gop_length;
    param.i_scenecut_threshold  = 0;

    /* Let libx264 size the thread pool from the core count. The zerolatency
     * tune already forces sliced threading, which is what keeps per-frame
     * latency flat as thread count rises. */
    param.i_threads = X264_THREADS_AUTO;

    /* Constrained VBR. ABR gives the target average; the VBV pair is what
     * actually bounds instantaneous output. i_vbv_buffer_size is in kbit and
     * i_vbv_max_bitrate in kbit/s, so round the bit figure up rather than
     * truncating a sub-kbit buffer to zero. */
    param.rc.i_rc_method        = X264_RC_ABR;
    param.rc.i_bitrate          = cfg->target_kbps;
    param.rc.i_vbv_max_bitrate  = cfg->max_kbps;
    param.rc.i_vbv_buffer_size  = (cfg->vbv_buffer_bits + 999) / 1000;
    /* Signal the HRD in the bitstream so a conforming decoder sizes its own
     * buffer the same way. */
    param.i_nal_hrd = X264_NAL_HRD_VBR;

    /* Annex-B with SPS/PPS repeated before every IDR: the RTP packetiser
     * splits on start codes, and a client joining mid-stream needs parameter
     * sets without an out-of-band exchange. */
    param.b_annexb          = 1;
    param.b_repeat_headers  = 1;
    param.b_aud             = 0;

    param.i_log_level = X264_LOG_WARNING;

    if (x264_param_apply_profile(&param, "high") < 0) {
        return VMM_X264_E_PROFILE;
    }

    info->threads = param.i_threads;

    vmm_x264_encoder *enc = calloc(1, sizeof(*enc));
    if (enc == NULL) {
        return VMM_X264_E_ALLOC;
    }

    enc->handle = x264_encoder_open(&param);
    if (enc->handle == NULL) {
        free(enc);
        return VMM_X264_E_OPEN;
    }

    /* x264_encoder_open may revise the thread count; report what it settled
     * on rather than what was asked for. */
    x264_param_t applied;
    x264_encoder_parameters(enc->handle, &applied);
    info->threads = applied.i_threads;

    *out = enc;
    return 0;
}

/* Shared tail of encode and flush: run one x264_encoder_encode and publish
 * the result. `pic_in` is NULL when draining. */
static int32_t drive(vmm_x264_encoder *enc,
                     x264_picture_t *pic_in,
                     vmm_x264_output *out)
{
    x264_nal_t *nals = NULL;
    int nal_count = 0;
    x264_picture_t pic_out;
    memset(&pic_out, 0, sizeof(pic_out));

    int size = x264_encoder_encode(enc->handle, &nals, &nal_count, pic_in, &pic_out);
    if (size < 0) {
        return VMM_X264_E_ENCODE;
    }
    if (size == 0) {
        return 0;   /* buffered, not an error */
    }
    if (nals == NULL || nal_count <= 0 || nals[0].p_payload == NULL) {
        return VMM_X264_E_NO_NAL;
    }

    /* libx264 writes every NAL of one access unit into a single contiguous
     * buffer, so the first NAL's payload plus `size` is the whole Annex-B
     * frame. The buffer belongs to the encoder and is valid until the next
     * call, which is what the header documents to the caller. */
    out->data     = nals[0].p_payload;
    out->size     = size;
    out->keyframe = pic_out.b_keyframe;
    out->pts      = pic_out.i_pts;
    out->dts      = pic_out.i_dts;
    return 1;
}

int32_t vmm_x264_encode(vmm_x264_encoder *enc,
                        const uint8_t *y, const uint8_t *u, const uint8_t *v,
                        int32_t y_stride, int32_t uv_stride,
                        int64_t pts,
                        int32_t force_idr,
                        vmm_x264_output *out)
{
    if (enc == NULL || out == NULL || y == NULL || u == NULL || v == NULL) {
        return VMM_X264_E_INVALID;
    }

    x264_picture_t pic_in;
    x264_picture_init(&pic_in);

    /* Point libx264 straight at the caller's planes. The design spec asks
     * the datapath not to allocate, and x264 only reads the input during
     * this call, which the caller's frame outlives. */
    pic_in.img.i_csp      = X264_CSP_I420;
    pic_in.img.i_plane    = 3;
    pic_in.img.plane[0]   = (uint8_t *)y;
    pic_in.img.plane[1]   = (uint8_t *)u;
    pic_in.img.plane[2]   = (uint8_t *)v;
    pic_in.img.i_stride[0] = y_stride;
    pic_in.img.i_stride[1] = uv_stride;
    pic_in.img.i_stride[2] = uv_stride;
    pic_in.i_pts  = pts;
    /* X264_TYPE_IDR, not X264_TYPE_I: an I frame alone does not reset the
     * reference list, so a client joining afterwards still could not decode.
     * IDR is what makes the frame a usable entry point. */
    pic_in.i_type = force_idr ? X264_TYPE_IDR : X264_TYPE_AUTO;

    return drive(enc, &pic_in, out);
}

int32_t vmm_x264_flush(vmm_x264_encoder *enc, vmm_x264_output *out)
{
    if (enc == NULL || out == NULL) {
        return VMM_X264_E_INVALID;
    }
    if (x264_encoder_delayed_frames(enc->handle) == 0) {
        return 0;
    }
    return drive(enc, NULL, out);
}

int32_t vmm_x264_delayed_frames(vmm_x264_encoder *enc)
{
    if (enc == NULL) {
        return VMM_X264_E_INVALID;
    }
    return x264_encoder_delayed_frames(enc->handle);
}

void vmm_x264_close(vmm_x264_encoder *enc)
{
    if (enc == NULL) {
        return;
    }
    if (enc->handle != NULL) {
        x264_encoder_close(enc->handle);
    }
    free(enc);
}

int32_t vmm_x264_build(void)
{
    return X264_BUILD;
}
