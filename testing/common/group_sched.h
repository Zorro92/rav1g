/*
 * A compute shader thread group on the host, for the shader checks (cdef_check, intra_check, warp_check,
 * sync_check): the threads of a group as fibers, and group memory that watches who reads what.
 *
 * What a GPU promises about the threads of a group is little: they all reach a group sync
 * (GroupMemoryBarrierWithGroupSync, AllMemoryBarrierWithGroupSync) before any of them goes on, and
 * that is all. Between two group syncs the threads run in any order and any interleaving. A plain
 * GroupMemoryBarrier() makes writes visible; it does not make any thread wait. Shaders written on
 * and for one GPU can come to depend on more, for example on the 64 threads of a group running in
 * lockstep as one wave, and then they are wrong everywhere else. So here:
 *
 *   - between two group syncs every thread runs alone, from the one to the other, and the order of
 *     the threads is drawn anew at every sync;
 *   - GroupMemoryBarrier() does nothing, and is counted (the checks reject shaders that call it);
 *   - a group sync that not all threads of the group reach is counted as an error;
 *   - group memory (GsArray) keeps, for every element, who wrote and who read it since the last
 *     group sync. A thread that reads what another thread wrote since then, or writes over what
 *     another thread read or wrote since then with another value, has a race: the result depends
 *     on the interleaving. Every race is recorded with the source line of the access. This does
 *     not depend on the order the threads were run in here.
 *
 * Use: the shader sources are compiled as C++ with 'groupshared T name[N][M];' turned into
 * 'static GsArray<T, (N), (M)> name;' (gs_groupshared.sed). One translation unit of the program
 * defines GROUP_SCHED_IMPLEMENTATION before including this file.
 */
#ifndef TESTING_COMMON_GROUP_SCHED_H_
#define TESTING_COMMON_GROUP_SCHED_H_

#include <stddef.h>
#include <stdint.h>
#include <string.h>

#include <type_traits>
#include <utility>

// ---------------------------------------------------------------------------
// The record of the run
// ---------------------------------------------------------------------------

enum { kGsMaxRaceSites = 64 };

enum GsRaceKind {
  GsReadAfterWrite,   // a thread reads what another thread wrote since the last group sync
  GsWriteAfterRead,   // a thread changes what another thread read since the last group sync
  GsWriteAfterWrite,  // two threads write different values to one element between two group syncs
};

struct GsRaceSite {
  const char *shader;  // gs_state.shader when the race was seen
  int line;            // source line of the second access
  int other_line;      // source line of the first access
  int kind;
  long count;
};

struct GsState {
  uint32_t epoch = 1;   // goes up at every group sync and at every new group
  uint32_t thread = 0;  // the running thread + 1; 0 outside a group
  const char *shader = "";
  long races = 0;
  long sync_errors = 0;     // group syncs that not all threads of the group reached
  long plain_barriers = 0;  // calls of GroupMemoryBarrier()
  long out_of_bounds = 0;   // group memory accesses outside the array
  long group_syncs = 0;
  long runaway_groups = 0;  // groups that were ended here because they would not end, see below
  uint32_t turn_accesses = 0;  // group memory accesses of the running thread since it was last resumed
  int site_count = 0;
  GsRaceSite sites[kGsMaxRaceSites];
};
extern GsState gs_state;

void gs_record_race(int kind, int line, int other_line);
// Prints the races recorded so far, one line per site, with the given indent.
void gs_print_races(const char *indent);
void gs_reset_report();

// A thread group that does not end is ended: a shader with a race can take its loop bounds from garbage
// (a mutant of the checks does), and then it would spin for hours. A group is a runaway when it makes
// more group syncs than any shader of the decoder comes near, or when one thread makes more group memory
// accesses between two group syncs than any does. It is counted in gs_state.runaway_groups, its threads
// are dropped where they are, and the checks fail on it. (A thread that loops without touching group
// memory cannot be caught from here; the scripts run the checks under a timeout for that.)
enum { kGsMaxGroupSyncs = 20000, kGsMaxTurnAccesses = 20000000 };
void gs_runaway_thread();

// Runs threads 0 .. threads - 1 of one thread group to the end. 'order' is the state of the generator
// that draws the order of the threads.
void gs_run_group(int threads, void (*body)(int thread, void *context), void *context, uint64_t *order);
// A group sync, called from a thread of the running group.
void gs_group_sync();

// ---------------------------------------------------------------------------
// Group memory
// ---------------------------------------------------------------------------

struct GsTag {
  uint32_t w_epoch = 0, w_thread = 0;
  uint32_t r_epoch = 0, r_thread = 0;  // r_thread ~0u: several threads
  int w_line = 0, r_line = 0;
};

inline void gs_note_read(GsTag *tag, int line) {
  GsState &s = gs_state;
  if (!s.thread) return;
  if (++s.turn_accesses > kGsMaxTurnAccesses) gs_runaway_thread();
  if (tag->w_epoch == s.epoch && tag->w_thread != s.thread) gs_record_race(GsReadAfterWrite, line, tag->w_line);
  if (tag->r_epoch != s.epoch) {
    tag->r_epoch = s.epoch;
    tag->r_thread = s.thread;
    tag->r_line = line;
  } else if (tag->r_thread != s.thread) {
    tag->r_thread = ~0u;
  }
}

inline void gs_note_write(GsTag *tag, int line, bool changes) {
  GsState &s = gs_state;
  if (!s.thread) return;
  if (++s.turn_accesses > kGsMaxTurnAccesses) gs_runaway_thread();
  if (changes) {
    if (tag->r_epoch == s.epoch && tag->r_thread != s.thread) gs_record_race(GsWriteAfterRead, line, tag->r_line);
    if (tag->w_epoch == s.epoch && tag->w_thread != s.thread) gs_record_race(GsWriteAfterWrite, line, tag->w_line);
  }
  tag->w_epoch = s.epoch;
  tag->w_thread = s.thread;
  tag->w_line = line;
}

// An array index that remembers the source line it was written on.
struct GsIdx {
  long i;
  int line;
  template <class I>
  GsIdx(const I &index, int at = __builtin_LINE()) : i((long)index), line(at) {}
};

// A reference to one element of group memory: a scalar.
template <class T>
struct GsScalarRef {
  T *p;
  GsTag *tag;
  int line;
  GsScalarRef(T *p_, GsTag *tag_, int line_) : p(p_), tag(tag_), line(line_) {}
  GsScalarRef(const GsScalarRef &) = default;
  operator T() const {
    gs_note_read(tag, line);
    return *p;
  }
  const GsScalarRef &operator=(T v) const {
    gs_note_write(tag, line, *p != v);
    *p = v;
    return *this;
  }
  const GsScalarRef &operator=(const GsScalarRef &o) const { return *this = (T)o; }
  const GsScalarRef &operator+=(T v) const { return *this = (T)(T(*this) + v); }
  const GsScalarRef &operator-=(T v) const { return *this = (T)(T(*this) - v); }
  const GsScalarRef &operator|=(T v) const { return *this = (T)(T(*this) | v); }
  const GsScalarRef &operator&=(T v) const { return *this = (T)(T(*this) & v); }
};

template <class V, class = void>
struct GsHasY : std::false_type {};
template <class V>
struct GsHasY<V, std::void_t<decltype(std::declval<V &>().y)>> : std::true_type {};
template <class V, class = void>
struct GsHasW : std::false_type {};
template <class V>
struct GsHasW<V, std::void_t<decltype(std::declval<V &>().w)>> : std::true_type {};

// A reference to one element of group memory: a vector of two or four components. The element is
// watched as a whole.
template <class V>
struct GsVec2Ref {
  typedef typename std::remove_reference<decltype(std::declval<V &>().x)>::type C;
  V *p;
  GsTag *tag;
  int line;
  GsScalarRef<C> x, y;
  GsVec2Ref(V *p_, GsTag *tag_, int line_)
      : p(p_), tag(tag_), line(line_), x(&p_->x, tag_, line_), y(&p_->y, tag_, line_) {}
  GsVec2Ref(const GsVec2Ref &o) : GsVec2Ref(o.p, o.tag, o.line) {}
  operator V() const {
    gs_note_read(tag, line);
    return *p;
  }
  // To a vector of another component type, as HLSL converts.
  template <class U, class = typename std::enable_if<std::is_constructible<U, V>::value && !std::is_same<U, V>::value &&
                                                     !std::is_arithmetic<U>::value>::type>
  operator U() const {
    gs_note_read(tag, line);
    return U(*p);
  }
  const GsVec2Ref &operator=(const V &v) const {
    gs_note_write(tag, line, p->x != v.x || p->y != v.y);
    *p = v;
    return *this;
  }
  const GsVec2Ref &operator=(const GsVec2Ref &o) const { return *this = (V)o; }
};

template <class V>
struct GsVec4Ref {
  typedef typename std::remove_reference<decltype(std::declval<V &>().x)>::type C;
  V *p;
  GsTag *tag;
  int line;
  GsScalarRef<C> x, y, z, w;
  GsVec4Ref(V *p_, GsTag *tag_, int line_)
      : p(p_),
        tag(tag_),
        line(line_),
        x(&p_->x, tag_, line_),
        y(&p_->y, tag_, line_),
        z(&p_->z, tag_, line_),
        w(&p_->w, tag_, line_) {}
  GsVec4Ref(const GsVec4Ref &o) : GsVec4Ref(o.p, o.tag, o.line) {}
  operator V() const {
    gs_note_read(tag, line);
    return *p;
  }
  // To a vector of another component type, as HLSL converts.
  template <class U, class = typename std::enable_if<std::is_constructible<U, V>::value && !std::is_same<U, V>::value &&
                                                     !std::is_arithmetic<U>::value>::type>
  operator U() const {
    gs_note_read(tag, line);
    return U(*p);
  }
  const GsVec4Ref &operator=(const V &v) const {
    gs_note_write(tag, line, p->x != v.x || p->y != v.y || p->z != v.z || p->w != v.w);
    *p = v;
    return *this;
  }
  const GsVec4Ref &operator=(const GsVec4Ref &o) const { return *this = (V)o; }
};

// The reference type of an element type, by its number of components. A shim with vector types of its
// own can specialise GsRefOf for them.
template <class T, class = void>
struct GsRefKind {
  enum { value = 1 };
};
template <class V>
struct GsRefKind<V, typename std::enable_if<GsHasY<V>::value && !GsHasW<V>::value>::type> {
  enum { value = 2 };
};
template <class V>
struct GsRefKind<V, typename std::enable_if<GsHasW<V>::value>::type> {
  enum { value = 4 };
};
template <class T, int Kind = GsRefKind<T>::value>
struct GsRefOf;
template <class T>
struct GsRefOf<T, 1> {
  typedef GsScalarRef<T> type;
};
template <class V>
struct GsRefOf<V, 2> {
  typedef GsVec2Ref<V> type;
};
template <class V>
struct GsRefOf<V, 4> {
  typedef GsVec4Ref<V> type;
};

template <size_t... Dims>
struct GsProduct;
template <>
struct GsProduct<> {
  enum : size_t { value = 1 };
};
template <size_t N, size_t... Rest>
struct GsProduct<N, Rest...> {
  enum : size_t { value = N * GsProduct<Rest...>::value };
};

// A view of group memory with the dimensions that are left to index.
template <class T, size_t... Dims>
struct GsView;
template <class T, size_t N>
struct GsView<T, N> {
  T *data;
  GsTag *tags;
  typename GsRefOf<T>::type operator[](GsIdx index) const {
    static T spare_value;
    static GsTag spare_tag;
    if (index.i < 0 || (size_t)index.i >= N) {
      gs_state.out_of_bounds++;
      spare_tag = GsTag();
      spare_value = T();
      return typename GsRefOf<T>::type(&spare_value, &spare_tag, index.line);
    }
    return typename GsRefOf<T>::type(data + index.i, tags + index.i, index.line);
  }
};
template <class T, size_t N, size_t M, size_t... Rest>
struct GsView<T, N, M, Rest...> {
  T *data;
  GsTag *tags;
  GsView<T, M, Rest...> operator[](GsIdx index) const {
    long i = index.i;
    if (i < 0 || (size_t)i >= N) {
      gs_state.out_of_bounds++;
      i = 0;
    }
    const size_t step = GsProduct<M, Rest...>::value;
    return GsView<T, M, Rest...>{ data + i * step, tags + i * step };
  }
};

// 'groupshared T name[N][M];' is 'static GsArray<T, (N), (M)> name;'.
template <class T, size_t... Dims>
struct GsArray {
  enum : size_t { kCount = GsProduct<Dims...>::value };
  T data[kCount];
  GsTag tags[kCount];
  auto operator[](GsIdx index) -> decltype((GsView<T, Dims...>{ data, tags })[index]) {
    return (GsView<T, Dims...>{ data, tags })[index];
  }
  // For filling the memory with garbage before a group runs: what a group finds there is not defined.
  void *bytes() { return data; }
  size_t byte_size() const { return sizeof(data); }
};

// ---------------------------------------------------------------------------
#ifdef GROUP_SCHED_IMPLEMENTATION

#include <limits.h>
#include <stdio.h>
#include <stdlib.h>

GsState gs_state;

void gs_record_race(int kind, int line, int other_line) {
  GsState &s = gs_state;
  s.races++;
  for (int i = 0; i < s.site_count; ++i) {
    GsRaceSite &site = s.sites[i];
    if (site.line == line && site.kind == kind && site.other_line == other_line && !strcmp(site.shader, s.shader)) {
      site.count++;
      return;
    }
  }
  if (s.site_count == kGsMaxRaceSites) return;
  GsRaceSite &site = s.sites[s.site_count++];
  site.shader = s.shader;
  site.line = line;
  site.other_line = other_line;
  site.kind = kind;
  site.count = 1;
}

void gs_print_races(const char *indent) {
  static const char *const what[3] = {
    "reads group memory that another thread wrote at line %d since the last group sync",
    "changes group memory that another thread read at line %d since the last group sync",
    "writes another value over what another thread wrote at line %d since the last group sync",
  };
  for (int i = 0; i < gs_state.site_count; ++i) {
    const GsRaceSite &site = gs_state.sites[i];
    printf("%s%s:%d ", indent, site.shader, site.line);
    printf(what[site.kind], site.other_line);
    printf(" (%ld times)\n", site.count);
  }
  if (gs_state.site_count == kGsMaxRaceSites) printf("%s(more sites not listed)\n", indent);
}

void gs_reset_report() {
  GsState &s = gs_state;
  s.races = s.sync_errors = s.plain_barriers = s.out_of_bounds = s.group_syncs = s.runaway_groups = 0;
  s.site_count = 0;
}

namespace gs_fibers {

enum { kMaxThreads = 1024, kStackSize = 256 * 1024 };
enum { StRun, StAtSync, StDone, StRunaway };

static void entry();

#if defined(__x86_64__) && !defined(GROUP_SCHED_UCONTEXT)
// Saves the callee-saved registers on the current stack and continues on another one. swapcontext() does
// the same with two system calls per switch, which would be most of the run time of the checks.
extern "C" void gs_switch_stack(void **save_sp, void *load_sp);
__asm__(".text\n"
        ".globl gs_switch_stack\n"
        ".type gs_switch_stack,@function\n"
        "gs_switch_stack:\n"
        "  pushq %rbp\n"
        "  pushq %rbx\n"
        "  pushq %r12\n"
        "  pushq %r13\n"
        "  pushq %r14\n"
        "  pushq %r15\n"
        "  movq %rsp, (%rdi)\n"
        "  movq %rsi, %rsp\n"
        "  popq %r15\n"
        "  popq %r14\n"
        "  popq %r13\n"
        "  popq %r12\n"
        "  popq %rbx\n"
        "  popq %rbp\n"
        "  ret\n"
        ".size gs_switch_stack,.-gs_switch_stack\n");

struct Fiber {
  void *sp;
  int state;
};
static Fiber fibers[kMaxThreads];
static char *stacks;
static void *scheduler_sp;
static int current;

static void prepare(int t) {
  uintptr_t *top = (uintptr_t *)(((uintptr_t)(stacks + (size_t)(t + 1) * kStackSize)) & ~(uintptr_t)15);
  *--top = 0;                  // keeps the stack of entry() aligned
  *--top = (uintptr_t)&entry;  // where the first switch returns to
  for (int i = 0; i < 6; ++i) *--top = 0;
  fibers[t].sp = top;
  fibers[t].state = StRun;
}
static void resume(int t) {
  current = t;
  gs_switch_stack(&scheduler_sp, fibers[t].sp);
}
static void suspend() { gs_switch_stack(&fibers[current].sp, scheduler_sp); }
#else
}  // namespace gs_fibers
#include <ucontext.h>
namespace gs_fibers {
struct Fiber {
  ucontext_t context;
  int state;
};
static Fiber fibers[kMaxThreads];
static char *stacks;
static ucontext_t scheduler_context;
static int current;

static void prepare(int t) {
  getcontext(&fibers[t].context);
  fibers[t].context.uc_stack.ss_sp = stacks + (size_t)t * kStackSize;
  fibers[t].context.uc_stack.ss_size = kStackSize;
  fibers[t].context.uc_link = nullptr;
  makecontext(&fibers[t].context, entry, 0);
  fibers[t].state = StRun;
}
static void resume(int t) {
  current = t;
  swapcontext(&scheduler_context, &fibers[t].context);
}
static void suspend() { swapcontext(&fibers[current].context, &scheduler_context); }
#endif

static void (*body)(int thread, void *context);
static void *body_context;

__attribute__((noinline, used)) static void entry() {
  body(current, body_context);
  fibers[current].state = StDone;
  suspend();
  abort();
}

}  // namespace gs_fibers

void gs_group_sync() {
  gs_fibers::fibers[gs_fibers::current].state = gs_fibers::StAtSync;
  gs_fibers::suspend();
}

// The running thread has made more group memory accesses in one turn than any shader does: it is not
// resumed again, and the scheduler ends its group.
void gs_runaway_thread() {
  gs_fibers::fibers[gs_fibers::current].state = gs_fibers::StRunaway;
  gs_fibers::suspend();
  abort();
}

void gs_run_group(int threads, void (*body)(int thread, void *context), void *context, uint64_t *order) {
  using namespace gs_fibers;
  if (threads > kMaxThreads) abort();
  if (!stacks) {
    stacks = (char *)malloc((size_t)kMaxThreads * kStackSize);
    if (!stacks) abort();
  }
  gs_fibers::body = body;
  body_context = context;
  gs_state.epoch++;
  for (int t = 0; t < threads; ++t) prepare(t);
  static int turn[kMaxThreads];
  long syncs = 0;
  for (;;) {
    // From one group sync to the next: every thread alone, in an order drawn anew.
    for (int t = 0; t < threads; ++t) turn[t] = t;
    for (int t = threads - 1; t > 0; --t) {
      *order = *order * 6364136223846793005ull + 1442695040888963407ull;
      const int other = (int)((*order >> 33) % (uint64_t)(t + 1));
      const int swap = turn[t];
      turn[t] = turn[other];
      turn[other] = swap;
    }
    int ran = 0;
    for (int n = 0; n < threads; ++n) {
      const int t = turn[n];
      if (fibers[t].state != StRun) continue;
      gs_state.thread = (uint32_t)t + 1;
      gs_state.turn_accesses = 0;
      resume(t);
      ran = 1;
    }
    gs_state.thread = 0;
    if (!ran) break;
    int at_sync = 0;
    int done = 0;
    int runaway = 0;
    for (int t = 0; t < threads; ++t) {
      at_sync += fibers[t].state == StAtSync;
      done += fibers[t].state == StDone;
      runaway += fibers[t].state == StRunaway;
    }
    if (runaway || ++syncs > kGsMaxGroupSyncs) {
      // The group does not end: its threads are dropped where they are (their stacks are reused).
      gs_state.runaway_groups++;
      break;
    }
    if (!at_sync) break;
    // A group sync that some threads never reach: on a GPU the group would hang or the sync would not
    // hold. It is counted, and the threads that wait go on.
    if (done) gs_state.sync_errors++;
    gs_state.group_syncs++;
    gs_state.epoch++;
    for (int t = 0; t < threads; ++t) {
      if (fibers[t].state == StAtSync) fibers[t].state = StRun;
    }
  }
  gs_state.epoch++;
}

#endif  // GROUP_SCHED_IMPLEMENTATION

#endif  // TESTING_COMMON_GROUP_SCHED_H_
