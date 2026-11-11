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
  AV1_PERF_CPU_WAIT_SLOT = 0, /* waiting for a free pipeline slot (frame data) */
  AV1_PERF_CPU_WAIT_SEC_SLOT, /* waiting for the second slot (extra coef buffer) */
  AV1_PERF_CPU_WAIT_FB,       /* waiting for a free reference frame buffer */
  AV1_PERF_CPU_WAIT_OUTPUT,   /* waiting in the player's output buffer callback */
  AV1_PERF_CPU_WAIT_IMAGE,    /* waiting for a free output image descriptor */
  AV1_PERF_CPU_WAIT_ITEM,     /* waiting for a free GPU work item */
  /* decode thread, working */
  AV1_PERF_CPU_SETUP,         /* av1_setup_frame, including its waits and memsets */
  AV1_PERF_CPU_MEMSET,        /* per-frame memsets in av1_setup_frame */
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
  AV1_PERF_CPU_COUNT
};

typedef struct av1_perf_frame {
  int frame_number;   /* decoder frame counter */
  int show_frame;     /* frame is output now */
  int showable_frame; /* hidden frame that may be shown later */
  int is_sef;         /* show-existing-frame that needed an output copy */
  int intra_frame;    /* key or intra-only frame */
  int used_sec_slot;  /* frame also took the second pipeline slot */
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
  int64_t memset_bytes; /* bytes cleared by the timed memsets */
  double complete_ms;   /* host clock when the frame's fence was seen */
} av1_perf_frame;

/* Called on the GPU completion thread, once per decoded frame, after the
 * frame's fence has completed and its results were read back. */
typedef void (*av1_perf_frame_cb_fn_t)(void *priv, const av1_perf_frame *frame);

/* Process-wide. Pass NULL to switch timing off. Change it only while no frame
 * is in flight (before the first decode call or after a drain). */
void av1_set_perf_callback(av1_perf_frame_cb_fn_t cb, void *priv);

/* Process-wide. Non-zero runs CDEF through cdef_filter_v2.hlsl instead of
 * cdef_filter.hlsl; the decoded output is the same. Off by default. Change it
 * only while no frame is in flight, like the callback above. */
void av1_set_cdef_v2(int enable);

/* Process-wide. Non-zero runs intra prediction through intra_v2.hlsl instead
 * of intra_main.hlsl + intra_filter.hlsl; the decoded output is the same.
 * Frames that allow intra block copy keep the original shaders. Off by
 * default. Change it only while no frame is in flight. */
void av1_set_intra_v2(int enable);

#ifdef __cplusplus
}
#endif

#endif /* AV1_PERF_H_ */
