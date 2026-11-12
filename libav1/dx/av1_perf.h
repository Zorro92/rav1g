/*
 * Per-frame timing records for the D3D12 decoder host.
 *
 * Test instrumentation: nothing here changes what is decoded. Timing is off
 * until a callback is installed with av1_set_perf_callback().
 *
 * This header is plain C and has no dependencies, so the record layout can be
 * shared with the player (and compiled on any host for the summary maths).
 */

#ifndef AV1_PERF_H_
#define AV1_PERF_H_

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* GPU stages, in the order the host records them into the frame's command
 * list. A stage's time is the GPU timestamp taken after it minus the
 * timestamp taken before it. */
enum {
  AV1_PERF_GPU_GEN_BLOCKS = 0, /* prediction block generation */
  AV1_PERF_GPU_RESIDUAL_CLEAR, /* residual buffer fill */
  AV1_PERF_GPU_IDCT_SORT,      /* transform block sort */
  AV1_PERF_GPU_IDCT,           /* inverse transform, all types */
  AV1_PERF_GPU_INTER,          /* inter prediction, all passes */
  AV1_PERF_GPU_RECON,          /* inter residual reconstruction */
  AV1_PERF_GPU_INTRA,          /* intra prediction, whole wavefront */
  AV1_PERF_GPU_LF_GEN,         /* loop filter block generation */
  AV1_PERF_GPU_LF_VERT,        /* loop filter, vertical edges (two passes) */
  AV1_PERF_GPU_LF_HORZ,        /* loop filter, horizontal edges */
  AV1_PERF_GPU_CDEF,
  AV1_PERF_GPU_SUPERRES,
  AV1_PERF_GPU_LOOP_REST,
  AV1_PERF_GPU_EXT_BORDERS,    /* reference border extension */
  AV1_PERF_GPU_FILM_GRAIN,
  AV1_PERF_GPU_COPY_PLANE,     /* copy_plane shader into the staging area */
  AV1_PERF_GPU_OUTPUT_COPY,    /* CopyBufferRegion / CopyTextureRegion */
  AV1_PERF_GPU_STAGE_COUNT,
  AV1_PERF_GPU_BEGIN = 255     /* first timestamp of a command list */
};

/* CPU timers. All values are milliseconds in the record. */
enum {
  /* decode thread, blocked */
  AV1_PERF_CPU_WAIT_SLOT = 0, /* waiting for a free pipeline slot (frame data); with -asyncclear the
                                 slot comes back only after its clears, so that time is in here too */
  AV1_PERF_CPU_WAIT_SEC_SLOT, /* waiting for a second slot (extra coef buffer) */
  AV1_PERF_CPU_WAIT_FB,       /* waiting for a free reference frame buffer */
  AV1_PERF_CPU_WAIT_OUTPUT,   /* waiting in the player's output buffer callback */
  AV1_PERF_CPU_WAIT_IMAGE,    /* waiting for a free output image descriptor */
  AV1_PERF_CPU_WAIT_ITEM,     /* waiting for a free GPU work item */
  /* decode thread, working */
  AV1_PERF_CPU_SETUP,         /* av1_setup_frame, including its waits and memsets */
  AV1_PERF_CPU_MEMSET,        /* per-frame memsets in av1_setup_frame, on the decode thread */
  AV1_PERF_CPU_MEMSET_TILE,   /* per-tile memsets, summed across tile threads */
  AV1_PERF_CPU_ENTROPY_WALL,  /* tile decode, wall time on the decode thread */
  AV1_PERF_CPU_ENTROPY_SUM,   /* tile decode, summed across tile threads */
  AV1_PERF_CPU_RECORD,        /* command list recording, waits excluded */
  AV1_PERF_CPU_REC_GEN_BLOCKS,
  AV1_PERF_CPU_REC_IDCT,
  AV1_PERF_CPU_REC_PRED,
  AV1_PERF_CPU_REC_LOOPFILTER,
  AV1_PERF_CPU_REC_CDEF_LR,
  AV1_PERF_CPU_REC_BORDERS,
  AV1_PERF_CPU_REC_COPY,      /* includes WAIT_OUTPUT and WAIT_IMAGE */
  AV1_PERF_CPU_SUBMIT,        /* Close + ExecuteCommandLists + Signal */
  AV1_PERF_CPU_FRAME,         /* decode thread: frame setup start to submit done */
  /* GPU completion thread */
  AV1_PERF_CPU_FENCE_WAIT,    /* blocked on the frame's fence event */
  AV1_PERF_CPU_GPU_LATENCY,   /* submit done to fence seen */
  AV1_PERF_CPU_NOTIFY,        /* time inside the frame-ready callback */
  AV1_PERF_CPU_ASYNC_CLEAR,   /* -asyncclear: the slot's clears, done here after the frame's fence */
  AV1_PERF_CPU_COUNT
};

typedef struct av1_perf_frame {
  int frame_number;   /* decoder frame counter */
  int show_frame;     /* frame is output now */
  int showable_frame; /* hidden frame that may be shown later */
  int is_sef;         /* show-existing-frame that needed an output copy */
  int intra_frame;    /* key or intra-only frame */
  int used_sec_slot;  /* frame also took a second pipeline slot */
  int intra_iters;    /* last intra wavefront iteration dispatched (-1: none); with -intrav2 the last step */
  int intra_v2;       /* intra went through intra_v2.hlsl (-intrav2) */
  int intra_dispatches; /* Dispatch calls of the intra stage */
  int intra_barriers;   /* UAV barriers of the intra stage */
  int intra_groups;     /* thread groups of the intra stage */
  int intra_passes;     /* thread groups times their passes; a group of the original shaders has one */
  int tile_count;

  int gpu_valid;      /* GPU timestamps were read back for this frame */
  int gpu_marks;      /* number of timestamps in the command list */
  double gpu_stage_ms[AV1_PERF_GPU_STAGE_COUNT];
  int gpu_stage_hits[AV1_PERF_GPU_STAGE_COUNT];
  double gpu_span_ms; /* first to last timestamp */
  double gpu_sum_ms;  /* sum of the stage times */
  double gpu_gap_ms;  /* previous frame's last timestamp to this frame's first; -1 unknown */

  double cpu_ms[AV1_PERF_CPU_COUNT];
  int64_t memset_bytes; /* bytes cleared by the timed memsets on the decode and tile threads */
  int64_t async_clear_bytes; /* bytes cleared on the completion thread (AV1_PERF_CPU_ASYNC_CLEAR) */
  double complete_ms;   /* host clock when the frame's fence was seen */
} av1_perf_frame;

/* Called on the GPU completion thread, once per decoded frame, after the
 * frame's fence has completed and its results were read back. */
typedef void (*av1_perf_frame_cb_fn_t)(void *priv, const av1_perf_frame *frame);

/* Process-wide. Pass NULL to switch timing off. Change it only while no frame
 * is in flight (before the first decode call or after a drain). */
void av1_set_perf_callback(av1_perf_frame_cb_fn_t cb, void *priv);

/* Process-wide. Non-zero runs CDEF through cdef_filter_v2.hlsl, zero through
 * the original cdef_filter.hlsl; the decoded output is the same. On by
 * default: an application that never calls this gets cdef_filter_v2. Change
 * it only while no frame is in flight, like the callback above. */
void av1_set_cdef_v2(int enable);

/* Process-wide. Non-zero runs intra prediction through intra_v2.hlsl instead
 * of intra_main.hlsl + intra_filter.hlsl; the decoded output is the same.
 * Frames that allow intra block copy keep the original shaders. Off by
 * default. Change it only while no frame is in flight. */
void av1_set_intra_v2(int enable);

/* Process-wide. Non-zero runs the original intra_main.hlsl (and its _hbd
 * build) from a second pipeline state compiled with FXC optimisation enabled
 * instead of the /Od build the project asks for; the shader source is the
 * same. Applies wherever the original intra path runs, so with
 * av1_set_intra_v2(1) only to the frames that keep it. Off by default. Change
 * it only while no frame is in flight. */
void av1_set_intra_opt(int enable);

/* Process-wide experiments for a device whose output differs from the
 * reference (docs/gpu-av1-decoder.md). Both leave the decoded output as it
 * is on a device that follows D3D. Change them only while no frame is in
 * flight.
 * av1_set_lf_h_split: how the horizontal loop filter is dispatched. 0 (the
 * default): one dispatch per plane. 1: the even rows of 64-row blocks, a UAV
 * barrier, the odd rows. 2: every row of blocks on its own, top to bottom,
 * with a UAV barrier behind each.
 * av1_set_intra_edge_fix: non-zero makes the 8-bit intra shader stop the taps
 * of the above edge filter where the 10-bit shader stops them. */
void av1_set_lf_h_split(int mode);
void av1_set_intra_edge_fix(int enable);

/* What the shaders of this build were compiled as: "" for the normal build,
 * "uavsync" for the build in which every group sync is a sync of all memory
 * (AllMemoryBarrierWithGroupSync), for the log of a test program. */
const char *av1_shader_variant(void);

/* Process-wide. Non-zero moves the per-frame clears of a pipeline slot (the
 * mode info grid and the intra iteration grid) from the decode thread to the
 * GPU completion thread, which does them after the fence of the frame that
 * used the slot and before the slot goes back to the pool. The decode thread
 * then finds the slot already cleared; it still clears on its own when the
 * cleared size does not match the frame (first use, frame size change). The
 * decoded output is the same. Off by default. Change it only while no frame
 * is in flight. */
void av1_set_async_clear(int enable);

/* Stage dumps: a debugging aid of the test programs (-dumpstages). For the
 * frames it is asked for, the decoder copies the buffer a stage of the frame's
 * command list has just written into a readback buffer, right behind the
 * stage, and hands the copies over after the frame's GPU work is done. The
 * stages, in the order of the command list (a frame has only those that run):
 *   idct          the residuals (kind 1: signed 16-bit samples)
 *   inter         the frame buffer after inter prediction (all passes)
 *   recon         after the residuals of the inter blocks were added
 *   intra         after intra prediction: the frame before the loop filter
 *   lf_vert_even  after the first of the two vertical loop filter passes
 *   lf_vert       after the second
 *   lf_horz       after the horizontal loop filter
 *   cdef          the buffer CDEF wrote
 *   superres_lr   the upscaled frame without CDEF, which loop restoration reads
 *   superres      the upscaled frame
 *   loop_rest     after loop restoration
 *   borders       the finished frame with its borders: what later frames read
 *   output        the output image (after film grain), as it is handed out
 * A copy needs a UAV-to-copy-source transition of the buffer in front of it and
 * the opposite one behind it, and the stages are a UAV barrier apart already,
 * so a dump does not change what the stages of a frame see of each other.
 * The decoded output is the same.
 *
 * Between them come the buffers the stages read besides the frame (kind 2,
 * "data"): no samples but lists and tables, with a text that says how they are
 * laid out (av1_stage_dump::layout: lines of key=value fields; the keys are
 * listed in testing/common/stage_dump.h). Each is copied where the first
 * stage that reads it starts, so it holds what that stage reads:
 *   mi_grid          the frame's grid of mode info pointers (8 bytes per 4x4 unit)
 *   mi_pool          the mode info records of the frame's tiles (the records the
 *                    grid points to; gen_pred_blocks and gen_lf_blocks read them)
 *   pred_blocks      the prediction block records gen_pred_blocks wrote
 *   pred_blocks_warp the warped prediction block records
 *   lf_params        the constants of the loop filter: levels, deltas, limits
 *   lf_blocks        the edge lists gen_lf_blocks_vert/hor wrote, which
 *                    loopfilter_vert/hor read: per plane and direction one
 *                    record of sixteen edges per block
 *   cdef_info        CDEF's strength index per 64x64 and skip flag per 8x8
 *   lr_info          loop restoration: type and coefficients per unit
 * The buffers the shaders write (pred_blocks*, lf_blocks) take the same
 * transition as a frame buffer; the others are read-only for the GPU and are
 * copied as they are. */
typedef struct av1_stage_dump_plane {
  uint32_t offset; /* of the first visible sample, in bytes from the start of data */
  uint32_t stride; /* bytes */
  uint32_t width;  /* visible samples */
  uint32_t height; /* visible rows; 0: the dump has no such plane */
} av1_stage_dump_plane;

typedef struct av1_stage_dump {
  int frame_number;     /* decoder frame counter: frames in decoding order, hidden frames included,
                           from 0; a show_existing_frame has the number of the frame it shows */
  int index;            /* position of the dump in the command list, from 0 */
  const char *stage;    /* see above */
  int kind;             /* 0: samples of a frame buffer or an output image, 1: residuals,
                           2: data (a list or table; no planes, see layout) */
  int bytes_per_sample; /* 1 or 2 (low byte first) */
  int bit_depth;
  int is_sef;           /* the command list of a show_existing_frame (output only) */
  const char *info;     /* one line about the frame: type, sizes, filters, references */
  av1_stage_dump_plane planes[3];
  const uint8_t *data;  /* the buffer as stored, valid during the call */
  size_t size;
  const char *layout;   /* kind 2: how data is laid out, as text; NULL otherwise */
} av1_stage_dump;

/* Called on the GPU completion thread, once per dump, in command list order,
 * after the frame's fence. */
typedef void (*av1_stage_dump_cb_fn_t)(void *priv, const av1_stage_dump *dump);

/* Process-wide. The frame numbers to dump (at most 256 are kept); cb NULL or
 * count 0 switches the dumps off. Change it only while no frame is in flight. */
void av1_set_stage_dump(av1_stage_dump_cb_fn_t cb, void *priv, const int *frames, int count);

/* Log lines of the decoder: the MEMORY lines of a decoder creation (what was
 * allocated, the video memory budget, and the allocation that failed). One
 * line per call, no line end. is_error is non-zero for a failure. */
typedef void (*av1_log_cb_fn_t)(void *priv, int is_error, const char *line);

/* Process-wide. Without a callback the lines go to the debugger output. Set it
 * before the pipeline cache and the decoder are created. */
void av1_set_log_callback(av1_log_cb_fn_t cb, void *priv);

/* One "MEMORY dxgi <label>: ..." line to the log callback: budget and usage
 * of the video memory of the adapter the ID3D12Device is on
 * (IDXGIAdapter3::QueryVideoMemoryInfo), as the decoder logs it around its
 * creation. For an application that logs its memory step by step. */
void av1_log_video_memory_now(void *d3d12device, const char *label);

/* One error line to the log callback, for the decoder's C code. */
void av1_log_error(const char *line);

/* What made the last decoder creation of this process fail, as one line; ""
 * when it did not fail. Valid until the next creation. The codec interface
 * returns it as the error detail of aom_codec_dec_init. */
const char *av1_last_create_error(void);

#ifdef __cplusplus
}
#endif

#endif /* AV1_PERF_H_ */
