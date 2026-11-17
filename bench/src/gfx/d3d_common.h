// Shared pieces of the Direct3D backends: HRESULT checks, a small COM pointer, the HLSL compiler
// (d3dcompiler_47.dll, loaded at run time), DXGI adapter selection and the window handle.
#pragma once

#include "gfx/gfx.h"

#ifndef NOMINMAX
#define NOMINMAX
#endif
#include <windows.h>
#include <dxgi1_6.h>

#include <string>
#include <utility>
#include <vector>

namespace gfx::d3d {

[[noreturn]] void failHr(const char* call, HRESULT hr, const char* file, int line);

#define HR_CHECK(expr)                                                         \
    do {                                                                       \
        HRESULT hr_ = (expr);                                                  \
        if (FAILED(hr_)) ::gfx::d3d::failHr(#expr, hr_, __FILE__, __LINE__);   \
    } while (0)

// Minimal COM smart pointer (no WRL dependency).
template <typename T>
class Com {
public:
    Com() = default;
    Com(const Com& o) : p_(o.p_) { if (p_) p_->AddRef(); }
    Com(Com&& o) noexcept : p_(std::exchange(o.p_, nullptr)) {}
    ~Com() { reset(); }
    Com& operator=(Com o) noexcept {
        std::swap(p_, o.p_);
        return *this;
    }
    void reset() {
        if (p_) p_->Release();
        p_ = nullptr;
    }
    T* get() const { return p_; }
    T* operator->() const { return p_; }
    T** put() {
        reset();
        return &p_;
    }
    explicit operator bool() const { return p_ != nullptr; }
    template <typename U>
    Com<U> as() const {
        Com<U> out;
        if (p_) p_->QueryInterface(__uuidof(U), reinterpret_cast<void**>(out.put()));
        return out;
    }

private:
    T* p_ = nullptr;
};

// Compiles HLSL from the shader table. `target` like "vs_5_0". Specialization constants become
// macros; the "Push" constant block is pinned to register b4 (SM 4.0 and 5.x).
std::vector<uint8_t> compileHlsl(const char* source, const char* name, const char* target, bool manualGamma,
                                 bool hdrOut, bool pinPush = true);
constexpr UINT kPushRegister = 4;

HWND windowHandle(SDL_Window* window);

// Picks an adapter (requested index, or the first hardware adapter) and fills the device info.
Com<IDXGIAdapter1> pickAdapter(IDXGIFactory1* factory, int requested, DeviceInfo& info);
bool tearingSupported(IDXGIFactory1* factory);

const char* dxgiFormatName(DXGI_FORMAT f);

LONGLONG qpcNow();
double qpcMs(LONGLONG from, LONGLONG to);
// Waits (at most 250 ms) until DXGI's frame statistics report the last Present as displayed and
// returns the ms from `startQpc` to its display time. False when the statistics are not available.
bool waitDxgiDisplayed(IDXGISwapChain* sc, LONGLONG startQpc, double& ms);

}  // namespace gfx::d3d
