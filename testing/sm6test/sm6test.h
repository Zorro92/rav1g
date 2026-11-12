/*
 * Shader Model 6 test (docs/test-player.md, "Shader Model 6 test"), for the test player and warp_decoder.
 *
 * The decoder's shaders are FXC shaders of Shader Model 5.0. Whether they could become DXC shaders of
 * Shader Model 6 depends on what the device of the console takes: DXIL at all, the wave intrinsics, native
 * 16-bit types, a wave size the shader asks for. Two things answer that:
 *
 *   Sm6LogCaps   what the device reports about itself (lines "CAPS ..."), at every launch
 *   Sm6RunTests  the test shaders of testing/sm6test/shaders, compiled with DXC, each run once on known
 *                input and compared with what the host computes (lines "SM6TEST ..."), with -sm6test
 *
 * Nothing here touches the decoder: the test has its own root signature, pipeline states, buffers and
 * command list, and its shaders are not in the decoder's shader set.
 */

#pragma once

struct ID3D12Device;
struct ID3D12CommandQueue;

// One line of the report, without a line end.
typedef void (*Sm6LogFn)(void* ctx, const char* line);

// The capability report: adapter, driver, shader model, wave operations, native 16-bit types and what else
// matters to compute shaders. A query the device refuses is reported with its HRESULT.
void Sm6LogCaps(ID3D12Device* device, Sm6LogFn log, void* ctx);

// Runs every test shader: root signature and pipeline state (a refused pipeline state is a result, the
// run goes on), one dispatch on known input, readback, comparison. One line per test,
//   SM6TEST <name>: PASS|FAIL|PSO FAILED hr=0x... | <details>
// and a summary line. |queue| may be NULL: the test then creates a queue of its own.
// Returns 0 when every test came to a result (PASS, FAIL or PSO FAILED), 1 when the test itself broke:
// the shaders are not built in, a buffer or the command list could not be created, a dispatch did not
// finish, the device was removed.
int Sm6RunTests(ID3D12Device* device, ID3D12CommandQueue* queue, Sm6LogFn log, void* ctx);

// 1 when the test shaders are compiled into this program (the build was given /p:Rav1gSm6TestShaders).
int Sm6TestShadersBuiltIn(void);
