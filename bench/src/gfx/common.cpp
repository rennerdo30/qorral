#include "gfx/gfx.h"

#include <SDL.h>

#include <cstdio>
#include <cstring>

namespace gfx {

// Backend factories, one per compiled-in API (see CMakeLists.txt).
#ifdef QB_HAVE_VULKAN
std::unique_ptr<Device> createVulkan(SDL_Window* window, const Options& opt);
int listVulkan();
#endif
#ifdef QB_HAVE_OPENGL
std::unique_ptr<Device> createOpenGL(SDL_Window* window, const Options& opt);
int listOpenGL();
#endif
#ifdef QB_HAVE_D3D
std::unique_ptr<Device> createD3D9(SDL_Window* window, const Options& opt);
std::unique_ptr<Device> createD3D10(SDL_Window* window, const Options& opt);
std::unique_ptr<Device> createD3D11(SDL_Window* window, const Options& opt);
std::unique_ptr<Device> createD3D12(SDL_Window* window, const Options& opt);
int listDxgi();
int listD3D9();
#endif

extern const ShaderSource kShaders[];
extern const size_t kShaderCount;

const char* apiName(Api a) {
    switch (a) {
        case Api::Vulkan: return "vulkan";
        case Api::OpenGL: return "opengl";
        case Api::D3D9: return "d3d9";
        case Api::D3D10: return "d3d10";
        case Api::D3D11: return "d3d11";
        case Api::D3D12: return "d3d12";
    }
    return "?";
}

std::vector<Api> availableApis() {
    std::vector<Api> v;
#ifdef QB_HAVE_VULKAN
    v.push_back(Api::Vulkan);
#endif
#ifdef QB_HAVE_OPENGL
    v.push_back(Api::OpenGL);
#endif
#ifdef QB_HAVE_D3D
    v.insert(v.end(), {Api::D3D12, Api::D3D11, Api::D3D10, Api::D3D9});
#endif
    return v;
}

bool parseApi(const std::string& s, Api& out) {
    for (Api a : {Api::Vulkan, Api::OpenGL, Api::D3D9, Api::D3D10, Api::D3D11, Api::D3D12}) {
        if (s == apiName(a)) {
            out = a;
            return true;
        }
    }
    if (s == "gl") { out = Api::OpenGL; return true; }
    if (s == "vk") { out = Api::Vulkan; return true; }
    return false;
}

uint32_t sdlWindowFlags(Api a) {
    if (a == Api::Vulkan) return SDL_WINDOW_VULKAN;
    if (a == Api::OpenGL) return SDL_WINDOW_OPENGL;
    return 0;
}

const char* presentName(Present p) {
    switch (p) {
        case Present::Immediate: return "immediate";
        case Present::Mailbox: return "mailbox";
        case Present::Fifo: return "fifo";
    }
    return "?";
}

std::unique_ptr<Device> Device::create(Api api, SDL_Window* window, const Options& opt) {
    switch (api) {
#ifdef QB_HAVE_VULKAN
        case Api::Vulkan: return createVulkan(window, opt);
#endif
#ifdef QB_HAVE_OPENGL
        case Api::OpenGL: return createOpenGL(window, opt);
#endif
#ifdef QB_HAVE_D3D
        case Api::D3D9: return createD3D9(window, opt);
        case Api::D3D10: return createD3D10(window, opt);
        case Api::D3D11: return createD3D11(window, opt);
        case Api::D3D12: return createD3D12(window, opt);
#endif
        default: break;
    }
    throw Error(std::string(apiName(api)) + " is not available in this build");
}

int Device::listGpus(Api api) {
    switch (api) {
#ifdef QB_HAVE_VULKAN
        case Api::Vulkan: return listVulkan();
#endif
#ifdef QB_HAVE_OPENGL
        case Api::OpenGL: return listOpenGL();
#endif
#ifdef QB_HAVE_D3D
        case Api::D3D9: return listD3D9();
        case Api::D3D10:
        case Api::D3D11:
        case Api::D3D12: return listDxgi();
#endif
        default: break;
    }
    std::printf("%s is not available in this build\n", apiName(api));
    return 1;
}

const ShaderSource& shader(const char* name) {
    for (size_t i = 0; i < kShaderCount; ++i)
        if (std::strcmp(kShaders[i].name, name) == 0) return kShaders[i];
    throw Error(std::string("unknown shader ") + name);
}

std::string hex4(uint32_t v) {
    char b[16];
    std::snprintf(b, sizeof b, "0x%04x", v);
    return b;
}

bool writeBmp(const std::string& path, const uint8_t* px, uint32_t w, uint32_t h, size_t rowPitch, bool bgra) {
    const uint32_t rowBytes = (w * 3 + 3) & ~3u;
    const uint32_t dataSize = rowBytes * h;
    uint8_t header[54] = {'B', 'M'};
    auto put32 = [&](int off, uint32_t v) {
        for (int i = 0; i < 4; ++i) header[off + i] = static_cast<uint8_t>(v >> (8 * i));
    };
    put32(2, 54 + dataSize);
    put32(10, 54);
    put32(14, 40);
    put32(18, w);
    put32(22, h);
    header[26] = 1;
    header[28] = 24;
    put32(34, dataSize);
    FILE* f = std::fopen(path.c_str(), "wb");
    if (!f) return false;
    bool ok = std::fwrite(header, 1, sizeof header, f) == sizeof header;
    std::vector<uint8_t> row(rowBytes, 0);
    for (uint32_t y = 0; y < h && ok; ++y) {
        const uint8_t* src = px + static_cast<size_t>(h - 1 - y) * rowPitch;  // BMP rows are bottom-up
        for (uint32_t x = 0; x < w; ++x) {
            const uint8_t* p = src + x * 4;
            row[x * 3 + 0] = bgra ? p[0] : p[2];
            row[x * 3 + 1] = p[1];
            row[x * 3 + 2] = bgra ? p[2] : p[0];
        }
        ok = std::fwrite(row.data(), 1, rowBytes, f) == rowBytes;
    }
    return std::fclose(f) == 0 && ok;
}

}  // namespace gfx
