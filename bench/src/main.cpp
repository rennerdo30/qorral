// qorral-bench: GPU benchmark with the same scenes in Vulkan, OpenGL and Direct3D 9/10/11/12.
#include "gfx/gfx.h"
#include "scenes/scenes.h"

#include <SDL.h>

#include <algorithm>
#include <chrono>
#include <cmath>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <filesystem>
#include <fstream>
#include <sstream>
#include <string>
#include <vector>

#ifdef _WIN32
#include <windows.h>
#else
#include <sys/utsname.h>
#endif

#ifndef QB_VERSION
#define QB_VERSION "dev"
#endif

namespace {

enum ExitCode { kExitOk = 0, kExitUsage = 1, kExitRuntime = 2, kExitAborted = 3, kExitValidation = 4 };

const char* const kAllScenes[] = {"raymarch", "instanced", "particles", "postfx"};
const char* const kExtraScenes[] = {"latency"};  // only on request, not part of "all" or the score

struct Options {
    std::vector<gfx::Api> apis;
    std::vector<std::string> scenes;
    double seconds = 10.0;
    double warmup = 2.0;
    int width = 1280;
    int height = 720;
    bool fullscreen = false;
    gfx::Present present = gfx::Present::Immediate;
    std::string presentName = "immediate";
    int gpu = -1;
    std::string jsonPath;
    bool listGpus = false;
    bool validate = false;
    bool fxaa = true;
    bool offscreen = false;
    std::string captureDir;
};

struct SceneResult {
    std::string name;
    size_t frames = 0;
    double avgFps = 0, avgMs = 0, medianMs = 0, p99Ms = 0, p999Ms = 0, low1Fps = 0;
    double gpuMedianMs = -1;  // < 0: not available
    size_t gpuSamples = 0;
    // Closed-loop scenes: frame start to displayed (or GPU done), see Device::waitPresented.
    size_t latencySamples = 0;
    double latencyMedianMs = 0, latencyP99Ms = 0, latencyMinMs = 0, latencyMaxMs = 0;
    std::string latencySource;
};

struct ApiRun {
    gfx::Api api{};
    gfx::DeviceInfo info;
    uint32_t width = 0, height = 0;
    uint32_t validationErrors = 0, validationWarnings = 0;
    std::vector<SceneResult> results;
    long score = 0;
    bool aborted = false;
    std::string error;  // non-empty when the API could not run
};

std::string apiList() {
    std::string s;
    for (gfx::Api a : gfx::availableApis()) s += std::string(s.empty() ? "" : ", ") + gfx::apiName(a);
    return s;
}

void usage() {
    std::printf(
        "qorral-bench %s - GPU benchmark: the same scenes in every graphics API\n\n"
        "Usage: qorral-bench [options]\n"
        "  --api NAME|all       %s, a comma list, or all (default vulkan)\n"
        "  --scene NAME|all     raymarch, instanced, particles, postfx, a comma list, or all (default all);\n"
        "                       latency: black, white for 250 ms after each key or click, one frame at a\n"
        "                       time; reports the in-guest frame latency (for qorral latency <vm>)\n"
        "  --seconds N          measured seconds per scene (default 10)\n"
        "  --warmup S           warmup seconds per scene, excluded from stats (default 2)\n"
        "  --width W --height H window size (default 1280x720)\n"
        "  --fullscreen         borderless fullscreen at desktop resolution\n"
        "  --present MODE       immediate|mailbox|fifo (default immediate; falls back to mailbox, then fifo)\n"
        "  --offscreen          render into an image, no window and no present (correctness checks, captures)\n"
        "  --gpu INDEX          use this GPU (see --list-gpus)\n"
        "  --json PATH          write results as JSON\n"
        "  --list-gpus          list the devices of the chosen API and exit\n"
        "  --validate           enable the API's debug layer if installed\n"
        "  --no-fxaa            skip FXAA in the postfx scene\n"
        "  --capture DIR        save each scene's last frame (t = warmup + seconds) as DIR/<api>-<scene>.bmp\n"
        "  --help               this text\n\n"
        "Exit codes: 0 ok, 1 usage, 2 API/runtime error, 3 aborted (Esc), 4 validation errors\n",
        QB_VERSION, apiList().c_str());
}

bool parseNumber(const char* s, double& out) {
    char* end = nullptr;
    out = std::strtod(s, &end);
    return end && *end == '\0' && std::isfinite(out);
}

std::vector<std::string> splitComma(const std::string& s) {
    std::vector<std::string> out;
    std::stringstream ss(s);
    std::string item;
    while (std::getline(ss, item, ',')) out.push_back(item);
    return out;
}

bool parseArgs(int argc, char** argv, Options& o, bool& helpOnly) {
    helpOnly = false;
    std::string sceneArg = "all", apiArg = "vulkan";
    for (int i = 1; i < argc; ++i) {
        std::string a = argv[i];
        auto value = [&](const char*& v) {
            if (i + 1 >= argc) {
                std::fprintf(stderr, "error: %s needs a value\n", a.c_str());
                return false;
            }
            v = argv[++i];
            return true;
        };
        const char* v = nullptr;
        double d = 0;
        if (a == "--help" || a == "-h") {
            helpOnly = true;
            return true;
        } else if (a == "--api") {
            if (!value(v)) return false;
            apiArg = v;
        } else if (a == "--scene") {
            if (!value(v)) return false;
            sceneArg = v;
        } else if (a == "--seconds") {
            if (!value(v) || !parseNumber(v, d) || d <= 0) { std::fprintf(stderr, "error: --seconds needs a positive number\n"); return false; }
            o.seconds = d;
        } else if (a == "--warmup") {
            if (!value(v) || !parseNumber(v, d) || d < 0) { std::fprintf(stderr, "error: --warmup needs a number >= 0\n"); return false; }
            o.warmup = d;
        } else if (a == "--width" || a == "--height") {
            if (!value(v) || !parseNumber(v, d) || d < 1 || d > 16384 || d != std::floor(d)) {
                std::fprintf(stderr, "error: %s needs an integer between 1 and 16384\n", a.c_str());
                return false;
            }
            (a == "--width" ? o.width : o.height) = static_cast<int>(d);
        } else if (a == "--fullscreen") {
            o.fullscreen = true;
        } else if (a == "--present") {
            if (!value(v)) return false;
            o.presentName = v;
            if (o.presentName == "immediate") o.present = gfx::Present::Immediate;
            else if (o.presentName == "mailbox") o.present = gfx::Present::Mailbox;
            else if (o.presentName == "fifo") o.present = gfx::Present::Fifo;
            else { std::fprintf(stderr, "error: --present must be immediate, mailbox or fifo\n"); return false; }
        } else if (a == "--offscreen") {
            o.offscreen = true;
        } else if (a == "--gpu") {
            if (!value(v) || !parseNumber(v, d) || d < 0 || d != std::floor(d)) { std::fprintf(stderr, "error: --gpu needs an index >= 0\n"); return false; }
            o.gpu = static_cast<int>(d);
        } else if (a == "--json") {
            if (!value(v)) return false;
            o.jsonPath = v;
        } else if (a == "--list-gpus") {
            o.listGpus = true;
        } else if (a == "--validate") {
            o.validate = true;
        } else if (a == "--capture") {
            if (!value(v)) return false;
            o.captureDir = v;
        } else if (a == "--no-fxaa") {
            o.fxaa = false;
        } else {
            std::fprintf(stderr, "error: unknown option %s (try --help)\n", a.c_str());
            return false;
        }
    }
    if (apiArg == "all") {
        o.apis = gfx::availableApis();
    } else {
        for (const std::string& item : splitComma(apiArg)) {
            gfx::Api api;
            if (!gfx::parseApi(item, api)) {
                std::fprintf(stderr, "error: unknown API '%s' (%s, all)\n", item.c_str(), apiList().c_str());
                return false;
            }
            const auto avail = gfx::availableApis();
            if (std::find(avail.begin(), avail.end(), api) == avail.end()) {
                std::fprintf(stderr, "error: %s is not available in this build (%s)\n", item.c_str(), apiList().c_str());
                return false;
            }
            o.apis.push_back(api);
        }
    }
    if (o.apis.empty()) { std::fprintf(stderr, "error: --api is empty\n"); return false; }
    if (sceneArg == "all") {
        o.scenes.assign(std::begin(kAllScenes), std::end(kAllScenes));
    } else {
        for (const std::string& item : splitComma(sceneArg)) {
            if (std::find(std::begin(kAllScenes), std::end(kAllScenes), item) == std::end(kAllScenes) &&
                std::find(std::begin(kExtraScenes), std::end(kExtraScenes), item) == std::end(kExtraScenes)) {
                std::fprintf(stderr, "error: unknown scene '%s' (raymarch, instanced, particles, postfx, latency, all)\n",
                             item.c_str());
                return false;
            }
            o.scenes.push_back(item);
        }
        if (o.scenes.empty()) { std::fprintf(stderr, "error: --scene is empty\n"); return false; }
    }
    return true;
}

std::unique_ptr<Scene> makeScene(const std::string& name, const Options& o) {
    if (name == "raymarch") return makeRaymarchScene();
    if (name == "instanced") return makeInstancedScene();
    if (name == "particles") return makeParticlesScene();
    if (name == "latency") return makeLatencyScene();
    return makePostfxScene(o.fxaa);
}

std::string osDescription() {
#ifdef _WIN32
    using RtlGetVersionFn = LONG(WINAPI*)(PRTL_OSVERSIONINFOW);
    HMODULE ntdll = GetModuleHandleW(L"ntdll.dll");
    if (ntdll) {
        auto fn = reinterpret_cast<RtlGetVersionFn>(reinterpret_cast<void*>(GetProcAddress(ntdll, "RtlGetVersion")));
        RTL_OSVERSIONINFOW vi{};
        vi.dwOSVersionInfoSize = sizeof vi;
        if (fn && fn(&vi) == 0) {
            char buf[96];
            std::snprintf(buf, sizeof buf, "Windows %lu.%lu.%lu", vi.dwMajorVersion, vi.dwMinorVersion, vi.dwBuildNumber);
            return buf;
        }
    }
    return "Windows";
#else
    std::string name = SDL_GetPlatform();
    std::ifstream f("/etc/os-release");
    std::string line;
    while (std::getline(f, line)) {
        if (line.rfind("PRETTY_NAME=", 0) == 0) {
            name = line.substr(12);
            name.erase(std::remove(name.begin(), name.end(), '"'), name.end());
            break;
        }
    }
    struct utsname u{};
    if (uname(&u) == 0) name += std::string(" (kernel ") + u.release + ", " + u.machine + ")";
    return name;
#endif
}

double percentile(const std::vector<double>& sorted, double q) {
    if (sorted.empty()) return 0.0;
    size_t rank = static_cast<size_t>(std::ceil(q * sorted.size()));
    rank = std::clamp<size_t>(rank, 1, sorted.size());
    return sorted[rank - 1];
}

void latencyStats(SceneResult& r, std::vector<double> ms, const std::string& source) {
    r.latencySamples = ms.size();
    r.latencySource = source;
    if (ms.empty()) return;
    std::sort(ms.begin(), ms.end());
    r.latencyMedianMs = percentile(ms, 0.5);
    r.latencyP99Ms = percentile(ms, 0.99);
    r.latencyMinMs = ms.front();
    r.latencyMaxMs = ms.back();
}

SceneResult computeStats(const std::string& name, std::vector<double> frameMs, std::vector<double> gpuMs) {
    SceneResult r;
    r.name = name;
    r.frames = frameMs.size();
    if (!frameMs.empty()) {
        double total = 0;
        for (double v : frameMs) total += v;
        std::sort(frameMs.begin(), frameMs.end());
        r.avgMs = total / frameMs.size();
        r.avgFps = total > 0 ? 1000.0 * frameMs.size() / total : 0;
        r.medianMs = percentile(frameMs, 0.5);
        r.p99Ms = percentile(frameMs, 0.99);
        r.p999Ms = percentile(frameMs, 0.999);
        // "1% low": average FPS over the slowest 1% of frames (at least one frame).
        size_t n = std::max<size_t>(1, static_cast<size_t>(std::ceil(frameMs.size() * 0.01)));
        double worst = 0;
        for (size_t i = frameMs.size() - n; i < frameMs.size(); ++i) worst += frameMs[i];
        r.low1Fps = worst > 0 ? 1000.0 * n / worst : 0;
    }
    r.gpuSamples = gpuMs.size();
    if (!gpuMs.empty()) {
        std::sort(gpuMs.begin(), gpuMs.end());
        r.gpuMedianMs = percentile(gpuMs, 0.5);
    }
    return r;
}

// Runs one scene for warmup + seconds. Returns false when the user aborted.
bool runScene(gfx::Device& dev, SDL_Window* window, const Options& o, const std::string& sceneName, size_t index,
              size_t total, SceneResult& result) {
    using clock = std::chrono::steady_clock;
    std::unique_ptr<Scene> scene = makeScene(sceneName, o);
    scene->init(dev);
    scene->createSized(dev);
    dev.finishFrames();
    dev.takeGpuTimes();

    std::vector<double> frameMs;
    frameMs.reserve(65536);
    const double duration = o.warmup + o.seconds;
    const auto start = clock::now();
    auto last = start;
    double lastTime = 0.0, titleTime = -1.0;
    size_t titleFrames = 0;
    bool needRecreate = false, aborted = false;
    const std::string api = gfx::apiName(dev.api());
    uint32_t inputs = 0;
    std::vector<double> latencyMs;
    std::string latencySource;

    auto seconds = [](clock::duration d) { return std::chrono::duration<double>(d).count(); };
    auto frame = [&](double t, float dt, bool measured, const std::string* capture) {
        if (!dev.beginFrame(measured)) return true;
        scene->record(dev, FrameInfo{dev.frameSlot(), t, dt, inputs});
        const bool recreate = dev.endFrame(capture);
        if (scene->closedLoop()) {
            std::string source;
            const double ms = dev.waitPresented(source);
            if (measured) {
                latencyMs.push_back(ms);
                latencySource = source;
            }
        }
        return recreate;
    };

    while (!aborted) {
        SDL_Event ev;
        while (SDL_PollEvent(&ev)) {
            if (ev.type == SDL_QUIT) aborted = true;
            if (ev.type == SDL_KEYDOWN && ev.key.keysym.sym == SDLK_ESCAPE) aborted = true;
            else if ((ev.type == SDL_KEYDOWN && !ev.key.repeat) || ev.type == SDL_MOUSEBUTTONDOWN) ++inputs;
            if (ev.type == SDL_WINDOWEVENT && (ev.window.event == SDL_WINDOWEVENT_SIZE_CHANGED ||
                                               ev.window.event == SDL_WINDOWEVENT_RESIZED))
                needRecreate = !o.offscreen;
        }
        if (aborted) break;
        const auto now = clock::now();
        const double t = seconds(now - start);
        if (t >= duration) break;

        if (!o.offscreen &&
            ((SDL_GetWindowFlags(window) & SDL_WINDOW_MINIMIZED) || (needRecreate && !dev.surfaceHasArea()))) {
            SDL_WaitEventTimeout(nullptr, 50);
            last = clock::now();
            continue;
        }
        if (needRecreate) {
            dev.waitIdle();
            scene->destroySized();
            dev.resize();
            scene->createSized(dev);
            needRecreate = false;
            last = clock::now();
            continue;
        }

        const bool measured = t >= o.warmup;
        const float dt = static_cast<float>(t - lastTime);
        lastTime = t;
        needRecreate = frame(t, dt, measured, nullptr);
        const auto end = clock::now();
        const double ms = 1000.0 * seconds(end - last);
        last = end;
        if (measured) frameMs.push_back(ms);

        ++titleFrames;
        const double tt = seconds(end - start);
        if (titleTime < 0) titleTime = tt;
        if (tt - titleTime >= 0.25 && !o.offscreen) {
            double fps = titleFrames / (tt - titleTime);
            int pct = static_cast<int>(std::min(100.0, 100.0 * tt / duration));
            char title[192];
            std::snprintf(title, sizeof title, "qorral-bench | %s | %s (%zu/%zu) | %.0f FPS | %s %d%%", api.c_str(),
                          sceneName.c_str(), index + 1, total, fps, tt < o.warmup ? "warmup" : "measuring", pct);
            SDL_SetWindowTitle(window, title);
            titleTime = tt;
            titleFrames = 0;
        }
    }

    // One extra, unmeasured frame at exactly t = warmup + seconds, saved as an image.
    if (!aborted && !o.captureDir.empty()) {
        if (!dev.canCapture()) {
            std::fprintf(stderr, "warning: this backbuffer cannot be captured\n");
        } else if (!needRecreate && (o.offscreen || !(SDL_GetWindowFlags(window) & SDL_WINDOW_MINIMIZED))) {
            const std::string path = o.captureDir + "/" + api + "-" + sceneName + ".bmp";
            frame(duration, static_cast<float>(duration - lastTime), false, &path);
        }
    }
    dev.finishFrames();
    scene->destroy();
    result = computeStats(sceneName, std::move(frameMs), dev.takeGpuTimes());
    latencyStats(result, std::move(latencyMs), latencySource);
    return !aborted;
}

std::string jsonEscape(const std::string& s) {
    std::string out;
    for (unsigned char c : s) {
        switch (c) {
            case '"': out += "\\\""; break;
            case '\\': out += "\\\\"; break;
            case '\n': out += "\\n"; break;
            case '\r': out += "\\r"; break;
            case '\t': out += "\\t"; break;
            default:
                if (c < 0x20) {
                    char b[8];
                    std::snprintf(b, sizeof b, "\\u%04x", c);
                    out += b;
                } else {
                    out += static_cast<char>(c);
                }
        }
    }
    return out;
}

std::string num(double v, int decimals = 3) {
    char b[64];
    std::snprintf(b, sizeof b, "%.*f", decimals, v);
    return b;
}

long computeScore(const std::vector<SceneResult>& results) {
    if (results.empty()) return 0;
    double logSum = 0;
    for (auto& r : results) {
        if (r.avgFps <= 0) return 0;
        logSum += std::log(r.avgFps);
    }
    return std::lround(10.0 * std::exp(logSum / results.size()));
}

// One API's results as JSON members, indented by `ind`.
std::string runJson(const ApiRun& run, const Options& o, const std::string& os, const std::string& ind) {
    const gfx::DeviceInfo& g = run.info;
    std::ostringstream j;
    j << ind << "\"api\": \"" << gfx::apiName(run.api) << "\",\n";
    if (!run.error.empty()) {
        j << ind << "\"error\": \"" << jsonEscape(run.error) << "\"\n";
        return j.str();
    }
    j << ind << "\"gpu\": {\n";
    j << ind << "  \"index\": " << g.gpuIndex << ",\n";
    j << ind << "  \"name\": \"" << jsonEscape(g.gpuName) << "\",\n";
    j << ind << "  \"type\": \"" << jsonEscape(g.gpuType) << "\",\n";
    j << ind << "  \"vendor_id\": \"" << gfx::hex4(g.vendorId) << "\",\n";
    j << ind << "  \"device_id\": \"" << gfx::hex4(g.deviceId) << "\",\n";
    j << ind << "  \"driver_name\": \"" << jsonEscape(g.driverName) << "\",\n";
    j << ind << "  \"driver_info\": \"" << jsonEscape(g.driverInfo) << "\",\n";
    j << ind << "  \"driver_version\": \"" << jsonEscape(g.driverVersion) << "\",\n";
    j << ind << "  \"driver_version_raw\": " << g.driverVersionRaw << ",\n";
    j << ind << "  \"api_version\": \"" << jsonEscape(g.apiVersion) << "\"\n";
    j << ind << "},\n";
    j << ind << "\"os\": \"" << jsonEscape(os) << "\",\n";
    j << ind << "\"resolution\": { \"width\": " << run.width << ", \"height\": " << run.height << " },\n";
    j << ind << "\"fullscreen\": " << (o.fullscreen ? "true" : "false") << ",\n";
    j << ind << "\"offscreen\": " << (o.offscreen ? "true" : "false") << ",\n";
    j << ind << "\"present_mode\": \"" << jsonEscape(g.presentMode) << "\",\n";
    j << ind << "\"present_mode_requested\": \"" << o.presentName << "\",\n";
    j << ind << "\"msaa_samples\": " << g.msaaSamples << ",\n";
    j << ind << "\"swapchain_format\": \"" << jsonEscape(g.swapFormat) << "\",\n";
    j << ind << "\"variant\": " << (g.variant.empty() ? std::string("null") : "\"" + jsonEscape(g.variant) + "\"") << ",\n";
    j << ind << "\"seconds_per_scene\": " << num(o.seconds, 2) << ",\n";
    j << ind << "\"warmup_seconds\": " << num(o.warmup, 2) << ",\n";
    j << ind << "\"gpu_timestamps\": " << (g.timestamps ? "true" : "false") << ",\n";
    j << ind << "\"validation\": " << (g.validation ? "true" : "false") << ",\n";
    j << ind << "\"sync_validation\": " << (g.syncValidation ? "true" : "false") << ",\n";
    j << ind << "\"validation_errors\": " << run.validationErrors << ",\n";
    j << ind << "\"scenes\": [\n";
    for (size_t i = 0; i < run.results.size(); ++i) {
        const SceneResult& r = run.results[i];
        j << ind << "  {\n";
        j << ind << "    \"name\": \"" << r.name << "\",\n";
        j << ind << "    \"frames\": " << r.frames << ",\n";
        j << ind << "    \"avg_fps\": " << num(r.avgFps, 2) << ",\n";
        j << ind << "    \"avg_ms\": " << num(r.avgMs) << ",\n";
        j << ind << "    \"median_ms\": " << num(r.medianMs) << ",\n";
        j << ind << "    \"p99_ms\": " << num(r.p99Ms) << ",\n";
        j << ind << "    \"p999_ms\": " << num(r.p999Ms) << ",\n";
        j << ind << "    \"low_1pct_fps\": " << num(r.low1Fps, 2) << ",\n";
        j << ind << "    \"gpu_median_ms\": " << (r.gpuMedianMs >= 0 ? num(r.gpuMedianMs) : std::string("null")) << ",\n";
        j << ind << "    \"gpu_samples\": " << r.gpuSamples;
        if (r.latencySamples) {
            j << ",\n" << ind << "    \"latency_ms\": { \"source\": \"" << r.latencySource << "\", \"samples\": "
              << r.latencySamples << ", \"median\": " << num(r.latencyMedianMs) << ", \"p99\": " << num(r.latencyP99Ms)
              << ", \"min\": " << num(r.latencyMinMs) << ", \"max\": " << num(r.latencyMaxMs) << " }";
        }
        j << "\n";
        j << ind << "  }" << (i + 1 < run.results.size() ? "," : "") << "\n";
    }
    j << ind << "],\n";
    j << ind << "\"score\": " << run.score << ",\n";
    j << ind << "\"aborted\": " << (run.aborted ? "true" : "false") << "\n";
    return j.str();
}

bool writeJson(const std::string& path, const std::vector<ApiRun>& runs, const Options& o, const std::string& os) {
    std::ostringstream j;
    j << "{\n  \"tool\": \"qorral-bench\",\n  \"version\": \"" << QB_VERSION << "\",\n";
    if (runs.size() == 1) {
        j << runJson(runs[0], o, os, "  ");
    } else {
        j << "  \"runs\": [\n";
        for (size_t i = 0; i < runs.size(); ++i)
            j << "    {\n" << runJson(runs[i], o, os, "      ") << "    }" << (i + 1 < runs.size() ? "," : "") << "\n";
        j << "  ]\n";
    }
    j << "}\n";
    std::ofstream f(path, std::ios::binary);
    if (!f) return false;
    f << j.str();
    return static_cast<bool>(f);
}

void printHeader(const ApiRun& run, const Options& o, const std::string& os) {
    const gfx::DeviceInfo& g = run.info;
    const std::string variant = g.variant.empty() ? "" : " (variant: " + g.variant + ")";
    std::printf("API        : %s %s%s\n", gfx::apiName(run.api), g.apiVersion.c_str(), variant.c_str());
    std::printf("GPU        : [%d] %s (%s)\n", g.gpuIndex, g.gpuName.c_str(), g.gpuType.c_str());
    std::printf("Driver     : %s %s (version %s)\n", g.driverName.c_str(), g.driverInfo.c_str(), g.driverVersion.c_str());
    std::printf("OS         : %s\n", os.c_str());
    std::printf("Resolution : %ux%u%s, present %s (requested %s), MSAA %ux, GPU timestamps %s\n", run.width, run.height,
                o.fullscreen ? " fullscreen" : "", g.presentMode.c_str(), o.offscreen ? "offscreen" : o.presentName.c_str(),
                g.msaaSamples, g.timestamps ? "yes" : "no");
    std::printf("Run        : %.1f s warmup + %.1f s per scene%s\n\n", o.warmup, o.seconds,
                g.validation ? (g.syncValidation ? ", validation layer on (with synchronization validation)"
                                                 : ", validation layer on")
                             : "");
    std::fflush(stdout);
}

void printTable(const ApiRun& run) {
    std::printf("%-10s %8s %10s %9s %9s %9s %9s %9s %9s\n", "scene", "frames", "avg FPS", "avg ms", "median",
                "p99 ms", "p99.9 ms", "1% low", "GPU ms");
    std::printf("%s\n", std::string(91, '-').c_str());
    for (auto& r : run.results) {
        std::string gpu = r.gpuMedianMs >= 0 ? num(r.gpuMedianMs) : "n/a";
        std::printf("%-10s %8zu %10.1f %9.3f %9.3f %9.3f %9.3f %9.1f %9s\n", r.name.c_str(), r.frames, r.avgFps, r.avgMs,
                    r.medianMs, r.p99Ms, r.p999Ms, r.low1Fps, gpu.c_str());
    }
    std::printf("%s\n", std::string(91, '-').c_str());
    for (auto& r : run.results)
        if (r.latencySamples)
            std::printf("Latency (%s, frame start to %s): median %.3f ms, p99 %.3f ms, min %.3f, max %.3f (%zu frames)\n",
                        r.name.c_str(), r.latencySource == "display" ? "displayed" : "GPU done", r.latencyMedianMs,
                        r.latencyP99Ms, r.latencyMinMs, r.latencyMaxMs, r.latencySamples);
    std::printf("Score (%s): %ld  (10 x geometric mean of average FPS over %zu scene%s)\n", gfx::apiName(run.api),
                run.score, run.results.size(), run.results.size() == 1 ? "" : "s");
}

void setGlAttributes() {
    SDL_GL_SetAttribute(SDL_GL_CONTEXT_PROFILE_MASK, SDL_GL_CONTEXT_PROFILE_CORE);
    SDL_GL_SetAttribute(SDL_GL_CONTEXT_MAJOR_VERSION, 4);
    SDL_GL_SetAttribute(SDL_GL_CONTEXT_MINOR_VERSION, 3);
    SDL_GL_SetAttribute(SDL_GL_FRAMEBUFFER_SRGB_CAPABLE, 1);
    SDL_GL_SetAttribute(SDL_GL_DOUBLEBUFFER, 1);
    SDL_GL_SetAttribute(SDL_GL_DEPTH_SIZE, 0);
    SDL_GL_SetAttribute(SDL_GL_STENCIL_SIZE, 0);
}

// Runs every scene with one API. Returns false when the user aborted.
bool runApi(gfx::Api api, const Options& o, const std::string& os, ApiRun& run, int& rc) {
    run.api = api;
    if (api == gfx::Api::OpenGL) setGlAttributes();
    Uint32 flags = gfx::sdlWindowFlags(api) | SDL_WINDOW_RESIZABLE | SDL_WINDOW_ALLOW_HIGHDPI;
    if (o.offscreen) flags = gfx::sdlWindowFlags(api) | SDL_WINDOW_HIDDEN;
    else if (o.fullscreen) flags |= SDL_WINDOW_FULLSCREEN_DESKTOP;
    SDL_Window* window = SDL_CreateWindow("qorral-bench", SDL_WINDOWPOS_CENTERED, SDL_WINDOWPOS_CENTERED, o.width,
                                          o.height, flags);
    if (!window) {
        run.error = std::string("SDL_CreateWindow failed: ") + SDL_GetError();
        std::fprintf(stderr, "error: %s\n", run.error.c_str());
        rc = kExitRuntime;
        return true;
    }
    bool aborted = false;
    try {
        gfx::Options go;
        go.gpuIndex = o.gpu;
        go.present = o.present;
        go.validate = o.validate;
        go.offscreen = o.offscreen;
        go.width = static_cast<uint32_t>(o.width);
        go.height = static_cast<uint32_t>(o.height);
        std::unique_ptr<gfx::Device> dev = gfx::Device::create(api, window, go);
        run.info = dev->info();
        run.width = dev->width();
        run.height = dev->height();
        printHeader(run, o, os);
        for (size_t i = 0; i < o.scenes.size(); ++i) {
            std::printf("running %-10s ...", o.scenes[i].c_str());
            std::fflush(stdout);
            SceneResult r;
            if (!runScene(*dev, window, o, o.scenes[i], i, o.scenes.size(), r)) {
                std::printf(" aborted\n");
                aborted = true;
                break;
            }
            std::printf(" %8.1f FPS\n", r.avgFps);
            run.results.push_back(r);
        }
        run.width = dev->width();
        run.height = dev->height();
        run.score = computeScore(run.results);
        run.aborted = aborted;
        std::printf("\n");
        printTable(run);
        if (aborted) std::printf("Aborted by the user; the score covers only finished scenes.\n");
        dev->waitIdle();
        run.validationErrors = dev->validationErrors();
        run.validationWarnings = dev->validationWarnings();
        dev.reset();
        std::printf("\n");
    } catch (const std::exception& e) {
        run.error = e.what();
        std::fprintf(stderr, "\n%s error: %s\n", gfx::apiName(api), e.what());
        rc = kExitRuntime;
    }
    SDL_DestroyWindow(window);
    return !aborted;
}

}  // namespace

int main(int argc, char** argv) {
    Options o;
    bool helpOnly = false;
    if (!parseArgs(argc, argv, o, helpOnly)) return kExitUsage;
    if (helpOnly) {
        usage();
        return kExitOk;
    }
    if (o.listGpus) {
        int rc = kExitOk;
        for (gfx::Api a : o.apis) {
            try {
                if (gfx::Device::listGpus(a) != 0) rc = kExitRuntime;
            } catch (const std::exception& e) {
                std::fprintf(stderr, "error: %s\n", e.what());
                rc = kExitRuntime;
            }
        }
        return rc;
    }
    if (!o.captureDir.empty()) {
        std::error_code ec;
        std::filesystem::create_directories(o.captureDir, ec);
        if (ec) {
            std::fprintf(stderr, "error: cannot create %s: %s\n", o.captureDir.c_str(), ec.message().c_str());
            return kExitUsage;
        }
    }

    SDL_SetMainReady();
#ifdef SDL_HINT_WINDOWS_DPI_AWARENESS
    SDL_SetHint(SDL_HINT_WINDOWS_DPI_AWARENESS, "permonitorv2");  // render exactly WxH pixels
#endif
    if (SDL_Init(SDL_INIT_VIDEO) != 0) {
        std::fprintf(stderr, "error: SDL_Init failed: %s\n", SDL_GetError());
        return kExitRuntime;
    }
    std::printf("qorral-bench %s\n\n", QB_VERSION);
    const std::string os = osDescription();
    int rc = kExitOk;
    bool aborted = false;
    std::vector<ApiRun> runs;
    for (gfx::Api api : o.apis) {
        ApiRun run;
        bool ok = runApi(api, o, os, run, rc);
        runs.push_back(run);
        if (run.info.validation) {
            std::printf("Validation (%s): %u error(s), %u warning(s)\n", gfx::apiName(api), run.validationErrors,
                        run.validationWarnings);
            if (run.validationErrors > 0 && rc == kExitOk) rc = kExitValidation;
        }
        if (!ok) {
            aborted = true;
            break;
        }
    }
    if (runs.size() > 1) {
        std::printf("Summary\n");
        for (const ApiRun& r : runs) {
            if (!r.error.empty()) {
                std::printf("  %-7s failed: %s\n", gfx::apiName(r.api), r.error.c_str());
            } else {
                const std::string variant = r.info.variant.empty() ? "" : "  (variant: " + r.info.variant + ")";
                std::printf("  %-7s score %6ld%s\n", gfx::apiName(r.api), r.score, variant.c_str());
            }
        }
    }
    if (!o.jsonPath.empty()) {
        if (writeJson(o.jsonPath, runs, o, os))
            std::printf("JSON written to %s\n", o.jsonPath.c_str());
        else {
            std::fprintf(stderr, "error: could not write %s\n", o.jsonPath.c_str());
            rc = kExitRuntime;
        }
    }
    if (aborted && rc == kExitOk) rc = kExitAborted;
    SDL_Quit();
    return rc;
}
