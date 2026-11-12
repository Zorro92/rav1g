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
  /* The inter prediction breakdown of the frame (av1_set_perf_detail), valid during the callback only;
   * NULL without it and for a show-existing copy. */
  const struct av1_perf_inter_detail *inter;
} av1_perf_frame;

/* ---------------------------------------------------------------------------
 * Inter prediction breakdown (test flag -gputimedetail). Nothing here changes
 * what is decoded.
 *
 * The inter stage of a frame's command list is a row of sections, each a list
 * of dispatches of one shader (one dispatch per block size), in this order.
 * Three UAV barriers divide it into passes whose shaders read what the pass
 * before wrote:
 *   pass 1  warp, single, comp_avg, comp_diff_y, comp_masked, comp_warp,
 *           chroma_2x2                                      then sync0
 *           (av1_set_inter_fast: single and comp_avg by motion vector class,
 *           dispatched behind comp_masked)
 *   pass 2  comp_diff_uv, comp_warp_diff_uv, obmc_above     then sync1
 *   pass 3  obmc_left                                       then sync2
 * recon (the residuals of the blocks whose prediction is not final when their
 * inter shader ends: OBMC, and palette blocks) is the stage behind it
 * (AV1_PERF_GPU_RECON) and ends with a barrier of its own.
 *
 * Mode 1 puts a timestamp behind every section that dispatched something and
 * behind every barrier. The GPU runs the dispatches of a pass side by side, so
 * a section's time is the time from the timestamp before it to its own: it
 * follows the order of submission and overlaps with its neighbours in the
 * pass. Exclusive are only the sums up to a barrier: the passes, and recon.
 * Mode 2 also puts a UAV barrier and a timestamp behind every dispatch of the
 * inter stage and of recon, so that each runs alone and has a time of its
 * own; the decoded output is the same, the stage gets slower by what the
 * barriers cost.
 * ------------------------------------------------------------------------- */
enum {
  AV1_PERF_INTER_WARP = 0,          /* one reference, warped (local or global): inter_warp */
  AV1_PERF_INTER_SINGLE,            /* one reference: inter_main, inter_scale */
  /* av1_set_inter_fast: one reference, by the fraction of the motion vector. A frame on that path has these
   * four in place of AV1_PERF_INTER_SINGLE. */
  AV1_PERF_INTER_SINGLE_COPY,       /* no fraction: inter_fast, a copy */
  AV1_PERF_INTER_SINGLE_H,          /* a fraction in x only: inter_fast, one horizontal pass */
  AV1_PERF_INTER_SINGLE_V,          /* a fraction in y only: inter_fast, one vertical pass */
  AV1_PERF_INTER_SINGLE_2D,         /* both: inter_main */
  AV1_PERF_INTER_COMP_AVG,          /* two references, averaged or distance weighted: inter_compound */
  /* av1_set_inter_fast: the same by the classes of the two references, AV1_PERF_INTER_COMP_AVG_FAST +
   * (class of reference 0 | class of reference 1 << 2), class 0 copy, 1 x only, 2 y only, 3 both:
   * inter_compound_fast, and inter_compound for the last one (both references of class 3). A frame on that
   * path has these sixteen in place of AV1_PERF_INTER_COMP_AVG. */
  AV1_PERF_INTER_COMP_AVG_FAST,
  AV1_PERF_INTER_COMP_AVG_FAST_LAST = AV1_PERF_INTER_COMP_AVG_FAST + 15,
  AV1_PERF_INTER_COMP_DIFF_Y,       /* two references, difference weighted, luma: writes the mask the chroma reads */
  AV1_PERF_INTER_COMP_MASKED,       /* two references, wedge mask */
  AV1_PERF_INTER_COMP_WARP,         /* two references, one or both globally warped: inter_scale_warp_compound */
  AV1_PERF_INTER_CHROMA_2X2,        /* the chroma of blocks under 8x8, in 2x2 pieces: inter_2x2chroma */
  AV1_PERF_INTER_SYNC0,             /* the UAV barrier behind pass 1 */
  AV1_PERF_INTER_COMP_DIFF_UV,      /* difference weighted, chroma: reads the mask */
  AV1_PERF_INTER_COMP_WARP_DIFF_UV, /* the same for globally warped blocks */
  AV1_PERF_INTER_OBMC_ABOVE,        /* overlapped prediction from the blocks above, blended into the frame */
  AV1_PERF_INTER_SYNC1,             /* the UAV barrier behind pass 2 */
  AV1_PERF_INTER_OBMC_LEFT,         /* overlapped prediction from the blocks to the left */
  AV1_PERF_INTER_SYNC2,             /* the UAV barrier behind pass 3: the end of the inter stage */
  AV1_PERF_INTER_RECON,             /* reconstruct_block: the stage AV1_PERF_GPU_RECON */
  AV1_PERF_INTER_SECTION_COUNT
};

enum { AV1_PERF_INTER_LISTS = 36 }; /* dispatches a section can have: one per block size */

/* One dispatch: the blocks of one size of a section. */
typedef struct av1_perf_inter_list {
  int width;   /* of the block of one record, in samples */
  int height;
  int records; /* block records of the dispatch; a chroma block has two, U and V */
  int groups;  /* thread groups of 64 threads */
  int threads; /* threads with work (the rest of the last group has none) */
  double ms;   /* mode 2: GPU time of the dispatch and its barrier; 0 otherwise */
} av1_perf_inter_list;

/* What a section predicted, counted where the tile threads push the blocks. recon has none of it. */
typedef struct av1_perf_inter_work {
  int64_t blocks_y;     /* luma prediction blocks */
  int64_t blocks_uv;    /* chroma prediction blocks; U and V are one block here */
  int64_t pixels_y;     /* luma samples predicted */
  int64_t pixels_uv;    /* chroma samples predicted, both planes */
  int64_t ref_blocks;   /* reference blocks fetched: one per prediction block, plane and reference */
  int64_t footprint;    /* samples of the references those fetches cover with an 8-tap filter in both
                           directions: (w + 7) * (h + 7) each (a scaled reference: the scaled size + 7) */
  int64_t footprint_mv; /* the same with the 7 only in a direction in which the motion vector has a
                           fraction: what a shader that looked at the vector would have to read */
} av1_perf_inter_work;

typedef struct av1_perf_inter_section {
  double ms;            /* GPU time: the section's timestamp minus the one before it (see above) */
  int timed;            /* the frame has a timestamp for the section */
  int dispatches;
  int groups;
  int64_t threads;
  int64_t records;
  int64_t pixels;       /* samples the records cover: records * width * height */
  int64_t loaded_bytes; /* reference bytes the threads load: threads times what one thread of the shader
                           loads for its reference rows, whatever the motion vector (an estimate for the
                           shaders of scaled references, whose row count depends on the block) */
  av1_perf_inter_work work;
  av1_perf_inter_list lists[AV1_PERF_INTER_LISTS];
} av1_perf_inter_section;

/* The inter blocks of the frame by what they use, in luma samples of the block (mode info blocks, whatever
 * section their parts went to). ref_area counts a block once per reference. */
typedef struct av1_perf_inter_blocks {
  int64_t blocks;
  int64_t area;
  int64_t area_compound;   /* two references */
  int64_t area_obmc;
  int64_t area_warp;       /* luma warped, locally or globally */
  int64_t area_wedge;      /* compound with a wedge mask */
  int64_t area_diffwtd;    /* compound with a difference weighted mask */
  int64_t area_interintra;
  int64_t area_scaled;     /* a reference of another size than the frame */
  int64_t ref_area;
  int64_t ref_area_mv[4];  /* by the fraction of the luma motion vector: none, horizontal only, vertical
                              only, both (a warped or scaled reference counts as both) */
  int64_t ref_area_filter[2][3]; /* [horizontal, vertical] by the kernel the block asks for: 8-tap (regular,
                                    smooth, sharp), 4-tap (the same for a side of 4), bilinear */
} av1_perf_inter_blocks;

typedef struct av1_perf_inter_detail {
  int mode;             /* 1 or 2, see above */
  int has_inter;        /* the frame has an inter stage; without one only recon can have numbers */
  int scaled;           /* the frame has a reference of another size: all sections but warp run the shaders
                           for scaled references (inter_scale_*) */
  int bytes_per_sample; /* of the frame buffers: 1 or 2 */
  int barriers_added;   /* mode 2: UAV barriers the mode added to the frame */
  int marks;            /* timestamps the detail added to the frame */
  av1_perf_inter_section sections[AV1_PERF_INTER_SECTION_COUNT];
  av1_perf_inter_blocks blocks;
} av1_perf_inter_detail;

/* Process-wide. 0 (the default): no inter breakdown. 1: section timestamps and work counters. 2: also a UAV
 * barrier and a timestamp behind every dispatch. Only with a perf callback installed. Change it only while
 * no frame is in flight, like the callback. */
void av1_set_perf_detail(int mode);

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

/* Process-wide. Non-zero predicts the inter blocks with one reference and the
 * averaged compound blocks by the fraction of their motion vectors
 * (inter_fast.hlsl, inter_compound_fast.hlsl: a copy, one filter pass, or
 * the two passes of the original shaders); the decoded output is the same.
 * Frames with a scaled reference keep the original path. Only in a decoder
 * created with aom_codec_dec_cfg_t::inter_fast. Off by default. Change it
 * only while no frame is in flight. */
void av1_set_inter_fast(int enable);

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
