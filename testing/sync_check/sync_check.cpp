/*
 * Host check of how the compute shaders hand data between the threads of a thread group: every
 * shader of the decoder that has group memory, compiled as C++ (to_cpp.py), run on generated input
 * with its thread groups emulated by testing/common/group_sched.h.
 *
 * A shader passes when
 *   - it has no race in its group memory: no thread reads what another thread wrote since the last
 *     group sync, and none overwrites what another read or wrote since then with another value;
 *   - all threads of a group reach every group sync, and every group ends;
 *   - it does not call GroupMemoryBarrier(), which makes no thread wait (--allow-plain-barrier
 *     drops this, for the mutants of run.sh: their races must be found as races);
 *   - what it writes to its buffers does not depend on the order in which the threads of a group
 *     run between two group syncs: every case is run twice, with two orders.
 * The input is random but shaped (shader_tu.cpp); it has no reference output. Whether a shader
 * computes the right picture is the business of the other checks and of the conformance run.
 */
#define GROUP_SCHED_IMPLEMENTATION
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include <vector>

#include "../common/group_sched.h"
#include "api.h"

long g_out_of_bounds = 0;

static ShaderApi *g_shaders = nullptr;
int register_shader(ShaderApi *api) {
  api->next = g_shaders;
  g_shaders = api;
  return 0;
}

struct ThreadContext {
  const ShaderApi *api;
  uint32_t group[3];
};

static void thread_body(int thread, void *context) {
  const ThreadContext *c = (const ThreadContext *)context;
  const uint32_t local[3] = { (uint32_t)(thread % c->api->threads_x),
                              (uint32_t)(thread / c->api->threads_x % c->api->threads_y),
                              (uint32_t)(thread / (c->api->threads_x * c->api->threads_y)) };
  c->api->run(c->group, local);
}

static void dispatch(const ShaderApi *api, const Dispatch &d, uint64_t garbage, uint64_t order) {
  const int threads = api->threads_x * api->threads_y * api->threads_z;
  for (int z = 0; z < d.groups_z; ++z) {
    for (int y = 0; y < d.groups_y; ++y) {
      for (int x = 0; x < d.groups_x; ++x) {
        ThreadContext context = { api, { (uint32_t)x, (uint32_t)y, (uint32_t)z } };
        api->scramble(&garbage);
        gs_run_group(threads, thread_body, &context, &order);
      }
    }
  }
}

int main(int argc, char **argv) {
  int cases = 60;
  uint64_t seed = 20261111;
  int allow_plain = 0;
  const char *only = nullptr;
  for (int i = 1; i < argc; ++i) {
    if (!strcmp(argv[i], "--cases") && i + 1 < argc) {
      cases = atoi(argv[++i]);
    } else if (!strcmp(argv[i], "--seed") && i + 1 < argc) {
      seed = strtoull(argv[++i], nullptr, 10);
    } else if (!strcmp(argv[i], "--only") && i + 1 < argc) {
      only = argv[++i];
    } else if (!strcmp(argv[i], "--allow-plain-barrier")) {
      allow_plain = 1;
    } else {
      fprintf(stderr, "usage: sync_check [--cases N] [--seed S] [--only shader] [--allow-plain-barrier]\n");
      return 2;
    }
  }

  // The shaders in name order.
  std::vector<ShaderApi *> shaders;
  for (ShaderApi *api = g_shaders; api; api = api->next) shaders.push_back(api);
  for (size_t i = 0; i < shaders.size(); ++i) {
    for (size_t k = i + 1; k < shaders.size(); ++k) {
      if (strcmp(shaders[k]->name, shaders[i]->name) < 0) std::swap(shaders[i], shaders[k]);
    }
  }

  World world;
  std::vector<uint8_t> ro[kRoBuffers], rw[kRwBuffers], start[kRwBuffers], first[kRwBuffers];
  for (int i = 0; i < kRoBuffers; ++i) {
    ro[i].resize(kRoSize);
    world.ro[i] = ro[i].data();
  }
  for (int i = 0; i < kRwBuffers; ++i) {
    rw[i].resize(kRwSize);
    world.rw[i] = rw[i].data();
  }

  int failed = 0;
  int checked = 0;
  long total_groups = 0;
  for (ShaderApi *api : shaders) {
    if (only && strcmp(only, api->name)) continue;
    ++checked;
    gs_reset_report();
    gs_state.shader = api->name;
    g_out_of_bounds = 0;
    Rng rng = { seed };
    long groups = 0;
    long order_dependent = 0;
    for (int n = 0; n < cases; ++n) {
      for (int i = 0; i < kRoBuffers; ++i) {
        for (size_t k = 0; k < kRoSize; k += 4) {
          const uint32_t v = rng.u32();
          memcpy(&ro[i][k], &v, 4);
        }
      }
      for (int i = 0; i < kRwBuffers; ++i) {
        for (size_t k = 0; k < kRwSize; k += 4) {
          const uint32_t v = rng.u32();
          memcpy(&rw[i][k], &v, 4);
        }
      }
      Dispatch d = { 1, 1, 1 };
      api->setup(&world, &rng, &d);
      for (int i = 0; i < kRwBuffers; ++i) start[i] = rw[i];
      const uint64_t garbage = rng.u32();
      // Twice from the same start, with the same garbage in the group memory and two thread orders.
      dispatch(api, d, garbage, 2 * (uint64_t)n + 1);
      for (int i = 0; i < kRwBuffers; ++i) {
        first[i] = rw[i];
        rw[i] = start[i];
      }
      dispatch(api, d, garbage, 0x9e3779b97f4a7c15ull * (uint64_t)(n + 1));
      for (int i = 0; i < kRwBuffers; ++i) order_dependent += first[i] != rw[i];
      groups += 2L * d.groups_x * d.groups_y * d.groups_z;
      if (gs_state.runaway_groups) break;  // its loop bounds are garbage: no point in more cases
    }
    total_groups += groups;
    const long plain = allow_plain ? 0 : gs_state.plain_barriers;
    const int bad = gs_state.races || gs_state.sync_errors || plain || order_dependent || gs_state.runaway_groups;
    printf("%-38s %s  %4d threads, %d group memory array(s), %ld groups, %ld group syncs", api->name, bad ? "FAIL" : "ok  ",
           api->threads_x * api->threads_y * api->threads_z, api->group_memory_arrays, groups, gs_state.group_syncs);
    if (gs_state.plain_barriers) printf(", %ld plain barriers", gs_state.plain_barriers);
    printf("\n");
    if (bad) {
      ++failed;
      if (plain) printf("    calls GroupMemoryBarrier(), which makes no thread wait\n");
      if (gs_state.runaway_groups)
        printf("    %ld thread groups did not end and were ended (more than %d group syncs, or more than %d group memory "
               "accesses of a thread between two)\n",
               gs_state.runaway_groups, (int)kGsMaxGroupSyncs, (int)kGsMaxTurnAccesses);
      if (gs_state.sync_errors) printf("    %ld group syncs were not reached by all threads of the group\n", gs_state.sync_errors);
      if (order_dependent) printf("    the output depends on the order of the threads in %ld runs\n", order_dependent);
      if (gs_state.races) printf("    %ld races in group memory:\n", gs_state.races);
      gs_print_races("      ");
    }
  }
  if (!checked) {
    printf("sync_check: no shader to check\n");
    return 2;
  }
  if (failed) {
    printf("sync_check: FAILED, %d of %d shaders\n", failed, checked);
    return 1;
  }
  printf("sync_check: %d shaders, %ld thread groups: no race in group memory, every group sync reached by all threads,\n"
         "no plain group memory barrier, output independent of the thread order\n",
         checked, total_groups);
  return 0;
}
