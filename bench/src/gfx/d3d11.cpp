// Direct3D 11 backend: feature level 11_0+, flip-model swapchain (tearing for "immediate"),
// HLSL shader model 5.0 from SPIRV-Cross. Storage buffers are raw buffers (ByteAddressBuffer).
#include "gfx/d3d_common.h"

#include <d3d11_1.h>

#include <algorithm>
#include <cstdio>
#include <cstring>

namespace gfx {
namespace {

using d3d::Com;

constexpr uint64_t kWaitTimeoutMs = 10000;

struct BufferRec {
    bool alive = false;
    BufferKind kind{};
    size_t size = 0;
    Com<ID3D11Buffer> buf;
    Com<ID3D11ShaderResourceView> srv;   // Storage
    Com<ID3D11UnorderedAccessView> uav;  // Storage
};

struct TextureRec {
    bool alive = false;
    Com<ID3D11Texture2D> tex;
    Com<ID3D11RenderTargetView> rtv[1];
    Com<ID3D11ShaderResourceView> srv;
    Com<ID3D11DepthStencilView> dsv;
    DXGI_FORMAT format{};
    uint32_t w = 0, h = 0, samples = 1;
};

struct PipelineRec {
    bool alive = false;
    bool compute = false;
    Com<ID3D11VertexShader> vs;
    Com<ID3D11PixelShader> ps;
    Com<ID3D11ComputeShader> cs;
    Com<ID3D11InputLayout> layout;
    Com<ID3D11RasterizerState> raster;
    Com<ID3D11BlendState> blend;
    Com<ID3D11DepthStencilState> depth;
    std::vector<UINT> strides;
    Res res[kMaxBindings]{};
};

class D3D11Device final : public Device {
public:
    D3D11Device(SDL_Window* window, const Options& opt);
    ~D3D11Device() override;

    Api api() const override { return Api::D3D11; }
    const DeviceInfo& info() const override { return info_; }
    uint32_t width() const override { return w_; }
    uint32_t height() const override { return h_; }
    bool supportsCompute() const override { return true; }
    uint32_t validationErrors() const override;

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
    Pipeline createCompute(const ComputeDesc& d) override;
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
        ID3D11Buffer* cb = buffers_.at(b.id - 1).buf.get();
        ctx_->VSSetConstantBuffers(binding, 1, &cb);
        ctx_->PSSetConstantBuffers(binding, 1, &cb);
    }
    void setTexture(uint32_t binding, Texture t) override {
        ID3D11ShaderResourceView* srv = textures_.at(t.id - 1).srv.get();
        ID3D11SamplerState* s = sampler_.get();
        ctx_->VSSetShaderResources(binding, 1, &srv);
        ctx_->PSSetShaderResources(binding, 1, &srv);
        ctx_->VSSetSamplers(binding, 1, &s);
        ctx_->PSSetSamplers(binding, 1, &s);
    }
    void setStorage(uint32_t binding, Buffer b) override;
    void setVertexBuffer(uint32_t slot, Buffer b) override {
        ID3D11Buffer* vb = buffers_.at(b.id - 1).buf.get();
        UINT stride = bound_->strides.at(slot), offset = 0;
        ctx_->IASetVertexBuffers(slot, 1, &vb, &stride, &offset);
    }
    void setIndexBuffer(Buffer b) override {
        ctx_->IASetIndexBuffer(buffers_.at(b.id - 1).buf.get(), DXGI_FORMAT_R32_UINT, 0);
    }
    void draw(uint32_t vertexCount, uint32_t instanceCount) override {
        ctx_->DrawInstanced(vertexCount, instanceCount, 0, 0);
    }
    void drawIndexed(uint32_t indexCount, uint32_t instanceCount, uint32_t firstInstance) override {
        ctx_->DrawIndexedInstanced(indexCount, instanceCount, 0, 0, firstInstance);
    }
    void dispatch(uint32_t groupsX) override { ctx_->Dispatch(groupsX, 1, 1); }
    void storageToCompute() override {
        ID3D11ShaderResourceView* none[kMaxBindings] = {};
        ctx_->VSSetShaderResources(0, kMaxBindings, none);
    }
    void storageToVertex() override {
        ID3D11UnorderedAccessView* none[kMaxBindings] = {};
        ctx_->CSSetUnorderedAccessViews(0, kMaxBindings, none, nullptr);
    }

private:
    void createBackbuffer();
    void collectTimestamps(uint32_t slot, bool wait);
    void unbindSrvs();
    ID3D11RenderTargetView* backbufferRtv() const { return backRtv_.get(); }

    SDL_Window* window_;
    HWND hwnd_ = nullptr;
    Options opt_;
    DeviceInfo info_;
    Com<IDXGIFactory2> factory_;
    Com<ID3D11Device> dev_;
    Com<ID3D11DeviceContext> ctx_;
    Com<ID3D11InfoQueue> infoQueue_;
    Com<IDXGISwapChain1> swap_;
    bool tearing_ = false;
    UINT syncInterval_ = 0, presentFlags_ = 0;
    Com<ID3D11Texture2D> backTex_;
    Com<ID3D11RenderTargetView> backRtv_;
    DXGI_FORMAT backFormat_ = DXGI_FORMAT_R8G8B8A8_UNORM;  // the resource; views are _SRGB
    uint32_t w_ = 0, h_ = 0;
    Com<ID3D11SamplerState> sampler_;
    Com<ID3D11Buffer> constBuf_;

    uint32_t slot_ = 0;
    uint64_t frameCounter_ = 0;
    Com<ID3D11Query> disjoint_[kFramesInFlight], tsBegin_[kFramesInFlight], tsEnd_[kFramesInFlight], done_[kFramesInFlight];
    bool slotPending_[kFramesInFlight]{};
    bool slotMeasured_[kFramesInFlight]{};
    std::vector<double> gpuTimesMs_;

    std::vector<BufferRec> buffers_;
    std::vector<TextureRec> textures_;
    std::vector<PipelineRec> pipelines_;
    const PipelineRec* bound_ = nullptr;
    PassDesc pass_{};
};

D3D11Device::D3D11Device(SDL_Window* window, const Options& opt) : window_(window), opt_(opt) {
    HR_CHECK(CreateDXGIFactory1(__uuidof(IDXGIFactory2), reinterpret_cast<void**>(factory_.put())));
    Com<IDXGIAdapter1> adapter = d3d::pickAdapter(factory_.get(), opt.gpuIndex, info_);
    const D3D_FEATURE_LEVEL levels[] = {D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0};
    D3D_FEATURE_LEVEL got{};
    UINT flags = D3D11_CREATE_DEVICE_BGRA_SUPPORT;
    HRESULT hr = E_FAIL;
    if (opt.validate) {
        hr = D3D11CreateDevice(adapter.get(), D3D_DRIVER_TYPE_UNKNOWN, nullptr, flags | D3D11_CREATE_DEVICE_DEBUG, levels, 2,
                               D3D11_SDK_VERSION, dev_.put(), &got, ctx_.put());
        if (SUCCEEDED(hr)) info_.validation = true;
        else std::fprintf(stderr, "warning: --validate given but the D3D11 debug layer is not installed\n");
    }
    if (FAILED(hr))
        HR_CHECK(D3D11CreateDevice(adapter.get(), D3D_DRIVER_TYPE_UNKNOWN, nullptr, flags, levels, 2, D3D11_SDK_VERSION,
                                   dev_.put(), &got, ctx_.put()));
    if (info_.validation) infoQueue_ = dev_.as<ID3D11InfoQueue>();
    info_.apiVersion = got == D3D_FEATURE_LEVEL_11_1 ? "11_1" : "11_0";
    UINT q = 0;
    info_.msaaSamples = SUCCEEDED(dev_->CheckMultisampleQualityLevels(DXGI_FORMAT_R8G8B8A8_UNORM_SRGB, 4, &q)) && q > 0 ? 4 : 1;
    info_.timestamps = true;
    if (auto dxgiDev = dev_.as<IDXGIDevice1>()) dxgiDev->SetMaximumFrameLatency(kFramesInFlight);

    D3D11_SAMPLER_DESC sd{};
    sd.Filter = D3D11_FILTER_MIN_MAG_MIP_LINEAR;
    sd.AddressU = sd.AddressV = sd.AddressW = D3D11_TEXTURE_ADDRESS_CLAMP;
    sd.MaxLOD = 0.0f;
    HR_CHECK(dev_->CreateSamplerState(&sd, sampler_.put()));
    D3D11_BUFFER_DESC cb{};
    cb.ByteWidth = 256;
    cb.Usage = D3D11_USAGE_DYNAMIC;
    cb.BindFlags = D3D11_BIND_CONSTANT_BUFFER;
    cb.CPUAccessFlags = D3D11_CPU_ACCESS_WRITE;
    HR_CHECK(dev_->CreateBuffer(&cb, nullptr, constBuf_.put()));
    for (uint32_t i = 0; i < kFramesInFlight; ++i) {
        D3D11_QUERY_DESC qd{D3D11_QUERY_TIMESTAMP_DISJOINT, 0};
        HR_CHECK(dev_->CreateQuery(&qd, disjoint_[i].put()));
        qd.Query = D3D11_QUERY_TIMESTAMP;
        HR_CHECK(dev_->CreateQuery(&qd, tsBegin_[i].put()));
        HR_CHECK(dev_->CreateQuery(&qd, tsEnd_[i].put()));
        qd.Query = D3D11_QUERY_EVENT;
        HR_CHECK(dev_->CreateQuery(&qd, done_[i].put()));
    }

    if (opt.offscreen) {
        info_.presentMode = "offscreen";
    } else {
        hwnd_ = d3d::windowHandle(window);
        tearing_ = d3d::tearingSupported(factory_.get());
        DXGI_SWAP_CHAIN_DESC1 scd{};
        scd.Format = backFormat_;
        scd.SampleDesc.Count = 1;
        scd.BufferUsage = DXGI_USAGE_RENDER_TARGET_OUTPUT;
        scd.BufferCount = 3;
        scd.SwapEffect = DXGI_SWAP_EFFECT_FLIP_DISCARD;
        scd.Flags = tearing_ ? DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING : 0;
        HR_CHECK(factory_->CreateSwapChainForHwnd(dev_.get(), hwnd_, &scd, nullptr, nullptr, swap_.put()));
        factory_->MakeWindowAssociation(hwnd_, DXGI_MWA_NO_ALT_ENTER);
        if (opt.present == Present::Fifo) {
            syncInterval_ = 1;
            info_.presentMode = "fifo";
        } else if (opt.present == Present::Immediate && tearing_) {
            presentFlags_ = DXGI_PRESENT_ALLOW_TEARING;
            info_.presentMode = "immediate";
        } else {
            info_.presentMode = "mailbox";  // flip model, interval 0 without tearing: the newest frame wins
        }
    }
    info_.swapFormat = "R8G8B8A8_UNORM_SRGB";
    createBackbuffer();
}

D3D11Device::~D3D11Device() {
    if (ctx_) {
        ctx_->ClearState();
        ctx_->Flush();
    }
}

uint32_t D3D11Device::validationErrors() const {
    if (!infoQueue_) return 0;
    uint32_t errors = 0;
    UINT64 n = infoQueue_->GetNumStoredMessages();
    for (UINT64 i = 0; i < n; ++i) {
        SIZE_T len = 0;
        infoQueue_->GetMessage(i, nullptr, &len);
        std::vector<char> buf(len);
        auto* m = reinterpret_cast<D3D11_MESSAGE*>(buf.data());
        if (SUCCEEDED(infoQueue_->GetMessage(i, m, &len)) &&
            (m->Severity == D3D11_MESSAGE_SEVERITY_ERROR || m->Severity == D3D11_MESSAGE_SEVERITY_CORRUPTION)) {
            ++errors;
            std::fprintf(stderr, "[d3d11 error] %.*s\n", static_cast<int>(m->DescriptionByteLength), m->pDescription);
        }
    }
    return errors;
}

void D3D11Device::createBackbuffer() {
    backRtv_.reset();
    backTex_.reset();
    if (opt_.offscreen) {
        w_ = opt_.width;
        h_ = opt_.height;
        D3D11_TEXTURE2D_DESC td{};
        td.Width = w_;
        td.Height = h_;
        td.MipLevels = td.ArraySize = 1;
        td.Format = DXGI_FORMAT_R8G8B8A8_TYPELESS;
        td.SampleDesc.Count = 1;
        td.BindFlags = D3D11_BIND_RENDER_TARGET;
        HR_CHECK(dev_->CreateTexture2D(&td, nullptr, backTex_.put()));
    } else {
        HR_CHECK(swap_->GetBuffer(0, __uuidof(ID3D11Texture2D), reinterpret_cast<void**>(backTex_.put())));
        D3D11_TEXTURE2D_DESC td{};
        backTex_->GetDesc(&td);
        w_ = td.Width;
        h_ = td.Height;
    }
    D3D11_RENDER_TARGET_VIEW_DESC rd{};
    rd.Format = DXGI_FORMAT_R8G8B8A8_UNORM_SRGB;
    rd.ViewDimension = D3D11_RTV_DIMENSION_TEXTURE2D;
    HR_CHECK(dev_->CreateRenderTargetView(backTex_.get(), &rd, backRtv_.put()));
}

void D3D11Device::resize() {
    waitIdle();
    if (opt_.offscreen) return;
    ctx_->OMSetRenderTargets(0, nullptr, nullptr);
    backRtv_.reset();
    backTex_.reset();
    ctx_->Flush();
    HR_CHECK(swap_->ResizeBuffers(0, 0, 0, DXGI_FORMAT_UNKNOWN, tearing_ ? DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING : 0));
    createBackbuffer();
}

void D3D11Device::waitIdle() {
    Com<ID3D11Query> q;
    D3D11_QUERY_DESC qd{D3D11_QUERY_EVENT, 0};
    HR_CHECK(dev_->CreateQuery(&qd, q.put()));
    ctx_->End(q.get());
    BOOL done = FALSE;
    const ULONGLONG start = GetTickCount64();
    while (ctx_->GetData(q.get(), &done, sizeof done, 0) != S_OK) {
        if (GetTickCount64() - start > kWaitTimeoutMs) throw Error("D3D11: the GPU did not finish within 10 s");
        SwitchToThread();
    }
}

// ---------------------------------------------------------------------------------------------
// Frames

void D3D11Device::collectTimestamps(uint32_t slot, bool wait) {
    if (!slotPending_[slot]) return;
    D3D11_QUERY_DATA_TIMESTAMP_DISJOINT dj{};
    const ULONGLONG start = GetTickCount64();
    for (;;) {
        HRESULT hr = ctx_->GetData(disjoint_[slot].get(), &dj, sizeof dj, 0);
        if (hr == S_OK) break;
        if (!wait) return;
        if (GetTickCount64() - start > kWaitTimeoutMs) throw Error("D3D11: timestamp results did not arrive within 10 s");
        SwitchToThread();
    }
    UINT64 t0 = 0, t1 = 0;
    while (ctx_->GetData(tsBegin_[slot].get(), &t0, sizeof t0, 0) != S_OK) SwitchToThread();
    while (ctx_->GetData(tsEnd_[slot].get(), &t1, sizeof t1, 0) != S_OK) SwitchToThread();
    slotPending_[slot] = false;
    if (slotMeasured_[slot] && !dj.Disjoint && dj.Frequency > 0 && t1 >= t0)
        gpuTimesMs_.push_back(static_cast<double>(t1 - t0) * 1000.0 / static_cast<double>(dj.Frequency));
}

bool D3D11Device::beginFrame(bool measured) {
    slot_ = static_cast<uint32_t>(frameCounter_ % kFramesInFlight);
    if (frameCounter_ >= kFramesInFlight) {
        // At most two frames in flight, as in the other backends.
        BOOL done = FALSE;
        const ULONGLONG start = GetTickCount64();
        while (ctx_->GetData(done_[slot_].get(), &done, sizeof done, 0) != S_OK) {
            if (GetTickCount64() - start > kWaitTimeoutMs) throw Error("D3D11: the GPU did not finish a frame within 10 s");
            SwitchToThread();
        }
    }
    collectTimestamps(slot_, true);
    slotMeasured_[slot_] = measured;
    ctx_->Begin(disjoint_[slot_].get());
    ctx_->End(tsBegin_[slot_].get());
    bound_ = nullptr;
    return true;
}

bool D3D11Device::endFrame(const std::string* capturePath) {
    ctx_->End(tsEnd_[slot_].get());
    ctx_->End(disjoint_[slot_].get());
    ctx_->End(done_[slot_].get());
    slotPending_[slot_] = true;
    if (capturePath) {
        D3D11_TEXTURE2D_DESC td{};
        backTex_->GetDesc(&td);
        td.Usage = D3D11_USAGE_STAGING;
        td.BindFlags = 0;
        td.CPUAccessFlags = D3D11_CPU_ACCESS_READ;
        td.MiscFlags = 0;
        Com<ID3D11Texture2D> staging;
        HR_CHECK(dev_->CreateTexture2D(&td, nullptr, staging.put()));
        ctx_->CopyResource(staging.get(), backTex_.get());
        D3D11_MAPPED_SUBRESOURCE m{};
        HR_CHECK(ctx_->Map(staging.get(), 0, D3D11_MAP_READ, 0, &m));
        if (!writeBmp(*capturePath, static_cast<const uint8_t*>(m.pData), w_, h_, m.RowPitch, false))
            std::fprintf(stderr, "warning: could not write %s\n", capturePath->c_str());
        ctx_->Unmap(staging.get(), 0);
    }
    ++frameCounter_;
    if (opt_.offscreen) {
        ctx_->Flush();
        return false;
    }
    HRESULT hr = swap_->Present(syncInterval_, presentFlags_);
    if (hr == DXGI_ERROR_DEVICE_REMOVED || hr == DXGI_ERROR_DEVICE_RESET)
        d3d::failHr("IDXGISwapChain::Present (device removed)", dev_->GetDeviceRemovedReason(), __FILE__, __LINE__);
    if (FAILED(hr)) d3d::failHr("IDXGISwapChain::Present", hr, __FILE__, __LINE__);
    return false;
}

void D3D11Device::finishFrames() {
    waitIdle();
    for (uint32_t i = 0; i < kFramesInFlight; ++i) collectTimestamps(i, true);
}

// ---------------------------------------------------------------------------------------------
// Resources

Buffer D3D11Device::createBuffer(BufferKind kind, size_t size, const void* data) {
    BufferRec r;
    r.alive = true;
    r.kind = kind;
    r.size = size;
    D3D11_BUFFER_DESC bd{};
    bd.ByteWidth = static_cast<UINT>(kind == BufferKind::Uniform ? (size + 15) & ~size_t(15) : size);
    D3D11_SUBRESOURCE_DATA init{data, 0, 0};
    switch (kind) {
        case BufferKind::Vertex:
        case BufferKind::Index:
            bd.Usage = D3D11_USAGE_IMMUTABLE;
            bd.BindFlags = kind == BufferKind::Vertex ? D3D11_BIND_VERTEX_BUFFER : D3D11_BIND_INDEX_BUFFER;
            break;
        case BufferKind::Uniform:
            bd.Usage = D3D11_USAGE_DYNAMIC;
            bd.BindFlags = D3D11_BIND_CONSTANT_BUFFER;
            bd.CPUAccessFlags = D3D11_CPU_ACCESS_WRITE;
            break;
        case BufferKind::Storage:
            bd.Usage = D3D11_USAGE_DEFAULT;
            bd.BindFlags = D3D11_BIND_SHADER_RESOURCE | D3D11_BIND_UNORDERED_ACCESS;
            bd.MiscFlags = D3D11_RESOURCE_MISC_BUFFER_ALLOW_RAW_VIEWS;
            break;
    }
    HR_CHECK(dev_->CreateBuffer(&bd, data ? &init : nullptr, r.buf.put()));
    if (kind == BufferKind::Storage) {
        D3D11_SHADER_RESOURCE_VIEW_DESC sv{};
        sv.Format = DXGI_FORMAT_R32_TYPELESS;
        sv.ViewDimension = D3D11_SRV_DIMENSION_BUFFEREX;
        sv.BufferEx.NumElements = static_cast<UINT>(size / 4);
        sv.BufferEx.Flags = D3D11_BUFFEREX_SRV_FLAG_RAW;
        HR_CHECK(dev_->CreateShaderResourceView(r.buf.get(), &sv, r.srv.put()));
        D3D11_UNORDERED_ACCESS_VIEW_DESC uv{};
        uv.Format = DXGI_FORMAT_R32_TYPELESS;
        uv.ViewDimension = D3D11_UAV_DIMENSION_BUFFER;
        uv.Buffer.NumElements = static_cast<UINT>(size / 4);
        uv.Buffer.Flags = D3D11_BUFFER_UAV_FLAG_RAW;
        HR_CHECK(dev_->CreateUnorderedAccessView(r.buf.get(), &uv, r.uav.put()));
    }
    buffers_.push_back(r);
    return Buffer{{static_cast<uint32_t>(buffers_.size())}};
}

void D3D11Device::updateBuffer(Buffer b, const void* data, size_t size) {
    BufferRec& r = buffers_.at(b.id - 1);
    D3D11_MAPPED_SUBRESOURCE m{};
    HR_CHECK(ctx_->Map(r.buf.get(), 0, D3D11_MAP_WRITE_DISCARD, 0, &m));
    std::memcpy(m.pData, data, std::min(size, r.size));
    ctx_->Unmap(r.buf.get(), 0);
}

Texture D3D11Device::createTexture(Format f, uint32_t w, uint32_t h, uint32_t samples, bool) {
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
    r.format = view;
    D3D11_TEXTURE2D_DESC td{};
    td.Width = w;
    td.Height = h;
    td.MipLevels = td.ArraySize = 1;
    td.Format = res;
    td.SampleDesc.Count = samples;
    td.BindFlags = D3D11_BIND_RENDER_TARGET | (samples == 1 ? D3D11_BIND_SHADER_RESOURCE : 0);
    HR_CHECK(dev_->CreateTexture2D(&td, nullptr, r.tex.put()));
    D3D11_RENDER_TARGET_VIEW_DESC rd{};
    rd.Format = view;
    rd.ViewDimension = samples > 1 ? D3D11_RTV_DIMENSION_TEXTURE2DMS : D3D11_RTV_DIMENSION_TEXTURE2D;
    HR_CHECK(dev_->CreateRenderTargetView(r.tex.get(), &rd, r.rtv[0].put()));
    if (samples == 1) {
        D3D11_SHADER_RESOURCE_VIEW_DESC sv{};
        sv.Format = view;
        sv.ViewDimension = D3D11_SRV_DIMENSION_TEXTURE2D;
        sv.Texture2D.MipLevels = 1;
        HR_CHECK(dev_->CreateShaderResourceView(r.tex.get(), &sv, r.srv.put()));
    }
    textures_.push_back(r);
    return Texture{{static_cast<uint32_t>(textures_.size())}};
}

Texture D3D11Device::createDepth(uint32_t w, uint32_t h, uint32_t samples) {
    TextureRec r;
    r.alive = true;
    r.w = w;
    r.h = h;
    r.samples = samples;
    D3D11_TEXTURE2D_DESC td{};
    td.Width = w;
    td.Height = h;
    td.MipLevels = td.ArraySize = 1;
    td.Format = DXGI_FORMAT_D32_FLOAT;
    td.SampleDesc.Count = samples;
    td.BindFlags = D3D11_BIND_DEPTH_STENCIL;
    HR_CHECK(dev_->CreateTexture2D(&td, nullptr, r.tex.put()));
    D3D11_DEPTH_STENCIL_VIEW_DESC dd{};
    dd.Format = DXGI_FORMAT_D32_FLOAT;
    dd.ViewDimension = samples > 1 ? D3D11_DSV_DIMENSION_TEXTURE2DMS : D3D11_DSV_DIMENSION_TEXTURE2D;
    HR_CHECK(dev_->CreateDepthStencilView(r.tex.get(), &dd, r.dsv.put()));
    textures_.push_back(r);
    return Texture{{static_cast<uint32_t>(textures_.size())}};
}

Pipeline D3D11Device::createPipeline(const PipelineDesc& d) {
    PipelineRec r;
    r.alive = true;
    std::copy(d.res, d.res + kMaxBindings, r.res);
    auto vs = d3d::compileHlsl(shader(d.vs).hlsl5, d.vs, "vs_5_0", false, d.hdrOut);
    auto ps = d3d::compileHlsl(shader(d.fs).hlsl5, d.fs, "ps_5_0", false, d.hdrOut);
    HR_CHECK(dev_->CreateVertexShader(vs.data(), vs.size(), nullptr, r.vs.put()));
    HR_CHECK(dev_->CreatePixelShader(ps.data(), ps.size(), nullptr, r.ps.put()));
    if (!d.attrs.empty()) {
        std::vector<D3D11_INPUT_ELEMENT_DESC> el;
        for (const VertexAttr& a : d.attrs) {
            const bool inst = d.vertexBindings.at(a.binding).perInstance;
            DXGI_FORMAT f = a.components == 1 ? DXGI_FORMAT_R32_FLOAT
                          : a.components == 2 ? DXGI_FORMAT_R32G32_FLOAT
                          : a.components == 3 ? DXGI_FORMAT_R32G32B32_FLOAT
                                              : DXGI_FORMAT_R32G32B32A32_FLOAT;
            el.push_back({"TEXCOORD", a.location, f, a.binding, a.offset,
                          inst ? D3D11_INPUT_PER_INSTANCE_DATA : D3D11_INPUT_PER_VERTEX_DATA, inst ? 1u : 0u});
        }
        HR_CHECK(dev_->CreateInputLayout(el.data(), static_cast<UINT>(el.size()), vs.data(), vs.size(), r.layout.put()));
    }
    for (const VertexBinding& b : d.vertexBindings) r.strides.push_back(b.stride);
    D3D11_RASTERIZER_DESC rs{};
    rs.FillMode = D3D11_FILL_SOLID;
    rs.CullMode = d.cullBack ? D3D11_CULL_BACK : D3D11_CULL_NONE;
    rs.FrontCounterClockwise = TRUE;  // same image as Vulkan, same winding rule
    rs.DepthClipEnable = TRUE;
    rs.MultisampleEnable = d.samples > 1;
    HR_CHECK(dev_->CreateRasterizerState(&rs, r.raster.put()));
    D3D11_BLEND_DESC bd{};
    bd.IndependentBlendEnable = FALSE;
    auto& rt = bd.RenderTarget[0];
    rt.RenderTargetWriteMask = D3D11_COLOR_WRITE_ENABLE_ALL;
    if (d.additive) {
        rt.BlendEnable = TRUE;
        rt.SrcBlend = rt.DestBlend = D3D11_BLEND_ONE;
        rt.BlendOp = D3D11_BLEND_OP_ADD;
        rt.SrcBlendAlpha = rt.DestBlendAlpha = D3D11_BLEND_ONE;
        rt.BlendOpAlpha = D3D11_BLEND_OP_ADD;
    }
    HR_CHECK(dev_->CreateBlendState(&bd, r.blend.put()));
    D3D11_DEPTH_STENCIL_DESC ds{};
    ds.DepthEnable = d.depthTest;
    ds.DepthWriteMask = d.depthWrite ? D3D11_DEPTH_WRITE_MASK_ALL : D3D11_DEPTH_WRITE_MASK_ZERO;
    ds.DepthFunc = D3D11_COMPARISON_LESS_EQUAL;
    HR_CHECK(dev_->CreateDepthStencilState(&ds, r.depth.put()));
    pipelines_.push_back(r);
    return Pipeline{{static_cast<uint32_t>(pipelines_.size())}};
}

Pipeline D3D11Device::createCompute(const ComputeDesc& d) {
    PipelineRec r;
    r.alive = true;
    r.compute = true;
    std::copy(d.res, d.res + kMaxBindings, r.res);
    auto cs = d3d::compileHlsl(shader(d.cs).hlsl5, d.cs, "cs_5_0", false, false);
    HR_CHECK(dev_->CreateComputeShader(cs.data(), cs.size(), nullptr, r.cs.put()));
    pipelines_.push_back(r);
    return Pipeline{{static_cast<uint32_t>(pipelines_.size())}};
}

// ---------------------------------------------------------------------------------------------
// Commands

void D3D11Device::unbindSrvs() {
    ID3D11ShaderResourceView* none[kMaxBindings] = {};
    ctx_->VSSetShaderResources(0, kMaxBindings, none);
    ctx_->PSSetShaderResources(0, kMaxBindings, none);
}

void D3D11Device::beginPass(const PassDesc& p) {
    pass_ = p;
    unbindSrvs();  // a texture sampled by the previous pass may be a target now
    ID3D11RenderTargetView* rtv[2] = {};
    ID3D11DepthStencilView* dsv = nullptr;
    uint32_t w = w_, h = h_, count = 1;
    if (!p.color) {
        rtv[0] = backbufferRtv();
    } else {
        const TextureRec& c = textures_.at(p.color.id - 1);
        rtv[0] = c.rtv[0].get();
        w = c.w;
        h = c.h;
    }
    if (p.color2) {
        rtv[1] = textures_.at(p.color2.id - 1).rtv[0].get();
        count = 2;
    }
    if (p.depth) dsv = textures_.at(p.depth.id - 1).dsv.get();
    ctx_->OMSetRenderTargets(count, rtv, dsv);
    D3D11_VIEWPORT vp{0.0f, 0.0f, static_cast<float>(w), static_cast<float>(h), 0.0f, 1.0f};
    ctx_->RSSetViewports(1, &vp);
    if (p.load == Load::Clear)
        for (uint32_t i = 0; i < count; ++i) ctx_->ClearRenderTargetView(rtv[i], p.clear);
    if (dsv) ctx_->ClearDepthStencilView(dsv, D3D11_CLEAR_DEPTH, 1.0f, 0);
}

void D3D11Device::endPass() {
    if (pass_.resolveToBackbuffer) {
        const TextureRec& c = textures_.at(pass_.color.id - 1);
        // Offscreen: the backbuffer is typeless, so resolve in sRGB (linear averaging) like
        // Vulkan; a flip-model swapchain buffer is UNORM, so the resolve happens on encoded values.
        ctx_->ResolveSubresource(backTex_.get(), 0, c.tex.get(), 0,
                                 opt_.offscreen ? DXGI_FORMAT_R8G8B8A8_UNORM_SRGB : DXGI_FORMAT_R8G8B8A8_UNORM);
    }
}

void D3D11Device::setPipeline(Pipeline p) {
    bound_ = &pipelines_.at(p.id - 1);
    if (bound_->compute) {
        ctx_->CSSetShader(bound_->cs.get(), nullptr, 0);
        return;
    }
    ctx_->IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
    ctx_->IASetInputLayout(bound_->layout.get());
    ctx_->VSSetShader(bound_->vs.get(), nullptr, 0);
    ctx_->PSSetShader(bound_->ps.get(), nullptr, 0);
    ctx_->RSSetState(bound_->raster.get());
    ctx_->OMSetBlendState(bound_->blend.get(), nullptr, 0xffffffff);
    ctx_->OMSetDepthStencilState(bound_->depth.get(), 0);
}

void D3D11Device::setConstants(const void* data, uint32_t size) {
    D3D11_MAPPED_SUBRESOURCE m{};
    HR_CHECK(ctx_->Map(constBuf_.get(), 0, D3D11_MAP_WRITE_DISCARD, 0, &m));
    std::memcpy(m.pData, data, std::min<uint32_t>(size, 256));
    ctx_->Unmap(constBuf_.get(), 0);
    ID3D11Buffer* cb = constBuf_.get();
    if (bound_->compute) {
        ctx_->CSSetConstantBuffers(d3d::kPushRegister, 1, &cb);
    } else {
        ctx_->VSSetConstantBuffers(d3d::kPushRegister, 1, &cb);
        ctx_->PSSetConstantBuffers(d3d::kPushRegister, 1, &cb);
    }
}

void D3D11Device::setStorage(uint32_t binding, Buffer b) {
    const BufferRec& r = buffers_.at(b.id - 1);
    if (bound_->res[binding] == Res::StorageRW) {
        ID3D11UnorderedAccessView* uav = r.uav.get();
        ctx_->CSSetUnorderedAccessViews(binding, 1, &uav, nullptr);
    } else {
        ID3D11ShaderResourceView* srv = r.srv.get();
        ctx_->VSSetShaderResources(binding, 1, &srv);
    }
}

}  // namespace

std::unique_ptr<Device> createD3D11(SDL_Window* window, const Options& opt) {
    return std::make_unique<D3D11Device>(window, opt);
}

}  // namespace gfx
