// OpenGL backend: core profile 4.3 (compute shaders, storage buffers, vertex attribute binding),
// plus clip control (4.5 or ARB_clip_control) so depth uses the 0..1 range like the other APIs.
//
// Render targets keep GL's lower-left origin, so their memory layout matches Vulkan's exactly (row
// 0 is NDC y = -1). Only pipelines that draw into the window use y-flipped vertex shaders, because
// GL shows row 0 of the window at the bottom.
#include "gfx/gfx.h"

#define GL_GLEXT_PROTOTYPES 1
#include <SDL.h>
#include <SDL_opengl.h>

#include <algorithm>
#include <chrono>
#include <cstdio>
#include <cstring>
#include <map>
#include <string>
#include <tuple>

namespace gfx {
namespace {

// Every GL entry point is loaded through SDL; none is linked directly.
#define QB_GL_FUNCS(X)                                                                                  \
    X(GetString) X(GetStringi) X(GetIntegerv) X(GetError) X(Enable) X(Disable) X(Viewport) X(Finish)     \
    X(DepthFunc) X(DepthMask) X(CullFace) X(FrontFace) X(BlendFunc) X(ColorMask) X(PixelStorei)          \
    X(ReadPixels) X(ReadBuffer) X(DrawBuffers) X(GenBuffers) X(DeleteBuffers) X(BindBuffer) X(BufferData) \
    X(BufferSubData) X(BindBufferBase) X(BindBufferRange) X(GenTextures) X(DeleteTextures) X(BindTexture) \
    X(ActiveTexture) X(TexStorage2D) X(TexStorage2DMultisample) X(TexParameteri) X(GenSamplers)         \
    X(DeleteSamplers) X(SamplerParameteri) X(BindSampler) X(GenFramebuffers) X(DeleteFramebuffers)       \
    X(BindFramebuffer) X(FramebufferTexture2D) X(CheckFramebufferStatus) X(BlitFramebuffer)              \
    X(ClearBufferfv) X(GetFramebufferAttachmentParameteriv) X(CreateShader) X(ShaderSource)             \
    X(CompileShader) X(GetShaderiv) X(GetShaderInfoLog) X(DeleteShader) X(CreateProgram) X(AttachShader) \
    X(LinkProgram) X(GetProgramiv) X(GetProgramInfoLog) X(DeleteProgram) X(UseProgram)                   \
    X(GetUniformBlockIndex) X(UniformBlockBinding) X(GenVertexArrays) X(DeleteVertexArrays)              \
    X(BindVertexArray) X(EnableVertexAttribArray) X(VertexAttribFormat) X(VertexAttribBinding)           \
    X(VertexBindingDivisor) X(BindVertexBuffer) X(DrawArraysInstanced) X(DrawElementsInstancedBaseInstance) \
    X(DispatchCompute) X(MemoryBarrier) X(FenceSync) X(ClientWaitSync) X(DeleteSync) X(GenQueries)       \
    X(DeleteQueries) X(QueryCounter) X(GetQueryObjectiv) X(GetQueryObjectui64v) X(DebugMessageCallback)  \
    X(DebugMessageControl)

struct GlFuncs {
#define QB_DECL(name) decltype(&gl##name) name = nullptr;
    QB_GL_FUNCS(QB_DECL)
#undef QB_DECL
    void (APIENTRYP ClipControl)(GLenum origin, GLenum depth) = nullptr;  // 4.5 / ARB_clip_control
};
GlFuncs gl;

#ifndef GL_LOWER_LEFT
#define GL_LOWER_LEFT 0x8CA1
#endif
#ifndef GL_ZERO_TO_ONE
#define GL_ZERO_TO_ONE 0x935F
#endif

constexpr GLuint kPushBinding = 4;          // the "Push" block of every shader
constexpr GLsizeiptr kConstRingBytes = 1 << 18;  // per frame in flight
constexpr uint64_t kFenceTimeoutNs = 10ull * 1000 * 1000 * 1000;

struct BufferRec {
    bool alive = false;
    BufferKind kind{};
    size_t size = 0;
    GLuint buf[kFramesInFlight]{};  // Uniform: one per frame in flight; others use [0]
};

struct TextureRec {
    bool alive = false;
    GLuint tex = 0;
    GLenum internal = 0;
    uint32_t w = 0, h = 0, samples = 1;
    bool depth = false;
};

struct PipelineRec {
    bool alive = false;
    bool compute = false;
    GLuint program = 0, vao = 0;
    PipelineDesc desc;  // state set at bind time
    std::vector<uint32_t> strides;
    bool flipped = false;  // draws into the window
};

class GlDevice final : public Device {
public:
    GlDevice(SDL_Window* window, const Options& opt);
    ~GlDevice() override;

    Api api() const override { return Api::OpenGL; }
    const DeviceInfo& info() const override { return info_; }
    uint32_t width() const override { return w_; }
    uint32_t height() const override { return h_; }
    bool supportsCompute() const override { return true; }
    uint32_t validationErrors() const override { return debugErrors_; }

    bool surfaceHasArea() override {
        if (opt_.offscreen) return true;
        int w = 0, h = 0;
        SDL_GL_GetDrawableSize(window_, &w, &h);
        return w > 0 && h > 0;
    }
    void resize() override;
    void waitIdle() override { gl.Finish(); }

    bool beginFrame(bool measured) override;
    uint32_t frameSlot() const override { return slot_; }
    bool endFrame(const std::string* capturePath) override;
    void finishFrames() override;
    std::vector<double> takeGpuTimes() override {
        std::vector<double> out;
        out.swap(gpuTimesMs_);
        return out;
    }
    bool canCapture() const override { return true; }
    double waitPresented(std::string& source) override {
        gl.Finish();  // after the swap: the frame and the swap have reached the GPU
        source = "gpu";
        return std::chrono::duration<double, std::milli>(std::chrono::steady_clock::now() - frameStart_).count();
    }

    Buffer createBuffer(BufferKind kind, size_t size, const void* data) override;
    void updateBuffer(Buffer b, const void* data, size_t size) override;
    Texture createTexture(Format f, uint32_t w, uint32_t h, uint32_t samples, bool renderTarget) override;
    Texture createDepth(uint32_t w, uint32_t h, uint32_t samples) override;
    Pipeline createPipeline(const PipelineDesc& d) override;
    Pipeline createCompute(const ComputeDesc& d) override;
    void destroy(Buffer b) override;
    void destroy(Texture t) override;
    void destroy(Pipeline p) override;
    uint32_t textureWidth(Texture t) const override { return textures_.at(t.id - 1).w; }
    uint32_t textureHeight(Texture t) const override { return textures_.at(t.id - 1).h; }

    void beginPass(const PassDesc& p) override;
    void endPass() override;
    void setPipeline(Pipeline p) override;
    void setConstants(const void* data, uint32_t size) override;
    void setUniform(uint32_t binding, Buffer b) override {
        const BufferRec& r = buffers_.at(b.id - 1);
        gl.BindBufferBase(GL_UNIFORM_BUFFER, binding, r.buf[r.kind == BufferKind::Uniform ? slot_ : 0]);
    }
    void setTexture(uint32_t binding, Texture t) override {
        gl.ActiveTexture(GL_TEXTURE0 + binding);
        gl.BindTexture(GL_TEXTURE_2D, textures_.at(t.id - 1).tex);
        gl.BindSampler(binding, sampler_);
    }
    void setStorage(uint32_t binding, Buffer b) override {
        gl.BindBufferBase(GL_SHADER_STORAGE_BUFFER, binding, buffers_.at(b.id - 1).buf[0]);
    }
    void setVertexBuffer(uint32_t slot, Buffer b) override {
        gl.BindVertexBuffer(slot, buffers_.at(b.id - 1).buf[0], 0, bound_->strides.at(slot));
    }
    void setIndexBuffer(Buffer b) override { gl.BindBuffer(GL_ELEMENT_ARRAY_BUFFER, buffers_.at(b.id - 1).buf[0]); }
    void draw(uint32_t vertexCount, uint32_t instanceCount) override {
        gl.DrawArraysInstanced(GL_TRIANGLES, 0, vertexCount, instanceCount);
    }
    void drawIndexed(uint32_t indexCount, uint32_t instanceCount, uint32_t firstInstance) override {
        gl.DrawElementsInstancedBaseInstance(GL_TRIANGLES, indexCount, GL_UNSIGNED_INT, nullptr, instanceCount,
                                             firstInstance);
    }
    void dispatch(uint32_t groupsX) override { gl.DispatchCompute(groupsX, 1, 1); }
    void storageToCompute() override { gl.MemoryBarrier(GL_SHADER_STORAGE_BARRIER_BIT); }
    void storageToVertex() override { gl.MemoryBarrier(GL_SHADER_STORAGE_BARRIER_BIT); }

    uint32_t debugErrors_ = 0;

private:
    GLuint compile(GLenum stage, const char* source, const char* name, bool hdrOut);
    GLuint link(std::initializer_list<GLuint> shaders, const char* name);
    GLuint framebuffer(GLuint c0, GLuint c1, GLuint depth, bool ms);
    void createBackbuffer();
    void collectTimestamps(uint32_t slot);
    GLuint backbufferFbo() const { return opt_.offscreen ? offscreenFbo_ : 0; }

    SDL_Window* window_;
    Options opt_;
    DeviceInfo info_;
    SDL_GLContext ctx_ = nullptr;
    uint32_t w_ = 0, h_ = 0;
    bool manualGamma_ = false;
    GLenum backbufferFormat_ = GL_SRGB8_ALPHA8;
    GLuint offscreenTex_ = 0, offscreenFbo_ = 0;
    GLuint sampler_ = 0;
    GLuint constRing_ = 0;
    GLint uboAlign_ = 256;
    GLintptr constOffset_ = 0;

    uint32_t slot_ = 0;
    uint64_t frameCounter_ = 0;
    std::chrono::steady_clock::time_point frameStart_{};
    GLsync fences_[kFramesInFlight]{};
    GLuint queries_[kFramesInFlight][2]{};
    bool slotPending_[kFramesInFlight]{};
    bool slotMeasured_[kFramesInFlight]{};
    std::vector<double> gpuTimesMs_;

    std::vector<BufferRec> buffers_;
    std::vector<TextureRec> textures_;
    std::vector<PipelineRec> pipelines_;
    std::map<std::tuple<GLuint, GLuint, GLuint>, GLuint> fbos_;

    const PipelineRec* bound_ = nullptr;
    PassDesc pass_{};
    GLuint passFbo_ = 0;
};

void APIENTRY debugCallback(GLenum, GLenum type, GLuint, GLenum severity, GLsizei, const GLchar* message,
                            const void* user) {
    auto* dev = static_cast<GlDevice*>(const_cast<void*>(user));
    if (type == GL_DEBUG_TYPE_ERROR || severity == GL_DEBUG_SEVERITY_HIGH) {
        dev->debugErrors_++;
        std::fprintf(stderr, "[gl error] %s\n", message);
    } else if (severity == GL_DEBUG_SEVERITY_MEDIUM) {
        std::fprintf(stderr, "[gl warning] %s\n", message);
    }
}

bool hasExtension(const char* name) {
    GLint n = 0;
    gl.GetIntegerv(GL_NUM_EXTENSIONS, &n);
    for (GLint i = 0; i < n; ++i)
        if (std::strcmp(reinterpret_cast<const char*>(gl.GetStringi(GL_EXTENSIONS, i)), name) == 0) return true;
    return false;
}

GlDevice::GlDevice(SDL_Window* window, const Options& opt) : window_(window), opt_(opt) {
    if (opt.validate) SDL_GL_SetAttribute(SDL_GL_CONTEXT_FLAGS, SDL_GL_CONTEXT_DEBUG_FLAG);
    ctx_ = SDL_GL_CreateContext(window);
    if (!ctx_) throw Error(std::string("SDL_GL_CreateContext (OpenGL 4.3 core) failed: ") + SDL_GetError());
    SDL_GL_MakeCurrent(window, ctx_);
#define QB_LOAD(name)                                                                       \
    gl.name = reinterpret_cast<decltype(gl.name)>(SDL_GL_GetProcAddress("gl" #name));       \
    if (!gl.name) throw Error("OpenGL entry point gl" #name " is missing");
    QB_GL_FUNCS(QB_LOAD)
#undef QB_LOAD
    gl.ClipControl = reinterpret_cast<decltype(gl.ClipControl)>(SDL_GL_GetProcAddress("glClipControl"));

    GLint major = 0, minor = 0;
    gl.GetIntegerv(GL_MAJOR_VERSION, &major);
    gl.GetIntegerv(GL_MINOR_VERSION, &minor);
    if (major < 4 || (major == 4 && minor < 3))
        throw Error("OpenGL " + std::to_string(major) + "." + std::to_string(minor) + " found, 4.3 is required");
    const bool clipControl = gl.ClipControl && ((major == 4 && minor >= 5) || major > 4 || hasExtension("GL_ARB_clip_control"));
    if (!clipControl) throw Error("OpenGL clip control (4.5 or GL_ARB_clip_control) is required");
    gl.ClipControl(GL_LOWER_LEFT, GL_ZERO_TO_ONE);

    if (opt.validate) {
        gl.Enable(GL_DEBUG_OUTPUT);
        gl.Enable(GL_DEBUG_OUTPUT_SYNCHRONOUS);
        gl.DebugMessageCallback(debugCallback, this);
        info_.validation = true;
    }

    info_.apiVersion = std::to_string(major) + "." + std::to_string(minor);
    info_.gpuName = reinterpret_cast<const char*>(gl.GetString(GL_RENDERER));
    info_.driverName = reinterpret_cast<const char*>(gl.GetString(GL_VENDOR));
    info_.driverInfo = reinterpret_cast<const char*>(gl.GetString(GL_VERSION));
    info_.driverVersion = info_.driverInfo;
    info_.gpuType = "unknown";
    GLint maxSamples = 1;
    gl.GetIntegerv(GL_MAX_SAMPLES, &maxSamples);
    info_.msaaSamples = maxSamples >= 4 ? 4 : 1;
    info_.timestamps = true;
    gl.GetIntegerv(GL_UNIFORM_BUFFER_OFFSET_ALIGNMENT, &uboAlign_);

    if (!opt.offscreen) {
        int si = opt.present == Present::Fifo ? 1 : 0;
        if (SDL_GL_SetSwapInterval(si) != 0 && si == 0) SDL_GL_SetSwapInterval(1);
        info_.presentMode = SDL_GL_GetSwapInterval() == 0 ? "immediate" : "fifo";
        GLint enc = GL_LINEAR;
        gl.BindFramebuffer(GL_FRAMEBUFFER, 0);
        gl.GetFramebufferAttachmentParameteriv(GL_FRAMEBUFFER, GL_BACK_LEFT, GL_FRAMEBUFFER_ATTACHMENT_COLOR_ENCODING, &enc);
        manualGamma_ = enc != GL_SRGB;
        backbufferFormat_ = manualGamma_ ? GL_RGBA8 : GL_SRGB8_ALPHA8;
        info_.swapFormat = manualGamma_ ? "RGBA8 (shader encodes sRGB)" : "SRGB8_ALPHA8";
    } else {
        info_.presentMode = "offscreen";
        info_.swapFormat = "SRGB8_ALPHA8";
    }
    gl.Enable(GL_FRAMEBUFFER_SRGB);

    gl.GenSamplers(1, &sampler_);
    gl.SamplerParameteri(sampler_, GL_TEXTURE_MIN_FILTER, GL_LINEAR);
    gl.SamplerParameteri(sampler_, GL_TEXTURE_MAG_FILTER, GL_LINEAR);
    gl.SamplerParameteri(sampler_, GL_TEXTURE_WRAP_S, GL_CLAMP_TO_EDGE);
    gl.SamplerParameteri(sampler_, GL_TEXTURE_WRAP_T, GL_CLAMP_TO_EDGE);
    gl.GenBuffers(1, &constRing_);
    gl.BindBuffer(GL_UNIFORM_BUFFER, constRing_);
    gl.BufferData(GL_UNIFORM_BUFFER, kConstRingBytes * kFramesInFlight, nullptr, GL_STREAM_DRAW);
    for (auto& q : queries_) gl.GenQueries(2, q);
    gl.DepthFunc(GL_LEQUAL);
    createBackbuffer();
}

GlDevice::~GlDevice() {
    if (!ctx_) return;
    gl.Finish();
    for (uint32_t i = 0; i < pipelines_.size(); ++i) destroy(Pipeline{{i + 1}});
    for (uint32_t i = 0; i < textures_.size(); ++i) destroy(Texture{{i + 1}});
    for (uint32_t i = 0; i < buffers_.size(); ++i) destroy(Buffer{{i + 1}});
    for (auto& [k, f] : fbos_) gl.DeleteFramebuffers(1, &f);
    if (offscreenFbo_) gl.DeleteFramebuffers(1, &offscreenFbo_);
    if (offscreenTex_) gl.DeleteTextures(1, &offscreenTex_);
    for (auto& f : fences_)
        if (f) gl.DeleteSync(f);
    for (auto& q : queries_) gl.DeleteQueries(2, q);
    gl.DeleteBuffers(1, &constRing_);
    gl.DeleteSamplers(1, &sampler_);
    SDL_GL_DeleteContext(ctx_);
}

void GlDevice::createBackbuffer() {
    if (offscreenFbo_) gl.DeleteFramebuffers(1, &offscreenFbo_);
    if (offscreenTex_) gl.DeleteTextures(1, &offscreenTex_);
    offscreenFbo_ = offscreenTex_ = 0;
    if (opt_.offscreen) {
        w_ = opt_.width;
        h_ = opt_.height;
        gl.GenTextures(1, &offscreenTex_);
        gl.BindTexture(GL_TEXTURE_2D, offscreenTex_);
        gl.TexStorage2D(GL_TEXTURE_2D, 1, GL_SRGB8_ALPHA8, w_, h_);
        gl.GenFramebuffers(1, &offscreenFbo_);
        gl.BindFramebuffer(GL_FRAMEBUFFER, offscreenFbo_);
        gl.FramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, offscreenTex_, 0);
    } else {
        int w = 0, h = 0;
        SDL_GL_GetDrawableSize(window_, &w, &h);
        w_ = static_cast<uint32_t>(std::max(w, 1));
        h_ = static_cast<uint32_t>(std::max(h, 1));
    }
}

void GlDevice::resize() {
    gl.Finish();
    createBackbuffer();
}

GLuint GlDevice::compile(GLenum stage, const char* source, const char* name, bool hdrOut) {
    // Specialization constants become macros after the #version line.
    std::string src = source;
    size_t eol = src.find('\n');
    std::string defs = "\n#define SPIRV_CROSS_CONSTANT_ID_0 " + std::string(manualGamma_ ? "1" : "0") +
                       "\n#define SPIRV_CROSS_CONSTANT_ID_1 " + std::string(hdrOut ? "1" : "0") + "\n";
    src.insert(eol == std::string::npos ? src.size() : eol, defs);
    GLuint s = gl.CreateShader(stage);
    const char* p = src.c_str();
    gl.ShaderSource(s, 1, &p, nullptr);
    gl.CompileShader(s);
    GLint ok = 0;
    gl.GetShaderiv(s, GL_COMPILE_STATUS, &ok);
    if (!ok) {
        char log[4096] = {};
        gl.GetShaderInfoLog(s, sizeof log, nullptr, log);
        gl.DeleteShader(s);
        throw Error(std::string("OpenGL shader ") + name + " does not compile: " + log);
    }
    return s;
}

GLuint GlDevice::link(std::initializer_list<GLuint> shaders, const char* name) {
    GLuint prog = gl.CreateProgram();
    for (GLuint s : shaders) gl.AttachShader(prog, s);
    gl.LinkProgram(prog);
    for (GLuint s : shaders) gl.DeleteShader(s);
    GLint ok = 0;
    gl.GetProgramiv(prog, GL_LINK_STATUS, &ok);
    if (!ok) {
        char log[4096] = {};
        gl.GetProgramInfoLog(prog, sizeof log, nullptr, log);
        gl.DeleteProgram(prog);
        throw Error(std::string("OpenGL program ") + name + " does not link: " + log);
    }
    GLuint push = gl.GetUniformBlockIndex(prog, "Push");
    if (push != GL_INVALID_INDEX) gl.UniformBlockBinding(prog, push, kPushBinding);
    return prog;
}

// ---------------------------------------------------------------------------------------------
// Frames

void GlDevice::collectTimestamps(uint32_t slot) {
    if (!slotPending_[slot]) return;
    GLint avail = 0;
    gl.GetQueryObjectiv(queries_[slot][1], GL_QUERY_RESULT_AVAILABLE, &avail);
    if (!avail) return;  // the fence said the frame is done; results normally are too
    slotPending_[slot] = false;
    GLuint64 t0 = 0, t1 = 0;
    gl.GetQueryObjectui64v(queries_[slot][0], GL_QUERY_RESULT, &t0);
    gl.GetQueryObjectui64v(queries_[slot][1], GL_QUERY_RESULT, &t1);
    if (slotMeasured_[slot] && t1 >= t0) gpuTimesMs_.push_back(static_cast<double>(t1 - t0) / 1.0e6);
}

bool GlDevice::beginFrame(bool measured) {
    frameStart_ = std::chrono::steady_clock::now();
    slot_ = static_cast<uint32_t>(frameCounter_ % kFramesInFlight);
    if (fences_[slot_]) {
        GLenum r = gl.ClientWaitSync(fences_[slot_], GL_SYNC_FLUSH_COMMANDS_BIT, kFenceTimeoutNs);
        if (r == GL_TIMEOUT_EXPIRED) throw Error("glClientWaitSync: the GPU did not finish a frame within 10 s");
        if (r == GL_WAIT_FAILED) throw Error("glClientWaitSync failed");
        gl.DeleteSync(fences_[slot_]);
        fences_[slot_] = nullptr;
    }
    collectTimestamps(slot_);
    slotMeasured_[slot_] = measured;
    constOffset_ = 0;
    gl.QueryCounter(queries_[slot_][0], GL_TIMESTAMP);
    bound_ = nullptr;
    return true;
}

bool GlDevice::endFrame(const std::string* capturePath) {
    gl.QueryCounter(queries_[slot_][1], GL_TIMESTAMP);
    slotPending_[slot_] = true;
    if (capturePath) {
        std::vector<uint8_t> px(size_t(w_) * h_ * 4), flipped(px.size());
        gl.BindFramebuffer(GL_READ_FRAMEBUFFER, backbufferFbo());
        if (!opt_.offscreen) gl.ReadBuffer(GL_BACK);
        gl.PixelStorei(GL_PACK_ALIGNMENT, 1);
        gl.ReadPixels(0, 0, w_, h_, GL_RGBA, GL_UNSIGNED_BYTE, px.data());
        const size_t row = size_t(w_) * 4;  // GL rows are bottom-up
        for (uint32_t y = 0; y < h_; ++y) std::memcpy(&flipped[y * row], &px[(h_ - 1 - y) * row], row);
        if (!writeBmp(*capturePath, flipped.data(), w_, h_, row, false))
            std::fprintf(stderr, "warning: could not write %s\n", capturePath->c_str());
    }
    fences_[slot_] = gl.FenceSync(GL_SYNC_GPU_COMMANDS_COMPLETE, 0);
    if (!opt_.offscreen) SDL_GL_SwapWindow(window_);  // offscreen: the fence wait flushes the frame
    ++frameCounter_;
    return false;
}

void GlDevice::finishFrames() {
    gl.Finish();
    for (uint32_t i = 0; i < kFramesInFlight; ++i) collectTimestamps(i);
}

// ---------------------------------------------------------------------------------------------
// Resources

Buffer GlDevice::createBuffer(BufferKind kind, size_t size, const void* data) {
    BufferRec r;
    r.alive = true;
    r.kind = kind;
    r.size = size;
    const GLenum target = kind == BufferKind::Uniform ? GL_UNIFORM_BUFFER
                        : kind == BufferKind::Storage ? GL_SHADER_STORAGE_BUFFER
                                                      : GL_ARRAY_BUFFER;
    const uint32_t copies = kind == BufferKind::Uniform ? kFramesInFlight : 1;
    for (uint32_t i = 0; i < copies; ++i) {
        gl.GenBuffers(1, &r.buf[i]);
        gl.BindBuffer(target, r.buf[i]);
        gl.BufferData(target, size, data, kind == BufferKind::Uniform ? GL_DYNAMIC_DRAW
                                          : kind == BufferKind::Storage ? GL_DYNAMIC_COPY
                                                                        : GL_STATIC_DRAW);
    }
    gl.BindBuffer(target, 0);
    buffers_.push_back(r);
    return Buffer{{static_cast<uint32_t>(buffers_.size())}};
}

void GlDevice::updateBuffer(Buffer b, const void* data, size_t size) {
    const BufferRec& r = buffers_.at(b.id - 1);
    gl.BindBuffer(GL_UNIFORM_BUFFER, r.buf[slot_]);
    gl.BufferSubData(GL_UNIFORM_BUFFER, 0, std::min(size, r.size), data);
}

Texture GlDevice::createTexture(Format f, uint32_t w, uint32_t h, uint32_t samples, bool) {
    TextureRec r;
    r.alive = true;
    r.internal = f == Format::RGBA8 ? GL_RGBA8 : f == Format::RGBA16F ? GL_RGBA16F : f == Format::RGBA32F ? GL_RGBA32F
                                                                                                   : backbufferFormat_;
    r.w = w;
    r.h = h;
    r.samples = samples;
    gl.GenTextures(1, &r.tex);
    if (samples > 1) {
        gl.BindTexture(GL_TEXTURE_2D_MULTISAMPLE, r.tex);
        gl.TexStorage2DMultisample(GL_TEXTURE_2D_MULTISAMPLE, samples, r.internal, w, h, GL_TRUE);
    } else {
        gl.BindTexture(GL_TEXTURE_2D, r.tex);
        gl.TexStorage2D(GL_TEXTURE_2D, 1, r.internal, w, h);
    }
    textures_.push_back(r);
    return Texture{{static_cast<uint32_t>(textures_.size())}};
}

Texture GlDevice::createDepth(uint32_t w, uint32_t h, uint32_t samples) {
    TextureRec r;
    r.alive = true;
    r.depth = true;
    r.internal = GL_DEPTH_COMPONENT32F;
    r.w = w;
    r.h = h;
    r.samples = samples;
    gl.GenTextures(1, &r.tex);
    if (samples > 1) {
        gl.BindTexture(GL_TEXTURE_2D_MULTISAMPLE, r.tex);
        gl.TexStorage2DMultisample(GL_TEXTURE_2D_MULTISAMPLE, samples, r.internal, w, h, GL_TRUE);
    } else {
        gl.BindTexture(GL_TEXTURE_2D, r.tex);
        gl.TexStorage2D(GL_TEXTURE_2D, 1, r.internal, w, h);
    }
    textures_.push_back(r);
    return Texture{{static_cast<uint32_t>(textures_.size())}};
}

void GlDevice::destroy(Buffer b) {
    BufferRec& r = buffers_.at(b.id - 1);
    if (!r.alive) return;
    for (GLuint& x : r.buf)
        if (x) gl.DeleteBuffers(1, &x);
    r = BufferRec{};
}

void GlDevice::destroy(Texture t) {
    TextureRec& r = textures_.at(t.id - 1);
    if (!r.alive) return;
    for (auto it = fbos_.begin(); it != fbos_.end();) {
        auto [a, b, c] = it->first;
        if (a == r.tex || b == r.tex || c == r.tex) {
            gl.DeleteFramebuffers(1, &it->second);
            it = fbos_.erase(it);
        } else {
            ++it;
        }
    }
    gl.DeleteTextures(1, &r.tex);
    r = TextureRec{};
}

void GlDevice::destroy(Pipeline p) {
    PipelineRec& r = pipelines_.at(p.id - 1);
    if (!r.alive) return;
    gl.DeleteProgram(r.program);
    if (r.vao) gl.DeleteVertexArrays(1, &r.vao);
    r = PipelineRec{};
}

Pipeline GlDevice::createPipeline(const PipelineDesc& d) {
    PipelineRec r;
    r.alive = true;
    r.desc = d;
    r.flipped = d.backbuffer;
    const ShaderSource& vs = shader(d.vs);
    const ShaderSource& fs = shader(d.fs);
    const char* vsrc = r.flipped ? vs.glslFlip : vs.glsl;
    if (!vsrc) throw Error(std::string("no GLSL for ") + d.vs);
    r.program = link({compile(GL_VERTEX_SHADER, vsrc, d.vs, d.hdrOut), compile(GL_FRAGMENT_SHADER, fs.glsl, d.fs, d.hdrOut)},
                     d.fs);
    gl.GenVertexArrays(1, &r.vao);
    gl.BindVertexArray(r.vao);
    for (const VertexAttr& a : d.attrs) {
        gl.EnableVertexAttribArray(a.location);
        gl.VertexAttribFormat(a.location, a.components, GL_FLOAT, GL_FALSE, a.offset);
        gl.VertexAttribBinding(a.location, a.binding);
    }
    for (uint32_t i = 0; i < d.vertexBindings.size(); ++i) {
        gl.VertexBindingDivisor(i, d.vertexBindings[i].perInstance ? 1 : 0);
        r.strides.push_back(d.vertexBindings[i].stride);
    }
    gl.BindVertexArray(0);
    pipelines_.push_back(r);
    return Pipeline{{static_cast<uint32_t>(pipelines_.size())}};
}

Pipeline GlDevice::createCompute(const ComputeDesc& d) {
    PipelineRec r;
    r.alive = true;
    r.compute = true;
    r.program = link({compile(GL_COMPUTE_SHADER, shader(d.cs).glsl, d.cs, false)}, d.cs);
    pipelines_.push_back(r);
    return Pipeline{{static_cast<uint32_t>(pipelines_.size())}};
}

// ---------------------------------------------------------------------------------------------
// Commands

GLuint GlDevice::framebuffer(GLuint c0, GLuint c1, GLuint depth, bool ms) {
    auto key = std::make_tuple(c0, c1, depth);
    auto it = fbos_.find(key);
    if (it != fbos_.end()) return it->second;
    GLuint fbo = 0;
    gl.GenFramebuffers(1, &fbo);
    gl.BindFramebuffer(GL_FRAMEBUFFER, fbo);
    const GLenum target = ms ? GL_TEXTURE_2D_MULTISAMPLE : GL_TEXTURE_2D;
    gl.FramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, target, c0, 0);
    if (c1) gl.FramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT1, target, c1, 0);
    if (depth) gl.FramebufferTexture2D(GL_FRAMEBUFFER, GL_DEPTH_ATTACHMENT, target, depth, 0);
    const GLenum bufs[2] = {GL_COLOR_ATTACHMENT0, GL_COLOR_ATTACHMENT1};
    gl.DrawBuffers(c1 ? 2 : 1, bufs);
    GLenum status = gl.CheckFramebufferStatus(GL_FRAMEBUFFER);
    if (status != GL_FRAMEBUFFER_COMPLETE) throw Error("OpenGL framebuffer incomplete: " + hex4(status));
    fbos_[key] = fbo;
    return fbo;
}

void GlDevice::beginPass(const PassDesc& p) {
    pass_ = p;
    uint32_t w = w_, h = h_;
    if (!p.color) {
        passFbo_ = backbufferFbo();
    } else {
        const TextureRec& c = textures_.at(p.color.id - 1);
        GLuint c1 = p.color2 ? textures_.at(p.color2.id - 1).tex : 0;
        GLuint d = p.depth ? textures_.at(p.depth.id - 1).tex : 0;
        passFbo_ = framebuffer(c.tex, c1, d, c.samples > 1);
        w = c.w;
        h = c.h;
    }
    gl.BindFramebuffer(GL_FRAMEBUFFER, passFbo_);
    gl.Viewport(0, 0, w, h);
    if (p.load == Load::Clear) {
        gl.ColorMask(GL_TRUE, GL_TRUE, GL_TRUE, GL_TRUE);
        gl.ClearBufferfv(GL_COLOR, 0, p.clear);
        if (p.color2) gl.ClearBufferfv(GL_COLOR, 1, p.clear);
    }
    if (p.depth) {
        const GLfloat one = 1.0f;
        gl.DepthMask(GL_TRUE);
        gl.ClearBufferfv(GL_DEPTH, 0, &one);
    }
}

void GlDevice::endPass() {
    if (pass_.resolveToBackbuffer) {
        gl.BindFramebuffer(GL_READ_FRAMEBUFFER, passFbo_);
        gl.BindFramebuffer(GL_DRAW_FRAMEBUFFER, backbufferFbo());
        gl.BlitFramebuffer(0, 0, w_, h_, 0, 0, w_, h_, GL_COLOR_BUFFER_BIT, GL_NEAREST);
    }
}

void GlDevice::setPipeline(Pipeline p) {
    bound_ = &pipelines_.at(p.id - 1);
    gl.UseProgram(bound_->program);
    if (bound_->compute) return;
    const PipelineDesc& d = bound_->desc;
    gl.BindVertexArray(bound_->vao);
    if (d.additive) {
        gl.Enable(GL_BLEND);
        gl.BlendFunc(GL_ONE, GL_ONE);
    } else {
        gl.Disable(GL_BLEND);
    }
    if (d.depthTest) gl.Enable(GL_DEPTH_TEST);
    else gl.Disable(GL_DEPTH_TEST);
    gl.DepthMask(d.depthWrite ? GL_TRUE : GL_FALSE);
    if (d.cullBack) {
        gl.Enable(GL_CULL_FACE);
        gl.CullFace(GL_BACK);
    } else {
        gl.Disable(GL_CULL_FACE);
    }
    // Vulkan's facing rule negates the area (its y axis points down), GL's does not: with the same
    // memory layout a Vulkan counter-clockwise triangle is clockwise in GL; the y flip mirrors it back.
    gl.FrontFace(bound_->flipped ? GL_CCW : GL_CW);
    if (d.samples > 1) gl.Enable(GL_MULTISAMPLE);
}

void GlDevice::setConstants(const void* data, uint32_t size) {
    const GLintptr base = GLintptr(slot_) * kConstRingBytes;
    if (constOffset_ + size > kConstRingBytes) throw Error("OpenGL constant ring is full");
    gl.BindBuffer(GL_UNIFORM_BUFFER, constRing_);
    gl.BufferSubData(GL_UNIFORM_BUFFER, base + constOffset_, size, data);
    gl.BindBufferRange(GL_UNIFORM_BUFFER, kPushBinding, constRing_, base + constOffset_, size);
    constOffset_ += (size + uboAlign_ - 1) / uboAlign_ * uboAlign_;
}

}  // namespace

std::unique_ptr<Device> createOpenGL(SDL_Window* window, const Options& opt) {
    return std::make_unique<GlDevice>(window, opt);
}

int listOpenGL() {
    std::printf("OpenGL uses the driver behind the window's context; there is no device list. Run with --api opengl\n"
                "to see the renderer and version in the header.\n");
    return 0;
}

}  // namespace gfx
