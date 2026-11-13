/*
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
 * Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
 * file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
 * License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
 * www.aomedia.org/license/patent.
 */

/*
 * What rav1g-dec still takes from the C++ of the test programs, behind a C interface (shim.cpp):
 *
 *   the stream readers of testing/common (IVF, WebM/Matroska through libwebm, OBU, MP4),
 *   the summary of the timing run (testing/player_common/perf_stats.h, -gputime),
 *   the stage dump files (testing/common/stage_dump.h, -dumpstages),
 *   the capability report and the Shader Model 6 test (testing/sm6test, -sm6test),
 *   the crash line and the process settings of the program,
 *   the decoder code of the test player itself (testing/player_common, -playersmoke; player_smoke.cpp).
 *
 * The test player uses the same code, so the two programs read the same streams and write the same lines
 * and files. src/shim.rs declares these functions for Rust.
 */
#ifndef RAV1G_DEC_SHIM_H_
#define RAV1G_DEC_SHIM_H_

#include <stddef.h>
#include <stdint.h>

#include "dx/av1_perf.h"

#ifdef __cplusplus
extern "C" {
#endif

/* One line of text, without a line end. */
typedef void (*rd_line_fn)(void *ctx, const char *line);

/* No error dialogs, no abort message box, unbuffered stdout, and the crash filter: an access violation or a
 * failed assert of the library ends the process with a "CRASH exception ..." line on stdout. */
void rd_process_init(void);

/* The log priority of testing/common/log: errors only, or everything from LOG_INFO with |verbose|. */
void rd_set_log_verbose(int verbose);

/* Stream reader. rd_reader_open: NULL when the file cannot be opened or its container is not IVF, WebM,
 * OBU or MP4. rd_reader_next: 1 and the next temporal unit, valid until the next call or the close; 0 at
 * the end of the stream. */
typedef struct rd_reader rd_reader;
rd_reader *rd_reader_open(const char *path);
int rd_reader_next(rd_reader *reader, const uint8_t **data, size_t *size);
void rd_reader_close(rd_reader *reader);

/* -gputime: the records of a task and their summary. The add functions may be called from any thread. */
typedef struct rd_perf rd_perf;
rd_perf *rd_perf_new(void);
void rd_perf_free(rd_perf *perf);
void rd_perf_add_frame(rd_perf *perf, const av1_perf_frame *frame);
void rd_perf_add_output(rd_perf *perf, double ms);
void rd_perf_add_decode_call(rd_perf *perf, double ms);
/* The summary lines, and with |progress| the per-frame lines behind them, without the "GPUTIME " prefix. */
void rd_perf_lines(rd_perf *perf, int progress, rd_line_fn line, void *ctx);

/* -dumpstages. rd_dump_parse_frames: "0,3,10-12" into at most |capacity| frame numbers; the count, or -1
 * when the text is not such a list of at most 256 frames. */
int rd_dump_parse_frames(const char *text, int *frames, int capacity);
typedef struct rd_dumps rd_dumps;
rd_dumps *rd_dumps_new(void);
void rd_dumps_free(rd_dumps *dumps);
/* Creates |dir| when it is not there and starts the files of the stream |name|. */
void rd_dumps_start(rd_dumps *dumps, const char *dir, const char *name);
/* The decoder was replaced for a new sequence of the stream. */
void rd_dumps_next_sequence(rd_dumps *dumps);
/* The callback for av1_set_stage_dump; its context is the rd_dumps. */
void rd_dumps_callback(void *dumps, const av1_stage_dump *dump);
/* After the drain: one "DUMPSTAGES frame ..." line per dump. */
void rd_dumps_take_lines(rd_dumps *dumps, rd_line_fn line, void *ctx);
int rd_dumps_count(rd_dumps *dumps);
uint64_t rd_dumps_bytes(rd_dumps *dumps);
/* The first write error as a line, when there was one. Returns 1 when there was. */
int rd_dumps_error(rd_dumps *dumps, rd_line_fn line, void *ctx);

/* The capability report ("CAPS ..." lines) of an ID3D12Device. */
void rd_sm6_log_caps(void *device, rd_line_fn line, void *ctx);
/* The Shader Model 6 test on an ID3D12Device and ID3D12CommandQueue ("SM6TEST ..." lines). 0 when every
 * test came to a result, 1 when the test itself broke. */
int rd_sm6_run_tests(void *device, void *queue, rd_line_fn line, void *ctx);

/* -playersmoke: the decoder code of the test player (testing/player_common) with the tasks of
 * <root>/args/args_av1.txt, on an ID3D12Device, through the functions of rav1g.h; its log goes to stdout.
 * 0 when the player's loop ended, 2 (and the process ends) when it did not within |timeout_s|. */
int rd_player_smoke(void *device, const char *root, int timeout_s);

#ifdef __cplusplus
}
#endif

#endif /* RAV1G_DEC_SHIM_H_ */
