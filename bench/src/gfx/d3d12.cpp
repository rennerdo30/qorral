// Direct3D 12 backend: feature level 11_0+, flip-model swapchain, HLSL 5.0 bytecode.
// Each pipeline gets a root signature built from its bindings: the "Push" block as root constants
// (b4), uniform buffers as root CBVs, textures and storage buffers as one-descriptor tables, and
// four static linear/clamp samplers. Descriptors for a draw are copied into a per-frame ring.
#include "gfx/d3d_common.h"

#include <d3d12.h>

#include <algorithm>
#include <cstdio>
#include <cstring>

namespace gfx {
namespace {

using d3d::Com;

constexpr UINT kRingPerFrame = 4096;   // shader-visible descriptors per frame in flight
constexpr UINT kCpuDescriptors = 4096;
constexpr UINT kRtvDescriptors = 1024;
constexpr UINT kDsvDescriptors = 256;
constexpr UINT kSwapBuffers = 3;
constexpr DWORD kWaitMs = 10000;

struct BufferRec {
    bool alive = false;
    BufferKind kind{};
    size_t size = 0;
    Com<ID3D12Resource> res[kFramesInFlight];  // Uniform: one per frame in flight; others [0]
    void* mapped[kFramesInFlight]{};
    D3D12_CPU_DESCRIPTOR_HANDLE srv{}, uav{};   // Storage
    D3D12_RESOURCE_STATES state = D3D12_RESOURCE_STATE_COMMON;
};

struct TextureRec {
    bool alive = false;
    Com<ID3D12Resource> res;
    DXGI_FORMAT view{};
    uint32_t w = 0, h = 0, samples = 1;
    bool depth = false;
    D3D12_CPU_DESCRIPTOR_HANDLE rtv{}, dsv{}, srv{};
    D3D12_RESOURCE_STATES state = D3D12_RESOURCE_STATE_COMMON;
};

struct PipelineRec {
    bool alive = false;
    bool compute = false;
    Com<ID3D12RootSignature> root;
    Com<ID3D12PipelineState> pso;
    std::vector<UINT> strides;
    Res res[kMaxBindings]{};
    int rootIndex[kMaxBindings] = {-1, -1, -1, -1};
    bool hasConstants = false;
};

class D3D12Device final : public Device {
public:
    D3D12Device(SDL_Window* window, const Options& opt);
    ~D3D12Device() override;

    Api api() const override { return Api::D3D12; }
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
    double waitPresented(std::string& source) override;

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
    void setUniform(uint32_t binding, Buffer b) override;
    void setTexture(uint32_t binding, Texture t) override;
    void setStorage(uint32_t binding, Buffer b) override;
    void setVertexBuffer(uint32_t slot, Buffer b) override;
    void setIndexBuffer(Buffer b) override;
    void draw(uint32_t vertexCount, uint32_t instanceCount) override {
        list_->DrawInstanced(vertexCount, instanceCount, 0, 0);
    }
    void drawIndexed(uint32_t indexCount, uint32_t instanceCount, uint32_t firstInstance) override {
        list_->DrawIndexedInstanced(indexCount, instanceCount, 0, 0, firstInstance);
    }
    void dispatch(uint32_t groupsX) override { list_->Dispatch(groupsX, 1, 1); }
    void storageToCompute() override;
    void storageToVertex() override;

private:
    void createBackbuffers();
    void releaseBackbuffers();
    void transition(ID3D12Resource* r, D3D12_RESOURCE_STATES& state, D3D12_RESOURCE_STATES to);
    D3D12_CPU_DESCRIPTOR_HANDLE allocCpu(ID3D12DescriptorHeap* heap, UINT& used, UINT capacity, UINT inc);
    void bindTable(uint32_t binding, D3D12_CPU_DESCRIPTOR_HANDLE src);
    void signalAndWait();
    void collectTimestamps(uint32_t slot);
    Com<ID3D12Resource> createCommitted(D3D12_HEAP_TYPE heap, const D3D12_RESOURCE_DESC& desc,
                                        D3D12_RESOURCE_STATES state, const D3D12_CLEAR_VALUE* clear = nullptr);
    void upload(ID3D12Resource* dst, const void* data, size_t size);
    ID3D12Resource* backbuffer() const { return opt_.offscreen ? offscreen_.get() : swapBuffers_[backIndex_].get(); }
    D3D12_RESOURCE_STATES& backbufferState() { return opt_.offscreen ? offscreenState_ : swapStates_[backIndex_]; }

    SDL_Window* window_;
    HWND hwnd_ = nullptr;
    Options opt_;
    DeviceInfo info_;
    Com<IDXGIFactory4> factory_;
    Com<ID3D12Device> dev_;
    Com<ID3D12InfoQueue> infoQueue_;
    Com<ID3D12CommandQueue> queue_;
    Com<ID3D12CommandAllocator> alloc_[kFramesInFlight];
    Com<ID3D12GraphicsCommandList> list_;
    Com<ID3D12Fence> fence_;
    HANDLE fenceEvent_ = nullptr;
    UINT64 fenceValue_ = 0;
    UINT64 slotFence_[kFramesInFlight]{};

    Com<IDXGISwapChain3> swap_;
    bool tearing_ = false;
    UINT syncInterval_ = 0, presentFlags_ = 0;
    Com<ID3D12Resource> swapBuffers_[kSwapBuffers];
    D3D12_RESOURCE_STATES swapStates_[kSwapBuffers]{};
    D3D12_CPU_DESCRIPTOR_HANDLE swapRtv_[kSwapBuffers]{};
    Com<ID3D12Resource> offscreen_;
    D3D12_RESOURCE_STATES offscreenState_ = D3D12_RESOURCE_STATE_RENDER_TARGET;
    D3D12_CPU_DESCRIPTOR_HANDLE offscreenRtv_{};
    UINT backIndex_ = 0;
    uint32_t w_ = 0, h_ = 0;

    Com<ID3D12DescriptorHeap> rtvHeap_, dsvHeap_, cpuHeap_, gpuHeap_;
    UINT rtvUsed_ = 0, dsvUsed_ = 0, cpuUsed_ = 0, ringUsed_ = 0;
    UINT rtvInc_ = 0, dsvInc_ = 0, srvInc_ = 0;

    Com<ID3D12QueryHeap> queryHeap_;
    Com<ID3D12Resource> queryReadback_;
    UINT64 tsFrequency_ = 1;
    bool slotPending_[kFramesInFlight]{};
    bool slotMeasured_[kFramesInFlight]{};
    std::vector<double> gpuTimesMs_;

    uint32_t slot_ = 0;
    uint64_t frameCounter_ = 0;
    LONGLONG frameStartQpc_ = 0;
    int statsFailures_ = 0;  // frame statistics unavailable after 3 failures in a row
    std::vector<BufferRec> buffers_;
    std::vector<TextureRec> textures_;
    std::vector<PipelineRec> pipelines_;
    const PipelineRec* bound_ = nullptr;
    PassDesc pass_{};
    Com<ID3D12Resource> capture_;
    D3D12_PLACED_SUBRESOURCE_FOOTPRINT captureFootprint_{};
};

D3D12_RESOURCE_DESC bufferDesc(UINT64 size, D3D12_RESOURCE_FLAGS flags = D3D12_RESOURCE_FLAG_NONE) {
    D3D12_RESOURCE_DESC d{};
    d.Dimension = D3D12_RESOURCE_DIMENSION_BUFFER;
    d.Width = size;
    d.Height = 1;
    d.DepthOrArraySize = 1;
    d.MipLevels = 1;
    d.SampleDesc.Count = 1;
    d.Layout = D3D12_TEXTURE_LAYOUT_ROW_MAJOR;
    d.Flags = flags;
    return d;
}

D3D12_RESOURCE_DESC textureDesc(DXGI_FORMAT f, uint32_t w, uint32_t h, uint32_t samples, D3D12_RESOURCE_FLAGS flags) {
    D3D12_RESOURCE_DESC d{};
    d.Dimension = D3D12_RESOURCE_DIMENSION_TEXTURE2D;
    d.Width = w;
    d.Height = h;
    d.DepthOrArraySize = 1;
    d.MipLevels = 1;
    d.Format = f;
    d.SampleDesc.Count = samples;
    d.Flags = flags;
    return d;
}

D3D12Device::D3D12Device(SDL_Window* window, const Options& opt) : window_(window), opt_(opt) {
    if (opt.validate) {
        Com<ID3D12Debug> debug;
        if (SUCCEEDED(D3D12GetDebugInterface(__uuidof(ID3D12Debug), reinterpret_cast<void**>(debug.put())))) {
            debug->EnableDebugLayer();
            info_.validation = true;
        } else {
            std::fprintf(stderr, "warning: --validate given but the D3D12 debug layer is not installed\n");
        }
    }
    HR_CHECK(CreateDXGIFactory1(__uuidof(IDXGIFactory4), reinterpret_cast<void**>(factory_.put())));
    Com<IDXGIAdapter1> adapter = d3d::pickAdapter(factory_.get(), opt.gpuIndex, info_);
    HR_CHECK(D3D12CreateDevice(adapter.get(), D3D_FEATURE_LEVEL_11_0, __uuidof(ID3D12Device),
                               reinterpret_cast<void**>(dev_.put())));
    if (info_.validation) infoQueue_ = dev_.as<ID3D12InfoQueue>();
    const D3D_FEATURE_LEVEL levels[] = {D3D_FEATURE_LEVEL_12_2, D3D_FEATURE_LEVEL_12_1, D3D_FEATURE_LEVEL_12_0,
                                        D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0};
    D3D12_FEATURE_DATA_FEATURE_LEVELS fl{5, levels, D3D_FEATURE_LEVEL_11_0};
    dev_->CheckFeatureSupport(D3D12_FEATURE_FEATURE_LEVELS, &fl, sizeof fl);
    switch (fl.MaxSupportedFeatureLevel) {
        case D3D_FEATURE_LEVEL_12_2: info_.apiVersion = "12_2"; break;
        case D3D_FEATURE_LEVEL_12_1: info_.apiVersion = "12_1"; break;
        case D3D_FEATURE_LEVEL_12_0: info_.apiVersion = "12_0"; break;
        case D3D_FEATURE_LEVEL_11_1: info_.apiVersion = "11_1"; break;
        default: info_.apiVersion = "11_0"; break;
    }
    D3D12_FEATURE_DATA_MULTISAMPLE_QUALITY_LEVELS ms{DXGI_FORMAT_R8G8B8A8_UNORM_SRGB, 4};
    info_.msaaSamples = SUCCEEDED(dev_->CheckFeatureSupport(D3D12_FEATURE_MULTISAMPLE_QUALITY_LEVELS, &ms, sizeof ms)) &&
                                ms.NumQualityLevels > 0
                            ? 4
                            : 1;

    D3D12_COMMAND_QUEUE_DESC qd{};
    qd.Type = D3D12_COMMAND_LIST_TYPE_DIRECT;
    HR_CHECK(dev_->CreateCommandQueue(&qd, __uuidof(ID3D12CommandQueue), reinterpret_cast<void**>(queue_.put())));
    for (auto& a : alloc_)
        HR_CHECK(dev_->CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT, __uuidof(ID3D12CommandAllocator),
                                              reinterpret_cast<void**>(a.put())));
    HR_CHECK(dev_->CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, alloc_[0].get(), nullptr,
                                     __uuidof(ID3D12GraphicsCommandList), reinterpret_cast<void**>(list_.put())));
    HR_CHECK(list_->Close());
    HR_CHECK(dev_->CreateFence(0, D3D12_FENCE_FLAG_NONE, __uuidof(ID3D12Fence), reinterpret_cast<void**>(fence_.put())));
    fenceEvent_ = CreateEventW(nullptr, FALSE, FALSE, nullptr);

    auto heap = [&](D3D12_DESCRIPTOR_HEAP_TYPE t, UINT n, bool visible, Com<ID3D12DescriptorHeap>& out) {
        D3D12_DESCRIPTOR_HEAP_DESC hd{t, n, visible ? D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE : D3D12_DESCRIPTOR_HEAP_FLAG_NONE, 0};
        HR_CHECK(dev_->CreateDescriptorHeap(&hd, __uuidof(ID3D12DescriptorHeap), reinterpret_cast<void**>(out.put())));
    };
    heap(D3D12_DESCRIPTOR_HEAP_TYPE_RTV, kRtvDescriptors, false, rtvHeap_);
    heap(D3D12_DESCRIPTOR_HEAP_TYPE_DSV, kDsvDescriptors, false, dsvHeap_);
    heap(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV, kCpuDescriptors, false, cpuHeap_);
    heap(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV, kRingPerFrame * kFramesInFlight, true, gpuHeap_);
    rtvInc_ = dev_->GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_RTV);
    dsvInc_ = dev_->GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_DSV);
    srvInc_ = dev_->GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV);

    D3D12_QUERY_HEAP_DESC qh{D3D12_QUERY_HEAP_TYPE_TIMESTAMP, 2 * kFramesInFlight, 0};
    HR_CHECK(dev_->CreateQueryHeap(&qh, __uuidof(ID3D12QueryHeap), reinterpret_cast<void**>(queryHeap_.put())));
    queryReadback_ = createCommitted(D3D12_HEAP_TYPE_READBACK, bufferDesc(sizeof(UINT64) * 2 * kFramesInFlight),
                                     D3D12_RESOURCE_STATE_COPY_DEST);
    info_.timestamps = SUCCEEDED(queue_->GetTimestampFrequency(&tsFrequency_)) && tsFrequency_ > 0;

    if (opt.offscreen) {
        info_.presentMode = "offscreen";
    } else {
        hwnd_ = d3d::windowHandle(window);
        tearing_ = d3d::tearingSupported(factory_.get());
        DXGI_SWAP_CHAIN_DESC1 scd{};
        scd.Format = DXGI_FORMAT_R8G8B8A8_UNORM;
        scd.SampleDesc.Count = 1;
        scd.BufferUsage = DXGI_USAGE_RENDER_TARGET_OUTPUT;
        scd.BufferCount = kSwapBuffers;
        scd.SwapEffect = DXGI_SWAP_EFFECT_FLIP_DISCARD;
        scd.Flags = tearing_ ? DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING : 0;
        Com<IDXGISwapChain1> sc1;
        HR_CHECK(factory_->CreateSwapChainForHwnd(queue_.get(), hwnd_, &scd, nullptr, nullptr, sc1.put()));
        swap_ = sc1.as<IDXGISwapChain3>();
        if (!swap_) throw Error("IDXGISwapChain3 is not available");
        factory_->MakeWindowAssociation(hwnd_, DXGI_MWA_NO_ALT_ENTER);
        if (opt.present == Present::Fifo) {
            syncInterval_ = 1;
            info_.presentMode = "fifo";
        } else if (opt.present == Present::Immediate && tearing_) {
            presentFlags_ = DXGI_PRESENT_ALLOW_TEARING;
            info_.presentMode = "immediate";
        } else {
            info_.presentMode = "mailbox";
        }
    }
    info_.swapFormat = "R8G8B8A8_UNORM_SRGB";
    createBackbuffers();
}

D3D12Device::~D3D12Device() {
    if (queue_ && fence_) {
        try {
            signalAndWait();
        } catch (...) {
        }
    }
    if (fenceEvent_) CloseHandle(fenceEvent_);
}

uint32_t D3D12Device::validationErrors() const {
    if (!infoQueue_) return 0;
    uint32_t errors = 0;
    UINT64 n = infoQueue_->GetNumStoredMessages();
    for (UINT64 i = 0; i < n; ++i) {
        SIZE_T len = 0;
        infoQueue_->GetMessage(i, nullptr, &len);
        std::vector<char> buf(len);
        auto* m = reinterpret_cast<D3D12_MESSAGE*>(buf.data());
        if (SUCCEEDED(infoQueue_->GetMessage(i, m, &len)) &&
            (m->Severity == D3D12_MESSAGE_SEVERITY_ERROR || m->Severity == D3D12_MESSAGE_SEVERITY_CORRUPTION)) {
            ++errors;
            std::fprintf(stderr, "[d3d12 error] %.*s\n", static_cast<int>(m->DescriptionByteLength), m->pDescription);
        }
    }
    return errors;
}

Com<ID3D12Resource> D3D12Device::createCommitted(D3D12_HEAP_TYPE heap, const D3D12_RESOURCE_DESC& desc,
                                                 D3D12_RESOURCE_STATES state, const D3D12_CLEAR_VALUE* clear) {
    D3D12_HEAP_PROPERTIES hp{};
    hp.Type = heap;
    Com<ID3D12Resource> r;
    HR_CHECK(dev_->CreateCommittedResource(&hp, D3D12_HEAP_FLAG_NONE, &desc, state, clear, __uuidof(ID3D12Resource),
                                           reinterpret_cast<void**>(r.put())));
    return r;
}

D3D12_CPU_DESCRIPTOR_HANDLE D3D12Device::allocCpu(ID3D12DescriptorHeap* heap, UINT& used, UINT capacity, UINT inc) {
    if (used >= capacity) throw Error("D3D12 descriptor heap is full");
    D3D12_CPU_DESCRIPTOR_HANDLE h = heap->GetCPUDescriptorHandleForHeapStart();
    h.ptr += SIZE_T(used++) * inc;
    return h;
}

void D3D12Device::releaseBackbuffers() {
    for (auto& b : swapBuffers_) b.reset();
    offscreen_.reset();
}

void D3D12Device::createBackbuffers() {
    D3D12_RENDER_TARGET_VIEW_DESC rd{};
    rd.Format = DXGI_FORMAT_R8G8B8A8_UNORM_SRGB;
    rd.ViewDimension = D3D12_RTV_DIMENSION_TEXTURE2D;
    if (opt_.offscreen) {
        w_ = opt_.width;
        h_ = opt_.height;
        D3D12_CLEAR_VALUE cv{DXGI_FORMAT_R8G8B8A8_UNORM_SRGB, {0, 0, 0, 1}};
        offscreen_ = createCommitted(D3D12_HEAP_TYPE_DEFAULT,
                                     textureDesc(DXGI_FORMAT_R8G8B8A8_TYPELESS, w_, h_, 1, D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET),
                                     D3D12_RESOURCE_STATE_RENDER_TARGET, &cv);
        offscreenState_ = D3D12_RESOURCE_STATE_RENDER_TARGET;
        if (!offscreenRtv_.ptr) offscreenRtv_ = allocCpu(rtvHeap_.get(), rtvUsed_, kRtvDescriptors, rtvInc_);
        dev_->CreateRenderTargetView(offscreen_.get(), &rd, offscreenRtv_);
    } else {
        DXGI_SWAP_CHAIN_DESC1 scd{};
        swap_->GetDesc1(&scd);
        w_ = scd.Width;
        h_ = scd.Height;
        for (UINT i = 0; i < kSwapBuffers; ++i) {
            HR_CHECK(swap_->GetBuffer(i, __uuidof(ID3D12Resource), reinterpret_cast<void**>(swapBuffers_[i].put())));
            if (!swapRtv_[i].ptr) swapRtv_[i] = allocCpu(rtvHeap_.get(), rtvUsed_, kRtvDescriptors, rtvInc_);
            dev_->CreateRenderTargetView(swapBuffers_[i].get(), &rd, swapRtv_[i]);
            swapStates_[i] = D3D12_RESOURCE_STATE_PRESENT;
        }
    }
}

void D3D12Device::resize() {
    waitIdle();
    if (opt_.offscreen) return;
    releaseBackbuffers();
    HR_CHECK(swap_->ResizeBuffers(kSwapBuffers, 0, 0, DXGI_FORMAT_UNKNOWN, tearing_ ? DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING : 0));
    createBackbuffers();
}

void D3D12Device::signalAndWait() {
    const UINT64 v = ++fenceValue_;
    HR_CHECK(queue_->Signal(fence_.get(), v));
    if (fence_->GetCompletedValue() < v) {
        HR_CHECK(fence_->SetEventOnCompletion(v, fenceEvent_));
        if (WaitForSingleObject(fenceEvent_, kWaitMs) != WAIT_OBJECT_0) throw Error("D3D12: the GPU did not finish within 10 s");
    }
}

void D3D12Device::waitIdle() { signalAndWait(); }

void D3D12Device::upload(ID3D12Resource* dst, const void* data, size_t size) {
    Com<ID3D12Resource> staging = createCommitted(D3D12_HEAP_TYPE_UPLOAD, bufferDesc(size), D3D12_RESOURCE_STATE_GENERIC_READ);
    void* p = nullptr;
    HR_CHECK(staging->Map(0, nullptr, &p));
    std::memcpy(p, data, size);
    staging->Unmap(0, nullptr);
    Com<ID3D12CommandAllocator> a;
    HR_CHECK(dev_->CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT, __uuidof(ID3D12CommandAllocator),
                                          reinterpret_cast<void**>(a.put())));
    Com<ID3D12GraphicsCommandList> l;
    HR_CHECK(dev_->CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, a.get(), nullptr,
                                     __uuidof(ID3D12GraphicsCommandList), reinterpret_cast<void**>(l.put())));
    l->CopyBufferRegion(dst, 0, staging.get(), 0, size);
    HR_CHECK(l->Close());
    ID3D12CommandList* lists[] = {l.get()};
    queue_->ExecuteCommandLists(1, lists);
    signalAndWait();
}

void D3D12Device::transition(ID3D12Resource* r, D3D12_RESOURCE_STATES& state, D3D12_RESOURCE_STATES to) {
    if (state == to) return;
    D3D12_RESOURCE_BARRIER b{};
    b.Type = D3D12_RESOURCE_BARRIER_TYPE_TRANSITION;
    b.Transition.pResource = r;
    b.Transition.Subresource = D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES;
    b.Transition.StateBefore = state;
    b.Transition.StateAfter = to;
    list_->ResourceBarrier(1, &b);
    state = to;
}

// ---------------------------------------------------------------------------------------------
// Frames

void D3D12Device::collectTimestamps(uint32_t slot) {
    if (!slotPending_[slot]) return;
    slotPending_[slot] = false;
    if (!slotMeasured_[slot] || !info_.timestamps) return;
    D3D12_RANGE range{sizeof(UINT64) * 2 * slot, sizeof(UINT64) * 2 * (slot + 1)};
    void* p = nullptr;
    HR_CHECK(queryReadback_->Map(0, &range, &p));
    const UINT64* t = reinterpret_cast<const UINT64*>(static_cast<const uint8_t*>(p) + range.Begin);
    if (t[1] >= t[0]) gpuTimesMs_.push_back(static_cast<double>(t[1] - t[0]) * 1000.0 / static_cast<double>(tsFrequency_));
    D3D12_RANGE none{0, 0};
    queryReadback_->Unmap(0, &none);
}

bool D3D12Device::beginFrame(bool measured) {
    frameStartQpc_ = d3d::qpcNow();
    slot_ = static_cast<uint32_t>(frameCounter_ % kFramesInFlight);
    if (fence_->GetCompletedValue() < slotFence_[slot_]) {
        HR_CHECK(fence_->SetEventOnCompletion(slotFence_[slot_], fenceEvent_));
        if (WaitForSingleObject(fenceEvent_, kWaitMs) != WAIT_OBJECT_0)
            throw Error("D3D12: the GPU did not finish a frame within 10 s");
    }
    collectTimestamps(slot_);
    slotMeasured_[slot_] = measured;
    HR_CHECK(alloc_[slot_]->Reset());
    HR_CHECK(list_->Reset(alloc_[slot_].get(), nullptr));
    ID3D12DescriptorHeap* heaps[] = {gpuHeap_.get()};
    list_->SetDescriptorHeaps(1, heaps);
    ringUsed_ = 0;
    if (!opt_.offscreen) backIndex_ = swap_->GetCurrentBackBufferIndex();
    if (info_.timestamps) list_->EndQuery(queryHeap_.get(), D3D12_QUERY_TYPE_TIMESTAMP, slot_ * 2);
    bound_ = nullptr;
    return true;
}

bool D3D12Device::endFrame(const std::string* capturePath) {
    ID3D12Resource* back = backbuffer();
    if (capturePath) {
        transition(back, backbufferState(), D3D12_RESOURCE_STATE_COPY_SOURCE);
        D3D12_RESOURCE_DESC d = back->GetDesc();
        UINT64 total = 0;
        dev_->GetCopyableFootprints(&d, 0, 1, 0, &captureFootprint_, nullptr, nullptr, &total);
        capture_ = createCommitted(D3D12_HEAP_TYPE_READBACK, bufferDesc(total), D3D12_RESOURCE_STATE_COPY_DEST);
        D3D12_TEXTURE_COPY_LOCATION src{}, dst{};
        src.pResource = back;
        src.Type = D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX;
        dst.pResource = capture_.get();
        dst.Type = D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT;
        dst.PlacedFootprint = captureFootprint_;
        list_->CopyTextureRegion(&dst, 0, 0, 0, &src, nullptr);
    }
    transition(back, backbufferState(), opt_.offscreen ? D3D12_RESOURCE_STATE_RENDER_TARGET : D3D12_RESOURCE_STATE_PRESENT);
    if (info_.timestamps) {
        list_->EndQuery(queryHeap_.get(), D3D12_QUERY_TYPE_TIMESTAMP, slot_ * 2 + 1);
        list_->ResolveQueryData(queryHeap_.get(), D3D12_QUERY_TYPE_TIMESTAMP, slot_ * 2, 2, queryReadback_.get(),
                                sizeof(UINT64) * 2 * slot_);
    }
    HR_CHECK(list_->Close());
    ID3D12CommandList* lists[] = {list_.get()};
    queue_->ExecuteCommandLists(1, lists);
    slotPending_[slot_] = true;
    if (!opt_.offscreen) {
        HRESULT hr = swap_->Present(syncInterval_, presentFlags_);
        if (hr == DXGI_ERROR_DEVICE_REMOVED || hr == DXGI_ERROR_DEVICE_RESET)
            d3d::failHr("IDXGISwapChain::Present (device removed)", dev_->GetDeviceRemovedReason(), __FILE__, __LINE__);
        if (FAILED(hr)) d3d::failHr("IDXGISwapChain::Present", hr, __FILE__, __LINE__);
    }
    slotFence_[slot_] = ++fenceValue_;
    HR_CHECK(queue_->Signal(fence_.get(), slotFence_[slot_]));
    ++frameCounter_;
    if (capturePath) {
        signalAndWait();
        void* p = nullptr;
        HR_CHECK(capture_->Map(0, nullptr, &p));
        if (!writeBmp(*capturePath, static_cast<const uint8_t*>(p) + captureFootprint_.Offset, w_, h_,
                      captureFootprint_.Footprint.RowPitch, false))
            std::fprintf(stderr, "warning: could not write %s\n", capturePath->c_str());
        capture_->Unmap(0, nullptr);
        capture_.reset();
    }
    return false;
}

double D3D12Device::waitPresented(std::string& source) {
    double ms = 0.0;
    if (!opt_.offscreen && statsFailures_ < 3) {
        if (d3d::waitDxgiDisplayed(swap_.get(), frameStartQpc_, ms)) {
            statsFailures_ = 0;
            source = "display";
            return ms;
        }
        ++statsFailures_;
    }
    signalAndWait();
    source = "gpu";
    return d3d::qpcMs(frameStartQpc_, d3d::qpcNow());
}

void D3D12Device::finishFrames() {
    signalAndWait();
    for (uint32_t i = 0; i < kFramesInFlight; ++i) collectTimestamps(i);
}

// ---------------------------------------------------------------------------------------------
// Resources

Buffer D3D12Device::createBuffer(BufferKind kind, size_t size, const void* data) {
    BufferRec r;
    r.alive = true;
    r.kind = kind;
    r.size = size;
    if (kind == BufferKind::Uniform) {
        const UINT64 aligned = (size + 255) & ~UINT64(255);
        for (uint32_t s = 0; s < kFramesInFlight; ++s) {
            r.res[s] = createCommitted(D3D12_HEAP_TYPE_UPLOAD, bufferDesc(aligned), D3D12_RESOURCE_STATE_GENERIC_READ);
            HR_CHECK(r.res[s]->Map(0, nullptr, &r.mapped[s]));
        }
    } else if (kind == BufferKind::Storage) {
        r.res[0] = createCommitted(D3D12_HEAP_TYPE_DEFAULT, bufferDesc(size, D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS),
                                   D3D12_RESOURCE_STATE_UNORDERED_ACCESS);
        r.state = D3D12_RESOURCE_STATE_UNORDERED_ACCESS;
        D3D12_SHADER_RESOURCE_VIEW_DESC sv{};
        sv.Format = DXGI_FORMAT_R32_TYPELESS;
        sv.ViewDimension = D3D12_SRV_DIMENSION_BUFFER;
        sv.Shader4ComponentMapping = D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING;
        sv.Buffer.NumElements = static_cast<UINT>(size / 4);
        sv.Buffer.Flags = D3D12_BUFFER_SRV_FLAG_RAW;
        r.srv = allocCpu(cpuHeap_.get(), cpuUsed_, kCpuDescriptors, srvInc_);
        dev_->CreateShaderResourceView(r.res[0].get(), &sv, r.srv);
        D3D12_UNORDERED_ACCESS_VIEW_DESC uv{};
        uv.Format = DXGI_FORMAT_R32_TYPELESS;
        uv.ViewDimension = D3D12_UAV_DIMENSION_BUFFER;
        uv.Buffer.NumElements = static_cast<UINT>(size / 4);
        uv.Buffer.Flags = D3D12_BUFFER_UAV_FLAG_RAW;
        r.uav = allocCpu(cpuHeap_.get(), cpuUsed_, kCpuDescriptors, srvInc_);
        dev_->CreateUnorderedAccessView(r.res[0].get(), nullptr, &uv, r.uav);
    } else {
        r.res[0] = createCommitted(D3D12_HEAP_TYPE_DEFAULT, bufferDesc(size), D3D12_RESOURCE_STATE_COMMON);
        if (data) upload(r.res[0].get(), data, size);
        r.state = D3D12_RESOURCE_STATE_COMMON;  // buffers decay to COMMON and promote implicitly on use
    }
    buffers_.push_back(r);
    return Buffer{{static_cast<uint32_t>(buffers_.size())}};
}

void D3D12Device::updateBuffer(Buffer b, const void* data, size_t size) {
    BufferRec& r = buffers_.at(b.id - 1);
    std::memcpy(r.mapped[slot_], data, std::min(size, r.size));
}

Texture D3D12Device::createTexture(Format f, uint32_t w, uint32_t h, uint32_t samples, bool) {
    TextureRec r;
    r.alive = true;
    r.w = w;
    r.h = h;
    r.samples = samples;
    DXGI_FORMAT res = f == Format::RGBA8     ? DXGI_FORMAT_R8G8B8A8_UNORM
                    : f == Format::RGBA16F   ? DXGI_FORMAT_R16G16B16A16_FLOAT
                    : f == Format::RGBA32F   ? DXGI_FORMAT_R32G32B32A32_FLOAT
                                             : DXGI_FORMAT_R8G8B8A8_TYPELESS;
    r.view = f == Format::Backbuffer ? DXGI_FORMAT_R8G8B8A8_UNORM_SRGB : res;
    D3D12_CLEAR_VALUE cv{r.view, {0, 0, 0, 0}};
    r.res = createCommitted(D3D12_HEAP_TYPE_DEFAULT, textureDesc(res, w, h, samples, D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET),
                            D3D12_RESOURCE_STATE_RENDER_TARGET, &cv);
    r.state = D3D12_RESOURCE_STATE_RENDER_TARGET;
    D3D12_RENDER_TARGET_VIEW_DESC rd{};
    rd.Format = r.view;
    rd.ViewDimension = samples > 1 ? D3D12_RTV_DIMENSION_TEXTURE2DMS : D3D12_RTV_DIMENSION_TEXTURE2D;
    r.rtv = allocCpu(rtvHeap_.get(), rtvUsed_, kRtvDescriptors, rtvInc_);
    dev_->CreateRenderTargetView(r.res.get(), &rd, r.rtv);
    if (samples == 1) {
        D3D12_SHADER_RESOURCE_VIEW_DESC sv{};
        sv.Format = r.view;
        sv.ViewDimension = D3D12_SRV_DIMENSION_TEXTURE2D;
        sv.Shader4ComponentMapping = D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING;
        sv.Texture2D.MipLevels = 1;
        r.srv = allocCpu(cpuHeap_.get(), cpuUsed_, kCpuDescriptors, srvInc_);
        dev_->CreateShaderResourceView(r.res.get(), &sv, r.srv);
    }
    textures_.push_back(r);
    return Texture{{static_cast<uint32_t>(textures_.size())}};
}

Texture D3D12Device::createDepth(uint32_t w, uint32_t h, uint32_t samples) {
    TextureRec r;
    r.alive = true;
    r.depth = true;
    r.w = w;
    r.h = h;
    r.samples = samples;
    D3D12_CLEAR_VALUE cv{};
    cv.Format = DXGI_FORMAT_D32_FLOAT;
    cv.DepthStencil.Depth = 1.0f;
    r.res = createCommitted(D3D12_HEAP_TYPE_DEFAULT,
                            textureDesc(DXGI_FORMAT_D32_FLOAT, w, h, samples,
                                        D3D12_RESOURCE_FLAG_ALLOW_DEPTH_STENCIL | D3D12_RESOURCE_FLAG_DENY_SHADER_RESOURCE),
                            D3D12_RESOURCE_STATE_DEPTH_WRITE, &cv);
    r.state = D3D12_RESOURCE_STATE_DEPTH_WRITE;
    D3D12_DEPTH_STENCIL_VIEW_DESC dd{};
    dd.Format = DXGI_FORMAT_D32_FLOAT;
    dd.ViewDimension = samples > 1 ? D3D12_DSV_DIMENSION_TEXTURE2DMS : D3D12_DSV_DIMENSION_TEXTURE2D;
    r.dsv = allocCpu(dsvHeap_.get(), dsvUsed_, kDsvDescriptors, dsvInc_);
    dev_->CreateDepthStencilView(r.res.get(), &dd, r.dsv);
    textures_.push_back(r);
    return Texture{{static_cast<uint32_t>(textures_.size())}};
}

// Root signature: [0] root constants b4 (when used), then one parameter per binding.
static Com<ID3D12RootSignature> makeRoot(ID3D12Device* dev, const Res* res, uint32_t constBytes, bool graphics,
                                         PipelineRec& rec) {
    std::vector<D3D12_ROOT_PARAMETER> params;
    std::vector<D3D12_DESCRIPTOR_RANGE> ranges(kMaxBindings);
    if (constBytes) {
        D3D12_ROOT_PARAMETER p{};
        p.ParameterType = D3D12_ROOT_PARAMETER_TYPE_32BIT_CONSTANTS;
        p.Constants = {d3d::kPushRegister, 0, (constBytes + 3) / 4};
        p.ShaderVisibility = D3D12_SHADER_VISIBILITY_ALL;
        params.push_back(p);
        rec.hasConstants = true;
    }
    for (uint32_t i = 0; i < kMaxBindings; ++i) {
        rec.res[i] = res[i];
        if (res[i] == Res::None) continue;
        D3D12_ROOT_PARAMETER p{};
        p.ShaderVisibility = D3D12_SHADER_VISIBILITY_ALL;
        if (res[i] == Res::Uniform) {
            p.ParameterType = D3D12_ROOT_PARAMETER_TYPE_CBV;
            p.Descriptor = {i, 0};
        } else {
            ranges[i] = {res[i] == Res::StorageRW ? D3D12_DESCRIPTOR_RANGE_TYPE_UAV : D3D12_DESCRIPTOR_RANGE_TYPE_SRV, 1, i,
                         0, 0};
            p.ParameterType = D3D12_ROOT_PARAMETER_TYPE_DESCRIPTOR_TABLE;
            p.DescriptorTable = {1, &ranges[i]};
        }
        rec.rootIndex[i] = static_cast<int>(params.size());
        params.push_back(p);
    }
    D3D12_STATIC_SAMPLER_DESC samplers[kMaxBindings]{};
    for (uint32_t i = 0; i < kMaxBindings; ++i) {
        samplers[i].Filter = D3D12_FILTER_MIN_MAG_MIP_LINEAR;
        samplers[i].AddressU = samplers[i].AddressV = samplers[i].AddressW = D3D12_TEXTURE_ADDRESS_MODE_CLAMP;
        samplers[i].MaxLOD = 0.0f;
        samplers[i].ShaderRegister = i;
        samplers[i].ShaderVisibility = D3D12_SHADER_VISIBILITY_ALL;
    }
    D3D12_ROOT_SIGNATURE_DESC rd{};
    rd.NumParameters = static_cast<UINT>(params.size());
    rd.pParameters = params.data();
    rd.NumStaticSamplers = kMaxBindings;
    rd.pStaticSamplers = samplers;
    rd.Flags = graphics ? D3D12_ROOT_SIGNATURE_FLAG_ALLOW_INPUT_ASSEMBLER_INPUT_LAYOUT : D3D12_ROOT_SIGNATURE_FLAG_NONE;
    ID3DBlob* blob = nullptr;
    ID3DBlob* err = nullptr;
    HRESULT hr = D3D12SerializeRootSignature(&rd, D3D_ROOT_SIGNATURE_VERSION_1, &blob, &err);
    if (FAILED(hr)) {
        std::string msg = err ? std::string(static_cast<const char*>(err->GetBufferPointer()), err->GetBufferSize()) : "";
        if (err) err->Release();
        throw Error("D3D12SerializeRootSignature failed: " + msg);
    }
    if (err) err->Release();
    Com<ID3D12RootSignature> root;
    hr = dev->CreateRootSignature(0, blob->GetBufferPointer(), blob->GetBufferSize(), __uuidof(ID3D12RootSignature),
                                  reinterpret_cast<void**>(root.put()));
    blob->Release();
    HR_CHECK(hr);
    return root;
}

Pipeline D3D12Device::createPipeline(const PipelineDesc& d) {
    PipelineRec r;
    r.alive = true;
    r.root = makeRoot(dev_.get(), d.res, d.constBytes, true, r);
    auto vs = d3d::compileHlsl(shader(d.vs).hlsl5, d.vs, "vs_5_0", false, d.hdrOut);
    auto ps = d3d::compileHlsl(shader(d.fs).hlsl5, d.fs, "ps_5_0", false, d.hdrOut);
    std::vector<D3D12_INPUT_ELEMENT_DESC> el;
    for (const VertexAttr& a : d.attrs) {
        const bool inst = d.vertexBindings.at(a.binding).perInstance;
        DXGI_FORMAT f = a.components == 1 ? DXGI_FORMAT_R32_FLOAT
                      : a.components == 2 ? DXGI_FORMAT_R32G32_FLOAT
                      : a.components == 3 ? DXGI_FORMAT_R32G32B32_FLOAT
                                          : DXGI_FORMAT_R32G32B32A32_FLOAT;
        el.push_back({"TEXCOORD", a.location, f, a.binding, a.offset,
                      inst ? D3D12_INPUT_CLASSIFICATION_PER_INSTANCE_DATA : D3D12_INPUT_CLASSIFICATION_PER_VERTEX_DATA,
                      inst ? 1u : 0u});
    }
    for (const VertexBinding& b : d.vertexBindings) r.strides.push_back(b.stride);

    D3D12_GRAPHICS_PIPELINE_STATE_DESC pd{};
    pd.pRootSignature = r.root.get();
    pd.VS = {vs.data(), vs.size()};
    pd.PS = {ps.data(), ps.size()};
    pd.InputLayout = {el.data(), static_cast<UINT>(el.size())};
    pd.PrimitiveTopologyType = D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE;
    pd.RasterizerState.FillMode = D3D12_FILL_MODE_SOLID;
    pd.RasterizerState.CullMode = d.cullBack ? D3D12_CULL_MODE_BACK : D3D12_CULL_MODE_NONE;
    pd.RasterizerState.FrontCounterClockwise = TRUE;
    pd.RasterizerState.DepthClipEnable = TRUE;
    pd.RasterizerState.MultisampleEnable = d.samples > 1;
    for (uint32_t i = 0; i < d.colorCount; ++i) {
        auto& rt = pd.BlendState.RenderTarget[i];
        rt.RenderTargetWriteMask = D3D12_COLOR_WRITE_ENABLE_ALL;
        rt.LogicOp = D3D12_LOGIC_OP_NOOP;
        rt.SrcBlend = rt.SrcBlendAlpha = D3D12_BLEND_ONE;
        rt.DestBlend = rt.DestBlendAlpha = d.additive ? D3D12_BLEND_ONE : D3D12_BLEND_ZERO;
        rt.BlendOp = rt.BlendOpAlpha = D3D12_BLEND_OP_ADD;
        rt.BlendEnable = d.additive;
    }
    pd.SampleMask = UINT_MAX;
    pd.DepthStencilState.DepthEnable = d.depthTest;
    pd.DepthStencilState.DepthWriteMask = d.depthWrite ? D3D12_DEPTH_WRITE_MASK_ALL : D3D12_DEPTH_WRITE_MASK_ZERO;
    pd.DepthStencilState.DepthFunc = D3D12_COMPARISON_FUNC_LESS_EQUAL;
    pd.DSVFormat = d.depth ? DXGI_FORMAT_D32_FLOAT : DXGI_FORMAT_UNKNOWN;
    pd.NumRenderTargets = d.colorCount;
    const DXGI_FORMAT color = d.backbuffer ? DXGI_FORMAT_R8G8B8A8_UNORM_SRGB
                            : d.color == Format::RGBA8     ? DXGI_FORMAT_R8G8B8A8_UNORM
                            : d.color == Format::RGBA16F   ? DXGI_FORMAT_R16G16B16A16_FLOAT
                            : d.color == Format::RGBA32F   ? DXGI_FORMAT_R32G32B32A32_FLOAT
                                                           : DXGI_FORMAT_R8G8B8A8_UNORM_SRGB;
    for (uint32_t i = 0; i < d.colorCount; ++i) pd.RTVFormats[i] = color;
    pd.SampleDesc.Count = d.samples;
    HR_CHECK(dev_->CreateGraphicsPipelineState(&pd, __uuidof(ID3D12PipelineState), reinterpret_cast<void**>(r.pso.put())));
    pipelines_.push_back(r);
    return Pipeline{{static_cast<uint32_t>(pipelines_.size())}};
}

Pipeline D3D12Device::createCompute(const ComputeDesc& d) {
    PipelineRec r;
    r.alive = true;
    r.compute = true;
    r.root = makeRoot(dev_.get(), d.res, d.constBytes, false, r);
    auto cs = d3d::compileHlsl(shader(d.cs).hlsl5, d.cs, "cs_5_0", false, false);
    D3D12_COMPUTE_PIPELINE_STATE_DESC cd{};
    cd.pRootSignature = r.root.get();
    cd.CS = {cs.data(), cs.size()};
    HR_CHECK(dev_->CreateComputePipelineState(&cd, __uuidof(ID3D12PipelineState), reinterpret_cast<void**>(r.pso.put())));
    pipelines_.push_back(r);
    return Pipeline{{static_cast<uint32_t>(pipelines_.size())}};
}

// ---------------------------------------------------------------------------------------------
// Commands

void D3D12Device::beginPass(const PassDesc& p) {
    pass_ = p;
    D3D12_CPU_DESCRIPTOR_HANDLE rtv[2]{};
    D3D12_CPU_DESCRIPTOR_HANDLE dsv{};
    uint32_t w = w_, h = h_, count = 1;
    if (!p.color) {
        transition(backbuffer(), backbufferState(), D3D12_RESOURCE_STATE_RENDER_TARGET);
        rtv[0] = opt_.offscreen ? offscreenRtv_ : swapRtv_[backIndex_];
    } else {
        TextureRec& c = textures_.at(p.color.id - 1);
        transition(c.res.get(), c.state, D3D12_RESOURCE_STATE_RENDER_TARGET);
        rtv[0] = c.rtv;
        w = c.w;
        h = c.h;
    }
    if (p.color2) {
        TextureRec& c2 = textures_.at(p.color2.id - 1);
        transition(c2.res.get(), c2.state, D3D12_RESOURCE_STATE_RENDER_TARGET);
        rtv[1] = c2.rtv;
        count = 2;
    }
    if (p.depth) dsv = textures_.at(p.depth.id - 1).dsv;
    list_->OMSetRenderTargets(count, rtv, FALSE, p.depth ? &dsv : nullptr);
    D3D12_VIEWPORT vp{0.0f, 0.0f, static_cast<float>(w), static_cast<float>(h), 0.0f, 1.0f};
    D3D12_RECT sc{0, 0, static_cast<LONG>(w), static_cast<LONG>(h)};
    list_->RSSetViewports(1, &vp);
    list_->RSSetScissorRects(1, &sc);
    if (p.load == Load::Clear)
        for (uint32_t i = 0; i < count; ++i) list_->ClearRenderTargetView(rtv[i], p.clear, 0, nullptr);
    if (p.depth) list_->ClearDepthStencilView(dsv, D3D12_CLEAR_FLAG_DEPTH, 1.0f, 0, 0, nullptr);
}

void D3D12Device::endPass() {
    if (!pass_.resolveToBackbuffer) return;
    TextureRec& c = textures_.at(pass_.color.id - 1);
    transition(c.res.get(), c.state, D3D12_RESOURCE_STATE_RESOLVE_SOURCE);
    transition(backbuffer(), backbufferState(), D3D12_RESOURCE_STATE_RESOLVE_DEST);
    list_->ResolveSubresource(backbuffer(), 0, c.res.get(), 0,
                              opt_.offscreen ? DXGI_FORMAT_R8G8B8A8_UNORM_SRGB : DXGI_FORMAT_R8G8B8A8_UNORM);
    transition(backbuffer(), backbufferState(), D3D12_RESOURCE_STATE_RENDER_TARGET);
}

void D3D12Device::setPipeline(Pipeline p) {
    bound_ = &pipelines_.at(p.id - 1);
    list_->SetPipelineState(bound_->pso.get());
    if (bound_->compute) {
        list_->SetComputeRootSignature(bound_->root.get());
    } else {
        list_->SetGraphicsRootSignature(bound_->root.get());
        list_->IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
    }
}

void D3D12Device::setConstants(const void* data, uint32_t size) {
    if (bound_->compute) list_->SetComputeRoot32BitConstants(0, (size + 3) / 4, data, 0);
    else list_->SetGraphicsRoot32BitConstants(0, (size + 3) / 4, data, 0);
}

void D3D12Device::setUniform(uint32_t binding, Buffer b) {
    const BufferRec& r = buffers_.at(b.id - 1);
    const D3D12_GPU_VIRTUAL_ADDRESS va = r.res[r.kind == BufferKind::Uniform ? slot_ : 0]->GetGPUVirtualAddress();
    if (bound_->compute) list_->SetComputeRootConstantBufferView(bound_->rootIndex[binding], va);
    else list_->SetGraphicsRootConstantBufferView(bound_->rootIndex[binding], va);
}

void D3D12Device::bindTable(uint32_t binding, D3D12_CPU_DESCRIPTOR_HANDLE src) {
    if (ringUsed_ >= kRingPerFrame) throw Error("D3D12 descriptor ring is full");
    const UINT index = slot_ * kRingPerFrame + ringUsed_++;
    D3D12_CPU_DESCRIPTOR_HANDLE dst = gpuHeap_->GetCPUDescriptorHandleForHeapStart();
    dst.ptr += SIZE_T(index) * srvInc_;
    dev_->CopyDescriptorsSimple(1, dst, src, D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV);
    D3D12_GPU_DESCRIPTOR_HANDLE gpu = gpuHeap_->GetGPUDescriptorHandleForHeapStart();
    gpu.ptr += UINT64(index) * srvInc_;
    if (bound_->compute) list_->SetComputeRootDescriptorTable(bound_->rootIndex[binding], gpu);
    else list_->SetGraphicsRootDescriptorTable(bound_->rootIndex[binding], gpu);
}

void D3D12Device::setTexture(uint32_t binding, Texture t) {
    TextureRec& r = textures_.at(t.id - 1);
    transition(r.res.get(), r.state,
               D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE | D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE);
    bindTable(binding, r.srv);
}

void D3D12Device::setStorage(uint32_t binding, Buffer b) {
    const BufferRec& r = buffers_.at(b.id - 1);
    bindTable(binding, bound_->res[binding] == Res::StorageRW ? r.uav : r.srv);
}

void D3D12Device::setVertexBuffer(uint32_t slot, Buffer b) {
    const BufferRec& r = buffers_.at(b.id - 1);
    D3D12_VERTEX_BUFFER_VIEW v{r.res[0]->GetGPUVirtualAddress(), static_cast<UINT>(r.size), bound_->strides.at(slot)};
    list_->IASetVertexBuffers(slot, 1, &v);
}

void D3D12Device::setIndexBuffer(Buffer b) {
    const BufferRec& r = buffers_.at(b.id - 1);
    D3D12_INDEX_BUFFER_VIEW v{r.res[0]->GetGPUVirtualAddress(), static_cast<UINT>(r.size), DXGI_FORMAT_R32_UINT};
    list_->IASetIndexBuffer(&v);
}

void D3D12Device::storageToCompute() {
    for (BufferRec& r : buffers_)
        if (r.alive && r.kind == BufferKind::Storage)
            transition(r.res[0].get(), r.state, D3D12_RESOURCE_STATE_UNORDERED_ACCESS);
}

void D3D12Device::storageToVertex() {
    for (BufferRec& r : buffers_)
        if (r.alive && r.kind == BufferKind::Storage)
            transition(r.res[0].get(), r.state, D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE);
}

}  // namespace

std::unique_ptr<Device> createD3D12(SDL_Window* window, const Options& opt) {
    return std::make_unique<D3D12Device>(window, opt);
}

}  // namespace gfx
