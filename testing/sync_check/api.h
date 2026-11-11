/*
 * What sync_check.cpp sees of one shader. Every shader is compiled as its own translation unit
 * (shader_tu.cpp), in its own namespace, and registers itself.
 */
#ifndef SYNC_CHECK_API_H_
#define SYNC_CHECK_API_H_

#include <stddef.h>
#include <stdint.h>

enum { kRoBuffers = 5, kRwBuffers = 2 };
enum : size_t { kRoSize = 1 << 20, kRwSize = 4 << 20 };

struct Rng {
  uint64_t state;
  uint32_t u32() {
    state += 0x9e3779b97f4a7c15ull;
    uint64_t z = state;
    z = (z ^ (z >> 30)) * 0xbf58476d1ce4e5b9ull;
    z = (z ^ (z >> 27)) * 0x94d049bb133111ebull;
    return (uint32_t)((z ^ (z >> 31)) >> 16);
  }
  int below(int n) { return (int)(u32() % (uint32_t)n); }
};

/* The buffers the shaders are bound to: read-only input and what they write. */
struct World {
  uint8_t *ro[kRoBuffers];
  uint8_t *rw[kRwBuffers];
};

struct Dispatch {
  int groups_x, groups_y, groups_z;
};

struct ShaderApi {
  const char *name;
  int threads_x, threads_y, threads_z;
  int group_memory_arrays;
  void (*setup)(World *world, Rng *rng, Dispatch *dispatch); /* constants, bindings, input records */
  void (*scramble)(uint64_t *state);                         /* garbage into the group memory */
  void (*run)(const uint32_t *group, const uint32_t *local);
  ShaderApi *next;
};

int register_shader(ShaderApi *api);

#endif /* SYNC_CHECK_API_H_ */
