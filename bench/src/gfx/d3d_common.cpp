#include "gfx/d3d_common.h"

#include <SDL.h>
#include <SDL_syswm.h>
#include <d3dcompiler.h>

#include <cstdio>
#include <cstring>

namespace gfx {
namespace d3d {

[[noreturn]] void failHr(const char* call, HRESULT hr, const char* file, int line) {
    const char* base = file;
    for (const char* p = file; *p; ++p)
        if (*p == '/' || *p == '\\') base = p + 1;
    char buf[64];
    std::snprintf(buf, sizeof buf, "0x%08lx", static_cast<unsigned long>(hr));
    throw Error(std::string(call) + " failed with HRESULT " + buf + " at " + base + ":" + std::to_string(line));
}

namespace {

using D3DCompileFn = HRESULT(WINAPI*)(LPCVOID, SIZE_T, LPCSTR, const D3D_SHADER_MACRO*, ID3DInclude*, LPCSTR, LPCSTR,
                                      UINT, UINT, ID3DBlob**, ID3DBlob**);

D3DCompileFn compiler() {
    static D3DCompileFn fn = [] {
        HMODULE m = LoadLibraryExW(L"d3dcompiler_47.dll", nullptr, LOAD_LIBRARY_SEARCH_SYSTEM32);
        if (!m) m = LoadLibraryW(L"d3dcompiler_47.dll");
        if (!m) throw Error("d3dcompiler_47.dll is not available (it ships with Windows 8.1 and later)");
        auto f = reinterpret_cast<D3DCompileFn>(reinterpret_cast<void*>(GetProcAddress(m, "D3DCompile")));
        if (!f) throw Error("d3dcompiler_47.dll has no D3DCompile");
        return f;
    }();
    return fn;
}

}  // namespace

std::vector<uint8_t> compileHlsl(const char* source, const char* name, const char* target, bool manualGamma,
                                 bool hdrOut, bool pinPush) {
    if (!source) throw Error(std::string("no HLSL for ") + name + " (" + target + ")");
    std::string src = source;
    if (pinPush) {
        const std::string from = "cbuffer Push\n";
        size_t at = src.find(from);
        if (at != std::string::npos)
            src.replace(at, from.size(), "cbuffer Push : register(b" + std::to_string(kPushRegister) + ")\n");
    }
    const D3D_SHADER_MACRO defines[] = {{"SPIRV_CROSS_CONSTANT_ID_0", manualGamma ? "1" : "0"},
                                        {"SPIRV_CROSS_CONSTANT_ID_1", hdrOut ? "1" : "0"},
                                        {nullptr, nullptr}};
    ID3DBlob* code = nullptr;
    ID3DBlob* errors = nullptr;
    HRESULT hr = compiler()(src.data(), src.size(), name, defines, nullptr, "main", target,
                            D3DCOMPILE_OPTIMIZATION_LEVEL3, 0, &code, &errors);
    std::string log;
    if (errors) {
        log.assign(static_cast<const char*>(errors->GetBufferPointer()), errors->GetBufferSize());
        errors->Release();
    }
    if (FAILED(hr) || !code) {
        if (code) code->Release();
        throw Error(std::string("HLSL ") + name + " (" + target + ") does not compile: " + log);
    }
    std::vector<uint8_t> out(static_cast<const uint8_t*>(code->GetBufferPointer()),
                             static_cast<const uint8_t*>(code->GetBufferPointer()) + code->GetBufferSize());
    code->Release();
    return out;
}

HWND windowHandle(SDL_Window* window) {
    SDL_SysWMinfo wm;
    SDL_VERSION(&wm.version);
    if (!window || !SDL_GetWindowWMInfo(window, &wm)) throw Error(std::string("SDL_GetWindowWMInfo failed: ") + SDL_GetError());
    return wm.info.win.window;
}

static std::string narrow(const wchar_t* w) {
    int n = WideCharToMultiByte(CP_UTF8, 0, w, -1, nullptr, 0, nullptr, nullptr);
    std::string s(n > 0 ? n - 1 : 0, '\0');
    if (n > 1) WideCharToMultiByte(CP_UTF8, 0, w, -1, s.data(), n, nullptr, nullptr);
    return s;
}

static void describe(IDXGIAdapter1* a, int index, DeviceInfo& info) {
    DXGI_ADAPTER_DESC1 d{};
    a->GetDesc1(&d);
    info.gpuIndex = index;
    info.gpuName = narrow(d.Description);
    info.vendorId = d.VendorId;
    info.deviceId = d.DeviceId;
    info.gpuType = (d.Flags & DXGI_ADAPTER_FLAG_SOFTWARE) ? "cpu" : (d.DedicatedVideoMemory >= (512ull << 20) ? "discrete" : "integrated");
    LARGE_INTEGER umd{};
    if (SUCCEEDED(a->CheckInterfaceSupport(__uuidof(IDXGIDevice), &umd))) {
        char buf[64];
        std::snprintf(buf, sizeof buf, "%u.%u.%u.%u", HIWORD(umd.HighPart), LOWORD(umd.HighPart), HIWORD(umd.LowPart),
                      LOWORD(umd.LowPart));
        info.driverVersion = buf;
        info.driverVersionRaw = static_cast<uint64_t>(umd.QuadPart);
    }
    info.driverName = "WDDM user-mode driver";
}

Com<IDXGIAdapter1> pickAdapter(IDXGIFactory1* factory, int requested, DeviceInfo& info) {
    Com<IDXGIAdapter1> chosen;
    int chosenIndex = -1;
    for (UINT i = 0;; ++i) {
        Com<IDXGIAdapter1> a;
        if (factory->EnumAdapters1(i, a.put()) == DXGI_ERROR_NOT_FOUND) break;
        DXGI_ADAPTER_DESC1 d{};
        a->GetDesc1(&d);
        if (requested >= 0) {
            if (static_cast<int>(i) == requested) {
                chosen = a;
                chosenIndex = static_cast<int>(i);
                break;
            }
        } else if (!(d.Flags & DXGI_ADAPTER_FLAG_SOFTWARE)) {
            chosen = a;
            chosenIndex = static_cast<int>(i);
            break;
        }
    }
    if (!chosen) {
        if (requested >= 0) throw Error("--gpu " + std::to_string(requested) + " is out of range (see --list-gpus)");
        throw Error("no hardware DXGI adapter found");
    }
    describe(chosen.get(), chosenIndex, info);
    return chosen;
}

bool tearingSupported(IDXGIFactory1* factory) {
    Com<IDXGIFactory5> f5;
    factory->QueryInterface(__uuidof(IDXGIFactory5), reinterpret_cast<void**>(f5.put()));
    if (!f5) return false;
    BOOL allow = FALSE;
    return SUCCEEDED(f5->CheckFeatureSupport(DXGI_FEATURE_PRESENT_ALLOW_TEARING, &allow, sizeof allow)) && allow;
}

LONGLONG qpcNow() {
    LARGE_INTEGER t;
    QueryPerformanceCounter(&t);
    return t.QuadPart;
}

double qpcMs(LONGLONG from, LONGLONG to) {
    static const double freq = [] {
        LARGE_INTEGER f;
        QueryPerformanceFrequency(&f);
        return static_cast<double>(f.QuadPart);
    }();
    return static_cast<double>(to - from) * 1000.0 / freq;
}

bool waitDxgiDisplayed(IDXGISwapChain* sc, LONGLONG startQpc, double& ms) {
    UINT last = 0;
    if (FAILED(sc->GetLastPresentCount(&last))) return false;
    const LONGLONG deadline = qpcNow();
    for (;;) {
        DXGI_FRAME_STATISTICS st{};
        HRESULT hr = sc->GetFrameStatistics(&st);
        if (SUCCEEDED(hr) && st.PresentCount >= last) {
            if (st.PresentCount != last) return false;  // the frame was replaced before it was shown
            ms = qpcMs(startQpc, st.SyncQPCTime.QuadPart);
            return ms >= 0.0;
        }
        if (FAILED(hr) && hr != DXGI_ERROR_FRAME_STATISTICS_DISJOINT) return false;
        if (qpcMs(deadline, qpcNow()) > 250.0) return false;
        SwitchToThread();
    }
}

const char* dxgiFormatName(DXGI_FORMAT f) {
    switch (f) {
        case DXGI_FORMAT_R8G8B8A8_UNORM: return "R8G8B8A8_UNORM";
        case DXGI_FORMAT_R8G8B8A8_UNORM_SRGB: return "R8G8B8A8_UNORM_SRGB";
        case DXGI_FORMAT_B8G8R8A8_UNORM: return "B8G8R8A8_UNORM";
        case DXGI_FORMAT_B8G8R8A8_UNORM_SRGB: return "B8G8R8A8_UNORM_SRGB";
        default: return "other";
    }
}

}  // namespace d3d

int listDxgi() {
    d3d::Com<IDXGIFactory1> factory;
    HR_CHECK(CreateDXGIFactory1(__uuidof(IDXGIFactory1), reinterpret_cast<void**>(factory.put())));
    std::printf("DXGI adapters (D3D10, D3D11, D3D12):\n");
    for (UINT i = 0;; ++i) {
        d3d::Com<IDXGIAdapter1> a;
        if (factory->EnumAdapters1(i, a.put()) == DXGI_ERROR_NOT_FOUND) break;
        DeviceInfo info;
        d3d::describe(a.get(), static_cast<int>(i), info);
        std::printf("  [%u] %s (%s), vendor %s device %s, driver %s\n", i, info.gpuName.c_str(), info.gpuType.c_str(),
                    hex4(info.vendorId).c_str(), hex4(info.deviceId).c_str(), info.driverVersion.c_str());
    }
    return 0;
}

}  // namespace gfx
