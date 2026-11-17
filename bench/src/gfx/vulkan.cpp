// Vulkan backend: core Vulkan 1.2 plus VK_KHR_swapchain (VK_EXT_debug_utils with --validate).
// Render passes, framebuffers and descriptor sets are cached, so a frame records only commands.
#include "gfx/gfx.h"

#include <SDL.h>
#include <SDL_vulkan.h>
#include <vulkan/vulkan.h>

#include <algorithm>
#include <array>
#include <chrono>
#include <cstdio>
#include <cstring>
#include <map>
#include <tuple>

namespace gfx {
namespace {

constexpr uint64_t kTimeoutNs = 10ull * 1000 * 1000 * 1000;  // a frame taking 10 s means a hung GPU
const char* kValidationLayer = "VK_LAYER_KHRONOS_validation";

const char* baseName(const char* path) {
    const char* b = path;
    for (const char* p = path; *p; ++p)
        if (*p == '/' || *p == '\\') b = p + 1;
    return b;
}

const char* vkResultName(VkResult r) {
    switch (r) {
#define C(x) \
    case x: return #x;
        C(VK_SUCCESS) C(VK_NOT_READY) C(VK_TIMEOUT) C(VK_EVENT_SET) C(VK_EVENT_RESET) C(VK_INCOMPLETE)
        C(VK_ERROR_OUT_OF_HOST_MEMORY) C(VK_ERROR_OUT_OF_DEVICE_MEMORY) C(VK_ERROR_INITIALIZATION_FAILED)
        C(VK_ERROR_DEVICE_LOST) C(VK_ERROR_MEMORY_MAP_FAILED) C(VK_ERROR_LAYER_NOT_PRESENT)
        C(VK_ERROR_EXTENSION_NOT_PRESENT) C(VK_ERROR_FEATURE_NOT_PRESENT) C(VK_ERROR_INCOMPATIBLE_DRIVER)
        C(VK_ERROR_TOO_MANY_OBJECTS) C(VK_ERROR_FORMAT_NOT_SUPPORTED) C(VK_ERROR_FRAGMENTED_POOL)
        C(VK_ERROR_UNKNOWN) C(VK_ERROR_OUT_OF_POOL_MEMORY) C(VK_ERROR_INVALID_EXTERNAL_HANDLE)
        C(VK_ERROR_FRAGMENTATION) C(VK_ERROR_INVALID_OPAQUE_CAPTURE_ADDRESS) C(VK_ERROR_SURFACE_LOST_KHR)
        C(VK_ERROR_NATIVE_WINDOW_IN_USE_KHR) C(VK_SUBOPTIMAL_KHR) C(VK_ERROR_OUT_OF_DATE_KHR)
        C(VK_ERROR_INCOMPATIBLE_DISPLAY_KHR) C(VK_ERROR_VALIDATION_FAILED_EXT)
#undef C
        default: return "VK_RESULT_UNKNOWN";
    }
}

[[noreturn]] void fail(const char* call, VkResult r, const char* file, int line) {
    throw Error(std::string(call) + " failed with " + vkResultName(r) + " (" + std::to_string(static_cast<int>(r)) +
                ") at " + baseName(file) + ":" + std::to_string(line));
}

#define VK_CHECK(expr)                                              \
    do {                                                            \
        VkResult vkr_ = (expr);                                     \
        if (vkr_ != VK_SUCCESS) fail(#expr, vkr_, __FILE__, __LINE__); \
    } while (0)

std::string formatApiVersion(uint32_t v) {
    return std::to_string(VK_API_VERSION_MAJOR(v)) + "." + std::to_string(VK_API_VERSION_MINOR(v)) + "." +
           std::to_string(VK_API_VERSION_PATCH(v));
}

std::string formatDriverVersion(uint32_t vendor, uint32_t v) {
    char buf[64];
    if (vendor == 0x10DE) {  // 10.8.8.6 packing
        std::snprintf(buf, sizeof buf, "%u.%u.%u.%u", (v >> 22) & 0x3ff, (v >> 14) & 0xff, (v >> 6) & 0xff, v & 0x3f);
        return buf;
    }
#ifdef _WIN32
    if (vendor == 0x8086) {  // 18.14 packing on Windows
        std::snprintf(buf, sizeof buf, "%u.%u", v >> 14, v & 0x3fff);
        return buf;
    }
#endif
    return formatApiVersion(v);
}

const char* deviceTypeName(VkPhysicalDeviceType t) {
    switch (t) {
        case VK_PHYSICAL_DEVICE_TYPE_DISCRETE_GPU: return "discrete";
        case VK_PHYSICAL_DEVICE_TYPE_INTEGRATED_GPU: return "integrated";
        case VK_PHYSICAL_DEVICE_TYPE_VIRTUAL_GPU: return "virtual";
        case VK_PHYSICAL_DEVICE_TYPE_CPU: return "cpu";
        default: return "other";
    }
}

const char* presentModeName(VkPresentModeKHR m) {
    switch (m) {
        case VK_PRESENT_MODE_IMMEDIATE_KHR: return "immediate";
        case VK_PRESENT_MODE_MAILBOX_KHR: return "mailbox";
        case VK_PRESENT_MODE_FIFO_KHR: return "fifo";
        case VK_PRESENT_MODE_FIFO_RELAXED_KHR: return "fifo_relaxed";
        default: return "other";
    }
}

const char* formatName(VkFormat f) {
    switch (f) {
        case VK_FORMAT_B8G8R8A8_SRGB: return "B8G8R8A8_SRGB";
        case VK_FORMAT_R8G8B8A8_SRGB: return "R8G8B8A8_SRGB";
        case VK_FORMAT_B8G8R8A8_UNORM: return "B8G8R8A8_UNORM";
        case VK_FORMAT_R8G8B8A8_UNORM: return "R8G8B8A8_UNORM";
        default: return "other";
    }
}

VKAPI_ATTR VkBool32 VKAPI_CALL debugCallback(VkDebugUtilsMessageSeverityFlagBitsEXT severity,
                                             VkDebugUtilsMessageTypeFlagsEXT types,
                                             const VkDebugUtilsMessengerCallbackDataEXT* data, void* user);

bool hasLayer(const char* name) {
    uint32_t n = 0;
    if (vkEnumerateInstanceLayerProperties(&n, nullptr) != VK_SUCCESS) return false;
    std::vector<VkLayerProperties> layers(n);
    if (vkEnumerateInstanceLayerProperties(&n, layers.data()) != VK_SUCCESS) return false;
    for (auto& l : layers)
        if (std::strcmp(l.layerName, name) == 0) return true;
    return false;
}

bool hasInstanceExtension(const char* layer, const char* name) {
    uint32_t n = 0;
    if (vkEnumerateInstanceExtensionProperties(layer, &n, nullptr) != VK_SUCCESS) return false;
    std::vector<VkExtensionProperties> exts(n);
    if (vkEnumerateInstanceExtensionProperties(layer, &n, exts.data()) != VK_SUCCESS) return false;
    for (auto& e : exts)
        if (std::strcmp(e.extensionName, name) == 0) return true;
    return false;
}

bool deviceHasSwapchain(VkPhysicalDevice pd) {
    uint32_t n = 0;
    if (vkEnumerateDeviceExtensionProperties(pd, nullptr, &n, nullptr) != VK_SUCCESS) return false;
    std::vector<VkExtensionProperties> exts(n);
    if (vkEnumerateDeviceExtensionProperties(pd, nullptr, &n, exts.data()) != VK_SUCCESS) return false;
    for (auto& e : exts)
        if (std::strcmp(e.extensionName, VK_KHR_SWAPCHAIN_EXTENSION_NAME) == 0) return true;
    return false;
}

// Queue family with graphics + compute (+ present when a surface is given), or -1.
int findQueueFamily(VkPhysicalDevice pd, VkSurfaceKHR surface) {
    uint32_t n = 0;
    vkGetPhysicalDeviceQueueFamilyProperties(pd, &n, nullptr);
    std::vector<VkQueueFamilyProperties> fams(n);
    vkGetPhysicalDeviceQueueFamilyProperties(pd, &n, fams.data());
    for (uint32_t i = 0; i < n; ++i) {
        const VkQueueFlags need = VK_QUEUE_GRAPHICS_BIT | VK_QUEUE_COMPUTE_BIT;
        if ((fams[i].queueFlags & need) != need) continue;
        if (surface) {
            VkBool32 present = VK_FALSE;
            VK_CHECK(vkGetPhysicalDeviceSurfaceSupportKHR(pd, i, surface, &present));
            if (!present) continue;
        }
        return static_cast<int>(i);
    }
    return -1;
}

int typeRank(VkPhysicalDeviceType t) {
    switch (t) {
        case VK_PHYSICAL_DEVICE_TYPE_DISCRETE_GPU: return 0;
        case VK_PHYSICAL_DEVICE_TYPE_INTEGRATED_GPU: return 1;
        case VK_PHYSICAL_DEVICE_TYPE_VIRTUAL_GPU: return 2;
        case VK_PHYSICAL_DEVICE_TYPE_OTHER: return 3;
        default: return 4;  // CPU
    }
}

void queryDriver(VkPhysicalDevice pd, std::string& name, std::string& info) {
    VkPhysicalDeviceProperties p{};
    vkGetPhysicalDeviceProperties(pd, &p);
    name.clear();
    info.clear();
    if (p.apiVersion < VK_API_VERSION_1_2) return;
    VkPhysicalDeviceDriverProperties drv{VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_DRIVER_PROPERTIES};
    VkPhysicalDeviceProperties2 p2{VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_PROPERTIES_2};
    p2.pNext = &drv;
    vkGetPhysicalDeviceProperties2(pd, &p2);
    name = drv.driverName;
    info = drv.driverInfo;
}

struct Image {
    VkImage image = VK_NULL_HANDLE;
    VkDeviceMemory memory = VK_NULL_HANDLE;
    VkImageView view = VK_NULL_HANDLE;
};

struct BufferRec {
    bool alive = false;
    BufferKind kind{};
    VkDeviceSize size = 0;
    // Uniform buffers have one host-visible copy per frame in flight; the others use [0].
    VkBuffer buffer[kFramesInFlight]{};
    VkDeviceMemory memory[kFramesInFlight]{};
    void* mapped[kFramesInFlight]{};
};

struct TextureRec {
    bool alive = false;
    Image img;
    VkFormat format = VK_FORMAT_UNDEFINED;
    uint32_t w = 0, h = 0, samples = 1;
    bool depth = false;
};

struct PipelineRec {
    bool alive = false;
    bool compute = false;
    VkPipeline pipeline = VK_NULL_HANDLE;
    VkPipelineLayout layout = VK_NULL_HANDLE;
    VkDescriptorSetLayout setLayout = VK_NULL_HANDLE;
    Res res[kMaxBindings]{};
    VkShaderStageFlags pushStages = 0;
    uint32_t constBytes = 0;
};

struct PassKey {
    VkFormat color;
    uint32_t colorCount;
    uint32_t samples;
    Load load;
    bool toBackbuffer;  // final layout PRESENT (windowed) or TRANSFER_SRC (offscreen)
    VkFormat depth;
    bool resolve;
    bool operator<(const PassKey& o) const {
        return std::tie(color, colorCount, samples, load, toBackbuffer, depth, resolve) <
               std::tie(o.color, o.colorCount, o.samples, o.load, o.toBackbuffer, o.depth, o.resolve);
    }
};

class VulkanDevice final : public Device {
public:
    VulkanDevice(SDL_Window* window, const Options& opt);
    ~VulkanDevice() override;

    Api api() const override { return Api::Vulkan; }
    const DeviceInfo& info() const override { return info_; }
    uint32_t width() const override { return extent_.width; }
    uint32_t height() const override { return extent_.height; }
    bool supportsCompute() const override { return true; }
    uint32_t validationErrors() const override { return validationErrors_; }
    uint32_t validationWarnings() const override { return validationWarnings_; }

    bool surfaceHasArea() override;
    void resize() override;
    void waitIdle() override { VK_CHECK(vkDeviceWaitIdle(device_)); }

    bool beginFrame(bool measured) override;
    uint32_t frameSlot() const override { return slot_; }
    bool endFrame(const std::string* capturePath) override;
    void finishFrames() override;
    std::vector<double> takeGpuTimes() override {
        std::vector<double> out;
        out.swap(gpuTimesMs_);
        return out;
    }
    bool canCapture() const override { return canCapture_; }
    double waitPresented(std::string& source) override;

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
    void endPass() override { vkCmdEndRenderPass(cmd_[slot_]); }
    void setPipeline(Pipeline p) override;
    void setConstants(const void* data, uint32_t size) override;
    void setUniform(uint32_t binding, Buffer b) override { bindRes(binding, b.id); }
    void setTexture(uint32_t binding, Texture t) override { bindRes(binding, t.id); }
    void setStorage(uint32_t binding, Buffer b) override { bindRes(binding, b.id); }
    void setVertexBuffer(uint32_t slot, Buffer b) override;
    void setIndexBuffer(Buffer b) override;
    void draw(uint32_t vertexCount, uint32_t instanceCount) override;
    void drawIndexed(uint32_t indexCount, uint32_t instanceCount, uint32_t firstInstance) override;
    void dispatch(uint32_t groupsX) override;
    void storageToCompute() override;
    void storageToVertex() override;

    uint32_t validationErrors_ = 0, validationWarnings_ = 0;

private:
    void createInstance(bool validate);
    void pickDevice(int requested);
    void createDevice();
    void createSwapchain();
    void destroySwapchain(bool keepHandle);
    void createOffscreenTarget();
    void collectTimestamps(uint32_t slot);
    uint32_t findMemoryType(uint32_t typeBits, VkMemoryPropertyFlags props) const;
    void allocBuffer(VkDeviceSize size, VkBufferUsageFlags usage, bool hostVisible, VkBuffer& buf,
                     VkDeviceMemory& mem, void** mapped);
    Image createImage(VkFormat format, uint32_t w, uint32_t h, VkImageUsageFlags usage, uint32_t samples,
                      VkImageAspectFlags aspect);
    void destroyImage(Image& img);
    void immediateSubmit(const std::function<void(VkCommandBuffer)>& fn);
    VkRenderPass renderPass(const PassKey& k);
    VkFramebuffer framebuffer(VkRenderPass pass, const std::vector<VkImageView>& views, uint32_t w, uint32_t h);
    VkShaderModule module(const char* name);
    void bindRes(uint32_t binding, uint32_t id);
    void flushBindings();
    void clearCaches();
    VkImageView backbufferView() const;
    VkImage backbufferImage() const;

    SDL_Window* window_ = nullptr;
    Options opt_;
    DeviceInfo info_;
    VkInstance instance_ = VK_NULL_HANDLE;
    VkDebugUtilsMessengerEXT messenger_ = VK_NULL_HANDLE;
    VkSurfaceKHR surface_ = VK_NULL_HANDLE;
    VkPhysicalDevice physical_ = VK_NULL_HANDLE;
    VkPhysicalDeviceProperties props_{};
    VkDevice device_ = VK_NULL_HANDLE;
    VkQueue queue_ = VK_NULL_HANDLE;
    uint32_t queueFamily_ = 0;
    VkFormat depthFormat_ = VK_FORMAT_UNDEFINED;
    VkSampler sampler_ = VK_NULL_HANDLE;

    // Backbuffer: the swapchain, or one offscreen image.
    VkSwapchainKHR swapchain_ = VK_NULL_HANDLE;
    VkFormat swapFormat_ = VK_FORMAT_UNDEFINED;
    VkExtent2D extent_{};
    VkPresentModeKHR presentMode_ = VK_PRESENT_MODE_FIFO_KHR;
    std::vector<VkImage> swapImages_;
    std::vector<VkImageView> swapViews_;
    std::vector<VkSemaphore> renderDone_;  // one per swapchain image
    Image offscreen_;
    bool manualGamma_ = false;
    bool canCapture_ = false;

    // Frames
    VkCommandPool cmdPool_ = VK_NULL_HANDLE;
    VkCommandBuffer cmd_[kFramesInFlight]{};
    VkFence fences_[kFramesInFlight]{};
    VkSemaphore imageAvailable_[kFramesInFlight]{};
    uint32_t slot_ = 0;
    uint32_t lastSlot_ = 0;
    uint32_t imageIndex_ = 0;
    std::chrono::steady_clock::time_point frameStart_{};
    bool presentWait_ = false;  // VK_KHR_present_id + VK_KHR_present_wait enabled
    uint64_t presentId_ = 0;
    PFN_vkWaitForPresentKHR waitForPresent_ = nullptr;
    bool recreateAfterFrame_ = false;
    uint64_t frameCounter_ = 0;
    VkQueryPool queryPool_ = VK_NULL_HANDLE;
    double timestampPeriodNs_ = 1.0;
    uint64_t timestampMask_ = ~0ull;
    bool slotPending_[kFramesInFlight]{};
    bool slotMeasured_[kFramesInFlight]{};
    std::vector<double> gpuTimesMs_;

    // Resources (handle id = index + 1)
    std::vector<BufferRec> buffers_;
    std::vector<TextureRec> textures_;
    std::vector<PipelineRec> pipelines_;
    std::map<PassKey, VkRenderPass> passes_;
    std::map<std::vector<uint64_t>, VkFramebuffer> framebuffers_;
    std::map<std::vector<uint64_t>, VkDescriptorSet> sets_;
    std::vector<VkDescriptorPool> pools_;
    uint32_t poolUsed_ = 0;

    // Recording state
    const PipelineRec* bound_ = nullptr;
    uint32_t boundRes_[kMaxBindings]{};
    bool bindingsDirty_ = false;
    VkRenderPass currentPass_ = VK_NULL_HANDLE;
};

VKAPI_ATTR VkBool32 VKAPI_CALL debugCallback(VkDebugUtilsMessageSeverityFlagBitsEXT severity,
                                             VkDebugUtilsMessageTypeFlagsEXT types,
                                             const VkDebugUtilsMessengerCallbackDataEXT* data, void* user) {
    auto* dev = static_cast<VulkanDevice*>(user);
    if (types == VK_DEBUG_UTILS_MESSAGE_TYPE_GENERAL_BIT_EXT && !(severity & VK_DEBUG_UTILS_MESSAGE_SEVERITY_ERROR_BIT_EXT)) {
        std::fprintf(stderr, "[vulkan loader] %s\n", data->pMessage);
        return VK_FALSE;
    }
    if (severity & VK_DEBUG_UTILS_MESSAGE_SEVERITY_ERROR_BIT_EXT) {
        if (dev) dev->validationErrors_++;
        std::fprintf(stderr, "[validation error] %s\n", data->pMessage);
    } else if (severity & VK_DEBUG_UTILS_MESSAGE_SEVERITY_WARNING_BIT_EXT) {
        if (dev) dev->validationWarnings_++;
        std::fprintf(stderr, "[validation warning] %s\n", data->pMessage);
    }
    return VK_FALSE;
}

// ---------------------------------------------------------------------------------------------
// Instance, device, swapchain

VulkanDevice::VulkanDevice(SDL_Window* window, const Options& opt) : window_(window), opt_(opt) {
    createInstance(opt.validate);
    if (!opt.offscreen) {
        if (!SDL_Vulkan_CreateSurface(window, instance_, &surface_))
            throw Error(std::string("SDL_Vulkan_CreateSurface failed: ") + SDL_GetError());
    }
    pickDevice(opt.gpuIndex);
    createDevice();
    if (opt.offscreen)
        createOffscreenTarget();
    else
        createSwapchain();
}

VulkanDevice::~VulkanDevice() {
    if (device_) {
        vkDeviceWaitIdle(device_);
        for (uint32_t i = 0; i < pipelines_.size(); ++i)
            if (pipelines_[i].alive) destroy(Pipeline{{i + 1}});
        for (uint32_t i = 0; i < textures_.size(); ++i)
            if (textures_[i].alive) destroy(Texture{{i + 1}});
        for (uint32_t i = 0; i < buffers_.size(); ++i)
            if (buffers_[i].alive) destroy(Buffer{{i + 1}});
        clearCaches();
        for (auto& [k, p] : passes_) vkDestroyRenderPass(device_, p, nullptr);
        for (VkDescriptorPool p : pools_) vkDestroyDescriptorPool(device_, p, nullptr);
        destroySwapchain(false);
        destroyImage(offscreen_);
        if (sampler_) vkDestroySampler(device_, sampler_, nullptr);
        for (uint32_t i = 0; i < kFramesInFlight; ++i) {
            if (fences_[i]) vkDestroyFence(device_, fences_[i], nullptr);
            if (imageAvailable_[i]) vkDestroySemaphore(device_, imageAvailable_[i], nullptr);
        }
        if (queryPool_) vkDestroyQueryPool(device_, queryPool_, nullptr);
        if (cmdPool_) vkDestroyCommandPool(device_, cmdPool_, nullptr);
        vkDestroyDevice(device_, nullptr);
    }
    if (surface_) vkDestroySurfaceKHR(instance_, surface_, nullptr);
    if (messenger_) {
        auto destroyFn = reinterpret_cast<PFN_vkDestroyDebugUtilsMessengerEXT>(
            vkGetInstanceProcAddr(instance_, "vkDestroyDebugUtilsMessengerEXT"));
        if (destroyFn) destroyFn(instance_, messenger_, nullptr);
    }
    if (instance_) vkDestroyInstance(instance_, nullptr);
}

void VulkanDevice::createInstance(bool validate) {
    uint32_t apiVersion = VK_API_VERSION_1_0;
    VK_CHECK(vkEnumerateInstanceVersion(&apiVersion));
    if (apiVersion < VK_API_VERSION_1_2)
        throw Error("the Vulkan loader supports only " + formatApiVersion(apiVersion) + ", 1.2 is required");

    std::vector<const char*> exts;
    if (!opt_.offscreen) {
        unsigned int extCount = 0;
        if (!SDL_Vulkan_GetInstanceExtensions(window_, &extCount, nullptr))
            throw Error(std::string("SDL_Vulkan_GetInstanceExtensions failed: ") + SDL_GetError());
        exts.resize(extCount);
        if (!SDL_Vulkan_GetInstanceExtensions(window_, &extCount, exts.data()))
            throw Error(std::string("SDL_Vulkan_GetInstanceExtensions failed: ") + SDL_GetError());
    }

    std::vector<const char*> layers;
    bool debugUtils = false, syncValidation = false;
    if (validate) {
        if (hasLayer(kValidationLayer)) {
            layers.push_back(kValidationLayer);
            info_.validation = true;
            debugUtils = hasInstanceExtension(nullptr, VK_EXT_DEBUG_UTILS_EXTENSION_NAME) ||
                         hasInstanceExtension(kValidationLayer, VK_EXT_DEBUG_UTILS_EXTENSION_NAME);
            syncValidation = hasInstanceExtension(kValidationLayer, VK_EXT_VALIDATION_FEATURES_EXTENSION_NAME);
            if (debugUtils) exts.push_back(VK_EXT_DEBUG_UTILS_EXTENSION_NAME);
            if (syncValidation) exts.push_back(VK_EXT_VALIDATION_FEATURES_EXTENSION_NAME);
        } else {
            std::fprintf(stderr, "warning: --validate given but %s is not installed; running without it\n",
                         kValidationLayer);
        }
    }

    VkApplicationInfo app{VK_STRUCTURE_TYPE_APPLICATION_INFO};
    app.pApplicationName = "qorral-bench";
    app.applicationVersion = VK_MAKE_API_VERSION(0, 2, 0, 0);
    app.pEngineName = "qorral-bench";
    app.apiVersion = VK_API_VERSION_1_2;

    VkDebugUtilsMessengerCreateInfoEXT dbg{VK_STRUCTURE_TYPE_DEBUG_UTILS_MESSENGER_CREATE_INFO_EXT};
    dbg.messageSeverity = VK_DEBUG_UTILS_MESSAGE_SEVERITY_WARNING_BIT_EXT | VK_DEBUG_UTILS_MESSAGE_SEVERITY_ERROR_BIT_EXT;
    dbg.messageType = VK_DEBUG_UTILS_MESSAGE_TYPE_GENERAL_BIT_EXT | VK_DEBUG_UTILS_MESSAGE_TYPE_VALIDATION_BIT_EXT |
                      VK_DEBUG_UTILS_MESSAGE_TYPE_PERFORMANCE_BIT_EXT;
    dbg.pfnUserCallback = debugCallback;
    dbg.pUserData = this;

    VkValidationFeatureEnableEXT enables[] = {VK_VALIDATION_FEATURE_ENABLE_SYNCHRONIZATION_VALIDATION_EXT};
    VkValidationFeaturesEXT features{VK_STRUCTURE_TYPE_VALIDATION_FEATURES_EXT};
    features.enabledValidationFeatureCount = 1;
    features.pEnabledValidationFeatures = enables;

    VkInstanceCreateInfo ci{VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO};
    ci.pApplicationInfo = &app;
    ci.enabledExtensionCount = static_cast<uint32_t>(exts.size());
    ci.ppEnabledExtensionNames = exts.data();
    ci.enabledLayerCount = static_cast<uint32_t>(layers.size());
    ci.ppEnabledLayerNames = layers.data();
    const void* chain = nullptr;
    if (debugUtils) {
        dbg.pNext = chain;
        chain = &dbg;
    }
    if (syncValidation) {
        features.pNext = chain;
        chain = &features;
    }
    ci.pNext = chain;
    VK_CHECK(vkCreateInstance(&ci, nullptr, &instance_));
    info_.syncValidation = syncValidation;

    if (debugUtils) {
        dbg.pNext = nullptr;
        auto create = reinterpret_cast<PFN_vkCreateDebugUtilsMessengerEXT>(
            vkGetInstanceProcAddr(instance_, "vkCreateDebugUtilsMessengerEXT"));
        if (create) VK_CHECK(create(instance_, &dbg, nullptr, &messenger_));
    }
}

void VulkanDevice::pickDevice(int requested) {
    uint32_t n = 0;
    VK_CHECK(vkEnumeratePhysicalDevices(instance_, &n, nullptr));
    if (n == 0) throw Error("no Vulkan device found");
    std::vector<VkPhysicalDevice> devs(n);
    VK_CHECK(vkEnumeratePhysicalDevices(instance_, &n, devs.data()));

    auto suitable = [&](VkPhysicalDevice pd, std::string& why) {
        VkPhysicalDeviceProperties p{};
        vkGetPhysicalDeviceProperties(pd, &p);
        if (p.apiVersion < VK_API_VERSION_1_2) { why = "supports only Vulkan " + formatApiVersion(p.apiVersion); return false; }
        if (!opt_.offscreen && !deviceHasSwapchain(pd)) { why = "has no VK_KHR_swapchain"; return false; }
        if (findQueueFamily(pd, surface_) < 0) { why = "has no graphics+compute queue that can present"; return false; }
        return true;
    };

    std::string why;
    int index = -1;
    if (requested >= 0) {
        if (static_cast<uint32_t>(requested) >= n)
            throw Error("--gpu " + std::to_string(requested) + " is out of range (" + std::to_string(n) + " devices)");
        if (!suitable(devs[requested], why))
            throw Error("GPU " + std::to_string(requested) + " is not usable: it " + why);
        index = requested;
    } else {
        int bestRank = 99;
        for (uint32_t i = 0; i < n; ++i) {
            if (!suitable(devs[i], why)) continue;
            VkPhysicalDeviceProperties p{};
            vkGetPhysicalDeviceProperties(devs[i], &p);
            int r = typeRank(p.deviceType);
            if (r < bestRank) { index = static_cast<int>(i); bestRank = r; }
        }
        if (index < 0) throw Error("no Vulkan 1.2 device with swapchain and presentation support found");
    }
    physical_ = devs[index];
    queueFamily_ = static_cast<uint32_t>(findQueueFamily(physical_, surface_));
    vkGetPhysicalDeviceProperties(physical_, &props_);
    info_.gpuIndex = index;
    info_.gpuName = props_.deviceName;
    info_.gpuType = deviceTypeName(props_.deviceType);
    info_.vendorId = props_.vendorID;
    info_.deviceId = props_.deviceID;
    info_.driverVersionRaw = props_.driverVersion;
    info_.driverVersion = formatDriverVersion(props_.vendorID, props_.driverVersion);
    info_.apiVersion = formatApiVersion(props_.apiVersion);
    queryDriver(physical_, info_.driverName, info_.driverInfo);
}

void VulkanDevice::createDevice() {
    float prio = 1.0f;
    VkDeviceQueueCreateInfo qci{VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO};
    qci.queueFamilyIndex = queueFamily_;
    qci.queueCount = 1;
    qci.pQueuePriorities = &prio;
    std::vector<const char*> exts;
    if (!opt_.offscreen) exts.push_back(VK_KHR_SWAPCHAIN_EXTENSION_NAME);
    // Present wait (latency scene): optional, only when the driver has both extensions and features.
    VkPhysicalDevicePresentIdFeaturesKHR idFeat{VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_PRESENT_ID_FEATURES_KHR};
    VkPhysicalDevicePresentWaitFeaturesKHR waitFeat{VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_PRESENT_WAIT_FEATURES_KHR};
    if (!opt_.offscreen) {
        uint32_t n = 0;
        vkEnumerateDeviceExtensionProperties(physical_, nullptr, &n, nullptr);
        std::vector<VkExtensionProperties> avail(n);
        vkEnumerateDeviceExtensionProperties(physical_, nullptr, &n, avail.data());
        auto has = [&](const char* e) {
            return std::any_of(avail.begin(), avail.end(), [&](const VkExtensionProperties& p) { return std::strcmp(p.extensionName, e) == 0; });
        };
        if (has(VK_KHR_PRESENT_ID_EXTENSION_NAME) && has(VK_KHR_PRESENT_WAIT_EXTENSION_NAME)) {
            idFeat.pNext = &waitFeat;
            VkPhysicalDeviceFeatures2 f2{VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_FEATURES_2};
            f2.pNext = &idFeat;
            vkGetPhysicalDeviceFeatures2(physical_, &f2);
            if (idFeat.presentId && waitFeat.presentWait) {
                presentWait_ = true;
                exts.push_back(VK_KHR_PRESENT_ID_EXTENSION_NAME);
                exts.push_back(VK_KHR_PRESENT_WAIT_EXTENSION_NAME);
            }
        }
    }
    idFeat.pNext = &waitFeat;
    waitFeat.pNext = nullptr;
    VkPhysicalDeviceFeatures features{};
    VkDeviceCreateInfo ci{VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO};
    ci.pNext = presentWait_ ? &idFeat : nullptr;
    ci.queueCreateInfoCount = 1;
    ci.pQueueCreateInfos = &qci;
    ci.enabledExtensionCount = static_cast<uint32_t>(exts.size());
    ci.ppEnabledExtensionNames = exts.data();
    ci.pEnabledFeatures = &features;
    VK_CHECK(vkCreateDevice(physical_, &ci, nullptr, &device_));
    if (presentWait_) {
        waitForPresent_ = reinterpret_cast<PFN_vkWaitForPresentKHR>(vkGetDeviceProcAddr(device_, "vkWaitForPresentKHR"));
        presentWait_ = waitForPresent_ != nullptr;
    }
    vkGetDeviceQueue(device_, queueFamily_, 0, &queue_);

    uint32_t nq = 0;
    vkGetPhysicalDeviceQueueFamilyProperties(physical_, &nq, nullptr);
    std::vector<VkQueueFamilyProperties> fams(nq);
    vkGetPhysicalDeviceQueueFamilyProperties(physical_, &nq, fams.data());
    uint32_t bits = fams[queueFamily_].timestampValidBits;
    info_.timestamps = bits > 0 && props_.limits.timestampPeriod > 0.0f;
    if (info_.timestamps) {
        timestampMask_ = bits >= 64 ? ~0ull : ((1ull << bits) - 1);
        timestampPeriodNs_ = props_.limits.timestampPeriod;
        VkQueryPoolCreateInfo qp{VK_STRUCTURE_TYPE_QUERY_POOL_CREATE_INFO};
        qp.queryType = VK_QUERY_TYPE_TIMESTAMP;
        qp.queryCount = 2 * kFramesInFlight;
        VK_CHECK(vkCreateQueryPool(device_, &qp, nullptr, &queryPool_));
    }

    const VkFormat depthCandidates[] = {VK_FORMAT_D32_SFLOAT, VK_FORMAT_X8_D24_UNORM_PACK32,
                                        VK_FORMAT_D24_UNORM_S8_UINT, VK_FORMAT_D16_UNORM};
    for (VkFormat f : depthCandidates) {
        VkFormatProperties fp{};
        vkGetPhysicalDeviceFormatProperties(physical_, f, &fp);
        if (fp.optimalTilingFeatures & VK_FORMAT_FEATURE_DEPTH_STENCIL_ATTACHMENT_BIT) {
            depthFormat_ = f;
            break;
        }
    }
    if (depthFormat_ == VK_FORMAT_UNDEFINED) throw Error("no supported depth format");
    VkSampleCountFlags both = props_.limits.framebufferColorSampleCounts & props_.limits.framebufferDepthSampleCounts;
    info_.msaaSamples = (both & VK_SAMPLE_COUNT_4_BIT) ? 4 : 1;

    VkCommandPoolCreateInfo pci{VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO};
    pci.flags = VK_COMMAND_POOL_CREATE_RESET_COMMAND_BUFFER_BIT;
    pci.queueFamilyIndex = queueFamily_;
    VK_CHECK(vkCreateCommandPool(device_, &pci, nullptr, &cmdPool_));
    VkCommandBufferAllocateInfo ai{VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO};
    ai.commandPool = cmdPool_;
    ai.level = VK_COMMAND_BUFFER_LEVEL_PRIMARY;
    ai.commandBufferCount = kFramesInFlight;
    VK_CHECK(vkAllocateCommandBuffers(device_, &ai, cmd_));
    for (uint32_t i = 0; i < kFramesInFlight; ++i) {
        VkFenceCreateInfo fci{VK_STRUCTURE_TYPE_FENCE_CREATE_INFO};
        fci.flags = VK_FENCE_CREATE_SIGNALED_BIT;
        VK_CHECK(vkCreateFence(device_, &fci, nullptr, &fences_[i]));
        VkSemaphoreCreateInfo sci{VK_STRUCTURE_TYPE_SEMAPHORE_CREATE_INFO};
        VK_CHECK(vkCreateSemaphore(device_, &sci, nullptr, &imageAvailable_[i]));
    }

    VkSamplerCreateInfo sc{VK_STRUCTURE_TYPE_SAMPLER_CREATE_INFO};
    sc.magFilter = VK_FILTER_LINEAR;
    sc.minFilter = VK_FILTER_LINEAR;
    sc.mipmapMode = VK_SAMPLER_MIPMAP_MODE_NEAREST;
    sc.addressModeU = sc.addressModeV = sc.addressModeW = VK_SAMPLER_ADDRESS_MODE_CLAMP_TO_EDGE;
    sc.maxLod = 0.0f;
    VK_CHECK(vkCreateSampler(device_, &sc, nullptr, &sampler_));
}

bool VulkanDevice::surfaceHasArea() {
    if (opt_.offscreen) return true;
    VkSurfaceCapabilitiesKHR caps{};
    VK_CHECK(vkGetPhysicalDeviceSurfaceCapabilitiesKHR(physical_, surface_, &caps));
    if (caps.currentExtent.width == 0xFFFFFFFFu) {
        int w = 0, h = 0;
        SDL_Vulkan_GetDrawableSize(window_, &w, &h);
        return w > 0 && h > 0;
    }
    return caps.currentExtent.width > 0 && caps.currentExtent.height > 0;
}

void VulkanDevice::destroySwapchain(bool keepHandle) {
    for (VkImageView v : swapViews_) vkDestroyImageView(device_, v, nullptr);
    swapViews_.clear();
    for (VkSemaphore s : renderDone_) vkDestroySemaphore(device_, s, nullptr);
    renderDone_.clear();
    if (!keepHandle && swapchain_) {
        vkDestroySwapchainKHR(device_, swapchain_, nullptr);
        swapchain_ = VK_NULL_HANDLE;
    }
}

void VulkanDevice::createOffscreenTarget() {
    swapFormat_ = VK_FORMAT_R8G8B8A8_SRGB;
    extent_ = {opt_.width, opt_.height};
    offscreen_ = createImage(swapFormat_, extent_.width, extent_.height,
                             VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT | VK_IMAGE_USAGE_TRANSFER_SRC_BIT, 1,
                             VK_IMAGE_ASPECT_COLOR_BIT);
    canCapture_ = true;
    manualGamma_ = false;
    info_.presentMode = "offscreen";
    info_.swapFormat = formatName(swapFormat_);
}

void VulkanDevice::createSwapchain() {
    VkSurfaceCapabilitiesKHR caps{};
    VK_CHECK(vkGetPhysicalDeviceSurfaceCapabilitiesKHR(physical_, surface_, &caps));

    uint32_t nf = 0;
    VK_CHECK(vkGetPhysicalDeviceSurfaceFormatsKHR(physical_, surface_, &nf, nullptr));
    std::vector<VkSurfaceFormatKHR> formats(nf);
    VK_CHECK(vkGetPhysicalDeviceSurfaceFormatsKHR(physical_, surface_, &nf, formats.data()));
    if (formats.empty()) throw Error("surface reports no formats");
    VkSurfaceFormatKHR chosen = formats[0];
    bool found = false;
    for (VkFormat want : {VK_FORMAT_B8G8R8A8_SRGB, VK_FORMAT_R8G8B8A8_SRGB, VK_FORMAT_B8G8R8A8_UNORM, VK_FORMAT_R8G8B8A8_UNORM}) {
        for (auto& f : formats) {
            if (f.format == want && f.colorSpace == VK_COLOR_SPACE_SRGB_NONLINEAR_KHR) {
                chosen = f;
                found = true;
                break;
            }
        }
        if (found) break;
    }
    swapFormat_ = chosen.format;
    manualGamma_ = !(swapFormat_ == VK_FORMAT_B8G8R8A8_SRGB || swapFormat_ == VK_FORMAT_R8G8B8A8_SRGB);

    uint32_t nm = 0;
    VK_CHECK(vkGetPhysicalDeviceSurfacePresentModesKHR(physical_, surface_, &nm, nullptr));
    std::vector<VkPresentModeKHR> modes(nm);
    VK_CHECK(vkGetPhysicalDeviceSurfacePresentModesKHR(physical_, surface_, &nm, modes.data()));
    std::vector<VkPresentModeKHR> order;
    if (opt_.present == Present::Immediate) order = {VK_PRESENT_MODE_IMMEDIATE_KHR, VK_PRESENT_MODE_MAILBOX_KHR};
    if (opt_.present == Present::Mailbox) order = {VK_PRESENT_MODE_MAILBOX_KHR, VK_PRESENT_MODE_IMMEDIATE_KHR};
    order.push_back(VK_PRESENT_MODE_FIFO_KHR);
    presentMode_ = VK_PRESENT_MODE_FIFO_KHR;
    for (VkPresentModeKHR m : order) {
        if (std::find(modes.begin(), modes.end(), m) != modes.end()) {
            presentMode_ = m;
            break;
        }
    }

    VkExtent2D extent = caps.currentExtent;
    if (extent.width == 0xFFFFFFFFu) {
        int w = 0, h = 0;
        SDL_Vulkan_GetDrawableSize(window_, &w, &h);
        extent.width = std::clamp(static_cast<uint32_t>(w), caps.minImageExtent.width, caps.maxImageExtent.width);
        extent.height = std::clamp(static_cast<uint32_t>(h), caps.minImageExtent.height, caps.maxImageExtent.height);
    }
    extent_ = extent;

    uint32_t count = std::max(3u, caps.minImageCount + 1);  // triple buffering keeps immediate/mailbox from stalling
    if (caps.maxImageCount > 0 && count > caps.maxImageCount) count = caps.maxImageCount;

    VkCompositeAlphaFlagBitsKHR alpha = VK_COMPOSITE_ALPHA_OPAQUE_BIT_KHR;
    if (!(caps.supportedCompositeAlpha & alpha)) {
        for (VkCompositeAlphaFlagBitsKHR a : {VK_COMPOSITE_ALPHA_INHERIT_BIT_KHR, VK_COMPOSITE_ALPHA_PRE_MULTIPLIED_BIT_KHR,
                                              VK_COMPOSITE_ALPHA_POST_MULTIPLIED_BIT_KHR}) {
            if (caps.supportedCompositeAlpha & a) { alpha = a; break; }
        }
    }

    VkSwapchainKHR old = swapchain_;
    VkSwapchainCreateInfoKHR ci{VK_STRUCTURE_TYPE_SWAPCHAIN_CREATE_INFO_KHR};
    ci.surface = surface_;
    ci.minImageCount = count;
    ci.imageFormat = chosen.format;
    ci.imageColorSpace = chosen.colorSpace;
    ci.imageExtent = extent;
    ci.imageArrayLayers = 1;
    ci.imageUsage = VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT;
    canCapture_ = (caps.supportedUsageFlags & VK_IMAGE_USAGE_TRANSFER_SRC_BIT) &&
                  (swapFormat_ == VK_FORMAT_B8G8R8A8_SRGB || swapFormat_ == VK_FORMAT_B8G8R8A8_UNORM ||
                   swapFormat_ == VK_FORMAT_R8G8B8A8_SRGB || swapFormat_ == VK_FORMAT_R8G8B8A8_UNORM);
    if (canCapture_) ci.imageUsage |= VK_IMAGE_USAGE_TRANSFER_SRC_BIT;
    ci.imageSharingMode = VK_SHARING_MODE_EXCLUSIVE;
    ci.preTransform = caps.currentTransform;
    ci.compositeAlpha = alpha;
    ci.presentMode = presentMode_;
    ci.clipped = VK_TRUE;
    ci.oldSwapchain = old;
    VK_CHECK(vkCreateSwapchainKHR(device_, &ci, nullptr, &swapchain_));
    for (VkImageView v : swapViews_) vkDestroyImageView(device_, v, nullptr);
    swapViews_.clear();
    for (VkSemaphore s : renderDone_) vkDestroySemaphore(device_, s, nullptr);
    renderDone_.clear();
    if (old) vkDestroySwapchainKHR(device_, old, nullptr);

    uint32_t n = 0;
    VK_CHECK(vkGetSwapchainImagesKHR(device_, swapchain_, &n, nullptr));
    swapImages_.resize(n);
    VK_CHECK(vkGetSwapchainImagesKHR(device_, swapchain_, &n, swapImages_.data()));
    for (VkImage img : swapImages_) {
        VkImageViewCreateInfo vci{VK_STRUCTURE_TYPE_IMAGE_VIEW_CREATE_INFO};
        vci.image = img;
        vci.viewType = VK_IMAGE_VIEW_TYPE_2D;
        vci.format = swapFormat_;
        vci.subresourceRange = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1};
        VkImageView view = VK_NULL_HANDLE;
        VK_CHECK(vkCreateImageView(device_, &vci, nullptr, &view));
        swapViews_.push_back(view);
        VkSemaphoreCreateInfo sci{VK_STRUCTURE_TYPE_SEMAPHORE_CREATE_INFO};
        VkSemaphore s = VK_NULL_HANDLE;
        VK_CHECK(vkCreateSemaphore(device_, &sci, nullptr, &s));
        renderDone_.push_back(s);
    }
    info_.presentMode = presentModeName(presentMode_);
    info_.swapFormat = formatName(swapFormat_);
}

void VulkanDevice::resize() {
    waitIdle();
    framebuffers_.clear();  // the swapchain views change
    clearCaches();
    if (!opt_.offscreen) createSwapchain();
}

VkImageView VulkanDevice::backbufferView() const {
    return opt_.offscreen ? offscreen_.view : swapViews_[imageIndex_];
}

VkImage VulkanDevice::backbufferImage() const {
    return opt_.offscreen ? offscreen_.image : swapImages_[imageIndex_];
}

// ---------------------------------------------------------------------------------------------
// Frames

void VulkanDevice::collectTimestamps(uint32_t slot) {
    if (!slotPending_[slot]) return;
    slotPending_[slot] = false;
    uint64_t data[4] = {};
    VkResult r = vkGetQueryPoolResults(device_, queryPool_, slot * 2, 2, sizeof data, data, 2 * sizeof(uint64_t),
                                       VK_QUERY_RESULT_64_BIT | VK_QUERY_RESULT_WITH_AVAILABILITY_BIT);
    if (r == VK_NOT_READY) return;
    VK_CHECK(r);
    if (!slotMeasured_[slot] || !data[1] || !data[3]) return;
    uint64_t ticks = ((data[2] & timestampMask_) - (data[0] & timestampMask_)) & timestampMask_;
    gpuTimesMs_.push_back(static_cast<double>(ticks) * timestampPeriodNs_ / 1.0e6);
}

bool VulkanDevice::beginFrame(bool measured) {
    frameStart_ = std::chrono::steady_clock::now();
    slot_ = static_cast<uint32_t>(frameCounter_ % kFramesInFlight);
    VkResult r = vkWaitForFences(device_, 1, &fences_[slot_], VK_TRUE, kTimeoutNs);
    if (r == VK_TIMEOUT) fail("vkWaitForFences (GPU did not finish a frame within 10 s)", r, __FILE__, __LINE__);
    VK_CHECK(r);
    collectTimestamps(slot_);

    recreateAfterFrame_ = false;
    if (!opt_.offscreen) {
        r = vkAcquireNextImageKHR(device_, swapchain_, kTimeoutNs, imageAvailable_[slot_], VK_NULL_HANDLE, &imageIndex_);
        if (r == VK_ERROR_OUT_OF_DATE_KHR) return false;
        if (r != VK_SUCCESS && r != VK_SUBOPTIMAL_KHR) fail("vkAcquireNextImageKHR", r, __FILE__, __LINE__);
        recreateAfterFrame_ = (r == VK_SUBOPTIMAL_KHR);
    }

    VK_CHECK(vkResetFences(device_, 1, &fences_[slot_]));
    VkCommandBuffer cmd = cmd_[slot_];
    VK_CHECK(vkResetCommandBuffer(cmd, 0));
    VkCommandBufferBeginInfo bi{VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO};
    bi.flags = VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT;
    VK_CHECK(vkBeginCommandBuffer(cmd, &bi));
    if (info_.timestamps) {
        vkCmdResetQueryPool(cmd, queryPool_, slot_ * 2, 2);
        vkCmdWriteTimestamp(cmd, VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT, queryPool_, slot_ * 2);
    }
    slotMeasured_[slot_] = measured;
    bound_ = nullptr;
    return true;
}

bool VulkanDevice::endFrame(const std::string* capturePath) {
    VkCommandBuffer cmd = cmd_[slot_];
    const bool capture = capturePath && canCapture_;
    VkBuffer capBuf = VK_NULL_HANDLE;
    VkDeviceMemory capMem = VK_NULL_HANDLE;
    void* capMapped = nullptr;
    if (capture) {
        allocBuffer(VkDeviceSize(extent_.width) * extent_.height * 4, VK_BUFFER_USAGE_TRANSFER_DST_BIT, true, capBuf,
                    capMem, &capMapped);
        const VkImageLayout steady = opt_.offscreen ? VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL : VK_IMAGE_LAYOUT_PRESENT_SRC_KHR;
        VkImageMemoryBarrier b{VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER};
        b.srcAccessMask = VK_ACCESS_COLOR_ATTACHMENT_WRITE_BIT;
        b.dstAccessMask = VK_ACCESS_TRANSFER_READ_BIT;
        b.oldLayout = steady;
        b.newLayout = VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL;
        b.srcQueueFamilyIndex = b.dstQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED;
        b.image = backbufferImage();
        b.subresourceRange = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1};
        vkCmdPipelineBarrier(cmd, VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT | VK_PIPELINE_STAGE_FRAGMENT_SHADER_BIT,
                             VK_PIPELINE_STAGE_TRANSFER_BIT, 0, 0, nullptr, 0, nullptr, 1, &b);
        VkBufferImageCopy region{};
        region.imageSubresource = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 0, 1};
        region.imageExtent = {extent_.width, extent_.height, 1};
        vkCmdCopyImageToBuffer(cmd, b.image, VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL, capBuf, 1, &region);
        b.srcAccessMask = VK_ACCESS_TRANSFER_READ_BIT;
        b.dstAccessMask = 0;
        b.oldLayout = VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL;
        b.newLayout = steady;
        vkCmdPipelineBarrier(cmd, VK_PIPELINE_STAGE_TRANSFER_BIT, VK_PIPELINE_STAGE_BOTTOM_OF_PIPE_BIT, 0, 0, nullptr, 0,
                             nullptr, 1, &b);
        VkBufferMemoryBarrier hb{VK_STRUCTURE_TYPE_BUFFER_MEMORY_BARRIER};
        hb.srcAccessMask = VK_ACCESS_TRANSFER_WRITE_BIT;
        hb.dstAccessMask = VK_ACCESS_HOST_READ_BIT;
        hb.srcQueueFamilyIndex = hb.dstQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED;
        hb.buffer = capBuf;
        hb.size = VK_WHOLE_SIZE;
        vkCmdPipelineBarrier(cmd, VK_PIPELINE_STAGE_TRANSFER_BIT, VK_PIPELINE_STAGE_HOST_BIT, 0, 0, nullptr, 1, &hb, 0,
                             nullptr);
    }
    if (info_.timestamps) vkCmdWriteTimestamp(cmd, VK_PIPELINE_STAGE_BOTTOM_OF_PIPE_BIT, queryPool_, slot_ * 2 + 1);
    VK_CHECK(vkEndCommandBuffer(cmd));

    VkPipelineStageFlags waitStage = VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT;
    VkSubmitInfo si{VK_STRUCTURE_TYPE_SUBMIT_INFO};
    if (!opt_.offscreen) {
        si.waitSemaphoreCount = 1;
        si.pWaitSemaphores = &imageAvailable_[slot_];
        si.pWaitDstStageMask = &waitStage;
        si.signalSemaphoreCount = 1;
        si.pSignalSemaphores = &renderDone_[imageIndex_];
    }
    si.commandBufferCount = 1;
    si.pCommandBuffers = &cmd;
    VK_CHECK(vkQueueSubmit(queue_, 1, &si, fences_[slot_]));
    slotPending_[slot_] = info_.timestamps;
    lastSlot_ = slot_;
    ++frameCounter_;

    VkResult r = VK_SUCCESS;
    if (!opt_.offscreen) {
        VkPresentInfoKHR pi{VK_STRUCTURE_TYPE_PRESENT_INFO_KHR};
        pi.waitSemaphoreCount = 1;
        pi.pWaitSemaphores = &renderDone_[imageIndex_];
        pi.swapchainCount = 1;
        pi.pSwapchains = &swapchain_;
        pi.pImageIndices = &imageIndex_;
        VkPresentIdKHR pid{VK_STRUCTURE_TYPE_PRESENT_ID_KHR};
        const uint64_t id = ++presentId_;
        if (presentWait_) {
            pid.swapchainCount = 1;
            pid.pPresentIds = &id;
            pi.pNext = &pid;
        }
        r = vkQueuePresentKHR(queue_, &pi);
    }
    if (capture) {
        VK_CHECK(vkWaitForFences(device_, 1, &fences_[slot_], VK_TRUE, kTimeoutNs));
        const bool bgra = swapFormat_ == VK_FORMAT_B8G8R8A8_SRGB || swapFormat_ == VK_FORMAT_B8G8R8A8_UNORM;
        if (!writeBmp(*capturePath, static_cast<const uint8_t*>(capMapped), extent_.width, extent_.height,
                      size_t(extent_.width) * 4, bgra))
            std::fprintf(stderr, "warning: could not write %s\n", capturePath->c_str());
        vkDestroyBuffer(device_, capBuf, nullptr);
        vkFreeMemory(device_, capMem, nullptr);
    }
    if (r == VK_ERROR_OUT_OF_DATE_KHR || r == VK_SUBOPTIMAL_KHR) return true;
    if (r != VK_SUCCESS) fail("vkQueuePresentKHR", r, __FILE__, __LINE__);
    return recreateAfterFrame_;
}

double VulkanDevice::waitPresented(std::string& source) {
    using clock = std::chrono::steady_clock;
    if (presentWait_) {
        VkResult r = waitForPresent_(device_, swapchain_, presentId_, 250ull * 1000 * 1000);
        if (r == VK_SUCCESS || r == VK_SUBOPTIMAL_KHR) {
            source = "display";
            return std::chrono::duration<double, std::milli>(clock::now() - frameStart_).count();
        }
    }
    VK_CHECK(vkWaitForFences(device_, 1, &fences_[lastSlot_], VK_TRUE, kTimeoutNs));
    source = "gpu";
    return std::chrono::duration<double, std::milli>(clock::now() - frameStart_).count();
}

void VulkanDevice::finishFrames() {
    waitIdle();
    for (uint32_t i = 0; i < kFramesInFlight; ++i) collectTimestamps(i);
}

// ---------------------------------------------------------------------------------------------
// Resources

uint32_t VulkanDevice::findMemoryType(uint32_t typeBits, VkMemoryPropertyFlags want) const {
    VkPhysicalDeviceMemoryProperties mp{};
    vkGetPhysicalDeviceMemoryProperties(physical_, &mp);
    for (uint32_t i = 0; i < mp.memoryTypeCount; ++i)
        if ((typeBits & (1u << i)) && (mp.memoryTypes[i].propertyFlags & want) == want) return i;
    // Device-local is a preference, not a requirement (some virtual GPUs expose only host memory).
    if (want & VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT) return findMemoryType(typeBits, want & ~VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT);
    throw Error("no suitable Vulkan memory type");
}

void VulkanDevice::allocBuffer(VkDeviceSize size, VkBufferUsageFlags usage, bool hostVisible, VkBuffer& buf,
                               VkDeviceMemory& mem, void** mapped) {
    VkBufferCreateInfo ci{VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO};
    ci.size = size;
    ci.usage = usage;
    ci.sharingMode = VK_SHARING_MODE_EXCLUSIVE;
    VK_CHECK(vkCreateBuffer(device_, &ci, nullptr, &buf));
    VkMemoryRequirements req{};
    vkGetBufferMemoryRequirements(device_, buf, &req);
    VkMemoryAllocateInfo ai{VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO};
    ai.allocationSize = req.size;
    ai.memoryTypeIndex = findMemoryType(req.memoryTypeBits, hostVisible
        ? (VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT | VK_MEMORY_PROPERTY_HOST_COHERENT_BIT)
        : VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT);
    VK_CHECK(vkAllocateMemory(device_, &ai, nullptr, &mem));
    VK_CHECK(vkBindBufferMemory(device_, buf, mem, 0));
    if (hostVisible && mapped) VK_CHECK(vkMapMemory(device_, mem, 0, VK_WHOLE_SIZE, 0, mapped));
}

void VulkanDevice::immediateSubmit(const std::function<void(VkCommandBuffer)>& fn) {
    VkCommandBufferAllocateInfo ai{VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO};
    ai.commandPool = cmdPool_;
    ai.level = VK_COMMAND_BUFFER_LEVEL_PRIMARY;
    ai.commandBufferCount = 1;
    VkCommandBuffer cmd = VK_NULL_HANDLE;
    VK_CHECK(vkAllocateCommandBuffers(device_, &ai, &cmd));
    VkCommandBufferBeginInfo bi{VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO};
    bi.flags = VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT;
    VK_CHECK(vkBeginCommandBuffer(cmd, &bi));
    fn(cmd);
    VK_CHECK(vkEndCommandBuffer(cmd));
    VkFenceCreateInfo fci{VK_STRUCTURE_TYPE_FENCE_CREATE_INFO};
    VkFence fence = VK_NULL_HANDLE;
    VK_CHECK(vkCreateFence(device_, &fci, nullptr, &fence));
    VkSubmitInfo si{VK_STRUCTURE_TYPE_SUBMIT_INFO};
    si.commandBufferCount = 1;
    si.pCommandBuffers = &cmd;
    VkResult r = vkQueueSubmit(queue_, 1, &si, fence);
    if (r == VK_SUCCESS) r = vkWaitForFences(device_, 1, &fence, VK_TRUE, kTimeoutNs);
    vkDestroyFence(device_, fence, nullptr);
    vkFreeCommandBuffers(device_, cmdPool_, 1, &cmd);
    if (r == VK_TIMEOUT) fail("vkWaitForFences (upload did not finish within 10 s)", r, __FILE__, __LINE__);
    VK_CHECK(r);
}

Buffer VulkanDevice::createBuffer(BufferKind kind, size_t size, const void* data) {
    BufferRec rec;
    rec.alive = true;
    rec.kind = kind;
    rec.size = size;
    if (kind == BufferKind::Uniform) {
        for (uint32_t s = 0; s < kFramesInFlight; ++s)
            allocBuffer(size, VK_BUFFER_USAGE_UNIFORM_BUFFER_BIT, true, rec.buffer[s], rec.memory[s], &rec.mapped[s]);
    } else {
        VkBufferUsageFlags usage = kind == BufferKind::Vertex ? VK_BUFFER_USAGE_VERTEX_BUFFER_BIT
                                 : kind == BufferKind::Index  ? VK_BUFFER_USAGE_INDEX_BUFFER_BIT
                                                              : VK_BUFFER_USAGE_STORAGE_BUFFER_BIT;
        allocBuffer(size, usage | VK_BUFFER_USAGE_TRANSFER_DST_BIT, false, rec.buffer[0], rec.memory[0], nullptr);
        if (data) {
            VkBuffer sb = VK_NULL_HANDLE;
            VkDeviceMemory sm = VK_NULL_HANDLE;
            void* mapped = nullptr;
            allocBuffer(size, VK_BUFFER_USAGE_TRANSFER_SRC_BIT, true, sb, sm, &mapped);
            std::memcpy(mapped, data, size);
            VkBuffer dst = rec.buffer[0];
            immediateSubmit([&](VkCommandBuffer cmd) {
                VkBufferCopy region{0, 0, size};
                vkCmdCopyBuffer(cmd, sb, dst, 1, &region);
            });
            vkDestroyBuffer(device_, sb, nullptr);
            vkFreeMemory(device_, sm, nullptr);
        }
    }
    buffers_.push_back(rec);
    return Buffer{{static_cast<uint32_t>(buffers_.size())}};
}

void VulkanDevice::updateBuffer(Buffer b, const void* data, size_t size) {
    BufferRec& r = buffers_.at(b.id - 1);
    std::memcpy(r.mapped[slot_], data, std::min<size_t>(size, r.size));
}

Image VulkanDevice::createImage(VkFormat format, uint32_t w, uint32_t h, VkImageUsageFlags usage, uint32_t samples,
                                VkImageAspectFlags aspect) {
    Image img;
    VkImageCreateInfo ci{VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO};
    ci.imageType = VK_IMAGE_TYPE_2D;
    ci.format = format;
    ci.extent = {w, h, 1};
    ci.mipLevels = 1;
    ci.arrayLayers = 1;
    ci.samples = static_cast<VkSampleCountFlagBits>(samples);
    ci.tiling = VK_IMAGE_TILING_OPTIMAL;
    ci.usage = usage;
    ci.sharingMode = VK_SHARING_MODE_EXCLUSIVE;
    ci.initialLayout = VK_IMAGE_LAYOUT_UNDEFINED;
    VK_CHECK(vkCreateImage(device_, &ci, nullptr, &img.image));
    VkMemoryRequirements req{};
    vkGetImageMemoryRequirements(device_, img.image, &req);
    VkMemoryAllocateInfo ai{VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO};
    ai.allocationSize = req.size;
    ai.memoryTypeIndex = findMemoryType(req.memoryTypeBits, VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT);
    VK_CHECK(vkAllocateMemory(device_, &ai, nullptr, &img.memory));
    VK_CHECK(vkBindImageMemory(device_, img.image, img.memory, 0));
    VkImageViewCreateInfo vci{VK_STRUCTURE_TYPE_IMAGE_VIEW_CREATE_INFO};
    vci.image = img.image;
    vci.viewType = VK_IMAGE_VIEW_TYPE_2D;
    vci.format = format;
    vci.subresourceRange = {aspect, 0, 1, 0, 1};
    VK_CHECK(vkCreateImageView(device_, &vci, nullptr, &img.view));
    return img;
}

void VulkanDevice::destroyImage(Image& img) {
    if (img.view) vkDestroyImageView(device_, img.view, nullptr);
    if (img.image) vkDestroyImage(device_, img.image, nullptr);
    if (img.memory) vkFreeMemory(device_, img.memory, nullptr);
    img = Image{};
}

Texture VulkanDevice::createTexture(Format f, uint32_t w, uint32_t h, uint32_t samples, bool renderTarget) {
    (void)renderTarget;
    TextureRec rec;
    rec.alive = true;
    rec.format = f == Format::RGBA8     ? VK_FORMAT_R8G8B8A8_UNORM
               : f == Format::RGBA16F   ? VK_FORMAT_R16G16B16A16_SFLOAT
               : f == Format::RGBA32F   ? VK_FORMAT_R32G32B32A32_SFLOAT
                                        : swapFormat_;
    rec.w = w;
    rec.h = h;
    rec.samples = samples;
    VkImageUsageFlags usage = VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT;
    usage |= samples > 1 ? VK_IMAGE_USAGE_TRANSIENT_ATTACHMENT_BIT : VK_IMAGE_USAGE_SAMPLED_BIT;
    rec.img = createImage(rec.format, w, h, usage, samples, VK_IMAGE_ASPECT_COLOR_BIT);
    textures_.push_back(rec);
    return Texture{{static_cast<uint32_t>(textures_.size())}};
}

Texture VulkanDevice::createDepth(uint32_t w, uint32_t h, uint32_t samples) {
    TextureRec rec;
    rec.alive = true;
    rec.depth = true;
    rec.format = depthFormat_;
    rec.w = w;
    rec.h = h;
    rec.samples = samples;
    rec.img = createImage(depthFormat_, w, h,
                          VK_IMAGE_USAGE_DEPTH_STENCIL_ATTACHMENT_BIT | VK_IMAGE_USAGE_TRANSIENT_ATTACHMENT_BIT, samples,
                          VK_IMAGE_ASPECT_DEPTH_BIT);
    textures_.push_back(rec);
    return Texture{{static_cast<uint32_t>(textures_.size())}};
}

void VulkanDevice::clearCaches() {
    for (auto& [k, fb] : framebuffers_) vkDestroyFramebuffer(device_, fb, nullptr);
    framebuffers_.clear();
    sets_.clear();
    for (VkDescriptorPool p : pools_) VK_CHECK(vkResetDescriptorPool(device_, p, 0));
    poolUsed_ = 0;
}

void VulkanDevice::destroy(Buffer b) {
    BufferRec& r = buffers_.at(b.id - 1);
    if (!r.alive) return;
    clearCaches();
    for (uint32_t s = 0; s < kFramesInFlight; ++s) {
        if (r.buffer[s]) vkDestroyBuffer(device_, r.buffer[s], nullptr);
        if (r.memory[s]) vkFreeMemory(device_, r.memory[s], nullptr);
    }
    r = BufferRec{};
}

void VulkanDevice::destroy(Texture t) {
    TextureRec& r = textures_.at(t.id - 1);
    if (!r.alive) return;
    clearCaches();
    destroyImage(r.img);
    r = TextureRec{};
}

void VulkanDevice::destroy(Pipeline p) {
    PipelineRec& r = pipelines_.at(p.id - 1);
    if (!r.alive) return;
    clearCaches();
    vkDestroyPipeline(device_, r.pipeline, nullptr);
    vkDestroyPipelineLayout(device_, r.layout, nullptr);
    vkDestroyDescriptorSetLayout(device_, r.setLayout, nullptr);
    r = PipelineRec{};
}

VkRenderPass VulkanDevice::renderPass(const PassKey& k) {
    auto it = passes_.find(k);
    if (it != passes_.end()) return it->second;

    const bool hasDepth = k.depth != VK_FORMAT_UNDEFINED;
    const VkImageLayout backbufferFinal =
        opt_.offscreen ? VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL : VK_IMAGE_LAYOUT_PRESENT_SRC_KHR;
    std::vector<VkAttachmentDescription> att;
    std::vector<VkAttachmentReference> colorRefs;
    for (uint32_t i = 0; i < k.colorCount; ++i) {
        VkAttachmentDescription c{};
        c.format = k.color;
        c.samples = static_cast<VkSampleCountFlagBits>(k.samples);
        c.loadOp = k.load == Load::Clear ? VK_ATTACHMENT_LOAD_OP_CLEAR
                 : k.load == Load::Load  ? VK_ATTACHMENT_LOAD_OP_LOAD
                                         : VK_ATTACHMENT_LOAD_OP_DONT_CARE;
        c.storeOp = k.resolve ? VK_ATTACHMENT_STORE_OP_DONT_CARE : VK_ATTACHMENT_STORE_OP_STORE;
        c.stencilLoadOp = VK_ATTACHMENT_LOAD_OP_DONT_CARE;
        c.stencilStoreOp = VK_ATTACHMENT_STORE_OP_DONT_CARE;
        if (k.resolve) {
            c.initialLayout = VK_IMAGE_LAYOUT_UNDEFINED;
            c.finalLayout = VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL;
        } else {
            c.initialLayout = k.load == Load::Load ? VK_IMAGE_LAYOUT_SHADER_READ_ONLY_OPTIMAL : VK_IMAGE_LAYOUT_UNDEFINED;
            c.finalLayout = k.toBackbuffer ? backbufferFinal : VK_IMAGE_LAYOUT_SHADER_READ_ONLY_OPTIMAL;
        }
        colorRefs.push_back({static_cast<uint32_t>(att.size()), VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL});
        att.push_back(c);
    }
    VkAttachmentReference depthRef{};
    if (hasDepth) {
        VkAttachmentDescription z{};
        z.format = k.depth;
        z.samples = static_cast<VkSampleCountFlagBits>(k.samples);
        z.loadOp = VK_ATTACHMENT_LOAD_OP_CLEAR;
        z.storeOp = VK_ATTACHMENT_STORE_OP_DONT_CARE;
        z.stencilLoadOp = VK_ATTACHMENT_LOAD_OP_DONT_CARE;
        z.stencilStoreOp = VK_ATTACHMENT_STORE_OP_DONT_CARE;
        z.initialLayout = VK_IMAGE_LAYOUT_UNDEFINED;
        z.finalLayout = VK_IMAGE_LAYOUT_DEPTH_STENCIL_ATTACHMENT_OPTIMAL;
        depthRef = {static_cast<uint32_t>(att.size()), VK_IMAGE_LAYOUT_DEPTH_STENCIL_ATTACHMENT_OPTIMAL};
        att.push_back(z);
    }
    VkAttachmentReference resolveRef{};
    if (k.resolve) {
        VkAttachmentDescription r{};
        r.format = k.color;
        r.samples = VK_SAMPLE_COUNT_1_BIT;
        r.loadOp = VK_ATTACHMENT_LOAD_OP_DONT_CARE;
        r.storeOp = VK_ATTACHMENT_STORE_OP_STORE;
        r.stencilLoadOp = VK_ATTACHMENT_LOAD_OP_DONT_CARE;
        r.stencilStoreOp = VK_ATTACHMENT_STORE_OP_DONT_CARE;
        r.initialLayout = VK_IMAGE_LAYOUT_UNDEFINED;
        r.finalLayout = backbufferFinal;
        resolveRef = {static_cast<uint32_t>(att.size()), VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL};
        att.push_back(r);
    }

    VkSubpassDescription sub{};
    sub.pipelineBindPoint = VK_PIPELINE_BIND_POINT_GRAPHICS;
    sub.colorAttachmentCount = k.colorCount;
    sub.pColorAttachments = colorRefs.data();
    sub.pResolveAttachments = k.resolve ? &resolveRef : nullptr;
    sub.pDepthStencilAttachment = hasDepth ? &depthRef : nullptr;

    // Incoming: earlier attachment writes (WAW), earlier sampling of the same image (WAR) and the
    // swapchain acquire wait (COLOR_ATTACHMENT_OUTPUT). Outgoing: results are sampled by later passes.
    VkSubpassDependency deps[2]{};
    deps[0].srcSubpass = VK_SUBPASS_EXTERNAL;
    deps[0].dstSubpass = 0;
    deps[0].srcStageMask = VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT | VK_PIPELINE_STAGE_FRAGMENT_SHADER_BIT |
                           VK_PIPELINE_STAGE_EARLY_FRAGMENT_TESTS_BIT | VK_PIPELINE_STAGE_LATE_FRAGMENT_TESTS_BIT;
    deps[0].srcAccessMask = VK_ACCESS_COLOR_ATTACHMENT_WRITE_BIT | VK_ACCESS_DEPTH_STENCIL_ATTACHMENT_WRITE_BIT;
    deps[0].dstStageMask = VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT | VK_PIPELINE_STAGE_EARLY_FRAGMENT_TESTS_BIT |
                           VK_PIPELINE_STAGE_LATE_FRAGMENT_TESTS_BIT;
    deps[0].dstAccessMask = VK_ACCESS_COLOR_ATTACHMENT_READ_BIT | VK_ACCESS_COLOR_ATTACHMENT_WRITE_BIT |
                            VK_ACCESS_DEPTH_STENCIL_ATTACHMENT_READ_BIT | VK_ACCESS_DEPTH_STENCIL_ATTACHMENT_WRITE_BIT;
    deps[1].srcSubpass = 0;
    deps[1].dstSubpass = VK_SUBPASS_EXTERNAL;
    deps[1].srcStageMask = VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT;
    deps[1].srcAccessMask = VK_ACCESS_COLOR_ATTACHMENT_WRITE_BIT;
    deps[1].dstStageMask = VK_PIPELINE_STAGE_FRAGMENT_SHADER_BIT;
    deps[1].dstAccessMask = VK_ACCESS_SHADER_READ_BIT;

    VkRenderPassCreateInfo ci{VK_STRUCTURE_TYPE_RENDER_PASS_CREATE_INFO};
    ci.attachmentCount = static_cast<uint32_t>(att.size());
    ci.pAttachments = att.data();
    ci.subpassCount = 1;
    ci.pSubpasses = &sub;
    ci.dependencyCount = 2;
    ci.pDependencies = deps;
    VkRenderPass pass = VK_NULL_HANDLE;
    VK_CHECK(vkCreateRenderPass(device_, &ci, nullptr, &pass));
    passes_[k] = pass;
    return pass;
}

VkFramebuffer VulkanDevice::framebuffer(VkRenderPass pass, const std::vector<VkImageView>& views, uint32_t w, uint32_t h) {
    std::vector<uint64_t> key{reinterpret_cast<uint64_t>(pass), w, h};
    for (VkImageView v : views) key.push_back(reinterpret_cast<uint64_t>(v));
    auto it = framebuffers_.find(key);
    if (it != framebuffers_.end()) return it->second;
    VkFramebufferCreateInfo ci{VK_STRUCTURE_TYPE_FRAMEBUFFER_CREATE_INFO};
    ci.renderPass = pass;
    ci.attachmentCount = static_cast<uint32_t>(views.size());
    ci.pAttachments = views.data();
    ci.width = w;
    ci.height = h;
    ci.layers = 1;
    VkFramebuffer fb = VK_NULL_HANDLE;
    VK_CHECK(vkCreateFramebuffer(device_, &ci, nullptr, &fb));
    framebuffers_[key] = fb;
    return fb;
}

VkShaderModule VulkanDevice::module(const char* name) {
    const ShaderSource& s = shader(name);
    VkShaderModuleCreateInfo ci{VK_STRUCTURE_TYPE_SHADER_MODULE_CREATE_INFO};
    ci.codeSize = s.spirvBytes;
    ci.pCode = s.spirv;
    VkShaderModule m = VK_NULL_HANDLE;
    VK_CHECK(vkCreateShaderModule(device_, &ci, nullptr, &m));
    return m;
}

static VkDescriptorType descriptorType(Res r) {
    switch (r) {
        case Res::Uniform: return VK_DESCRIPTOR_TYPE_UNIFORM_BUFFER;
        case Res::Texture: return VK_DESCRIPTOR_TYPE_COMBINED_IMAGE_SAMPLER;
        default: return VK_DESCRIPTOR_TYPE_STORAGE_BUFFER;
    }
}

static void makeLayouts(VkDevice dev, const Res* res, uint32_t constBytes, VkShaderStageFlags stages, PipelineRec& rec) {
    std::vector<VkDescriptorSetLayoutBinding> b;
    for (uint32_t i = 0; i < kMaxBindings; ++i) {
        rec.res[i] = res[i];
        if (res[i] != Res::None) b.push_back({i, descriptorType(res[i]), 1, stages, nullptr});
    }
    VkDescriptorSetLayoutCreateInfo lci{VK_STRUCTURE_TYPE_DESCRIPTOR_SET_LAYOUT_CREATE_INFO};
    lci.bindingCount = static_cast<uint32_t>(b.size());
    lci.pBindings = b.data();
    VK_CHECK(vkCreateDescriptorSetLayout(dev, &lci, nullptr, &rec.setLayout));
    VkPushConstantRange range{stages, 0, constBytes};
    VkPipelineLayoutCreateInfo ci{VK_STRUCTURE_TYPE_PIPELINE_LAYOUT_CREATE_INFO};
    ci.setLayoutCount = 1;
    ci.pSetLayouts = &rec.setLayout;
    ci.pushConstantRangeCount = constBytes ? 1 : 0;
    ci.pPushConstantRanges = constBytes ? &range : nullptr;
    VK_CHECK(vkCreatePipelineLayout(dev, &ci, nullptr, &rec.layout));
    rec.pushStages = stages;
    rec.constBytes = constBytes;
}

Pipeline VulkanDevice::createPipeline(const PipelineDesc& d) {
    PipelineRec rec;
    rec.alive = true;
    makeLayouts(device_, d.res, d.constBytes, VK_SHADER_STAGE_VERTEX_BIT | VK_SHADER_STAGE_FRAGMENT_BIT, rec);

    PassKey key{};
    key.color = d.backbuffer ? swapFormat_
              : d.color == Format::RGBA8   ? VK_FORMAT_R8G8B8A8_UNORM
              : d.color == Format::RGBA16F ? VK_FORMAT_R16G16B16A16_SFLOAT
              : d.color == Format::RGBA32F ? VK_FORMAT_R32G32B32A32_SFLOAT
                                           : swapFormat_;
    key.colorCount = d.colorCount;
    key.samples = d.samples;
    key.load = Load::Clear;
    key.toBackbuffer = d.backbuffer;
    key.depth = d.depth ? depthFormat_ : VK_FORMAT_UNDEFINED;
    key.resolve = d.backbuffer && d.samples > 1;
    VkRenderPass pass = renderPass(key);

    VkShaderModule vs = module(d.vs);
    VkShaderModule fs = module(d.fs);
    const uint32_t specData[2] = {manualGamma_ ? 1u : 0u, d.hdrOut ? 1u : 0u};
    const VkSpecializationMapEntry entries[2] = {{0, 0, 4}, {1, 4, 4}};
    VkSpecializationInfo spec{2, entries, sizeof specData, specData};
    VkPipelineShaderStageCreateInfo stages[2]{};
    stages[0] = {VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO, nullptr, 0, VK_SHADER_STAGE_VERTEX_BIT, vs, "main", &spec};
    stages[1] = {VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO, nullptr, 0, VK_SHADER_STAGE_FRAGMENT_BIT, fs, "main", &spec};

    std::vector<VkVertexInputBindingDescription> vb;
    for (uint32_t i = 0; i < d.vertexBindings.size(); ++i)
        vb.push_back({i, d.vertexBindings[i].stride,
                      d.vertexBindings[i].perInstance ? VK_VERTEX_INPUT_RATE_INSTANCE : VK_VERTEX_INPUT_RATE_VERTEX});
    std::vector<VkVertexInputAttributeDescription> va;
    for (const VertexAttr& a : d.attrs) {
        VkFormat f = a.components == 1 ? VK_FORMAT_R32_SFLOAT
                   : a.components == 2 ? VK_FORMAT_R32G32_SFLOAT
                   : a.components == 3 ? VK_FORMAT_R32G32B32_SFLOAT
                                       : VK_FORMAT_R32G32B32A32_SFLOAT;
        va.push_back({a.location, a.binding, f, a.offset});
    }
    VkPipelineVertexInputStateCreateInfo vi{VK_STRUCTURE_TYPE_PIPELINE_VERTEX_INPUT_STATE_CREATE_INFO};
    vi.vertexBindingDescriptionCount = static_cast<uint32_t>(vb.size());
    vi.pVertexBindingDescriptions = vb.data();
    vi.vertexAttributeDescriptionCount = static_cast<uint32_t>(va.size());
    vi.pVertexAttributeDescriptions = va.data();

    VkPipelineInputAssemblyStateCreateInfo ia{VK_STRUCTURE_TYPE_PIPELINE_INPUT_ASSEMBLY_STATE_CREATE_INFO};
    ia.topology = VK_PRIMITIVE_TOPOLOGY_TRIANGLE_LIST;
    VkPipelineViewportStateCreateInfo vp{VK_STRUCTURE_TYPE_PIPELINE_VIEWPORT_STATE_CREATE_INFO};
    vp.viewportCount = 1;
    vp.scissorCount = 1;
    VkPipelineRasterizationStateCreateInfo rs{VK_STRUCTURE_TYPE_PIPELINE_RASTERIZATION_STATE_CREATE_INFO};
    rs.polygonMode = VK_POLYGON_MODE_FILL;
    rs.cullMode = d.cullBack ? VK_CULL_MODE_BACK_BIT : VK_CULL_MODE_NONE;
    rs.frontFace = VK_FRONT_FACE_COUNTER_CLOCKWISE;
    rs.lineWidth = 1.0f;
    VkPipelineMultisampleStateCreateInfo ms{VK_STRUCTURE_TYPE_PIPELINE_MULTISAMPLE_STATE_CREATE_INFO};
    ms.rasterizationSamples = static_cast<VkSampleCountFlagBits>(d.samples);
    VkPipelineDepthStencilStateCreateInfo ds{VK_STRUCTURE_TYPE_PIPELINE_DEPTH_STENCIL_STATE_CREATE_INFO};
    ds.depthTestEnable = d.depthTest;
    ds.depthWriteEnable = d.depthWrite;
    ds.depthCompareOp = VK_COMPARE_OP_LESS_OR_EQUAL;
    VkPipelineColorBlendAttachmentState ba[2]{};
    for (auto& a : ba) {
        a.colorWriteMask = VK_COLOR_COMPONENT_R_BIT | VK_COLOR_COMPONENT_G_BIT | VK_COLOR_COMPONENT_B_BIT | VK_COLOR_COMPONENT_A_BIT;
        if (d.additive) {
            a.blendEnable = VK_TRUE;
            a.srcColorBlendFactor = a.dstColorBlendFactor = VK_BLEND_FACTOR_ONE;
            a.colorBlendOp = VK_BLEND_OP_ADD;
            a.srcAlphaBlendFactor = a.dstAlphaBlendFactor = VK_BLEND_FACTOR_ONE;
            a.alphaBlendOp = VK_BLEND_OP_ADD;
        }
    }
    VkPipelineColorBlendStateCreateInfo cb{VK_STRUCTURE_TYPE_PIPELINE_COLOR_BLEND_STATE_CREATE_INFO};
    cb.attachmentCount = d.colorCount;
    cb.pAttachments = ba;
    VkDynamicState dyn[] = {VK_DYNAMIC_STATE_VIEWPORT, VK_DYNAMIC_STATE_SCISSOR};
    VkPipelineDynamicStateCreateInfo dy{VK_STRUCTURE_TYPE_PIPELINE_DYNAMIC_STATE_CREATE_INFO};
    dy.dynamicStateCount = 2;
    dy.pDynamicStates = dyn;

    VkGraphicsPipelineCreateInfo ci{VK_STRUCTURE_TYPE_GRAPHICS_PIPELINE_CREATE_INFO};
    ci.stageCount = 2;
    ci.pStages = stages;
    ci.pVertexInputState = &vi;
    ci.pInputAssemblyState = &ia;
    ci.pViewportState = &vp;
    ci.pRasterizationState = &rs;
    ci.pMultisampleState = &ms;
    ci.pDepthStencilState = &ds;
    ci.pColorBlendState = &cb;
    ci.pDynamicState = &dy;
    ci.layout = rec.layout;
    ci.renderPass = pass;
    VkResult r = vkCreateGraphicsPipelines(device_, VK_NULL_HANDLE, 1, &ci, nullptr, &rec.pipeline);
    vkDestroyShaderModule(device_, vs, nullptr);
    vkDestroyShaderModule(device_, fs, nullptr);
    VK_CHECK(r);
    pipelines_.push_back(rec);
    return Pipeline{{static_cast<uint32_t>(pipelines_.size())}};
}

Pipeline VulkanDevice::createCompute(const ComputeDesc& d) {
    PipelineRec rec;
    rec.alive = true;
    rec.compute = true;
    makeLayouts(device_, d.res, d.constBytes, VK_SHADER_STAGE_COMPUTE_BIT, rec);
    VkShaderModule m = module(d.cs);
    VkComputePipelineCreateInfo ci{VK_STRUCTURE_TYPE_COMPUTE_PIPELINE_CREATE_INFO};
    ci.stage = {VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO, nullptr, 0, VK_SHADER_STAGE_COMPUTE_BIT, m, "main", nullptr};
    ci.layout = rec.layout;
    VkResult r = vkCreateComputePipelines(device_, VK_NULL_HANDLE, 1, &ci, nullptr, &rec.pipeline);
    vkDestroyShaderModule(device_, m, nullptr);
    VK_CHECK(r);
    pipelines_.push_back(rec);
    return Pipeline{{static_cast<uint32_t>(pipelines_.size())}};
}

// ---------------------------------------------------------------------------------------------
// Commands

void VulkanDevice::beginPass(const PassDesc& p) {
    VkCommandBuffer cmd = cmd_[slot_];
    PassKey key{};
    std::vector<VkImageView> views;
    std::vector<VkClearValue> clears;
    uint32_t w = extent_.width, h = extent_.height;
    VkClearValue cc{};
    std::memcpy(cc.color.float32, p.clear, sizeof p.clear);
    if (!p.color) {
        key.color = swapFormat_;
        key.samples = 1;
        key.toBackbuffer = true;
        views.push_back(backbufferView());
    } else {
        const TextureRec& t = textures_.at(p.color.id - 1);
        key.color = t.format;
        key.samples = t.samples;
        key.toBackbuffer = false;
        w = t.w;
        h = t.h;
        views.push_back(t.img.view);
    }
    key.colorCount = 1;
    clears.push_back(cc);
    if (p.color2) {
        key.colorCount = 2;
        views.push_back(textures_.at(p.color2.id - 1).img.view);
        clears.push_back(cc);
    }
    key.load = p.load;
    key.depth = VK_FORMAT_UNDEFINED;
    if (p.depth) {
        key.depth = depthFormat_;
        views.push_back(textures_.at(p.depth.id - 1).img.view);
        VkClearValue dc{};
        dc.depthStencil = {1.0f, 0};
        clears.push_back(dc);
    }
    if (p.resolveToBackbuffer) {
        key.resolve = true;
        key.toBackbuffer = true;
        views.push_back(backbufferView());
    }
    VkRenderPass pass = renderPass(key);
    VkFramebuffer fb = framebuffer(pass, views, w, h);
    VkRenderPassBeginInfo bi{VK_STRUCTURE_TYPE_RENDER_PASS_BEGIN_INFO};
    bi.renderPass = pass;
    bi.framebuffer = fb;
    bi.renderArea = {{0, 0}, {w, h}};
    const bool anyClear = p.load == Load::Clear || p.depth;
    bi.clearValueCount = anyClear ? static_cast<uint32_t>(clears.size()) : 0;
    bi.pClearValues = anyClear ? clears.data() : nullptr;
    vkCmdBeginRenderPass(cmd, &bi, VK_SUBPASS_CONTENTS_INLINE);
    VkViewport vp{0.0f, 0.0f, static_cast<float>(w), static_cast<float>(h), 0.0f, 1.0f};
    VkRect2D sc{{0, 0}, {w, h}};
    vkCmdSetViewport(cmd, 0, 1, &vp);
    vkCmdSetScissor(cmd, 0, 1, &sc);
    currentPass_ = pass;
}

void VulkanDevice::setPipeline(Pipeline p) {
    bound_ = &pipelines_.at(p.id - 1);
    vkCmdBindPipeline(cmd_[slot_], bound_->compute ? VK_PIPELINE_BIND_POINT_COMPUTE : VK_PIPELINE_BIND_POINT_GRAPHICS,
                      bound_->pipeline);
    std::memset(boundRes_, 0, sizeof boundRes_);
    bindingsDirty_ = true;
}

void VulkanDevice::setConstants(const void* data, uint32_t size) {
    vkCmdPushConstants(cmd_[slot_], bound_->layout, bound_->pushStages, 0, size, data);
}

void VulkanDevice::bindRes(uint32_t binding, uint32_t id) {
    boundRes_[binding] = id;
    bindingsDirty_ = true;
}

void VulkanDevice::flushBindings() {
    if (!bindingsDirty_) return;
    bindingsDirty_ = false;
    bool any = false;
    std::vector<uint64_t> key{reinterpret_cast<uint64_t>(bound_->setLayout)};
    for (uint32_t i = 0; i < kMaxBindings; ++i) {
        if (bound_->res[i] == Res::None) continue;
        any = true;
        uint64_t k = boundRes_[i];
        if (bound_->res[i] == Res::Uniform) k |= uint64_t(slot_) << 32;
        key.push_back(k);
    }
    if (!any) return;
    VkDescriptorSet set = VK_NULL_HANDLE;
    auto it = sets_.find(key);
    if (it != sets_.end()) {
        set = it->second;
    } else {
        constexpr uint32_t kSetsPerPool = 256;
        if (poolUsed_ >= kSetsPerPool * pools_.size()) {  // every pool is full: add one
            VkDescriptorPoolSize sizes[] = {{VK_DESCRIPTOR_TYPE_UNIFORM_BUFFER, kSetsPerPool * kMaxBindings},
                                            {VK_DESCRIPTOR_TYPE_COMBINED_IMAGE_SAMPLER, kSetsPerPool * kMaxBindings},
                                            {VK_DESCRIPTOR_TYPE_STORAGE_BUFFER, kSetsPerPool * kMaxBindings}};
            VkDescriptorPoolCreateInfo ci{VK_STRUCTURE_TYPE_DESCRIPTOR_POOL_CREATE_INFO};
            ci.maxSets = kSetsPerPool;
            ci.poolSizeCount = 3;
            ci.pPoolSizes = sizes;
            VkDescriptorPool pool = VK_NULL_HANDLE;
            VK_CHECK(vkCreateDescriptorPool(device_, &ci, nullptr, &pool));
            pools_.push_back(pool);
        }
        VkDescriptorSetAllocateInfo ai{VK_STRUCTURE_TYPE_DESCRIPTOR_SET_ALLOCATE_INFO};
        ai.descriptorPool = pools_[poolUsed_ / kSetsPerPool];
        ai.descriptorSetCount = 1;
        ai.pSetLayouts = &bound_->setLayout;
        VK_CHECK(vkAllocateDescriptorSets(device_, &ai, &set));
        ++poolUsed_;
        for (uint32_t i = 0; i < kMaxBindings; ++i) {
            if (bound_->res[i] == Res::None) continue;
            if (!boundRes_[i]) throw Error("binding " + std::to_string(i) + " is not set");
            VkWriteDescriptorSet w{VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET};
            w.dstSet = set;
            w.dstBinding = i;
            w.descriptorCount = 1;
            w.descriptorType = descriptorType(bound_->res[i]);
            VkDescriptorImageInfo ii{};
            VkDescriptorBufferInfo bi{};
            if (bound_->res[i] == Res::Texture) {
                ii = {sampler_, textures_.at(boundRes_[i] - 1).img.view, VK_IMAGE_LAYOUT_SHADER_READ_ONLY_OPTIMAL};
                w.pImageInfo = &ii;
            } else {
                const BufferRec& b = buffers_.at(boundRes_[i] - 1);
                bi = {b.buffer[b.kind == BufferKind::Uniform ? slot_ : 0], 0, VK_WHOLE_SIZE};
                w.pBufferInfo = &bi;
            }
            vkUpdateDescriptorSets(device_, 1, &w, 0, nullptr);
        }
        sets_[key] = set;
    }
    vkCmdBindDescriptorSets(cmd_[slot_], bound_->compute ? VK_PIPELINE_BIND_POINT_COMPUTE : VK_PIPELINE_BIND_POINT_GRAPHICS,
                            bound_->layout, 0, 1, &set, 0, nullptr);
}

void VulkanDevice::setVertexBuffer(uint32_t slot, Buffer b) {
    VkDeviceSize zero = 0;
    vkCmdBindVertexBuffers(cmd_[slot_], slot, 1, &buffers_.at(b.id - 1).buffer[0], &zero);
}

void VulkanDevice::setIndexBuffer(Buffer b) {
    vkCmdBindIndexBuffer(cmd_[slot_], buffers_.at(b.id - 1).buffer[0], 0, VK_INDEX_TYPE_UINT32);
}

void VulkanDevice::draw(uint32_t vertexCount, uint32_t instanceCount) {
    flushBindings();
    vkCmdDraw(cmd_[slot_], vertexCount, instanceCount, 0, 0);
}

void VulkanDevice::drawIndexed(uint32_t indexCount, uint32_t instanceCount, uint32_t firstInstance) {
    flushBindings();
    vkCmdDrawIndexed(cmd_[slot_], indexCount, instanceCount, 0, 0, firstInstance);
}

void VulkanDevice::dispatch(uint32_t groupsX) {
    flushBindings();
    vkCmdDispatch(cmd_[slot_], groupsX, 1, 1);
}

void VulkanDevice::storageToCompute() {
    // The previous frame's vertex reads (and simulation writes) finish before the next dispatch.
    VkMemoryBarrier b{VK_STRUCTURE_TYPE_MEMORY_BARRIER};
    b.srcAccessMask = VK_ACCESS_SHADER_WRITE_BIT;
    b.dstAccessMask = VK_ACCESS_SHADER_READ_BIT | VK_ACCESS_SHADER_WRITE_BIT;
    vkCmdPipelineBarrier(cmd_[slot_], VK_PIPELINE_STAGE_VERTEX_SHADER_BIT | VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT,
                         VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT, 0, 1, &b, 0, nullptr, 0, nullptr);
}

void VulkanDevice::storageToVertex() {
    VkMemoryBarrier b{VK_STRUCTURE_TYPE_MEMORY_BARRIER};
    b.srcAccessMask = VK_ACCESS_SHADER_WRITE_BIT;
    b.dstAccessMask = VK_ACCESS_SHADER_READ_BIT;
    vkCmdPipelineBarrier(cmd_[slot_], VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT, VK_PIPELINE_STAGE_VERTEX_SHADER_BIT, 0, 1, &b,
                         0, nullptr, 0, nullptr);
}

}  // namespace

std::unique_ptr<Device> createVulkan(SDL_Window* window, const Options& opt) {
    return std::make_unique<VulkanDevice>(window, opt);
}

int listVulkan() {
    VkApplicationInfo app{VK_STRUCTURE_TYPE_APPLICATION_INFO};
    app.pApplicationName = "qorral-bench";
    app.apiVersion = VK_API_VERSION_1_2;
    VkInstanceCreateInfo ci{VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO};
    ci.pApplicationInfo = &app;
    VkInstance inst = VK_NULL_HANDLE;
    VK_CHECK(vkCreateInstance(&ci, nullptr, &inst));
    uint32_t n = 0;
    VK_CHECK(vkEnumeratePhysicalDevices(inst, &n, nullptr));
    std::vector<VkPhysicalDevice> devs(n);
    VK_CHECK(vkEnumeratePhysicalDevices(inst, &n, devs.data()));
    std::printf("%u Vulkan device(s):\n", n);
    for (uint32_t i = 0; i < n; ++i) {
        VkPhysicalDeviceProperties p{};
        vkGetPhysicalDeviceProperties(devs[i], &p);
        std::string dn, di;
        queryDriver(devs[i], dn, di);
        bool ok = p.apiVersion >= VK_API_VERSION_1_2 && deviceHasSwapchain(devs[i]) && findQueueFamily(devs[i], VK_NULL_HANDLE) >= 0;
        std::printf("  [%u] %s (%s), Vulkan %s, driver %s %s %s%s\n", i, p.deviceName, deviceTypeName(p.deviceType),
                    formatApiVersion(p.apiVersion).c_str(), dn.c_str(), di.c_str(),
                    formatDriverVersion(p.vendorID, p.driverVersion).c_str(), ok ? "" : "  [unsupported: needs Vulkan 1.2 + swapchain]");
    }
    vkDestroyInstance(inst, nullptr);
    return 0;
}

}  // namespace gfx
