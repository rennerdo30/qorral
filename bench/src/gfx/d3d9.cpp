// Direct3D 9Ex backend: shader model 3.0, FLIPEX presentation, sRGB writes to the backbuffer.
//
// SM 3.0 has no constant buffers: each shader's constant table (CTAB) gives the register of every
// constant it uses, and the HLSL source gives that constant's offset in its block ("Push" or the
// uniform buffer at binding N), so blocks are uploaded register by register. It has no vertex
// index either: shaders without vertex attributes read a float stream 0, 1, 2, ... (TEXCOORD7).
// No compute shaders: the particle scene runs its texture variant.
#include "gfx/d3d_common.h"

#include <d3d9.h>

#include <algorithm>
#include <cstdio>
#include <cmath>
#include <cstring>
#include <sstream>

namespace gfx {
namespace {

using d3d::Com;

constexpr DWORD kWaitMs = 10000;

struct BufferRec {
    bool alive = false;
    BufferKind kind{};
    size_t size = 0;
    Com<IDirect3DVertexBuffer9> vb;
    Com<IDirect3DIndexBuffer9> ib;
    std::vector<uint8_t> cpu;  // Uniform: the latest data
};

struct TextureRec {
    bool alive = false;
    Com<IDirect3DTexture9> tex;      // sampleable targets
    Com<IDirect3DSurface9> surface;  // level 0 of tex, or a multisampled/depth surface
    D3DFORMAT format{};
    uint32_t w = 0, h = 0, samples = 1;
    bool backbufferFormat = false;
};

// One constant (or array) of a shader: where its data comes from and which registers it fills.
struct ConstSlot {
    int block;  // -1: "Push", else the uniform buffer binding
    uint32_t offset, reg, count;
};

struct StageMap {
    std::vector<ConstSlot> consts;
    int halfPixelReg = -1;
    int samplerReg[kMaxBindings] = {-1, -1, -1, -1};
};

struct PipelineRec {
    bool alive = false;
    PipelineDesc desc;
    Com<IDirect3DVertexShader9> vs;
    Com<IDirect3DPixelShader9> ps;
    Com<IDirect3DVertexDeclaration9> decl;
    StageMap vsMap, psMap;
    bool vertexIdStream = false;
    std::vector<UINT> strides;
};

// ---------------------------------------------------------------------------------------------
// Shader reflection: CTAB + packoffset

struct CtabEntry {
    std::string name;
    uint16_t set, reg, count;
};

std::vector<CtabEntry> parseCtab(const std::vector<uint8_t>& code) {
    std::vector<CtabEntry> out;
    const size_t words = code.size() / 4;
    const auto* t = reinterpret_cast<const uint32_t*>(code.data());
    for (size_t i = 1; i < words;) {
        uint32_t tok = t[i];
        if ((tok & 0xffff) == 0xfffe) {  // comment
            uint32_t len = tok >> 16;
            if (i + 1 < words && t[i + 1] == 0x42415443) {  // 'CTAB'
                const uint8_t* base = reinterpret_cast<const uint8_t*>(&t[i + 2]);
                const size_t avail = (len - 1) * 4;
                auto u32 = [&](size_t off) { uint32_t v = 0; if (off + 4 <= avail) std::memcpy(&v, base + off, 4); return v; };
                auto u16 = [&](size_t off) { uint16_t v = 0; if (off + 2 <= avail) std::memcpy(&v, base + off, 2); return v; };
                const uint32_t n = u32(12), infoOff = u32(16);
                for (uint32_t k = 0; k < n; ++k) {
                    size_t e = infoOff + k * 20;
                    uint32_t nameOff = u32(e);
                    std::string name;
                    for (size_t c = nameOff; c < avail && base[c]; ++c) name += static_cast<char>(base[c]);
                    out.push_back({name, u16(e + 4), u16(e + 6), u16(e + 8)});
                }
                return out;
            }
            i += 1 + len;
        } else {
            ++i;
        }
    }
    return out;
}

// From the HLSL text: block membership and byte offset of every packoffset'd constant, and the
// binding of each named block (cbuffer X : register(bN)). Strips the cbuffer registers (SM 3.0
// has no constant buffers).
struct SourceConst {
    std::string name;
    int block;
    uint32_t offset;
};

std::vector<SourceConst> parseSource(std::string& src) {
    std::vector<SourceConst> out;
    std::istringstream in(src);
    std::string line, rebuilt;
    int block = -2;  // -2: outside a cbuffer
    while (std::getline(in, line)) {
        size_t cb = line.find("cbuffer ");
        if (cb != std::string::npos && line.find("cbuffer") == line.find_first_not_of(" \t")) {
            std::string name = line.substr(cb + 8);
            size_t colon = name.find(':');
            int binding = 0;
            size_t reg = line.find("register(b");
            if (reg != std::string::npos) binding = std::atoi(line.c_str() + reg + 10);
            name = name.substr(0, colon);
            name.erase(name.find_last_not_of(" \t\r") + 1);
            block = name == "Push" ? -1 : binding;
            line = "cbuffer " + name;  // no register on SM 3.0
        } else if (block != -2 && line.find("};") != std::string::npos) {
            block = -2;
        } else if (block != -2) {
            size_t po = line.find("packoffset(c");
            size_t colon = line.find(':');
            if (po != std::string::npos && colon != std::string::npos) {
                std::string decl = line.substr(0, colon);
                decl.erase(decl.find_last_not_of(" \t") + 1);
                size_t br = decl.find('[');
                if (br != std::string::npos) decl = decl.substr(0, br);
                std::string name = decl.substr(decl.find_last_of(" \t") + 1);
                uint32_t reg = static_cast<uint32_t>(std::atoi(line.c_str() + po + 12));
                uint32_t comp = 0;
                size_t dot = line.find('.', po);
                if (dot != std::string::npos && dot < line.find(')', po)) comp = static_cast<uint32_t>(std::string("xyzw").find(line[dot + 1]));
                out.push_back({name, block, reg * 16 + comp * 4});
            }
        }
        rebuilt += line + "\n";
    }
    src = rebuilt;
    return out;
}

// Sampler bindings from the GLSL of the same shader: layout(binding = N) uniform sampler2D NAME;
int samplerBinding(const char* glsl, const std::string& name) {
    if (!glsl) return -1;
    std::string g = glsl;
    size_t at = g.find("uniform sampler2D " + name + ";");
    if (at == std::string::npos) return -1;
    size_t lb = g.rfind("binding = ", at);
    if (lb == std::string::npos || g.rfind('\n', at) > lb) return -1;
    return std::atoi(g.c_str() + lb + 10);
}

StageMap reflect(const std::vector<uint8_t>& code, const std::vector<SourceConst>& src, const char* glsl) {
    StageMap m;
    for (const CtabEntry& e : parseCtab(code)) {
        if (e.set == 3) {  // sampler
            int b = samplerBinding(glsl, e.name);
            if (b < 0) {  // hand-written shaders name their sampler register; use it as the binding
                b = e.reg;
            }
            if (b >= 0 && b < static_cast<int>(kMaxBindings)) m.samplerReg[b] = e.reg;
        } else if (e.set == 2) {  // float4
            if (e.name == "gl_HalfPixel") {
                m.halfPixelReg = e.reg;
                continue;
            }
            for (const SourceConst& s : src)
                if (s.name == e.name) m.consts.push_back({s.block, s.offset, e.reg, e.count});
        }
    }
    return m;
}

// ---------------------------------------------------------------------------------------------

class D3D9Device final : public Device {
public:
    D3D9Device(SDL_Window* window, const Options& opt);
    ~D3D9Device() override = default;

    Api api() const override { return Api::D3D9; }
    const DeviceInfo& info() const override { return info_; }
    uint32_t width() const override { return w_; }
    uint32_t height() const override { return h_; }
    bool supportsCompute() const override { return false; }

    bool surfaceHasArea() override {
        if (opt_.offscreen) return true;
        RECT r{};
        GetClientRect(hwnd_, &r);
        return r.right > r.left && r.bottom > r.top;
    }
    void resize() override;
    void waitIdle() override;

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

    Buffer createBuffer(BufferKind kind, size_t size, const void* data) override;
    void updateBuffer(Buffer b, const void* data, size_t size) override;
    Texture createTexture(Format f, uint32_t w, uint32_t h, uint32_t samples, bool renderTarget) override;
    Texture createDepth(uint32_t w, uint32_t h, uint32_t samples) override;
    Pipeline createPipeline(const PipelineDesc& d) override;
    Pipeline createCompute(const ComputeDesc&) override { throw Error("Direct3D 9 has no compute shaders"); }
    void destroy(Buffer b) override { buffers_.at(b.id - 1) = BufferRec{}; }
    void destroy(Texture t) override { textures_.at(t.id - 1) = TextureRec{}; }
    void destroy(Pipeline p) override { pipelines_.at(p.id - 1) = PipelineRec{}; }
    uint32_t textureWidth(Texture t) const override { return textures_.at(t.id - 1).w; }
    uint32_t textureHeight(Texture t) const override { return textures_.at(t.id - 1).h; }

    void beginPass(const PassDesc& p) override;
    void endPass() override;
    void setPipeline(Pipeline p) override;
    void setConstants(const void* data, uint32_t size) override { upload(-1, static_cast<const uint8_t*>(data), size); }
    void setUniform(uint32_t binding, Buffer b) override {
        const BufferRec& r = buffers_.at(b.id - 1);
        upload(static_cast<int>(binding), r.cpu.data(), static_cast<uint32_t>(r.cpu.size()));
    }
    void setTexture(uint32_t binding, Texture t) override;
    void setStorage(uint32_t, Buffer) override { throw Error("Direct3D 9 has no storage buffers"); }
    void setVertexBuffer(uint32_t slot, Buffer b) override;
    void setIndexBuffer(Buffer b) override { HR_CHECK(dev_->SetIndices(buffers_.at(b.id - 1).ib.get())); }
    void draw(uint32_t vertexCount, uint32_t instanceCount) override;
    void drawIndexed(uint32_t indexCount, uint32_t instanceCount, uint32_t firstInstance) override;
    void dispatch(uint32_t) override { throw Error("Direct3D 9 has no compute shaders"); }
    void storageToCompute() override {}
    void storageToVertex() override {}

private:
    void createBackbuffer();
    void upload(int block, const uint8_t* data, uint32_t size);
    void setHalfPixel();
    bool waitQuery(IDirect3DQuery9* q, void* data, DWORD size);
    void collectTimestamps(uint32_t slot);
    IDirect3DSurface9* backbuffer() const { return back_.get(); }

    SDL_Window* window_;
    HWND hwnd_ = nullptr;
    Options opt_;
    DeviceInfo info_;
    UINT adapter_ = D3DADAPTER_DEFAULT;
    Com<IDirect3D9Ex> d3d_;
    Com<IDirect3DDevice9Ex> dev_;
    D3DPRESENT_PARAMETERS pp_{};
    Com<IDirect3DSurface9> back_;
    bool srgbWrite_ = true;
    uint32_t w_ = 0, h_ = 0;
    uint32_t passW_ = 0, passH_ = 0;
    Com<IDirect3DVertexBuffer9> vertexIds_;
    uint32_t vertexIdCount_ = 0;

    uint32_t slot_ = 0;
    uint64_t frameCounter_ = 0;
    Com<IDirect3DQuery9> disjoint_[kFramesInFlight], freq_[kFramesInFlight], tsBegin_[kFramesInFlight],
        tsEnd_[kFramesInFlight], done_[kFramesInFlight];
    bool slotPending_[kFramesInFlight]{};
    bool slotMeasured_[kFramesInFlight]{};
    std::vector<double> gpuTimesMs_;

    std::vector<BufferRec> buffers_;
    std::vector<TextureRec> textures_;
    std::vector<PipelineRec> pipelines_;
    const PipelineRec* bound_ = nullptr;
    PassDesc pass_{};
    IDirect3DVertexBuffer9* stream0_ = nullptr;
    UINT stream0Stride_ = 0, stream0Vertices_ = 0;
    IDirect3DVertexBuffer9* instanceBuf_ = nullptr;
    UINT instanceStride_ = 0;
};

D3D9Device::D3D9Device(SDL_Window* window, const Options& opt) : window_(window), opt_(opt) {
    HR_CHECK(Direct3DCreate9Ex(D3D_SDK_VERSION, d3d_.put()));
    if (opt.gpuIndex >= 0) {
        if (static_cast<UINT>(opt.gpuIndex) >= d3d_->GetAdapterCount())
            throw Error("--gpu " + std::to_string(opt.gpuIndex) + " is out of range for D3D9 (see --list-gpus)");
        adapter_ = static_cast<UINT>(opt.gpuIndex);
    }
    D3DADAPTER_IDENTIFIER9 id{};
    HR_CHECK(d3d_->GetAdapterIdentifier(adapter_, 0, &id));
    info_.gpuIndex = static_cast<int>(adapter_);
    info_.gpuName = id.Description;
    info_.gpuType = "unknown";
    info_.vendorId = id.VendorId;
    info_.deviceId = id.DeviceId;
    info_.driverName = id.Driver;
    char ver[64];
    std::snprintf(ver, sizeof ver, "%u.%u.%u.%u", HIWORD(id.DriverVersion.HighPart), LOWORD(id.DriverVersion.HighPart),
                  HIWORD(id.DriverVersion.LowPart), LOWORD(id.DriverVersion.LowPart));
    info_.driverVersion = ver;
    info_.driverVersionRaw = static_cast<uint64_t>(id.DriverVersion.QuadPart);
    info_.apiVersion = "9Ex, shader model 3.0";
    info_.variant = "particles simulated in a pixel shader (no compute shaders), float hash for respawns";

    if (FAILED(d3d_->CheckDeviceFormat(adapter_, D3DDEVTYPE_HAL, D3DFMT_X8R8G8B8, D3DUSAGE_QUERY_VERTEXTEXTURE,
                                       D3DRTYPE_TEXTURE, D3DFMT_A32B32G32R32F)))
        throw Error("this D3D9 device has no vertex texture fetch for A32B32G32R32F");
    srgbWrite_ = SUCCEEDED(d3d_->CheckDeviceFormat(adapter_, D3DDEVTYPE_HAL, D3DFMT_X8R8G8B8, D3DUSAGE_QUERY_SRGBWRITE,
                                                   D3DRTYPE_SURFACE, D3DFMT_X8R8G8B8));
    info_.msaaSamples = SUCCEEDED(d3d_->CheckDeviceMultiSampleType(adapter_, D3DDEVTYPE_HAL, D3DFMT_X8R8G8B8, TRUE,
                                                                   D3DMULTISAMPLE_4_SAMPLES, nullptr)) &&
                                SUCCEEDED(d3d_->CheckDeviceMultiSampleType(adapter_, D3DDEVTYPE_HAL, D3DFMT_D24X8, TRUE,
                                                                           D3DMULTISAMPLE_4_SAMPLES, nullptr))
                            ? 4
                            : 1;

    hwnd_ = d3d::windowHandle(window);
    pp_.Windowed = TRUE;
    pp_.hDeviceWindow = hwnd_;
    if (opt.offscreen) {
        pp_.BackBufferWidth = pp_.BackBufferHeight = 1;
        pp_.BackBufferFormat = D3DFMT_X8R8G8B8;
        pp_.BackBufferCount = 1;
        pp_.SwapEffect = D3DSWAPEFFECT_DISCARD;
        pp_.PresentationInterval = D3DPRESENT_INTERVAL_IMMEDIATE;
        info_.presentMode = "offscreen";
    } else {
        pp_.BackBufferFormat = D3DFMT_X8R8G8B8;
        pp_.BackBufferCount = 3;
        pp_.SwapEffect = D3DSWAPEFFECT_FLIPEX;
        pp_.PresentationInterval = opt.present == Present::Fifo ? D3DPRESENT_INTERVAL_ONE : D3DPRESENT_INTERVAL_IMMEDIATE;
        info_.presentMode = opt.present == Present::Fifo ? "fifo (flipex)" : "immediate (flipex)";
    }
    HR_CHECK(d3d_->CreateDeviceEx(adapter_, D3DDEVTYPE_HAL, hwnd_,
                                  D3DCREATE_HARDWARE_VERTEXPROCESSING | D3DCREATE_FPU_PRESERVE, &pp_, nullptr,
                                  dev_.put()));
    dev_->SetMaximumFrameLatency(kFramesInFlight);
    info_.swapFormat = srgbWrite_ ? "X8R8G8B8 (sRGB writes)" : "X8R8G8B8 (shader encodes sRGB)";

    bool ts = true;
    for (uint32_t i = 0; i < kFramesInFlight && ts; ++i) {
        ts = SUCCEEDED(dev_->CreateQuery(D3DQUERYTYPE_TIMESTAMPDISJOINT, disjoint_[i].put())) &&
             SUCCEEDED(dev_->CreateQuery(D3DQUERYTYPE_TIMESTAMPFREQ, freq_[i].put())) &&
             SUCCEEDED(dev_->CreateQuery(D3DQUERYTYPE_TIMESTAMP, tsBegin_[i].put())) &&
             SUCCEEDED(dev_->CreateQuery(D3DQUERYTYPE_TIMESTAMP, tsEnd_[i].put()));
        HR_CHECK(dev_->CreateQuery(D3DQUERYTYPE_EVENT, done_[i].put()));
    }
    info_.timestamps = ts;
    createBackbuffer();
}

void D3D9Device::createBackbuffer() {
    back_.reset();
    if (opt_.offscreen) {
        w_ = opt_.width;
        h_ = opt_.height;
        HR_CHECK(dev_->CreateRenderTarget(w_, h_, D3DFMT_A8R8G8B8, D3DMULTISAMPLE_NONE, 0, FALSE, back_.put(), nullptr));
    } else {
        HR_CHECK(dev_->GetBackBuffer(0, 0, D3DBACKBUFFER_TYPE_MONO, back_.put()));
        D3DSURFACE_DESC d{};
        back_->GetDesc(&d);
        w_ = d.Width;
        h_ = d.Height;
    }
}

void D3D9Device::resize() {
    waitIdle();
    if (opt_.offscreen) return;
    back_.reset();
    pp_.BackBufferWidth = pp_.BackBufferHeight = 0;  // the window's client size
    HR_CHECK(dev_->ResetEx(&pp_, nullptr));
    dev_->SetMaximumFrameLatency(kFramesInFlight);
    createBackbuffer();
}

bool D3D9Device::waitQuery(IDirect3DQuery9* q, void* data, DWORD size) {
    const ULONGLONG start = GetTickCount64();
    for (;;) {
        HRESULT hr = q->GetData(data, size, D3DGETDATA_FLUSH);
        if (hr == S_OK) return true;
        if (FAILED(hr)) return false;  // e.g. the device was reset
        if (GetTickCount64() - start > kWaitMs) throw Error("D3D9: the GPU did not finish within 10 s");
        SwitchToThread();
    }
}

void D3D9Device::waitIdle() {
    Com<IDirect3DQuery9> q;
    HR_CHECK(dev_->CreateQuery(D3DQUERYTYPE_EVENT, q.put()));
    q->Issue(D3DISSUE_END);
    BOOL done = FALSE;
    waitQuery(q.get(), &done, sizeof done);
}

void D3D9Device::collectTimestamps(uint32_t slot) {
    if (!slotPending_[slot]) return;
    slotPending_[slot] = false;
    if (!info_.timestamps) return;
    BOOL disjoint = TRUE;
    UINT64 freq = 0, t0 = 0, t1 = 0;
    if (!waitQuery(disjoint_[slot].get(), &disjoint, sizeof disjoint) || !waitQuery(freq_[slot].get(), &freq, sizeof freq) ||
        !waitQuery(tsBegin_[slot].get(), &t0, sizeof t0) || !waitQuery(tsEnd_[slot].get(), &t1, sizeof t1))
        return;
    if (slotMeasured_[slot] && !disjoint && freq > 0 && t1 >= t0)
        gpuTimesMs_.push_back(static_cast<double>(t1 - t0) * 1000.0 / static_cast<double>(freq));
}

bool D3D9Device::beginFrame(bool measured) {
    slot_ = static_cast<uint32_t>(frameCounter_ % kFramesInFlight);
    if (frameCounter_ >= kFramesInFlight) {
        BOOL done = FALSE;
        waitQuery(done_[slot_].get(), &done, sizeof done);
    }
    collectTimestamps(slot_);
    slotMeasured_[slot_] = measured;
    HR_CHECK(dev_->BeginScene());
    if (info_.timestamps) {
        disjoint_[slot_]->Issue(D3DISSUE_BEGIN);
        tsBegin_[slot_]->Issue(D3DISSUE_END);
    }
    bound_ = nullptr;
    return true;
}

bool D3D9Device::endFrame(const std::string* capturePath) {
    if (info_.timestamps) {
        tsEnd_[slot_]->Issue(D3DISSUE_END);
        disjoint_[slot_]->Issue(D3DISSUE_END);
        freq_[slot_]->Issue(D3DISSUE_END);
    }
    done_[slot_]->Issue(D3DISSUE_END);
    HR_CHECK(dev_->EndScene());
    slotPending_[slot_] = true;
    if (capturePath) {
        Com<IDirect3DSurface9> sys;
        D3DSURFACE_DESC d{};
        back_->GetDesc(&d);
        HR_CHECK(dev_->CreateOffscreenPlainSurface(w_, h_, d.Format, D3DPOOL_SYSTEMMEM, sys.put(), nullptr));
        HR_CHECK(dev_->GetRenderTargetData(back_.get(), sys.get()));
        D3DLOCKED_RECT lr{};
        HR_CHECK(sys->LockRect(&lr, nullptr, D3DLOCK_READONLY));
        if (!writeBmp(*capturePath, static_cast<const uint8_t*>(lr.pBits), w_, h_, lr.Pitch, true))
            std::fprintf(stderr, "warning: could not write %s\n", capturePath->c_str());
        sys->UnlockRect();
    }
    ++frameCounter_;
    if (opt_.offscreen) return false;
    HRESULT hr = dev_->PresentEx(nullptr, nullptr, nullptr, nullptr, 0);
    if (hr == D3DERR_DEVICELOST || hr == D3DERR_DEVICEHUNG || hr == D3DERR_DEVICEREMOVED)
        d3d::failHr("IDirect3DDevice9Ex::PresentEx (device lost)", hr, __FILE__, __LINE__);
    if (FAILED(hr)) d3d::failHr("IDirect3DDevice9Ex::PresentEx", hr, __FILE__, __LINE__);
    return false;
}

void D3D9Device::finishFrames() {
    waitIdle();
    for (uint32_t i = 0; i < kFramesInFlight; ++i) collectTimestamps(i);
}

// ---------------------------------------------------------------------------------------------
// Resources

Buffer D3D9Device::createBuffer(BufferKind kind, size_t size, const void* data) {
    BufferRec r;
    r.alive = true;
    r.kind = kind;
    r.size = size;
    void* p = nullptr;
    switch (kind) {
        case BufferKind::Vertex:
            HR_CHECK(dev_->CreateVertexBuffer(static_cast<UINT>(size), D3DUSAGE_WRITEONLY, 0, D3DPOOL_DEFAULT, r.vb.put(), nullptr));
            if (data) {
                HR_CHECK(r.vb->Lock(0, 0, &p, 0));
                std::memcpy(p, data, size);
                r.vb->Unlock();
            }
            break;
        case BufferKind::Index:
            HR_CHECK(dev_->CreateIndexBuffer(static_cast<UINT>(size), D3DUSAGE_WRITEONLY, D3DFMT_INDEX32, D3DPOOL_DEFAULT,
                                             r.ib.put(), nullptr));
            if (data) {
                HR_CHECK(r.ib->Lock(0, 0, &p, 0));
                std::memcpy(p, data, size);
                r.ib->Unlock();
            }
            break;
        case BufferKind::Uniform:
            r.cpu.assign(size, 0);
            if (data) std::memcpy(r.cpu.data(), data, size);
            break;
        case BufferKind::Storage:
            throw Error("Direct3D 9 has no storage buffers");
    }
    buffers_.push_back(std::move(r));
    return Buffer{{static_cast<uint32_t>(buffers_.size())}};
}

void D3D9Device::updateBuffer(Buffer b, const void* data, size_t size) {
    BufferRec& r = buffers_.at(b.id - 1);
    std::memcpy(r.cpu.data(), data, std::min(size, r.cpu.size()));
}

Texture D3D9Device::createTexture(Format f, uint32_t w, uint32_t h, uint32_t samples, bool) {
    TextureRec r;
    r.alive = true;
    r.w = w;
    r.h = h;
    r.samples = samples;
    r.backbufferFormat = f == Format::Backbuffer;
    r.format = f == Format::RGBA8     ? D3DFMT_A8R8G8B8
             : f == Format::RGBA16F   ? D3DFMT_A16B16G16R16F
             : f == Format::RGBA32F   ? D3DFMT_A32B32G32R32F
                                      : D3DFMT_X8R8G8B8;
    if (samples > 1) {
        HR_CHECK(dev_->CreateRenderTarget(w, h, r.format, static_cast<D3DMULTISAMPLE_TYPE>(samples), 0, FALSE,
                                          r.surface.put(), nullptr));
    } else {
        HR_CHECK(dev_->CreateTexture(w, h, 1, D3DUSAGE_RENDERTARGET, r.format, D3DPOOL_DEFAULT, r.tex.put(), nullptr));
        HR_CHECK(r.tex->GetSurfaceLevel(0, r.surface.put()));
    }
    textures_.push_back(r);
    return Texture{{static_cast<uint32_t>(textures_.size())}};
}

Texture D3D9Device::createDepth(uint32_t w, uint32_t h, uint32_t samples) {
    TextureRec r;
    r.alive = true;
    r.w = w;
    r.h = h;
    r.samples = samples;
    r.format = D3DFMT_D24X8;
    HR_CHECK(dev_->CreateDepthStencilSurface(w, h, D3DFMT_D24X8, samples > 1 ? static_cast<D3DMULTISAMPLE_TYPE>(samples)
                                                                            : D3DMULTISAMPLE_NONE,
                                             0, TRUE, r.surface.put(), nullptr));
    textures_.push_back(r);
    return Texture{{static_cast<uint32_t>(textures_.size())}};
}

Pipeline D3D9Device::createPipeline(const PipelineDesc& d) {
    PipelineRec r;
    r.alive = true;
    r.desc = d;
    const ShaderSource& vss = shader(d.vs);
    const ShaderSource& pss = shader(d.fs);
    if (!vss.hlsl3 || !pss.hlsl3) throw Error(std::string("no shader model 3.0 version of ") + (vss.hlsl3 ? d.fs : d.vs));
    const bool manualGamma = !srgbWrite_;
    std::string vsrc = vss.hlsl3, psrc = pss.hlsl3;
    auto vconsts = parseSource(vsrc);
    auto pconsts = parseSource(psrc);
    auto vs = d3d::compileHlsl(vsrc.c_str(), d.vs, "vs_3_0", manualGamma, d.hdrOut, false);
    auto ps = d3d::compileHlsl(psrc.c_str(), d.fs, "ps_3_0", manualGamma, d.hdrOut, false);
    HR_CHECK(dev_->CreateVertexShader(reinterpret_cast<const DWORD*>(vs.data()), r.vs.put()));
    HR_CHECK(dev_->CreatePixelShader(reinterpret_cast<const DWORD*>(ps.data()), r.ps.put()));
    r.vsMap = reflect(vs, vconsts, vss.glsl);
    r.psMap = reflect(ps, pconsts, pss.glsl);

    std::vector<D3DVERTEXELEMENT9> el;
    if (d.attrs.empty()) {
        r.vertexIdStream = true;
        el.push_back({0, 0, D3DDECLTYPE_FLOAT1, D3DDECLMETHOD_DEFAULT, D3DDECLUSAGE_TEXCOORD, 7});
    } else {
        for (const VertexAttr& a : d.attrs) {
            BYTE type = a.components == 1 ? D3DDECLTYPE_FLOAT1
                      : a.components == 2 ? D3DDECLTYPE_FLOAT2
                      : a.components == 3 ? D3DDECLTYPE_FLOAT3
                                          : D3DDECLTYPE_FLOAT4;
            el.push_back({static_cast<WORD>(a.binding), static_cast<WORD>(a.offset), type, D3DDECLMETHOD_DEFAULT,
                          D3DDECLUSAGE_TEXCOORD, static_cast<BYTE>(a.location)});
        }
    }
    el.push_back(D3DDECL_END());
    HR_CHECK(dev_->CreateVertexDeclaration(el.data(), r.decl.put()));
    for (const VertexBinding& b : d.vertexBindings) r.strides.push_back(b.stride);
    pipelines_.push_back(r);
    return Pipeline{{static_cast<uint32_t>(pipelines_.size())}};
}

// ---------------------------------------------------------------------------------------------
// Commands

void D3D9Device::beginPass(const PassDesc& p) {
    pass_ = p;
    for (DWORD i = 0; i < kMaxBindings; ++i) {  // no target may stay bound as a texture
        dev_->SetTexture(i, nullptr);
        dev_->SetTexture(D3DVERTEXTEXTURESAMPLER0 + i, nullptr);
    }
    bool srgb = false;
    if (!p.color) {
        HR_CHECK(dev_->SetRenderTarget(0, backbuffer()));
        passW_ = w_;
        passH_ = h_;
        srgb = srgbWrite_;
    } else {
        const TextureRec& c = textures_.at(p.color.id - 1);
        HR_CHECK(dev_->SetRenderTarget(0, c.surface.get()));
        passW_ = c.w;
        passH_ = c.h;
        srgb = c.backbufferFormat && srgbWrite_;
    }
    HR_CHECK(dev_->SetRenderTarget(1, p.color2 ? textures_.at(p.color2.id - 1).surface.get() : nullptr));
    HR_CHECK(dev_->SetDepthStencilSurface(p.depth ? textures_.at(p.depth.id - 1).surface.get() : nullptr));
    dev_->SetRenderState(D3DRS_SRGBWRITEENABLE, srgb ? TRUE : FALSE);
    D3DVIEWPORT9 vp{0, 0, passW_, passH_, 0.0f, 1.0f};
    dev_->SetViewport(&vp);
    DWORD flags = (p.load == Load::Clear ? D3DCLEAR_TARGET : 0) | (p.depth ? D3DCLEAR_ZBUFFER : 0);
    if (flags) {
        // Clear ignores sRGB writes, so sRGB targets get the encoded color, as the other APIs do.
        auto encode = [&](float v) {
            v = std::clamp(v, 0.0f, 1.0f);
            if (srgb) v = v <= 0.0031308f ? v * 12.92f : 1.055f * std::pow(v, 1.0f / 2.4f) - 0.055f;
            return static_cast<int>(v * 255.0f + 0.5f);
        };
        dev_->Clear(0, nullptr, flags,
                    D3DCOLOR_ARGB(encode(p.clear[3]), encode(p.clear[0]), encode(p.clear[1]), encode(p.clear[2])), 1.0f, 0);
    }
}

void D3D9Device::endPass() {
    if (pass_.resolveToBackbuffer) {
        const TextureRec& c = textures_.at(pass_.color.id - 1);
        HR_CHECK(dev_->StretchRect(c.surface.get(), nullptr, backbuffer(), nullptr, D3DTEXF_NONE));
    }
}

void D3D9Device::setHalfPixel() {
    // D3D9 samples pixel centers at integer coordinates: shift by half a pixel. With SPIRV-Cross's
    // SM 3.0 formula (x -= hp.x * w, y += hp.y * w, then y flipped), (1/w, -1/h) matches Vulkan's
    // image exactly (checked against Vulkan captures).
    const float hp[4] = {1.0f / static_cast<float>(passW_), -1.0f / static_cast<float>(passH_), 0.0f, 0.0f};
    if (bound_->vsMap.halfPixelReg >= 0) dev_->SetVertexShaderConstantF(bound_->vsMap.halfPixelReg, hp, 1);
    if (bound_->psMap.halfPixelReg >= 0) dev_->SetPixelShaderConstantF(bound_->psMap.halfPixelReg, hp, 1);
}

void D3D9Device::setPipeline(Pipeline p) {
    bound_ = &pipelines_.at(p.id - 1);
    const PipelineDesc& d = bound_->desc;
    dev_->SetVertexShader(bound_->vs.get());
    dev_->SetPixelShader(bound_->ps.get());
    dev_->SetVertexDeclaration(bound_->decl.get());
    // Front faces are counter-clockwise on screen, so cull the clockwise ones.
    dev_->SetRenderState(D3DRS_CULLMODE, d.cullBack ? D3DCULL_CW : D3DCULL_NONE);
    dev_->SetRenderState(D3DRS_ZENABLE, d.depthTest ? D3DZB_TRUE : D3DZB_FALSE);
    dev_->SetRenderState(D3DRS_ZWRITEENABLE, d.depthWrite ? TRUE : FALSE);
    dev_->SetRenderState(D3DRS_ZFUNC, D3DCMP_LESSEQUAL);
    dev_->SetRenderState(D3DRS_ALPHABLENDENABLE, d.additive ? TRUE : FALSE);
    if (d.additive) {
        dev_->SetRenderState(D3DRS_SRCBLEND, D3DBLEND_ONE);
        dev_->SetRenderState(D3DRS_DESTBLEND, D3DBLEND_ONE);
        dev_->SetRenderState(D3DRS_BLENDOP, D3DBLENDOP_ADD);
    }
    dev_->SetRenderState(D3DRS_MULTISAMPLEANTIALIAS, d.samples > 1 ? TRUE : FALSE);
    setHalfPixel();
}

void D3D9Device::upload(int block, const uint8_t* data, uint32_t size) {
    for (int stage = 0; stage < 2; ++stage) {
        const StageMap& m = stage == 0 ? bound_->vsMap : bound_->psMap;
        for (const ConstSlot& c : m.consts) {
            if (c.block != block || c.offset >= size) continue;
            // Whole registers only; a partial last register is padded with zeros.
            float regs[4 * 32] = {};
            const uint32_t count = std::min<uint32_t>(c.count, 32);
            std::memcpy(regs, data + c.offset, std::min<size_t>(size - c.offset, count * 16));
            if (stage == 0) dev_->SetVertexShaderConstantF(c.reg, regs, count);
            else dev_->SetPixelShaderConstantF(c.reg, regs, count);
        }
    }
}

void D3D9Device::setTexture(uint32_t binding, Texture t) {
    const TextureRec& r = textures_.at(t.id - 1);
    // 32-bit float textures are read texel-exact (point); their filtering is optional in D3D9.
    const DWORD filter = r.format == D3DFMT_A32B32G32R32F ? D3DTEXF_POINT : D3DTEXF_LINEAR;
    auto bind = [&](DWORD sampler) {
        dev_->SetTexture(sampler, r.tex.get());
        dev_->SetSamplerState(sampler, D3DSAMP_MINFILTER, filter);
        dev_->SetSamplerState(sampler, D3DSAMP_MAGFILTER, filter);
        dev_->SetSamplerState(sampler, D3DSAMP_MIPFILTER, D3DTEXF_NONE);
        dev_->SetSamplerState(sampler, D3DSAMP_ADDRESSU, D3DTADDRESS_CLAMP);
        dev_->SetSamplerState(sampler, D3DSAMP_ADDRESSV, D3DTADDRESS_CLAMP);
        dev_->SetSamplerState(sampler, D3DSAMP_SRGBTEXTURE, FALSE);
    };
    if (bound_->psMap.samplerReg[binding] >= 0) bind(static_cast<DWORD>(bound_->psMap.samplerReg[binding]));
    if (bound_->vsMap.samplerReg[binding] >= 0)
        bind(D3DVERTEXTEXTURESAMPLER0 + static_cast<DWORD>(bound_->vsMap.samplerReg[binding]));
}

void D3D9Device::setVertexBuffer(uint32_t slot, Buffer b) {
    const BufferRec& r = buffers_.at(b.id - 1);
    const UINT stride = bound_->strides.at(slot);
    if (bound_->desc.vertexBindings.at(slot).perInstance) {
        instanceBuf_ = r.vb.get();
        instanceStride_ = stride;
    } else {
        stream0_ = r.vb.get();
        stream0Stride_ = stride;
        stream0Vertices_ = static_cast<UINT>(r.size / stride);
        HR_CHECK(dev_->SetStreamSource(slot, r.vb.get(), 0, stride));
    }
}

void D3D9Device::draw(uint32_t vertexCount, uint32_t instanceCount) {
    if (instanceCount != 1) throw Error("D3D9: non-indexed instanced draws are not supported");
    if (bound_->vertexIdStream) {
        if (vertexIdCount_ < vertexCount) {
            vertexIds_.reset();
            HR_CHECK(dev_->CreateVertexBuffer(vertexCount * 4, D3DUSAGE_WRITEONLY, 0, D3DPOOL_DEFAULT, vertexIds_.put(), nullptr));
            float* p = nullptr;
            HR_CHECK(vertexIds_->Lock(0, 0, reinterpret_cast<void**>(&p), 0));
            for (uint32_t i = 0; i < vertexCount; ++i) p[i] = static_cast<float>(i);
            vertexIds_->Unlock();
            vertexIdCount_ = vertexCount;
        }
        HR_CHECK(dev_->SetStreamSource(0, vertexIds_.get(), 0, 4));
    }
    HR_CHECK(dev_->DrawPrimitive(D3DPT_TRIANGLELIST, 0, vertexCount / 3));
}

void D3D9Device::drawIndexed(uint32_t indexCount, uint32_t instanceCount, uint32_t firstInstance) {
    if (instanceBuf_) {
        HR_CHECK(dev_->SetStreamSource(1, instanceBuf_, firstInstance * instanceStride_, instanceStride_));
        HR_CHECK(dev_->SetStreamSourceFreq(0, D3DSTREAMSOURCE_INDEXEDDATA | instanceCount));
        HR_CHECK(dev_->SetStreamSourceFreq(1, D3DSTREAMSOURCE_INSTANCEDATA | 1u));
    }
    HR_CHECK(dev_->DrawIndexedPrimitive(D3DPT_TRIANGLELIST, 0, 0, stream0Vertices_, 0, indexCount / 3));
    if (instanceBuf_) {
        dev_->SetStreamSourceFreq(0, 1);
        dev_->SetStreamSourceFreq(1, 1);
    }
}

}  // namespace

std::unique_ptr<Device> createD3D9(SDL_Window* window, const Options& opt) {
    return std::make_unique<D3D9Device>(window, opt);
}

int listD3D9() {
    Com<IDirect3D9Ex> d3d;
    HR_CHECK(Direct3DCreate9Ex(D3D_SDK_VERSION, d3d.put()));
    std::printf("D3D9 adapters (one per display output):\n");
    for (UINT i = 0; i < d3d->GetAdapterCount(); ++i) {
        D3DADAPTER_IDENTIFIER9 id{};
        if (FAILED(d3d->GetAdapterIdentifier(i, 0, &id))) continue;
        std::printf("  [%u] %s on %s, vendor %s device %s\n", i, id.Description, id.DeviceName, hex4(id.VendorId).c_str(),
                    hex4(id.DeviceId).c_str());
    }
    return 0;
}

}  // namespace gfx
