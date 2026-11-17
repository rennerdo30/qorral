// A thin rendering layer with one backend per graphics API, so every scene does exactly the same
// work in every API. It covers only what the scenes need: buffers, render targets, graphics and
// compute pipelines, passes, draws, dispatches, and a frame loop with GPU timing and capture.
//
// Conventions (all backends): Vulkan clip space (y down, depth 0..1), row 0 of a render target is
// its top row, counter-clockwise triangles are front faces, set 0 bindings 0..3 per pipeline, plus
// one small block of per-draw constants ("Push" in the shaders).
#pragma once

#include <cstddef>
#include <cstdint>
#include <functional>
#include <memory>
#include <stdexcept>
#include <string>
#include <vector>

struct SDL_Window;

namespace gfx {

constexpr uint32_t kFramesInFlight = 2;
constexpr uint32_t kMaxBindings = 4;

enum class Api { Vulkan, OpenGL, D3D9, D3D10, D3D11, D3D12 };
const char* apiName(Api a);                       // "vulkan", "opengl", "d3d9", ...
bool parseApi(const std::string& s, Api& out);    // accepts the names above
std::vector<Api> availableApis();                 // compiled into this binary, in display order
uint32_t sdlWindowFlags(Api a);                   // SDL_WINDOW_VULKAN / SDL_WINDOW_OPENGL / 0

// Thrown for every failed API call; the message names the call, the result and the source line.
class Error : public std::runtime_error {
public:
    using std::runtime_error::runtime_error;
};

enum class Present { Immediate, Mailbox, Fifo };
const char* presentName(Present p);

struct Options {
    int gpuIndex = -1;          // -1: pick automatically
    Present present = Present::Immediate;
    bool validate = false;      // API debug layer, when installed
    bool offscreen = false;     // render into an internal image, no window and no present
    uint32_t width = 1280, height = 720;  // offscreen size (windowed: the window's drawable size)
};

struct Handle {
    uint32_t id = 0;
    explicit operator bool() const { return id != 0; }
};
struct Buffer : Handle {};
struct Texture : Handle {};
struct Pipeline : Handle {};

enum class BufferKind {
    Vertex,   // static, from init data
    Index,    // static 32-bit indices
    Uniform,  // rewritten every frame with updateBuffer (backends keep a copy per frame in flight)
    Storage,  // GPU-only, written by compute, read by vertex shaders
};

enum class Format {
    RGBA8,       // UNORM; the scenes encode sRGB themselves where they store LDR
    RGBA16F,
    RGBA32F,
    Backbuffer,  // the backbuffer's format, e.g. for a multisampled target resolved into it
};

enum class Res : uint8_t { None, Uniform, Texture, StorageRead, StorageRW };

struct VertexBinding {
    uint32_t stride;
    bool perInstance;
};
struct VertexAttr {
    uint32_t location, binding, offset, components;  // 32-bit float components
};

struct PipelineDesc {
    const char* vs = nullptr;  // shader names, e.g. "fullscreen.vert"
    const char* fs = nullptr;
    std::vector<VertexBinding> vertexBindings;
    std::vector<VertexAttr> attrs;
    Res res[kMaxBindings] = {};  // resource type at each binding
    uint32_t constBytes = 0;     // size of the "Push" block, 0 when unused
    // Target: the backbuffer (backbuffer = true) or a texture of this format.
    bool backbuffer = true;
    Format color = Format::RGBA8;
    uint32_t colorCount = 1;  // 2: a second target of the same format (PassDesc::color2)
    bool depth = false;
    uint32_t samples = 1;
    bool cullBack = false;
    bool depthTest = false, depthWrite = false;
    bool additive = false;  // ONE + ONE blending
    bool hdrOut = false;    // shader writes linear HDR (specialization constant 1)
};

struct ComputeDesc {
    const char* cs = nullptr;
    Res res[kMaxBindings] = {};
    uint32_t constBytes = 0;
};

enum class Load { Clear, Load, DontCare };

struct PassDesc {
    Texture color;            // empty: the backbuffer
    Texture color2;           // optional second target (same size and format), for two-output pipelines
    Texture depth;            // optional, same size and sample count as color
    bool resolveToBackbuffer = false;  // color is multisampled; resolve it into the backbuffer at the end
    Load load = Load::Clear;
    float clear[4] = {0, 0, 0, 0};
};

struct DeviceInfo {
    std::string apiVersion;     // e.g. "1.4.343", "4.6", "11_1", "12_0"
    std::string gpuName, gpuType, driverName, driverInfo, driverVersion;
    uint32_t vendorId = 0, deviceId = 0;
    uint64_t driverVersionRaw = 0;
    int gpuIndex = 0;
    std::string presentMode;    // the mode actually used
    std::string swapFormat;     // e.g. "B8G8R8A8_SRGB"
    uint32_t msaaSamples = 1;   // the MSAA level scenes should use (4 when supported, else 1)
    bool timestamps = false;
    bool validation = false;
    bool syncValidation = false;
    std::string variant;        // non-empty when a scene runs a different but equivalent path, e.g. particles on textures
};

// Per-frame timing (all optional; < 0 means not available).
struct FrameTiming {
    double gpuMs = -1;          // GPU time of the frame (timestamps at its start and end)
};

class Device {
public:
    virtual ~Device() = default;
    static std::unique_ptr<Device> create(Api api, SDL_Window* window, const Options& opt);
    static int listGpus(Api api);  // prints the devices; returns an exit code

    virtual Api api() const = 0;
    virtual const DeviceInfo& info() const = 0;
    virtual uint32_t width() const = 0;
    virtual uint32_t height() const = 0;
    virtual bool supportsCompute() const = 0;   // false: D3D9, D3D10 (scenes use their texture variant)
    virtual uint32_t validationErrors() const { return 0; }
    virtual uint32_t validationWarnings() const { return 0; }

    // Window
    virtual bool surfaceHasArea() = 0;   // false while minimized
    virtual void resize() = 0;           // after a window size change; the caller recreates sized resources
    virtual void waitIdle() = 0;

    // Frames. beginFrame returns false when the swapchain must be recreated (call resize()).
    virtual bool beginFrame(bool measured) = 0;
    virtual uint32_t frameSlot() const = 0;  // 0..kFramesInFlight-1
    // endFrame submits and presents; with capturePath, the frame is also saved as a 24-bit BMP.
    // Returns true when the swapchain must be recreated.
    virtual bool endFrame(const std::string* capturePath = nullptr) = 0;
    virtual void finishFrames() = 0;                    // waits for the GPU, collects pending timings
    virtual std::vector<double> takeGpuTimes() = 0;     // GPU ms of measured frames since the last call
    virtual bool canCapture() const = 0;

    // Resources (destroy only when the GPU is idle: after waitIdle or finishFrames)
    virtual Buffer createBuffer(BufferKind kind, size_t size, const void* data) = 0;
    virtual void updateBuffer(Buffer b, const void* data, size_t size) = 0;  // Uniform buffers, once per frame
    virtual Texture createTexture(Format f, uint32_t w, uint32_t h, uint32_t samples, bool renderTarget) = 0;
    virtual Texture createDepth(uint32_t w, uint32_t h, uint32_t samples) = 0;
    virtual Pipeline createPipeline(const PipelineDesc& d) = 0;
    virtual Pipeline createCompute(const ComputeDesc& d) = 0;
    virtual void destroy(Buffer b) = 0;
    virtual void destroy(Texture t) = 0;
    virtual void destroy(Pipeline p) = 0;
    virtual uint32_t textureWidth(Texture t) const = 0;
    virtual uint32_t textureHeight(Texture t) const = 0;

    // Commands (between beginFrame and endFrame)
    virtual void beginPass(const PassDesc& p) = 0;
    virtual void endPass() = 0;
    virtual void setPipeline(Pipeline p) = 0;
    virtual void setConstants(const void* data, uint32_t size) = 0;   // after setPipeline
    virtual void setUniform(uint32_t binding, Buffer b) = 0;
    virtual void setTexture(uint32_t binding, Texture t) = 0;         // linear filter, clamp
    virtual void setStorage(uint32_t binding, Buffer b) = 0;
    virtual void setVertexBuffer(uint32_t slot, Buffer b) = 0;
    virtual void setIndexBuffer(Buffer b) = 0;
    virtual void draw(uint32_t vertexCount, uint32_t instanceCount = 1) = 0;
    virtual void drawIndexed(uint32_t indexCount, uint32_t instanceCount, uint32_t firstInstance) = 0;
    // Compute (outside passes). Storage written by a dispatch is visible to the following draws
    // after storageToVertex(); vertex reads finish before the next dispatch after storageToCompute().
    virtual void dispatch(uint32_t groupsX) = 0;
    virtual void storageToCompute() = 0;
    virtual void storageToVertex() = 0;
};

// Collects destruction callbacks, run in reverse creation order by flush().
class ResourceList {
public:
    void add(std::function<void()> fn) { fns_.push_back(std::move(fn)); }
    void flush() {
        for (auto it = fns_.rbegin(); it != fns_.rend(); ++it) (*it)();
        fns_.clear();
    }

private:
    std::vector<std::function<void()>> fns_;
};

// Shader sources generated at build time from shaders/*.glsl files.
struct ShaderSource {
    const char* name;
    const uint32_t* spirv;   // Vulkan
    size_t spirvBytes;
    const char* glsl;        // OpenGL 4.3 (SPIRV-Cross)
    const char* glslFlip;    // vertex shaders only: y flipped, for passes into the window
    const char* hlsl5;       // D3D11/D3D12, shader model 5.0 (SPIRV-Cross, y flipped in vertex shaders)
    const char* hlsl4;       // D3D10, shader model 4.0 (nullptr when the shader needs SM5 features)
    const char* hlsl3;       // D3D9, shader model 3.0 (hand-written variants; nullptr when none)
};
const ShaderSource& shader(const char* name);  // throws gfx::Error for unknown names

// Small helpers shared by backends.
bool writeBmp(const std::string& path, const uint8_t* px, uint32_t w, uint32_t h, size_t rowPitch, bool bgra);
std::string hex4(uint32_t v);

}  // namespace gfx
