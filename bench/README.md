# qorral-bench

A GPU benchmark that runs the **same four scenes in Vulkan, OpenGL and Direct3D 9, 10, 11 and
12**, on Windows and Linux. It compares graphics APIs, drivers, hosts and virtual machines
(virtio-gpu with Venus or virgl, and Qorral's Windows guest driver) like for like.

All content is procedural: there are no asset files. Shaders are written once in GLSL and compiled at
build time for every API (see "How every API does the same work").

## Scenes

Each scene runs for a warmup period (not measured), then for a measured period. Animation is driven
by the time since the scene started, not by the frame count, so the same moment looks the same on
every machine and in every API (see `--capture`).

| Scene | Content | Stresses |
|---|---|---|
| `raymarch` | Full-screen SDF raymarcher: rotating Menger sponge, twisted pillars repeated over the plane, metaball orbs fused with a ring, rolling terrain. Soft shadows (64 steps), 5-tap AO, one reflection bounce, fog, ACES. | ALU |
| `instanced` | 240,000 instances (icospheres with 320 triangles, tori with 576; about 107M triangles per frame), animated in the vertex shader, GGX lighting from a sun and 8 moving point lights, depth buffer, 4x MSAA (1x if unsupported). | vertex, raster |
| `particles` | 1,048,576 particles simulated on the GPU every frame (two octaves of curl noise, three moving attractors, respawns), drawn as additive camera-facing quads (6.3M vertices) into an RGBA16F target, then tonemapped. | compute, bandwidth |
| `postfx` | The instanced scene with 80,000 objects rendered to RGBA16F, then a bloom chain (13-tap downsample with soft threshold, 6 down and 5 up passes), ACES tonemapping and FXAA. | bandwidth, fill rate |

Per scene the benchmark reports frames, average FPS, average, median, p99 and p99.9 frame time
(CPU time between presented frames), 1% low FPS (average over the slowest 1% of frames), and the
median **GPU time** per frame from timestamp queries.

**Score** = round(10 × geometric mean of the scenes' average FPS), per API. Scores compare only for
the same scene set, resolution and present mode. GPU time is the steadier number for raw GPU
throughput; FPS is what a user sees.

## APIs

| `--api` | Version | Swapchain | Notes |
|---|---|---|---|
| `vulkan` | 1.2 core + `VK_KHR_swapchain` | immediate, mailbox or FIFO | |
| `opengl` | 4.3 core + clip control (4.5 or `ARB_clip_control`) | swap interval 0 or 1 | |
| `d3d12` | feature level 11_0+ | flip model; tearing for immediate | |
| `d3d11` | feature level 11_0+ | flip model; tearing for immediate | |
| `d3d10` | 10.1 (10_0 fallback), shader model 4.0 | flip model when DXGI accepts it, else blit model | particles: **variant** |
| `d3d9` | 9Ex, shader model 3.0 | FLIPEX | particles: **variant**; MSAA resolve on sRGB-encoded values |

`--api all` runs every API in this build, one after the other, each in its own window.

**Variants.** Direct3D 9 and 10 have no compute shaders. There the particle simulation runs in a
pixel shader on two 1024×1024 RGBA32F textures (position and velocity, ping-ponged), and the vertex
shader reads them. The simulation code is the same (`shaders/particles_common.glsl`); Direct3D 9 uses
a float hash for respawns because shader model 3.0 has no integer math. Runs with a variant say so
in the output and in the JSON (`"variant"`).

## How every API does the same work

- **One set of scenes** on a thin rendering layer (`src/gfx/gfx.h`): buffers, render targets,
  graphics and compute pipelines, passes, draws and dispatches. Each API has one backend under
  `src/gfx/`. Every API runs the same passes, draws, formats and sample counts.
- **One set of shaders** in `shaders/` (GLSL): `glslc` compiles SPIR-V for Vulkan, and SPIRV-Cross
  turns it into GLSL 4.3 for OpenGL and HLSL (shader model 5.0, 4.0 and 3.0) for Direct3D. HLSL is
  compiled when the program starts (`d3dcompiler_47.dll`, part of Windows). Three Direct3D 9 shaders
  are written by hand in `shaders/d3d9/`, where shader model 3.0 lacks a feature (vertex index,
  `texelFetch`, integer math).
- **Same conventions everywhere:** Vulkan clip space (y down, depth 0..1), row 0 at the top, two
  frames in flight, linear clamp sampling, sRGB backbuffers.
- **Checked by images:** `--offscreen --capture DIR` writes each scene's final frame. On a host GPU,
  raymarch, instanced and postfx are pixel-identical between Vulkan, Direct3D 10, 11 and 12, and
  nearly so for OpenGL and Direct3D 9 (MSAA resolve). Particles depend slightly on the frame timing.

## Latency

`--scene latency` (not part of `all` or the score) shows a black screen that turns white for 250 ms
after every key or mouse-button press, and renders one frame at a time.

- **In the guest:** each frame waits until it is displayed, and the benchmark reports the time from
  the frame's start (`latency_ms` in the JSON, with its `source`):
  - `display`: the API reported when the frame was shown (DXGI frame statistics for D3D10, 11 and
    12, present statistics for D3D9Ex, `VK_KHR_present_wait` for Vulkan);
  - `gpu`: the API cannot tell, so this is the time until the GPU finished the frame (OpenGL,
    offscreen runs, and drivers without these features).
- **Input to display, from the host:** with the scene running in a VM, `qorral latency <vm>` presses
  a key through QEMU and times until the changed frame reaches QEMU's display (Qorral's QEMU
  `x-qorral-latency-probe`). It reports median, p90, min and max over `--samples` presses. Showing
  the frame in a host window comes on top of that number.

## Command line

```
qorral-bench [--api NAME|all] [--scene NAME|all] [--seconds N] [--warmup S] [--width W --height H]
             [--fullscreen] [--present immediate|mailbox|fifo] [--offscreen] [--gpu INDEX]
             [--json PATH] [--list-gpus] [--validate] [--no-fxaa] [--capture DIR]
```

| Option | Default | Meaning |
|---|---|---|
| `--api` | `vulkan` | `vulkan`, `opengl`, `d3d9`, `d3d10`, `d3d11`, `d3d12`, a comma list, or `all` |
| `--scene` | `all` | `raymarch`, `instanced`, `particles`, `postfx`, a comma list, or `all` |
| `--seconds` | 10 | measured seconds per scene |
| `--warmup` | 2 | warmup seconds per scene (not measured) |
| `--width`, `--height` | 1280x720 | window size in pixels |
| `--fullscreen` | off | borderless fullscreen at the desktop resolution |
| `--present` | `immediate` | requested present mode; falls back (immediate, mailbox, FIFO). The mode used is printed and stored in the JSON. |
| `--offscreen` | off | render into an image: no window and no present. For correctness checks and captures, not for frame rates a user would see. |
| `--gpu` | auto | device index from `--list-gpus` |
| `--json` | none | write the results as JSON |
| `--list-gpus` | | list the devices of the chosen API(s) and exit |
| `--validate` | off | the API's debug layer, when installed (Vulkan validation layer, D3D debug layers, GL debug output) |
| `--no-fxaa` | off | the postfx scene skips FXAA |
| `--capture DIR` | off | after each scene, render one extra frame at t = warmup + seconds and save it as `DIR/<api>-<scene>.bmp` |

Keys: **Esc** (or closing the window) aborts. Exit codes: `0` ok, `1` usage error, `2` API or
runtime error (the message names the call and its result), `3` aborted, `4` validation errors.

## JSON

One API: an object with `tool`, `version` and the run. Several APIs: `tool`, `version` and `runs`,
one object per API. Each run:

```json
{
  "api": "d3d11",
  "gpu": { "index": 0, "name": "Example GPU", "type": "discrete", "vendor_id": "0x1234",
           "device_id": "0x5678", "driver_name": "...", "driver_info": "...",
           "driver_version": "1.2.3.4", "driver_version_raw": 0, "api_version": "11_1" },
  "os": "Windows 10.0.26300",
  "resolution": { "width": 1280, "height": 720 },
  "fullscreen": false, "offscreen": false,
  "present_mode": "immediate", "present_mode_requested": "immediate",
  "msaa_samples": 4, "swapchain_format": "R8G8B8A8_UNORM_SRGB", "variant": null,
  "seconds_per_scene": 10.00, "warmup_seconds": 2.00,
  "gpu_timestamps": true, "validation": false, "sync_validation": false, "validation_errors": 0,
  "scenes": [
    { "name": "raymarch", "frames": 4190, "avg_fps": 419.00, "avg_ms": 2.387, "median_ms": 2.160,
      "p99_ms": 3.370, "p999_ms": 4.190, "low_1pct_fps": 270.50, "gpu_median_ms": 1.841, "gpu_samples": 4190 }
  ],
  "score": 4190,
  "aborted": false
}
```

A run that could not start has only `api` and `error`. `gpu_median_ms` is `null` without timestamps.

## Building

Needs CMake 3.20+, a C++17 compiler, SDL2, `glslc` (shaderc) or `glslangValidator`, and
`spirv-cross`. Vulkan needs the Vulkan headers and loader; OpenGL needs only SDL; Direct3D needs the
Windows SDK headers (MinGW-w64 has them). Options: `-DQB_VULKAN=OFF`, `-DQB_OPENGL=OFF`,
`-DQB_D3D=OFF` (Windows only).

### Windows (MSYS2 UCRT64)

```sh
pacman -S mingw-w64-ucrt-x86_64-{cmake,ninja,gcc,SDL2,vulkan-headers,vulkan-loader,shaderc,spirv-cross}
cmake -S bench -B <build> -G Ninja -DCMAKE_BUILD_TYPE=Release
ninja -C <build>
```

The result is `<build>/qorral-bench.exe` plus `SDL2.dll`. libstdc++, libgcc and winpthread are linked
statically. At run time it uses the system's `vulkan-1.dll`, `opengl32.dll`, `d3d*.dll` and
`d3dcompiler_47.dll`.

### Linux (Arch)

```sh
sudo pacman -S cmake ninja gcc sdl2 vulkan-headers vulkan-icd-loader shaderc spirv-cross
cmake -S bench -B build -G Ninja -DCMAKE_BUILD_TYPE=Release
ninja -C build
```

You also need a Vulkan and an OpenGL driver; in a VM with virtio-gpu, Venus (`vulkan-virtio`) and
virgl (Mesa).

## Tips for comparable runs

- Same resolution, present mode and scene set on every machine and API. `immediate` measures
  throughput; `fifo` is capped by the refresh rate.
- Close other GPU load: anything else drawing on the same GPU shows up as frame-time spikes, while
  GPU time stays steady.
- Overlay layers (screen recorders, game launchers) hook every API. For Vulkan,
  `VK_LOADER_LAYERS_DISABLE='~implicit~'` keeps them out.
- Compare `--list-gpus` first in VMs, and use `--gpu` to make the choice explicit.

## Files

- `src/main.cpp`: command line, scene loop, statistics, table and JSON output.
- `src/gfx/gfx.h`: the rendering layer; `src/gfx/{vulkan,opengl,d3d9,d3d10,d3d11,d3d12}.cpp`: the
  backends; `src/gfx/d3d_common.*`: shared Direct3D code (adapter choice, HLSL compile).
- `src/scenes/`: the four scenes and the shared instanced-mesh renderer.
- `shaders/`: GLSL sources and includes; `shaders/d3d9/`: hand-written shader model 3.0 versions.
- `cmake/`: embedding SPIR-V and generated shader text into the binary.
