/*
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
 * Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
 * file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
 * License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
 * www.aomedia.org/license/patent.
 */

// rav1g-dec -playersmoke <directory>: the decoder code of the test player itself (testing/player_common:
// the task file, the decoder creation, the output pool, the md5 check), run as the player runs it, through
// rav1g.h on the Rust library of this program, on the device of this program. The player's own program is
// a UWP app and cannot run in CI; this is the part of it that uses the decoder, compiled from the same
// files and calling the same functions of rav1g.h as in the packages. The tasks are the lines of
// <directory>/args/args_av1.txt, as in the player's LocalState, and the player's log goes to stdout.

#include <chrono>
#include <condition_variable>
#include <mutex>

#include "player_common/decoder_creator.h"
#include "player_common/decoder.h"

#include "shim.h"

namespace {

// The player's window, as far as the decoders talk to it: nothing is shown (the tasks have -nopresent).
class SmokeDisplay : public DisplayCallbacks {
 public:
  virtual void AddSource(AV1Decoder* source) { (void)source; }
  virtual void RemoveSource(AV1Decoder* source) { (void)source; }
  virtual void AllJobDoneNotify() {
    std::lock_guard<std::mutex> lock(cs_);
    done_ = true;
    cv_.notify_all();
  }
  bool Wait(int seconds) {
    std::unique_lock<std::mutex> lock(cs_);
    return cv_.wait_for(lock, std::chrono::seconds(seconds), [this] { return done_; });
  }

 private:
  std::mutex cs_;
  std::condition_variable cv_;
  bool done_ = false;
};

}  // namespace

// Returns 0 when the player's loop ended (what its tasks did is in its log), 2 when it did not end
// within |timeout_s|: a launch that hangs.
extern "C" int rd_player_smoke(void* device, const char* root, int timeout_s) {
  logging::set_log_priority(logging::LOG_INFO);
  SmokeDisplay display;
  // As PlayerWindow::Run: the player gives the decoder its device and no queue.
  d3d_resources resources(static_cast<ID3D12Device*>(device), 0);
  Decoders decoders(0, resources, &display, root);
  decoders.Start();
  if (!display.Wait(timeout_s)) {
    printf("PLAYERSMOKE the player did not end its launch within %d s\n", timeout_s);
    fflush(stdout);
    // Its threads may hang: no destructors, no exit handlers.
    TerminateProcess(GetCurrentProcess(), 2);
    return 2;
  }
  decoders.Stop(taskStop);
  printf("PLAYERSMOKE the player ended its launch\n");
  fflush(stdout);
  return 0;
}
