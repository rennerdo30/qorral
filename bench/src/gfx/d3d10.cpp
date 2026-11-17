// Direct3D 10.1 backend: feature level 10_1 (10_0 as fallback), HLSL shader model 4.0 from
// SPIRV-Cross. There are no compute shaders, so the particle scene runs its texture variant.
// d3d10_1.dll is loaded at run time (MinGW has no import library for it).
#include "gfx/d3d_common.h"

#include <d3d10_1.h>

#include <algorithm>
#include <cstdio>
#include <cstring>

namespace gfx {
namespace {

using d3d::Com;

constexpr uint64_t kWaitTimeoutMs = 10000;

using CreateDevice1Fn = HRESULT(WINAPI*)(IDXGIAdapter*, D3D10_DRIVER_TYPE, HMODULE, UINT, D3D10_FEATURE_LEVEL1, UINT,
                                         ID3D10Device1**);

struct BufferRec {
    bool alive = false;
    BufferKind kind{};
    size_t size = 0;
    Com<ID3D10Buffer> buf;
};

struct TextureRec {
    bool alive = false;
    Com<ID3D10Texture2D> tex;
    Com<ID3D10RenderTargetView> rtv;
    Com<ID3D10ShaderResourceView> srv;
    Com<ID3D10DepthStencilView> dsv;
    uint32_t w = 0, h = 0, samples = 1;
};

struct PipelineRec {
    bool alive = false;
    Com<ID3D10VertexShader> vs;
    Com<ID3D10PixelShader> ps;
    Com<ID3D10InputLayout> layout;
    Com<ID3D10RasterizerState> raster;
    Com<ID3D10BlendState> blend;
    Com<ID3D10DepthStencilState> depth;
    std::vector<UINT> strides;
};

class D3D10Device final : public Device {
public:
    D3D10Device(SDL_Window* window, const Options& opt);
    ~D3D10Device() override {
        if (dev_) {
            dev_->ClearState();
            dev_->Flush();
        }
    }

    Api api() const override { return Api::D3D10; }
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
    double waitPresented(std::string& source) override;

    Buffer createBuffer(BufferKind kind, size_t size, const void* data) override;
    void updateBuffer(Buffer b, const void* data, size_t size) override;
    Texture createTexture(Format f, uint32_t w, uint32_t h, uint32_t samples, bool renderTarget) override;
    Texture createDepth(uint32_t w, uint32_t h, uint32_t samples) override;
    Pipeline createPipeline(const PipelineDesc& d) override;
    Pipeline createCompute(const ComputeDesc&) override { throw Error("Direct3D 10 has no compute shaders"); }
    void destroy(Buffer b) override { buffers_.at(b.id - 1) = BufferRec{}; }
    void destroy(Texture t) override { textures_.at(t.id - 1) = TextureRec{}; }
    void destroy(Pipeline p) override { pipelines_.at(p.id - 1) = PipelineRec{}; }
    uint32_t textureWidth(Texture t) const override { return textures_.at(t.id - 1).w; }
    uint32_t textureHeight(Texture t) const override { return textures_.at(t.id - 1).h; }

    void beginPass(const PassDesc& p) override;
    void endPass() override;
    void setPipeline(Pipeline p) override;
    void setConstants(const void* data, uint32_t size) override;
    void setUniform(uint32_t binding, Buffer b) override {
        ID3D10Buffer* cb = buffers_.at(b.id - 1).buf.get();
        dev_->VSSetConstantBuffers(binding, 1, &cb);
        dev_->PSSetConstantBuffers(binding, 1, &cb);
    }
    void setTexture(uint32_t binding, Texture t) override {
        ID3D10ShaderResourceView* srv = textures_.at(t.id - 1).srv.get();
        ID3D10SamplerState* s = sampler_.get();
        dev_->VSSetShaderResources(binding, 1, &srv);
        dev_->PSSetShaderResources(binding, 1, &srv);
        dev_->VSSetSamplers(binding, 1, &s);
        dev_->PSSetSamplers(binding, 1, &s);
    }
    void setStorage(uint32_t, Buffer) override { throw Error("Direct3D 10 has no storage buffers"); }
    void setVertexBuffer(uint32_t slot, Buffer b) override {
        ID3D10Buffer* vb = buffers_.at(b.id - 1).buf.get();
        UINT stride = bound_->strides.at(slot), offset = 0;
        dev_->IASetVertexBuffers(slot, 1, &vb, &stride, &offset);
    }
    void setIndexBuffer(Buffer b) override { dev_->IASetIndexBuffer(buffers_.at(b.id - 1).buf.get(), DXGI_FORMAT_R32_UINT, 0); }
    void draw(uint32_t vertexCount, uint32_t instanceCount) override { dev_->DrawInstanced(vertexCount, instanceCount, 0, 0); }
    void drawIndexed(uint32_t indexCount, uint32_t instanceCount, uint32_t firstInstance) override {
        dev_->DrawIndexedInstanced(indexCount, instanceCount, 0, 0, firstInstance);
    }
    void dispatch(uint32_t) override { throw Error("Direct3D 10 has no compute shaders"); }
    void storageToCompute() override {}
    void storageToVertex() override {}

private:
    void createBackbuffer();
    void collectTimestamps(uint32_t slot);
    bool waitQuery(ID3D10Query* q, void* data, UINT size);

    SDL_Window* window_;
    HWND hwnd_ = nullptr;
    Options opt_;
    DeviceInfo info_;
    Com<IDXGIFactory1> factory_;
    Com<ID3D10Device1> dev_;
    Com<IDXGISwapChain> swap_;
    bool flipModel_ = false, tearing_ = false;
    UINT syncInterval_ = 0, presentFlags_ = 0;
    Com<ID3D10Texture2D> backTex_;
    Com<ID3D10RenderTargetView> backRtv_;
    uint32_t w_ = 0, h_ = 0;
    Com<ID3D10SamplerState> sampler_;
    Com<ID3D10Buffer> constBuf_;

    uint32_t slot_ = 0;
    uint64_t frameCounter_ = 0;
    LONGLONG frameStartQpc_ = 0;
    int statsFailures_ = 0;  // frame statistics unavailable after 3 failures in a row
    Com<ID3D10Query> disjoint_[kFramesInFlight], tsBegin_[kFramesInFlight], tsEnd_[kFramesInFlight], done_[kFramesInFlight];
    bool slotPending_[kFramesInFlight]{};
    bool slotMeasured_[kFramesInFlight]{};
    std::vector<double> gpuTimesMs_;

    std::vector<BufferRec> buffers_;
    std::vector<TextureRec> textures_;
    std::vector<PipelineRec> pipelines_;
    const PipelineRec* bound_ = nullptr;
    PassDesc pass_{};
};

D3D10Device::D3D10Device(SDL_Window* window, const Options& opt) : window_(window), opt_(opt) {
    HMODULE lib = LoadLibraryExW(L"d3d10_1.dll", nullptr, LOAD_LIBRARY_SEARCH_SYSTEM32);
    if (!lib) throw Error("d3d10_1.dll is not available");
    auto create = reinterpret_cast<CreateDevice1Fn>(reinterpret_cast<void*>(GetProcAddress(lib, "D3D10CreateDevice1")));
    if (!create) throw Error("d3d10_1.dll has no D3D10CreateDevice1");
    HR_CHECK(CreateDXGIFactory1(__uuidof(IDXGIFactory1), reinterpret_cast<void**>(factory_.put())));
    Com<IDXGIAdapter1> adapter = d3d::pickAdapter(factory_.get(), opt.gpuIndex, info_);
    UINT flags = D3D10_CREATE_DEVICE_BGRA_SUPPORT;
    HRESULT hr = E_FAIL;
    if (opt.validate) {
        hr = create(adapter.get(), D3D10_DRIVER_TYPE_HARDWARE, nullptr, flags | D3D10_CREATE_DEVICE_DEBUG,
                    D3D10_FEATURE_LEVEL_10_1, D3D10_1_SDK_VERSION, dev_.put());
        if (SUCCEEDED(hr)) info_.validation = true;
        else std::fprintf(stderr, "warning: --validate given but the D3D10 debug layer is not installed\n");
    }
    D3D10_FEATURE_LEVEL1 level = D3D10_FEATURE_LEVEL_10_1;
    if (FAILED(hr)) hr = create(adapter.get(), D3D10_DRIVER_TYPE_HARDWARE, nullptr, flags, level, D3D10_1_SDK_VERSION, dev_.put());
    if (FAILED(hr)) {
        level = D3D10_FEATURE_LEVEL_10_0;
        HR_CHECK(create(adapter.get(), D3D10_DRIVER_TYPE_HARDWARE, nullptr, flags, level, D3D10_1_SDK_VERSION, dev_.put()));
    }
    info_.apiVersion = level == D3D10_FEATURE_LEVEL_10_1 ? "10_1" : "10_0";
    info_.variant = "particles simulated in a pixel shader (no compute shaders)";
    UINT q = 0;
    info_.msaaSamples = SUCCEEDED(dev_->CheckMultisampleQualityLevels(DXGI_FORMAT_R8G8B8A8_UNORM_SRGB, 4, &q)) && q > 0 ? 4 : 1;
    info_.timestamps = true;
    if (auto dxgiDev = dev_.as<IDXGIDevice1>()) dxgiDev->SetMaximumFrameLatency(kFramesInFlight);

    D3D10_SAMPLER_DESC sd{};
    sd.Filter = D3D10_FILTER_MIN_MAG_MIP_LINEAR;
    sd.AddressU = sd.AddressV = sd.AddressW = D3D10_TEXTURE_ADDRESS_CLAMP;
    sd.MaxLOD = 0.0f;
    HR_CHECK(dev_->CreateSamplerState(&sd, sampler_.put()));
    D3D10_BUFFER_DESC cb{};
    cb.ByteWidth = 256;
    cb.Usage = D3D10_USAGE_DYNAMIC;
    cb.BindFlags = D3D10_BIND_CONSTANT_BUFFER;
    cb.CPUAccessFlags = D3D10_CPU_ACCESS_WRITE;
    HR_CHECK(dev_->CreateBuffer(&cb, nullptr, constBuf_.put()));
    for (uint32_t i = 0; i < kFramesInFlight; ++i) {
        D3D10_QUERY_DESC qd{D3D10_QUERY_TIMESTAMP_DISJOINT, 0};
        HR_CHECK(dev_->CreateQuery(&qd, disjoint_[i].put()));
        qd.Query = D3D10_QUERY_TIMESTAMP;
        HR_CHECK(dev_->CreateQuery(&qd, tsBegin_[i].put()));
        HR_CHECK(dev_->CreateQuery(&qd, tsEnd_[i].put()));
        qd.Query = D3D10_QUERY_EVENT;
        HR_CHECK(dev_->CreateQuery(&qd, done_[i].put()));
    }

    if (opt.offscreen) {
        info_.presentMode = "offscreen";
    } else {
        hwnd_ = d3d::windowHandle(window);
        // The flip model, as for D3D11 and D3D12, when DXGI 1.2 accepts this device; else the classic model.
        Com<IDXGIFactory2> f2 = factory_.as<IDXGIFactory2>();
        tearing_ = d3d::tearingSupported(factory_.get());
        if (f2) {
            DXGI_SWAP_CHAIN_DESC1 scd{};
            scd.Format = DXGI_FORMAT_R8G8B8A8_UNORM;
            scd.SampleDesc.Count = 1;
            scd.BufferUsage = DXGI_USAGE_RENDER_TARGET_OUTPUT;
            scd.BufferCount = 3;
            scd.SwapEffect = DXGI_SWAP_EFFECT_FLIP_DISCARD;
            scd.Flags = tearing_ ? DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING : 0;
            Com<IDXGISwapChain1> sc1;
            if (SUCCEEDED(f2->CreateSwapChainForHwnd(dev_.get(), hwnd_, &scd, nullptr, nullptr, sc1.put()))) {
                swap_ = sc1.as<IDXGISwapChain>();
                flipModel_ = true;
            }
        }
        if (!swap_) {
            tearing_ = false;
            DXGI_SWAP_CHAIN_DESC scd{};
            scd.BufferDesc.Format = DXGI_FORMAT_R8G8B8A8_UNORM;
            scd.SampleDesc.Count = 1;
            scd.BufferUsage = DXGI_USAGE_RENDER_TARGET_OUTPUT;
            scd.BufferCount = 1;
            scd.OutputWindow = hwnd_;
            scd.Windowed = TRUE;
            scd.SwapEffect = DXGI_SWAP_EFFECT_DISCARD;
            HR_CHECK(factory_->CreateSwapChain(dev_.get(), &scd, swap_.put()));
        }
        factory_->MakeWindowAssociation(hwnd_, DXGI_MWA_NO_ALT_ENTER);
        if (opt.present == Present::Fifo) {
            syncInterval_ = 1;
            info_.presentMode = "fifo";
        } else if (tearing_ && opt.present == Present::Immediate) {
            presentFlags_ = DXGI_PRESENT_ALLOW_TEARING;
            info_.presentMode = "immediate";
        } else {
            info_.presentMode = flipModel_ ? "mailbox" : "immediate (blit model)";
        }
    }
    info_.swapFormat = "R8G8B8A8_UNORM_SRGB";
    createBackbuffer();
}

void D3D10Device::createBackbuffer() {
    backRtv_.reset();
    backTex_.reset();
    if (opt_.offscreen) {
        w_ = opt_.width;
        h_ = opt_.height;
        D3D10_TEXTURE2D_DESC td{};
        td.Width = w_;
        td.Height = h_;
        td.MipLevels = td.ArraySize = 1;
        td.Format = DXGI_FORMAT_R8G8B8A8_TYPELESS;
        td.SampleDesc.Count = 1;
        td.BindFlags = D3D10_BIND_RENDER_TARGET;
        HR_CHECK(dev_->CreateTexture2D(&td, nullptr, backTex_.put()));
    } else {
        HR_CHECK(swap_->GetBuffer(0, __uuidof(ID3D10Texture2D), reinterpret_cast<void**>(backTex_.put())));
        D3D10_TEXTURE2D_DESC td{};
        backTex_->GetDesc(&td);
        w_ = td.Width;
        h_ = td.Height;
    }
    D3D10_RENDER_TARGET_VIEW_DESC rd{};
    rd.Format = DXGI_FORMAT_R8G8B8A8_UNORM_SRGB;
    rd.ViewDimension = D3D10_RTV_DIMENSION_TEXTURE2D;
    HR_CHECK(dev_->CreateRenderTargetView(backTex_.get(), &rd, backRtv_.put()));
}

void D3D10Device::resize() {
    waitIdle();
    if (opt_.offscreen) return;
    dev_->OMSetRenderTargets(0, nullptr, nullptr);
    backRtv_.reset();
    backTex_.reset();
    HR_CHECK(swap_->ResizeBuffers(0, 0, 0, DXGI_FORMAT_UNKNOWN, tearing_ ? DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING : 0));
    createBackbuffer();
}

bool D3D10Device::waitQuery(ID3D10Query* q, void* data, UINT size) {
    const ULONGLONG start = GetTickCount64();
    while (q->GetData(data, size, 0) != S_OK) {
        if (GetTickCount64() - start > kWaitTimeoutMs) throw Error("D3D10: the GPU did not finish within 10 s");
        SwitchToThread();
    }
    return true;
}

void D3D10Device::waitIdle() {
    Com<ID3D10Query> q;
    D3D10_QUERY_DESC qd{D3D10_QUERY_EVENT, 0};
    HR_CHECK(dev_->CreateQuery(&qd, q.put()));
    q->End();
    BOOL done = FALSE;
    waitQuery(q.get(), &done, sizeof done);
}

void D3D10Device::collectTimestamps(uint32_t slot) {
    if (!slotPending_[slot]) return;
    D3D10_QUERY_DATA_TIMESTAMP_DISJOINT dj{};
    UINT64 t0 = 0, t1 = 0;
    waitQuery(disjoint_[slot].get(), &dj, sizeof dj);
    waitQuery(tsBegin_[slot].get(), &t0, sizeof t0);
    waitQuery(tsEnd_[slot].get(), &t1, sizeof t1);
    slotPending_[slot] = false;
    if (slotMeasured_[slot] && !dj.Disjoint && dj.Frequency > 0 && t1 >= t0)
        gpuTimesMs_.push_back(static_cast<double>(t1 - t0) * 1000.0 / static_cast<double>(dj.Frequency));
}

bool D3D10Device::beginFrame(bool measured) {
    frameStartQpc_ = d3d::qpcNow();
    slot_ = static_cast<uint32_t>(frameCounter_ % kFramesInFlight);
    if (frameCounter_ >= kFramesInFlight) {
        BOOL done = FALSE;
        waitQuery(done_[slot_].get(), &done, sizeof done);
    }
    collectTimestamps(slot_);
    slotMeasured_[slot_] = measured;
    disjoint_[slot_]->Begin();
    tsBegin_[slot_]->End();
    bound_ = nullptr;
    return true;
}

bool D3D10Device::endFrame(const std::string* capturePath) {
    tsEnd_[slot_]->End();
    disjoint_[slot_]->End();
    done_[slot_]->End();
    slotPending_[slot_] = true;
    if (capturePath) {
        D3D10_TEXTURE2D_DESC td{};
        backTex_->GetDesc(&td);
        td.Usage = D3D10_USAGE_STAGING;
        td.BindFlags = 0;
        td.CPUAccessFlags = D3D10_CPU_ACCESS_READ;
        td.MiscFlags = 0;
        Com<ID3D10Texture2D> staging;
        HR_CHECK(dev_->CreateTexture2D(&td, nullptr, staging.put()));
        dev_->CopyResource(staging.get(), backTex_.get());
        D3D10_MAPPED_TEXTURE2D m{};
        HR_CHECK(staging->Map(0, D3D10_MAP_READ, 0, &m));
        if (!writeBmp(*capturePath, static_cast<const uint8_t*>(m.pData), w_, h_, m.RowPitch, false))
            std::fprintf(stderr, "warning: could not write %s\n", capturePath->c_str());
        staging->Unmap(0);
    }
    ++frameCounter_;
    if (opt_.offscreen) {
        dev_->Flush();
        return false;
    }
    HRESULT hr = swap_->Present(syncInterval_, presentFlags_);
    if (hr == DXGI_ERROR_DEVICE_REMOVED || hr == DXGI_ERROR_DEVICE_RESET)
        d3d::failHr("IDXGISwapChain::Present (device removed)", dev_->GetDeviceRemovedReason(), __FILE__, __LINE__);
    if (FAILED(hr)) d3d::failHr("IDXGISwapChain::Present", hr, __FILE__, __LINE__);
    return false;
}

double D3D10Device::waitPresented(std::string& source) {
    // GPU first, so a failed statistics wait does not add to the fallback time.
    waitIdle();
    const LONGLONG gpuDone = d3d::qpcNow();
    double ms = 0.0;
    if (!opt_.offscreen && statsFailures_ < 3) {
        if (d3d::waitDxgiDisplayed(swap_.get(), frameStartQpc_, ms)) {
            statsFailures_ = 0;
            source = "display";
            return ms;
        }
        ++statsFailures_;
    }
    source = "gpu";
    return d3d::qpcMs(frameStartQpc_, gpuDone);
}

void D3D10Device::finishFrames() {
    waitIdle();
    for (uint32_t i = 0; i < kFramesInFlight; ++i) collectTimestamps(i);
}

Buffer D3D10Device::createBuffer(BufferKind kind, size_t size, const void* data) {
    if (kind == BufferKind::Storage) throw Error("Direct3D 10 has no storage buffers");
    BufferRec r;
    r.alive = true;
    r.kind = kind;
    r.size = size;
    D3D10_BUFFER_DESC bd{};
    bd.ByteWidth = static_cast<UINT>(kind == BufferKind::Uniform ? (size + 15) & ~size_t(15) : size);
    if (kind == BufferKind::Uniform) {
        bd.Usage = D3D10_USAGE_DYNAMIC;
        bd.BindFlags = D3D10_BIND_CONSTANT_BUFFER;
        bd.CPUAccessFlags = D3D10_CPU_ACCESS_WRITE;
    } else {
        bd.Usage = D3D10_USAGE_IMMUTABLE;
        bd.BindFlags = kind == BufferKind::Vertex ? D3D10_BIND_VERTEX_BUFFER : D3D10_BIND_INDEX_BUFFER;
    }
    D3D10_SUBRESOURCE_DATA init{data, 0, 0};
    HR_CHECK(dev_->CreateBuffer(&bd, data ? &init : nullptr, r.buf.put()));
    buffers_.push_back(r);
    return Buffer{{static_cast<uint32_t>(buffers_.size())}};
}

void D3D10Device::updateBuffer(Buffer b, const void* data, size_t size) {
    BufferRec& r = buffers_.at(b.id - 1);
    void* p = nullptr;
    HR_CHECK(r.buf->Map(D3D10_MAP_WRITE_DISCARD, 0, &p));
    std::memcpy(p, data, std::min(size, r.size));
    r.buf->Unmap();
}

Texture D3D10Device::createTexture(Format f, uint32_t w, uint32_t h, uint32_t samples, bool) {
    TextureRec r;
    r.alive = true;
    r.w = w;
    r.h = h;
    r.samples = samples;
    DXGI_FORMAT res = f == Format::RGBA8     ? DXGI_FORMAT_R8G8B8A8_UNORM
                    : f == Format::RGBA16F   ? DXGI_FORMAT_R16G16B16A16_FLOAT
                    : f == Format::RGBA32F   ? DXGI_FORMAT_R32G32B32A32_FLOAT
                                             : DXGI_FORMAT_R8G8B8A8_TYPELESS;
    DXGI_FORMAT view = f == Format::Backbuffer ? DXGI_FORMAT_R8G8B8A8_UNORM_SRGB : res;
    D3D10_TEXTURE2D_DESC td{};
    td.Width = w;
    td.Height = h;
    td.MipLevels = td.ArraySize = 1;
    td.Format = res;
    td.SampleDesc.Count = samples;
    td.BindFlags = D3D10_BIND_RENDER_TARGET | (samples == 1 ? D3D10_BIND_SHADER_RESOURCE : 0);
    HR_CHECK(dev_->CreateTexture2D(&td, nullptr, r.tex.put()));
    D3D10_RENDER_TARGET_VIEW_DESC rd{};
    rd.Format = view;
    rd.ViewDimension = samples > 1 ? D3D10_RTV_DIMENSION_TEXTURE2DMS : D3D10_RTV_DIMENSION_TEXTURE2D;
    HR_CHECK(dev_->CreateRenderTargetView(r.tex.get(), &rd, r.rtv.put()));
    if (samples == 1) {
        D3D10_SHADER_RESOURCE_VIEW_DESC sv{};
        sv.Format = view;
        sv.ViewDimension = D3D10_SRV_DIMENSION_TEXTURE2D;
        sv.Texture2D.MipLevels = 1;
        HR_CHECK(dev_->CreateShaderResourceView(r.tex.get(), &sv, r.srv.put()));
    }
    textures_.push_back(r);
    return Texture{{static_cast<uint32_t>(textures_.size())}};
}

Texture D3D10Device::createDepth(uint32_t w, uint32_t h, uint32_t samples) {
    TextureRec r;
    r.alive = true;
    r.w = w;
    r.h = h;
    r.samples = samples;
    D3D10_TEXTURE2D_DESC td{};
    td.Width = w;
    td.Height = h;
    td.MipLevels = td.ArraySize = 1;
    td.Format = DXGI_FORMAT_D32_FLOAT;
    td.SampleDesc.Count = samples;
    td.BindFlags = D3D10_BIND_DEPTH_STENCIL;
    HR_CHECK(dev_->CreateTexture2D(&td, nullptr, r.tex.put()));
    D3D10_DEPTH_STENCIL_VIEW_DESC dd{};
    dd.Format = DXGI_FORMAT_D32_FLOAT;
    dd.ViewDimension = samples > 1 ? D3D10_DSV_DIMENSION_TEXTURE2DMS : D3D10_DSV_DIMENSION_TEXTURE2D;
    HR_CHECK(dev_->CreateDepthStencilView(r.tex.get(), &dd, r.dsv.put()));
    textures_.push_back(r);
    return Texture{{static_cast<uint32_t>(textures_.size())}};
}

Pipeline D3D10Device::createPipeline(const PipelineDesc& d) {
    PipelineRec r;
    r.alive = true;
    auto vs = d3d::compileHlsl(shader(d.vs).hlsl4, d.vs, "vs_4_0", false, d.hdrOut);
    auto ps = d3d::compileHlsl(shader(d.fs).hlsl4, d.fs, "ps_4_0", false, d.hdrOut);
    HR_CHECK(dev_->CreateVertexShader(vs.data(), vs.size(), r.vs.put()));
    HR_CHECK(dev_->CreatePixelShader(ps.data(), ps.size(), r.ps.put()));
    if (!d.attrs.empty()) {
        std::vector<D3D10_INPUT_ELEMENT_DESC> el;
        for (const VertexAttr& a : d.attrs) {
            const bool inst = d.vertexBindings.at(a.binding).perInstance;
            DXGI_FORMAT f = a.components == 1 ? DXGI_FORMAT_R32_FLOAT
                          : a.components == 2 ? DXGI_FORMAT_R32G32_FLOAT
                          : a.components == 3 ? DXGI_FORMAT_R32G32B32_FLOAT
                                              : DXGI_FORMAT_R32G32B32A32_FLOAT;
            el.push_back({"TEXCOORD", a.location, f, a.binding, a.offset,
                          inst ? D3D10_INPUT_PER_INSTANCE_DATA : D3D10_INPUT_PER_VERTEX_DATA, inst ? 1u : 0u});
        }
        HR_CHECK(dev_->CreateInputLayout(el.data(), static_cast<UINT>(el.size()), vs.data(), vs.size(), r.layout.put()));
    }
    for (const VertexBinding& b : d.vertexBindings) r.strides.push_back(b.stride);
    D3D10_RASTERIZER_DESC rs{};
    rs.FillMode = D3D10_FILL_SOLID;
    rs.CullMode = d.cullBack ? D3D10_CULL_BACK : D3D10_CULL_NONE;
    rs.FrontCounterClockwise = TRUE;
    rs.DepthClipEnable = TRUE;
    rs.MultisampleEnable = d.samples > 1;
    HR_CHECK(dev_->CreateRasterizerState(&rs, r.raster.put()));
    D3D10_BLEND_DESC bd{};
    for (auto& m : bd.RenderTargetWriteMask) m = D3D10_COLOR_WRITE_ENABLE_ALL;
    if (d.additive) {
        for (auto& e : bd.BlendEnable) e = TRUE;
        bd.SrcBlend = bd.DestBlend = D3D10_BLEND_ONE;
        bd.BlendOp = D3D10_BLEND_OP_ADD;
        bd.SrcBlendAlpha = bd.DestBlendAlpha = D3D10_BLEND_ONE;
        bd.BlendOpAlpha = D3D10_BLEND_OP_ADD;
    } else {
        bd.SrcBlend = bd.SrcBlendAlpha = D3D10_BLEND_ONE;
        bd.DestBlend = bd.DestBlendAlpha = D3D10_BLEND_ZERO;
        bd.BlendOp = bd.BlendOpAlpha = D3D10_BLEND_OP_ADD;
    }
    HR_CHECK(dev_->CreateBlendState(&bd, r.blend.put()));
    D3D10_DEPTH_STENCIL_DESC ds{};
    ds.DepthEnable = d.depthTest;
    ds.DepthWriteMask = d.depthWrite ? D3D10_DEPTH_WRITE_MASK_ALL : D3D10_DEPTH_WRITE_MASK_ZERO;
    ds.DepthFunc = D3D10_COMPARISON_LESS_EQUAL;
    HR_CHECK(dev_->CreateDepthStencilState(&ds, r.depth.put()));
    pipelines_.push_back(r);
    return Pipeline{{static_cast<uint32_t>(pipelines_.size())}};
}

void D3D10Device::beginPass(const PassDesc& p) {
    pass_ = p;
    ID3D10ShaderResourceView* none[kMaxBindings] = {};
    dev_->VSSetShaderResources(0, kMaxBindings, none);
    dev_->PSSetShaderResources(0, kMaxBindings, none);
    ID3D10RenderTargetView* rtv[2] = {};
    ID3D10DepthStencilView* dsv = nullptr;
    uint32_t w = w_, h = h_, count = 1;
    if (!p.color) {
        rtv[0] = backRtv_.get();
    } else {
        const TextureRec& c = textures_.at(p.color.id - 1);
        rtv[0] = c.rtv.get();
        w = c.w;
        h = c.h;
    }
    if (p.color2) {
        rtv[1] = textures_.at(p.color2.id - 1).rtv.get();
        count = 2;
    }
    if (p.depth) dsv = textures_.at(p.depth.id - 1).dsv.get();
    dev_->OMSetRenderTargets(count, rtv, dsv);
    D3D10_VIEWPORT vp{0, 0, w, h, 0.0f, 1.0f};
    dev_->RSSetViewports(1, &vp);
    if (p.load == Load::Clear)
        for (uint32_t i = 0; i < count; ++i) dev_->ClearRenderTargetView(rtv[i], p.clear);
    if (dsv) dev_->ClearDepthStencilView(dsv, D3D10_CLEAR_DEPTH, 1.0f, 0);
}

void D3D10Device::endPass() {
    if (!pass_.resolveToBackbuffer) return;
    const TextureRec& c = textures_.at(pass_.color.id - 1);
    dev_->ResolveSubresource(backTex_.get(), 0, c.tex.get(), 0,
                             opt_.offscreen ? DXGI_FORMAT_R8G8B8A8_UNORM_SRGB : DXGI_FORMAT_R8G8B8A8_UNORM);
}

void D3D10Device::setPipeline(Pipeline p) {
    bound_ = &pipelines_.at(p.id - 1);
    dev_->IASetPrimitiveTopology(D3D10_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
    dev_->IASetInputLayout(bound_->layout.get());
    dev_->VSSetShader(bound_->vs.get());
    dev_->PSSetShader(bound_->ps.get());
    dev_->RSSetState(bound_->raster.get());
    dev_->OMSetBlendState(bound_->blend.get(), nullptr, 0xffffffff);
    dev_->OMSetDepthStencilState(bound_->depth.get(), 0);
}

void D3D10Device::setConstants(const void* data, uint32_t size) {
    void* p = nullptr;
    HR_CHECK(constBuf_->Map(D3D10_MAP_WRITE_DISCARD, 0, &p));
    std::memcpy(p, data, std::min<uint32_t>(size, 256));
    constBuf_->Unmap();
    ID3D10Buffer* cb = constBuf_.get();
    dev_->VSSetConstantBuffers(d3d::kPushRegister, 1, &cb);
    dev_->PSSetConstantBuffers(d3d::kPushRegister, 1, &cb);
}

}  // namespace

std::unique_ptr<Device> createD3D10(SDL_Window* window, const Options& opt) {
    return std::make_unique<D3D10Device>(window, opt);
}

}  // namespace gfx
