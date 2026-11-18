//! Qorral's Vulkan compositor.
//!
//! Guest scanouts are shown straight from the guest's GPU memory. QEMU exports each scanout
//! buffer's Vulkan allocation once (`qorral-display`'s `ScanoutBlob`), together with the image
//! the guest created on it; the compositor recreates that image on the same physical device,
//! imports the memory as its dedicated allocation and samples it. Nothing is copied and nothing
//! leaves VRAM. Recreating the image matters: the driver may compress image memory, and only an
//! identical image reads it back correctly (a plain buffer reads compressed bytes).
//!
//! When the image is unknown, the memory is imported as a buffer and copied VRAM to VRAM with a
//! transfer (which decompresses) before drawing: correct, but not zero copy.
//!
//! This version renders into an offscreen target (snapshots, tests, timing); the window
//! swapchain shares everything but the target.

use ash::vk;
use std::collections::HashMap;
use std::fmt;
use std::time::{Duration, Instant};

const FORMAT: vk::Format = vk::Format::B8G8R8A8_UNORM;
const VERT: &[u8] = include_bytes!("../shaders/fullscreen.vert.spv");
const FRAG_BUFFER: &[u8] = include_bytes!("../shaders/guest.frag.spv");
const FRAG_IMAGE: &[u8] = include_bytes!("../shaders/guest_image.frag.spv");
const MAX_GUEST_BUFFERS: u32 = 64;
const PUSH_SIZE: u32 = 32;

#[derive(Debug)]
pub enum Error {
    /// The Vulkan loader is missing.
    Load(String),
    Vk(&'static str, vk::Result),
    /// No Vulkan device with the guest's device UUID.
    NoDevice(String),
    Unsupported(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Load(e) => write!(f, "Vulkan is not available: {e}"),
            Error::Vk(call, r) => write!(f, "{call} failed: {r}"),
            Error::NoDevice(u) => write!(f, "no Vulkan device with UUID {u} (the GPU the guest renders on)"),
            Error::Unsupported(s) => f.write_str(s),
        }
    }
}

impl std::error::Error for Error {}

fn vkr(call: &'static str) -> impl FnOnce(vk::Result) -> Error {
    move |r| Error::Vk(call, r)
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Where a scanout's pixels are in its memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Layout {
    pub offset: u64,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
}

/// The guest's image on the memory, exactly as the host driver created it (Vulkan values).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageParams {
    pub flags: u32,
    pub image_type: u32,
    pub format: u32,
    pub width: u32,
    pub height: u32,
    pub depth: u32,
    pub mip_levels: u32,
    pub array_layers: u32,
    pub samples: u32,
    pub tiling: u32,
    pub usage: u32,
    pub sharing_mode: u32,
}

/// A native window to present into (Windows: `HWND` and `HINSTANCE`).
#[derive(Debug, Clone, Copy)]
pub struct NativeWindow {
    pub hwnd: isize,
    pub hinstance: isize,
}

/// How presented frames reach the screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresentMode {
    /// Lowest latency: no wait for the display's refresh (may tear).
    Immediate,
    /// No tearing, no queue: the newest frame replaces a waiting one.
    Mailbox,
    /// Waits for the display's refresh.
    Fifo,
}

/// What one [`Compositor::present`] took.
#[derive(Debug, Clone, Copy, Default)]
pub struct PresentTiming {
    /// Waiting for a swapchain image.
    pub acquire: Duration,
    /// Recording, submitting and the GPU finishing the frame.
    pub gpu: Duration,
    /// The present call itself.
    pub present: Duration,
}

/// The guest memory to import (from `ScanoutBlob`).
#[derive(Debug, Clone, Copy)]
pub struct GuestMemory {
    /// NT handle (`OPAQUE_WIN32`); consumed by [`Compositor::import`].
    pub handle: isize,
    pub allocation_size: u64,
    pub memory_type_index: u32,
    pub layout: Layout,
    pub image: Option<ImageParams>,
}

/// One imported guest buffer.
enum Guest {
    /// Zero copy: the guest's memory bound to an identical image, sampled in place.
    Image { image: vk::Image, view: vk::ImageView, memory: vk::DeviceMemory, set: vk::DescriptorSet, src: [f32; 4] },
    /// Fallback: the memory as a buffer, copied to `copy` with a transfer before drawing.
    Buffer {
        buffer: vk::Buffer,
        memory: vk::DeviceMemory,
        copy: vk::Buffer,
        copy_memory: vk::DeviceMemory,
        set: vk::DescriptorSet,
        layout: Layout,
    },
}

struct Pipeline {
    set_layout: vk::DescriptorSetLayout,
    layout: vk::PipelineLayout,
    pipeline: vk::Pipeline,
}

/// The device, pipelines and target. One per window (or per offscreen renderer).
pub struct Compositor {
    _entry: ash::Entry,
    instance: ash::Instance,
    device: ash::Device,
    physical: vk::PhysicalDevice,
    queue: vk::Queue,
    queue_family: u32,
    queue_families: Vec<u32>,
    memory_types: vk::PhysicalDeviceMemoryProperties,
    pub device_name: String,

    pool: vk::DescriptorPool,
    sampler: vk::Sampler,
    render_pass: vk::RenderPass,
    /// The same pass ending in PRESENT_SRC, for the window.
    present_pass: vk::RenderPass,
    image_pipe: Pipeline,
    buffer_pipe: Pipeline,
    commands: vk::CommandPool,
    cmd: vk::CommandBuffer,
    fence: vk::Fence,

    target: Target,
    window: Option<Window>,
    guests: HashMap<u32, Guest>,
}

/// A window target: surface, swapchain and its framebuffers.
struct Window {
    surface_fn: ash::khr::surface::Instance,
    swapchain_fn: ash::khr::swapchain::Device,
    surface: vk::SurfaceKHR,
    swapchain: vk::SwapchainKHR,
    mode: vk::PresentModeKHR,
    extent: vk::Extent2D,
    views: Vec<vk::ImageView>,
    framebuffers: Vec<vk::Framebuffer>,
    acquired: vk::Semaphore,
    rendered: vk::Semaphore,
}

/// The offscreen target and its readback buffer.
struct Target {
    width: u32,
    height: u32,
    image: vk::Image,
    memory: vk::DeviceMemory,
    view: vk::ImageView,
    framebuffer: vk::Framebuffer,
    readback: vk::Buffer,
    readback_memory: vk::DeviceMemory,
}

const NULL_PIPE: Pipeline = Pipeline {
    set_layout: vk::DescriptorSetLayout::null(),
    layout: vk::PipelineLayout::null(),
    pipeline: vk::Pipeline::null(),
};

impl Compositor {
    /// Opens the GPU whose `deviceUUID` is `device_uuid` (the one the guest renders on: imports
    /// only work there) and prepares an offscreen target of `width` x `height`.
    pub fn new(device_uuid: &[u8; 16], width: u32, height: u32) -> Result<Compositor, Error> {
        Self::open(device_uuid, width, height, None)
    }

    /// Like [`Compositor::new`], and presents into `window` (initially `width` x `height`
    /// pixels) with the lowest-latency mode available from `prefer`.
    pub fn for_window(
        device_uuid: &[u8; 16],
        window: NativeWindow,
        width: u32,
        height: u32,
        prefer: PresentMode,
    ) -> Result<Compositor, Error> {
        let mut c = Self::open(device_uuid, width, height, Some(window))?;
        c.create_window(window, width, height, prefer)?;
        Ok(c)
    }

    fn open(
        device_uuid: &[u8; 16],
        width: u32,
        height: u32,
        window: Option<NativeWindow>,
    ) -> Result<Compositor, Error> {
        // SAFETY: loading the system Vulkan loader.
        let entry = unsafe { ash::Entry::load() }.map_err(|e| Error::Load(e.to_string()))?;
        let app = vk::ApplicationInfo::default().application_name(c"qorral").api_version(vk::API_VERSION_1_1);
        let exts = [ash::khr::surface::NAME.as_ptr(), ash::khr::win32_surface::NAME.as_ptr()];
        let mut info = vk::InstanceCreateInfo::default().application_info(&app);
        if window.is_some() {
            info = info.enabled_extension_names(&exts);
        }
        // SAFETY: valid create info; the instance is destroyed in Drop.
        let instance = unsafe { entry.create_instance(&info, None) }.map_err(vkr("vkCreateInstance"))?;
        match Self::with_instance(entry, instance.clone(), device_uuid, width, height, window.is_some()) {
            Ok(c) => Ok(c),
            Err(e) => {
                // SAFETY: nothing else uses the instance.
                unsafe { instance.destroy_instance(None) };
                Err(e)
            }
        }
    }

    fn with_instance(
        entry: ash::Entry,
        instance: ash::Instance,
        device_uuid: &[u8; 16],
        width: u32,
        height: u32,
        windowed: bool,
    ) -> Result<Compositor, Error> {
        // SAFETY: plain queries on a live instance.
        let physicals = unsafe { instance.enumerate_physical_devices() }.map_err(vkr("enumerate devices"))?;
        let mut chosen = None;
        for pd in physicals {
            let mut id = vk::PhysicalDeviceIDProperties::default();
            let name = {
                let mut p2 = vk::PhysicalDeviceProperties2::default().push_next(&mut id);
                unsafe { instance.get_physical_device_properties2(pd, &mut p2) };
                p2.properties.device_name_as_c_str().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
            };
            if id.device_uuid == *device_uuid {
                chosen = Some((pd, name));
                break;
            }
        }
        let (physical, device_name) = chosen.ok_or_else(|| Error::NoDevice(hex(device_uuid)))?;

        // SAFETY: queries on a live physical device.
        let families = unsafe { instance.get_physical_device_queue_family_properties(physical) };
        let queue_family = families
            .iter()
            .position(|f| f.queue_flags.contains(vk::QueueFlags::GRAPHICS))
            .ok_or_else(|| Error::Unsupported(format!("{device_name} has no graphics queue")))?
            as u32;
        let exts = unsafe { instance.enumerate_device_extension_properties(physical) }
            .map_err(vkr("enumerate device extensions"))?;
        let has = |n: &std::ffi::CStr| exts.iter().any(|e| e.extension_name_as_c_str() == Ok(n));
        if !has(ash::khr::external_memory_win32::NAME) {
            return Err(Error::Unsupported(format!("{device_name} cannot import Win32 memory handles")));
        }
        let mut ext_names = vec![ash::khr::external_memory_win32::NAME.as_ptr()];
        if windowed {
            ext_names.push(ash::khr::swapchain::NAME.as_ptr());
        }
        let prio = [1.0f32];
        let queues = [vk::DeviceQueueCreateInfo::default().queue_family_index(queue_family).queue_priorities(&prio)];
        let info = vk::DeviceCreateInfo::default().queue_create_infos(&queues).enabled_extension_names(&ext_names);
        // SAFETY: valid create info; destroyed in Drop (or below on failure).
        let device = unsafe { instance.create_device(physical, &info, None) }.map_err(vkr("vkCreateDevice"))?;
        let queue = unsafe { device.get_device_queue(queue_family, 0) };
        let memory_types = unsafe { instance.get_physical_device_memory_properties(physical) };

        let mut c = Compositor {
            _entry: entry,
            instance,
            device,
            physical,
            queue,
            queue_family,
            queue_families: (0..families.len() as u32).collect(),
            memory_types,
            device_name,
            pool: vk::DescriptorPool::null(),
            sampler: vk::Sampler::null(),
            render_pass: vk::RenderPass::null(),
            present_pass: vk::RenderPass::null(),
            image_pipe: NULL_PIPE,
            buffer_pipe: NULL_PIPE,
            commands: vk::CommandPool::null(),
            cmd: vk::CommandBuffer::null(),
            fence: vk::Fence::null(),
            target: Target {
                width,
                height,
                image: vk::Image::null(),
                memory: vk::DeviceMemory::null(),
                view: vk::ImageView::null(),
                framebuffer: vk::Framebuffer::null(),
                readback: vk::Buffer::null(),
                readback_memory: vk::DeviceMemory::null(),
            },
            window: None,
            guests: HashMap::new(),
        };
        // From here Drop cleans up whatever was created.
        c.create_shared()?;
        c.image_pipe = c.create_pipeline(vk::DescriptorType::COMBINED_IMAGE_SAMPLER, FRAG_IMAGE)?;
        c.buffer_pipe = c.create_pipeline(vk::DescriptorType::STORAGE_BUFFER, FRAG_BUFFER)?;
        c.create_target(width, height)?;
        Ok(c)
    }

    fn memory_type(&self, bits: u32, want: vk::MemoryPropertyFlags) -> Option<u32> {
        (0..self.memory_types.memory_type_count)
            .find(|&i| bits & (1 << i) != 0 && self.memory_types.memory_types[i as usize].property_flags.contains(want))
    }

    /// Descriptor pool, sampler, render pass, command buffer and fence.
    fn create_shared(&mut self) -> Result<(), Error> {
        let d = &self.device;
        // SAFETY: valid create infos; objects are destroyed in Drop.
        unsafe {
            let sizes = [
                vk::DescriptorPoolSize { ty: vk::DescriptorType::STORAGE_BUFFER, descriptor_count: MAX_GUEST_BUFFERS },
                vk::DescriptorPoolSize {
                    ty: vk::DescriptorType::COMBINED_IMAGE_SAMPLER,
                    descriptor_count: MAX_GUEST_BUFFERS,
                },
            ];
            self.pool = d
                .create_descriptor_pool(
                    &vk::DescriptorPoolCreateInfo::default()
                        .flags(vk::DescriptorPoolCreateFlags::FREE_DESCRIPTOR_SET)
                        .max_sets(MAX_GUEST_BUFFERS)
                        .pool_sizes(&sizes),
                    None,
                )
                .map_err(vkr("descriptor pool"))?;
            self.sampler = d
                .create_sampler(
                    &vk::SamplerCreateInfo::default()
                        .mag_filter(vk::Filter::LINEAR)
                        .min_filter(vk::Filter::LINEAR)
                        .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                        .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                        .address_mode_w(vk::SamplerAddressMode::CLAMP_TO_EDGE),
                    None,
                )
                .map_err(vkr("sampler"))?;
            let attachments = [vk::AttachmentDescription::default()
                .format(FORMAT)
                .samples(vk::SampleCountFlags::TYPE_1)
                .load_op(vk::AttachmentLoadOp::CLEAR)
                .store_op(vk::AttachmentStoreOp::STORE)
                .initial_layout(vk::ImageLayout::UNDEFINED)
                .final_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)];
            let color = [vk::AttachmentReference { attachment: 0, layout: vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL }];
            let subpasses = [vk::SubpassDescription::default()
                .pipeline_bind_point(vk::PipelineBindPoint::GRAPHICS)
                .color_attachments(&color)];
            self.render_pass = d
                .create_render_pass(
                    &vk::RenderPassCreateInfo::default().attachments(&attachments).subpasses(&subpasses),
                    None,
                )
                .map_err(vkr("render pass"))?;
            let present_attachments = [attachments[0].final_layout(vk::ImageLayout::PRESENT_SRC_KHR)];
            self.present_pass = d
                .create_render_pass(
                    &vk::RenderPassCreateInfo::default().attachments(&present_attachments).subpasses(&subpasses),
                    None,
                )
                .map_err(vkr("present render pass"))?;
            self.commands = d
                .create_command_pool(
                    &vk::CommandPoolCreateInfo::default()
                        .queue_family_index(self.queue_family)
                        .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER),
                    None,
                )
                .map_err(vkr("command pool"))?;
            self.cmd = d
                .allocate_command_buffers(
                    &vk::CommandBufferAllocateInfo::default().command_pool(self.commands).command_buffer_count(1),
                )
                .map_err(vkr("command buffer"))?[0];
            self.fence = d.create_fence(&vk::FenceCreateInfo::default(), None).map_err(vkr("fence"))?;
        }
        Ok(())
    }

    /// A pipeline drawing the guest from one descriptor of type `ty` with fragment shader `frag`.
    fn create_pipeline(&self, ty: vk::DescriptorType, frag: &[u8]) -> Result<Pipeline, Error> {
        let d = &self.device;
        let mut p = NULL_PIPE;
        let bindings = [vk::DescriptorSetLayoutBinding::default()
            .binding(0)
            .descriptor_type(ty)
            .descriptor_count(1)
            .stage_flags(vk::ShaderStageFlags::FRAGMENT)];
        // SAFETY: valid create infos; on failure the partial pipeline is destroyed.
        unsafe {
            let result = (|| -> Result<(), Error> {
                p.set_layout = d
                    .create_descriptor_set_layout(
                        &vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings),
                        None,
                    )
                    .map_err(vkr("descriptor set layout"))?;
                let push =
                    [vk::PushConstantRange { stage_flags: vk::ShaderStageFlags::FRAGMENT, offset: 0, size: PUSH_SIZE }];
                let set_layouts = [p.set_layout];
                p.layout = d
                    .create_pipeline_layout(
                        &vk::PipelineLayoutCreateInfo::default().set_layouts(&set_layouts).push_constant_ranges(&push),
                        None,
                    )
                    .map_err(vkr("pipeline layout"))?;
                let module = |bytes: &[u8]| -> Result<vk::ShaderModule, Error> {
                    let words = ash::util::read_spv(&mut std::io::Cursor::new(bytes))
                        .map_err(|e| Error::Unsupported(format!("bad shader: {e}")))?;
                    d.create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&words), None)
                        .map_err(vkr("shader module"))
                };
                let vert = module(VERT)?;
                let frag = match module(frag) {
                    Ok(f) => f,
                    Err(e) => {
                        d.destroy_shader_module(vert, None);
                        return Err(e);
                    }
                };
                let stages = [
                    vk::PipelineShaderStageCreateInfo::default()
                        .stage(vk::ShaderStageFlags::VERTEX)
                        .module(vert)
                        .name(c"main"),
                    vk::PipelineShaderStageCreateInfo::default()
                        .stage(vk::ShaderStageFlags::FRAGMENT)
                        .module(frag)
                        .name(c"main"),
                ];
                let vertex_input = vk::PipelineVertexInputStateCreateInfo::default();
                let assembly =
                    vk::PipelineInputAssemblyStateCreateInfo::default().topology(vk::PrimitiveTopology::TRIANGLE_LIST);
                let viewport = vk::PipelineViewportStateCreateInfo::default().viewport_count(1).scissor_count(1);
                let raster = vk::PipelineRasterizationStateCreateInfo::default()
                    .polygon_mode(vk::PolygonMode::FILL)
                    .cull_mode(vk::CullModeFlags::NONE)
                    .line_width(1.0);
                let multisample = vk::PipelineMultisampleStateCreateInfo::default()
                    .rasterization_samples(vk::SampleCountFlags::TYPE_1);
                let blend_attachments =
                    [vk::PipelineColorBlendAttachmentState::default().color_write_mask(vk::ColorComponentFlags::RGBA)];
                let blend = vk::PipelineColorBlendStateCreateInfo::default().attachments(&blend_attachments);
                let dynamic_states = [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR];
                let dynamic = vk::PipelineDynamicStateCreateInfo::default().dynamic_states(&dynamic_states);
                let info = [vk::GraphicsPipelineCreateInfo::default()
                    .stages(&stages)
                    .vertex_input_state(&vertex_input)
                    .input_assembly_state(&assembly)
                    .viewport_state(&viewport)
                    .rasterization_state(&raster)
                    .multisample_state(&multisample)
                    .color_blend_state(&blend)
                    .dynamic_state(&dynamic)
                    .layout(p.layout)
                    .render_pass(self.render_pass)];
                let result = d.create_graphics_pipelines(vk::PipelineCache::null(), &info, None);
                d.destroy_shader_module(vert, None);
                d.destroy_shader_module(frag, None);
                p.pipeline = result.map_err(|(_, r)| Error::Vk("graphics pipeline", r))?[0];
                Ok(())
            })();
            if let Err(e) = result {
                self.destroy_pipeline(&p);
                return Err(e);
            }
        }
        Ok(p)
    }

    fn destroy_pipeline(&self, p: &Pipeline) {
        // SAFETY: null handles are ignored; the GPU does not use these when called.
        unsafe {
            self.device.destroy_pipeline(p.pipeline, None);
            self.device.destroy_pipeline_layout(p.layout, None);
            self.device.destroy_descriptor_set_layout(p.set_layout, None);
        }
    }

    fn create_target(&mut self, width: u32, height: u32) -> Result<(), Error> {
        // SAFETY: valid create infos; objects destroyed in Drop.
        unsafe {
            let d = &self.device;
            self.target.image = d
                .create_image(
                    &vk::ImageCreateInfo::default()
                        .image_type(vk::ImageType::TYPE_2D)
                        .format(FORMAT)
                        .extent(vk::Extent3D { width, height, depth: 1 })
                        .mip_levels(1)
                        .array_layers(1)
                        .samples(vk::SampleCountFlags::TYPE_1)
                        .tiling(vk::ImageTiling::OPTIMAL)
                        .usage(vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::TRANSFER_SRC),
                    None,
                )
                .map_err(vkr("target image"))?;
            let req = d.get_image_memory_requirements(self.target.image);
            let ty = self
                .memory_type(req.memory_type_bits, vk::MemoryPropertyFlags::DEVICE_LOCAL)
                .ok_or_else(|| Error::Unsupported("no device-local memory for the target".into()))?;
            self.target.memory = d
                .allocate_memory(
                    &vk::MemoryAllocateInfo::default().allocation_size(req.size).memory_type_index(ty),
                    None,
                )
                .map_err(vkr("target memory"))?;
            d.bind_image_memory(self.target.image, self.target.memory, 0).map_err(vkr("bind target"))?;
            self.target.view = d
                .create_image_view(
                    &vk::ImageViewCreateInfo::default()
                        .image(self.target.image)
                        .view_type(vk::ImageViewType::TYPE_2D)
                        .format(FORMAT)
                        .subresource_range(color_range()),
                    None,
                )
                .map_err(vkr("target view"))?;
            let views = [self.target.view];
            self.target.framebuffer = d
                .create_framebuffer(
                    &vk::FramebufferCreateInfo::default()
                        .render_pass(self.render_pass)
                        .attachments(&views)
                        .width(width)
                        .height(height)
                        .layers(1),
                    None,
                )
                .map_err(vkr("framebuffer"))?;
            let (b, m) = self.create_buffer(
                u64::from(width) * u64::from(height) * 4,
                vk::BufferUsageFlags::TRANSFER_DST,
                vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
            )?;
            self.target.readback = b;
            self.target.readback_memory = m;
        }
        Ok(())
    }

    /// A buffer with its own memory.
    fn create_buffer(
        &self,
        size: u64,
        usage: vk::BufferUsageFlags,
        props: vk::MemoryPropertyFlags,
    ) -> Result<(vk::Buffer, vk::DeviceMemory), Error> {
        let d = &self.device;
        // SAFETY: valid create infos; destroyed on failure.
        unsafe {
            let b = d
                .create_buffer(&vk::BufferCreateInfo::default().size(size).usage(usage), None)
                .map_err(vkr("buffer"))?;
            let req = d.get_buffer_memory_requirements(b);
            let Some(ty) = self.memory_type(req.memory_type_bits, props) else {
                d.destroy_buffer(b, None);
                return Err(Error::Unsupported(format!("no memory type with {props:?}")));
            };
            let m = match d.allocate_memory(
                &vk::MemoryAllocateInfo::default().allocation_size(req.size).memory_type_index(ty),
                None,
            ) {
                Ok(m) => m,
                Err(r) => {
                    d.destroy_buffer(b, None);
                    return Err(Error::Vk("buffer memory", r));
                }
            };
            if let Err(r) = d.bind_buffer_memory(b, m, 0) {
                d.destroy_buffer(b, None);
                d.free_memory(m, None);
                return Err(Error::Vk("bind buffer", r));
            }
            Ok((b, m))
        }
    }

    /// Imports guest buffer `resource_id` (replacing an earlier import of it). The handle in
    /// `mem` is consumed (closed) whatever happens. Returns whether the import is zero copy.
    pub fn import(&mut self, resource_id: u32, mem: GuestMemory) -> Result<bool, Error> {
        self.forget(resource_id);
        let result = match mem.image {
            Some(img) if img.usage & vk::ImageUsageFlags::SAMPLED.as_raw() != 0 => {
                self.import_image(&mem, img).map(|g| (g, true))
            }
            _ => self.import_buffer(&mem).map(|g| (g, false)),
        };
        close_handle(mem.handle);
        let (g, zero_copy) = result?;
        self.guests.insert(resource_id, g);
        Ok(zero_copy)
    }

    fn import_memory(
        &self,
        mem: &GuestMemory,
        reqs: vk::MemoryRequirements,
        dedicated_image: vk::Image,
    ) -> Result<vk::DeviceMemory, Error> {
        if reqs.memory_type_bits & (1 << mem.memory_type_index) == 0 || reqs.size > mem.allocation_size {
            return Err(Error::Unsupported(format!(
                "guest memory (type {}, {} bytes) does not fit (types {:#x}, {} bytes needed)",
                mem.memory_type_index, mem.allocation_size, reqs.memory_type_bits, reqs.size
            )));
        }
        let mut import = vk::ImportMemoryWin32HandleInfoKHR::default()
            .handle_type(vk::ExternalMemoryHandleTypeFlags::OPAQUE_WIN32)
            .handle(mem.handle);
        let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(dedicated_image);
        let mut info = vk::MemoryAllocateInfo::default()
            .allocation_size(mem.allocation_size)
            .memory_type_index(mem.memory_type_index)
            .push_next(&mut import);
        if dedicated_image != vk::Image::null() {
            info = info.push_next(&mut dedicated);
        }
        // SAFETY: valid import of a handle we own; importing does not take it over.
        unsafe { self.device.allocate_memory(&info, None) }.map_err(vkr("import guest memory"))
    }

    fn alloc_set(&self, layout: vk::DescriptorSetLayout) -> Result<vk::DescriptorSet, Error> {
        let layouts = [layout];
        // SAFETY: the pool and layout are live.
        unsafe {
            self.device.allocate_descriptor_sets(
                &vk::DescriptorSetAllocateInfo::default().descriptor_pool(self.pool).set_layouts(&layouts),
            )
        }
        .map(|s| s[0])
        .map_err(vkr("descriptor set"))
    }

    fn import_image(&self, mem: &GuestMemory, p: ImageParams) -> Result<Guest, Error> {
        let d = &self.device;
        let concurrent = vk::SharingMode::from_raw(p.sharing_mode as i32) == vk::SharingMode::CONCURRENT
            && self.queue_families.len() > 1;
        let mut ext =
            vk::ExternalMemoryImageCreateInfo::default().handle_types(vk::ExternalMemoryHandleTypeFlags::OPAQUE_WIN32);
        let mut info = vk::ImageCreateInfo::default()
            .flags(vk::ImageCreateFlags::from_raw(p.flags))
            .image_type(vk::ImageType::from_raw(p.image_type as i32))
            .format(vk::Format::from_raw(p.format as i32))
            .extent(vk::Extent3D { width: p.width, height: p.height, depth: p.depth })
            .mip_levels(p.mip_levels)
            .array_layers(p.array_layers)
            .samples(vk::SampleCountFlags::from_raw(p.samples))
            .tiling(vk::ImageTiling::from_raw(p.tiling as i32))
            .usage(vk::ImageUsageFlags::from_raw(p.usage))
            .initial_layout(vk::ImageLayout::UNDEFINED)
            .push_next(&mut ext);
        info = if concurrent {
            info.sharing_mode(vk::SharingMode::CONCURRENT).queue_family_indices(&self.queue_families)
        } else {
            info.sharing_mode(vk::SharingMode::EXCLUSIVE)
        };
        // SAFETY: valid create infos; everything is destroyed on failure.
        unsafe {
            let image = d.create_image(&info, None).map_err(vkr("guest image"))?;
            let memory = match self.import_memory(mem, d.get_image_memory_requirements(image), image) {
                Ok(m) => m,
                Err(e) => {
                    d.destroy_image(image, None);
                    return Err(e);
                }
            };
            let fail = |e: Error| {
                d.destroy_image(image, None);
                d.free_memory(memory, None);
                e
            };
            d.bind_image_memory(image, memory, 0).map_err(|r| fail(Error::Vk("bind guest image", r)))?;
            let view = d
                .create_image_view(
                    &vk::ImageViewCreateInfo::default()
                        .image(image)
                        .view_type(vk::ImageViewType::TYPE_2D)
                        .format(vk::Format::from_raw(p.format as i32))
                        .subresource_range(color_range()),
                    None,
                )
                .map_err(|r| fail(Error::Vk("guest image view", r)))?;
            let set = match self.alloc_set(self.image_pipe.set_layout) {
                Ok(s) => s,
                Err(e) => {
                    d.destroy_image_view(view, None);
                    return Err(fail(e));
                }
            };
            let image_info = [vk::DescriptorImageInfo {
                sampler: self.sampler,
                image_view: view,
                image_layout: vk::ImageLayout::GENERAL,
            }];
            d.update_descriptor_sets(
                &[vk::WriteDescriptorSet::default()
                    .dst_set(set)
                    .dst_binding(0)
                    .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                    .image_info(&image_info)],
                &[],
            );
            // The scanout may be a part of the image: find it from the offset and stride.
            let l = mem.layout;
            let (sy, sx) = if l.stride > 0 {
                (l.offset / u64::from(l.stride), (l.offset % u64::from(l.stride)) / 4)
            } else {
                (0, 0)
            };
            let src = [sx as f32, sy as f32, l.width as f32, l.height as f32];
            Ok(Guest::Image { image, view, memory, set, src })
        }
    }

    fn import_buffer(&self, mem: &GuestMemory) -> Result<Guest, Error> {
        let l = mem.layout;
        let end = l.offset + u64::from(l.stride) * u64::from(l.height);
        if !l.stride.is_multiple_of(4)
            || !l.offset.is_multiple_of(4)
            || end > mem.allocation_size
            || l.stride / 4 < l.width
        {
            return Err(Error::Unsupported(format!("guest buffer layout not drawable: {l:?}")));
        }
        let d = &self.device;
        // SAFETY: valid create infos; everything is destroyed on failure.
        unsafe {
            let mut ext = vk::ExternalMemoryBufferCreateInfo::default()
                .handle_types(vk::ExternalMemoryHandleTypeFlags::OPAQUE_WIN32);
            let buffer = d
                .create_buffer(
                    &vk::BufferCreateInfo::default()
                        .size(end)
                        .usage(vk::BufferUsageFlags::TRANSFER_SRC)
                        .push_next(&mut ext),
                    None,
                )
                .map_err(vkr("guest buffer"))?;
            let memory = match self.import_memory(mem, d.get_buffer_memory_requirements(buffer), vk::Image::null()) {
                Ok(m) => m,
                Err(e) => {
                    d.destroy_buffer(buffer, None);
                    return Err(e);
                }
            };
            let fail = |e: Error| {
                d.destroy_buffer(buffer, None);
                d.free_memory(memory, None);
                e
            };
            d.bind_buffer_memory(buffer, memory, 0).map_err(|r| fail(Error::Vk("bind guest buffer", r)))?;
            let (copy, copy_memory) = self
                .create_buffer(
                    end,
                    vk::BufferUsageFlags::STORAGE_BUFFER | vk::BufferUsageFlags::TRANSFER_DST,
                    vk::MemoryPropertyFlags::DEVICE_LOCAL,
                )
                .map_err(fail)?;
            let set = match self.alloc_set(self.buffer_pipe.set_layout) {
                Ok(s) => s,
                Err(e) => {
                    d.destroy_buffer(copy, None);
                    d.free_memory(copy_memory, None);
                    return Err(fail(e));
                }
            };
            let info = [vk::DescriptorBufferInfo { buffer: copy, offset: 0, range: vk::WHOLE_SIZE }];
            d.update_descriptor_sets(
                &[vk::WriteDescriptorSet::default()
                    .dst_set(set)
                    .dst_binding(0)
                    .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                    .buffer_info(&info)],
                &[],
            );
            Ok(Guest::Buffer { buffer, memory, copy, copy_memory, set, layout: l })
        }
    }

    /// Releases the import of `resource_id` (the guest freed it).
    pub fn forget(&mut self, resource_id: u32) {
        let Some(g) = self.guests.remove(&resource_id) else { return };
        let d = &self.device;
        // SAFETY: the GPU is idle between draws (each draw waits), so nothing uses them.
        unsafe {
            match g {
                Guest::Image { image, view, memory, set, .. } => {
                    let _ = d.free_descriptor_sets(self.pool, &[set]);
                    d.destroy_image_view(view, None);
                    d.destroy_image(image, None);
                    d.free_memory(memory, None);
                }
                Guest::Buffer { buffer, memory, copy, copy_memory, set, .. } => {
                    let _ = d.free_descriptor_sets(self.pool, &[set]);
                    d.destroy_buffer(copy, None);
                    d.free_memory(copy_memory, None);
                    d.destroy_buffer(buffer, None);
                    d.free_memory(memory, None);
                }
            }
        }
    }

    pub fn has(&self, resource_id: u32) -> bool {
        self.guests.contains_key(&resource_id)
    }

    /// Records drawing guest buffer `resource_id` into `dst` of a render pass instance (into
    /// the begun command buffer).
    fn record(
        &self,
        resource_id: u32,
        dst: [f32; 4],
        pass: vk::RenderPass,
        framebuffer: vk::Framebuffer,
        extent: vk::Extent2D,
    ) -> Result<(), Error> {
        let g = self
            .guests
            .get(&resource_id)
            .ok_or_else(|| Error::Unsupported(format!("guest buffer {resource_id} not imported")))?;
        let d = &self.device;
        let mut push = [0u8; PUSH_SIZE as usize];
        let put = |push: &mut [u8], at: usize, bytes: [u8; 4]| push[at..at + 4].copy_from_slice(&bytes);
        for (i, v) in dst.iter().enumerate() {
            put(&mut push, i * 4, v.to_le_bytes());
        }
        let (pipe, set) = match g {
            Guest::Image { set, src, .. } => {
                for (i, v) in src.iter().enumerate() {
                    put(&mut push, 16 + i * 4, v.to_le_bytes());
                }
                (&self.image_pipe, *set)
            }
            Guest::Buffer { set, layout: l, .. } => {
                let words = [(l.offset / 4) as u32, l.stride / 4, l.width, l.height];
                for (i, v) in words.iter().enumerate() {
                    put(&mut push, 16 + i * 4, v.to_le_bytes());
                }
                (&self.buffer_pipe, *set)
            }
        };
        // SAFETY: one command buffer, recorded and waited for here; handles are live.
        unsafe {
            match g {
                Guest::Image { image, .. } => {
                    // Take the guest's image from the external queue (QEMU's renderer), keeping
                    // its contents (GENERAL to GENERAL: no layout change that could discard them).
                    let acquire = [vk::ImageMemoryBarrier::default()
                        .src_access_mask(vk::AccessFlags::empty())
                        .dst_access_mask(vk::AccessFlags::SHADER_READ)
                        .old_layout(vk::ImageLayout::GENERAL)
                        .new_layout(vk::ImageLayout::GENERAL)
                        .src_queue_family_index(vk::QUEUE_FAMILY_EXTERNAL)
                        .dst_queue_family_index(self.queue_family)
                        .image(*image)
                        .subresource_range(color_range())];
                    d.cmd_pipeline_barrier(
                        self.cmd,
                        vk::PipelineStageFlags::TOP_OF_PIPE,
                        vk::PipelineStageFlags::FRAGMENT_SHADER,
                        vk::DependencyFlags::empty(),
                        &[],
                        &[],
                        &acquire,
                    );
                }
                Guest::Buffer { buffer, copy, layout: l, .. } => {
                    let acquire = [vk::BufferMemoryBarrier::default()
                        .src_access_mask(vk::AccessFlags::empty())
                        .dst_access_mask(vk::AccessFlags::TRANSFER_READ)
                        .src_queue_family_index(vk::QUEUE_FAMILY_EXTERNAL)
                        .dst_queue_family_index(self.queue_family)
                        .buffer(*buffer)
                        .offset(0)
                        .size(vk::WHOLE_SIZE)];
                    d.cmd_pipeline_barrier(
                        self.cmd,
                        vk::PipelineStageFlags::TOP_OF_PIPE,
                        vk::PipelineStageFlags::TRANSFER,
                        vk::DependencyFlags::empty(),
                        &[],
                        &acquire,
                        &[],
                    );
                    d.cmd_copy_buffer(
                        self.cmd,
                        *buffer,
                        *copy,
                        &[vk::BufferCopy {
                            src_offset: l.offset,
                            dst_offset: l.offset,
                            size: u64::from(l.stride) * u64::from(l.height),
                        }],
                    );
                    let ready = [vk::BufferMemoryBarrier::default()
                        .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                        .dst_access_mask(vk::AccessFlags::SHADER_READ)
                        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                        .buffer(*copy)
                        .offset(0)
                        .size(vk::WHOLE_SIZE)];
                    d.cmd_pipeline_barrier(
                        self.cmd,
                        vk::PipelineStageFlags::TRANSFER,
                        vk::PipelineStageFlags::FRAGMENT_SHADER,
                        vk::DependencyFlags::empty(),
                        &[],
                        &ready,
                        &[],
                    );
                }
            }
            let clear = [vk::ClearValue { color: vk::ClearColorValue { float32: [0.06, 0.06, 0.07, 1.0] } }];
            d.cmd_begin_render_pass(
                self.cmd,
                &vk::RenderPassBeginInfo::default()
                    .render_pass(pass)
                    .framebuffer(framebuffer)
                    .render_area(vk::Rect2D {
                        offset: vk::Offset2D { x: 0, y: 0 },
                        extent: vk::Extent2D { width: extent.width, height: extent.height },
                    })
                    .clear_values(&clear),
                vk::SubpassContents::INLINE,
            );
            d.cmd_bind_pipeline(self.cmd, vk::PipelineBindPoint::GRAPHICS, pipe.pipeline);
            d.cmd_set_viewport(
                self.cmd,
                0,
                &[vk::Viewport { x: dst[0], y: dst[1], width: dst[2], height: dst[3], min_depth: 0.0, max_depth: 1.0 }],
            );
            let x0 = (dst[0].max(0.0) as u32).min(extent.width);
            let y0 = (dst[1].max(0.0) as u32).min(extent.height);
            let x1 = ((dst[0] + dst[2]).max(0.0) as u32).min(extent.width);
            let y1 = ((dst[1] + dst[3]).max(0.0) as u32).min(extent.height);
            d.cmd_set_scissor(
                self.cmd,
                0,
                &[vk::Rect2D {
                    offset: vk::Offset2D { x: x0 as i32, y: y0 as i32 },
                    extent: vk::Extent2D { width: x1.saturating_sub(x0), height: y1.saturating_sub(y0) },
                }],
            );
            d.cmd_bind_descriptor_sets(self.cmd, vk::PipelineBindPoint::GRAPHICS, pipe.layout, 0, &[set], &[]);
            d.cmd_push_constants(self.cmd, pipe.layout, vk::ShaderStageFlags::FRAGMENT, 0, &push);
            d.cmd_draw(self.cmd, 3, 1, 0, 0);
            d.cmd_end_render_pass(self.cmd);
        }
        Ok(())
    }

    fn begin(&self) -> Result<(), Error> {
        // SAFETY: the command buffer is not in use (every submit is waited for).
        unsafe {
            self.device.reset_command_buffer(self.cmd, vk::CommandBufferResetFlags::empty()).map_err(vkr("reset"))?;
            self.device
                .begin_command_buffer(
                    self.cmd,
                    &vk::CommandBufferBeginInfo::default().flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
                )
                .map_err(vkr("begin"))
        }
    }

    /// Draws guest buffer `resource_id` into `dst` (x, y, w, h in target pixels, scaled when the
    /// size differs) of the offscreen target and waits for the GPU. With `readback`, the target is
    /// copied to host memory for [`Compositor::pixels`]. Returns the time from submit to
    /// completion.
    pub fn draw(&mut self, resource_id: u32, dst: [f32; 4], readback: bool) -> Result<Duration, Error> {
        let t = &self.target;
        let extent = vk::Extent2D { width: t.width, height: t.height };
        self.begin()?;
        self.record(resource_id, dst, self.render_pass, t.framebuffer, extent)?;
        let d = &self.device;
        // SAFETY: the command buffer was begun above; handles are live.
        unsafe {
            if readback {
                d.cmd_copy_image_to_buffer(
                    self.cmd,
                    t.image,
                    vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                    t.readback,
                    &[vk::BufferImageCopy {
                        buffer_offset: 0,
                        buffer_row_length: 0,
                        buffer_image_height: 0,
                        image_subresource: vk::ImageSubresourceLayers {
                            aspect_mask: vk::ImageAspectFlags::COLOR,
                            mip_level: 0,
                            base_array_layer: 0,
                            layer_count: 1,
                        },
                        image_offset: vk::Offset3D { x: 0, y: 0, z: 0 },
                        image_extent: vk::Extent3D { width: t.width, height: t.height, depth: 1 },
                    }],
                );
            }
            d.end_command_buffer(self.cmd).map_err(vkr("end"))?;
            let cmds = [self.cmd];
            let submit = [vk::SubmitInfo::default().command_buffers(&cmds)];
            let t0 = Instant::now();
            d.queue_submit(self.queue, &submit, self.fence).map_err(vkr("submit"))?;
            d.wait_for_fences(&[self.fence], true, 5_000_000_000).map_err(vkr("wait"))?;
            let took = t0.elapsed();
            d.reset_fences(&[self.fence]).map_err(vkr("reset fence"))?;
            Ok(took)
        }
    }

    fn create_window(&mut self, w: NativeWindow, width: u32, height: u32, prefer: PresentMode) -> Result<(), Error> {
        let surface_fn = ash::khr::surface::Instance::new(&self._entry, &self.instance);
        let win32_fn = ash::khr::win32_surface::Instance::new(&self._entry, &self.instance);
        let swapchain_fn = ash::khr::swapchain::Device::new(&self.instance, &self.device);
        // SAFETY: a live HWND and HINSTANCE from the caller; destroyed in Drop.
        let surface = unsafe {
            win32_fn.create_win32_surface(
                &vk::Win32SurfaceCreateInfoKHR::default().hwnd(w.hwnd).hinstance(w.hinstance),
                None,
            )
        }
        .map_err(vkr("window surface"))?;
        // SAFETY: queries on live objects.
        let ok = unsafe { surface_fn.get_physical_device_surface_support(self.physical, self.queue_family, surface) };
        let modes = unsafe { surface_fn.get_physical_device_surface_present_modes(self.physical, surface) };
        let (Ok(true), Ok(modes)) = (ok, modes) else {
            unsafe { surface_fn.destroy_surface(surface, None) };
            return Err(Error::Unsupported(format!("{} cannot present to this window", self.device_name)));
        };
        let order: &[vk::PresentModeKHR] = match prefer {
            PresentMode::Immediate => &[vk::PresentModeKHR::IMMEDIATE, vk::PresentModeKHR::MAILBOX],
            PresentMode::Mailbox => &[vk::PresentModeKHR::MAILBOX, vk::PresentModeKHR::IMMEDIATE],
            PresentMode::Fifo => &[],
        };
        let mode = order.iter().copied().find(|m| modes.contains(m)).unwrap_or(vk::PresentModeKHR::FIFO);
        // SAFETY: plain semaphore creation; destroyed in Drop.
        let (acquired, rendered) = unsafe {
            (
                self.device.create_semaphore(&vk::SemaphoreCreateInfo::default(), None).map_err(vkr("semaphore"))?,
                self.device.create_semaphore(&vk::SemaphoreCreateInfo::default(), None).map_err(vkr("semaphore"))?,
            )
        };
        self.window = Some(Window {
            surface_fn,
            swapchain_fn,
            surface,
            swapchain: vk::SwapchainKHR::null(),
            mode,
            extent: vk::Extent2D { width, height },
            views: Vec::new(),
            framebuffers: Vec::new(),
            acquired,
            rendered,
        });
        self.resize(width, height)
    }

    /// The present mode in use (the lowest-latency one the window offers).
    pub fn present_mode(&self) -> Option<PresentMode> {
        self.window.as_ref().map(|w| match w.mode {
            vk::PresentModeKHR::IMMEDIATE => PresentMode::Immediate,
            vk::PresentModeKHR::MAILBOX => PresentMode::Mailbox,
            _ => PresentMode::Fifo,
        })
    }

    /// (Re)creates the swapchain for a window of `width` x `height` pixels.
    pub fn resize(&mut self, width: u32, height: u32) -> Result<(), Error> {
        let Some(w) = self.window.as_mut() else { return Ok(()) };
        let d = &self.device;
        // SAFETY: the GPU is idle (every frame is waited for); old objects are not in use.
        unsafe {
            let caps = w
                .surface_fn
                .get_physical_device_surface_capabilities(self.physical, w.surface)
                .map_err(vkr("surface capabilities"))?;
            let extent = if caps.current_extent.width != u32::MAX {
                caps.current_extent
            } else {
                vk::Extent2D {
                    width: width.clamp(caps.min_image_extent.width, caps.max_image_extent.width),
                    height: height.clamp(caps.min_image_extent.height, caps.max_image_extent.height),
                }
            };
            for f in w.framebuffers.drain(..) {
                d.destroy_framebuffer(f, None);
            }
            for v in w.views.drain(..) {
                d.destroy_image_view(v, None);
            }
            if extent.width == 0 || extent.height == 0 {
                // Minimised: nothing to present into until it is restored.
                w.extent = extent;
                return Ok(());
            }
            let max = if caps.max_image_count == 0 { u32::MAX } else { caps.max_image_count };
            let count = (caps.min_image_count + 1).min(max);
            let old = w.swapchain;
            w.swapchain = w
                .swapchain_fn
                .create_swapchain(
                    &vk::SwapchainCreateInfoKHR::default()
                        .surface(w.surface)
                        .min_image_count(count)
                        .image_format(FORMAT)
                        .image_color_space(vk::ColorSpaceKHR::SRGB_NONLINEAR)
                        .image_extent(extent)
                        .image_array_layers(1)
                        .image_usage(vk::ImageUsageFlags::COLOR_ATTACHMENT)
                        .image_sharing_mode(vk::SharingMode::EXCLUSIVE)
                        .pre_transform(caps.current_transform)
                        .composite_alpha(vk::CompositeAlphaFlagsKHR::OPAQUE)
                        .present_mode(w.mode)
                        .clipped(true)
                        .old_swapchain(old),
                    None,
                )
                .map_err(vkr("swapchain"))?;
            w.swapchain_fn.destroy_swapchain(old, None);
            w.extent = extent;
            for image in w.swapchain_fn.get_swapchain_images(w.swapchain).map_err(vkr("swapchain images"))? {
                let view = d
                    .create_image_view(
                        &vk::ImageViewCreateInfo::default()
                            .image(image)
                            .view_type(vk::ImageViewType::TYPE_2D)
                            .format(FORMAT)
                            .subresource_range(color_range()),
                        None,
                    )
                    .map_err(vkr("swapchain view"))?;
                w.views.push(view);
                let views = [view];
                w.framebuffers.push(
                    d.create_framebuffer(
                        &vk::FramebufferCreateInfo::default()
                            .render_pass(self.present_pass)
                            .attachments(&views)
                            .width(extent.width)
                            .height(extent.height)
                            .layers(1),
                        None,
                    )
                    .map_err(vkr("swapchain framebuffer"))?,
                );
            }
        }
        Ok(())
    }

    /// The window's size in pixels as the swapchain sees it.
    pub fn window_size(&self) -> (u32, u32) {
        self.window.as_ref().map(|w| (w.extent.width, w.extent.height)).unwrap_or((0, 0))
    }

    /// Presents guest buffer `resource_id` into the window right away, fitted into `dst` (x, y,
    /// w, h in window pixels), and waits until the GPU finished it (one frame in flight, none
    /// queued). An out-of-date swapchain is recreated and the frame presented again.
    pub fn present(&mut self, resource_id: u32, dst: [f32; 4]) -> Result<PresentTiming, Error> {
        let mut timing = PresentTiming::default();
        for attempt in 0..2 {
            let Some(w) = self.window.as_ref() else {
                return Err(Error::Unsupported("no window to present to".into()));
            };
            if w.extent.width == 0 || w.extent.height == 0 {
                return Ok(timing);
            }
            let t0 = Instant::now();
            // SAFETY: live swapchain and semaphore; the previous frame was waited for.
            let acquired =
                unsafe { w.swapchain_fn.acquire_next_image(w.swapchain, 1_000_000_000, w.acquired, vk::Fence::null()) };
            let index = match acquired {
                Ok((i, _)) => i,
                Err(vk::Result::ERROR_OUT_OF_DATE_KHR) if attempt == 0 => {
                    let (width, height) = (w.extent.width, w.extent.height);
                    self.resize(width, height)?;
                    continue;
                }
                Err(r) => return Err(Error::Vk("acquire", r)),
            };
            timing.acquire = t0.elapsed();
            let t1 = Instant::now();
            let (framebuffer, extent, acquired_sem, rendered_sem, swapchain) =
                (w.framebuffers[index as usize], w.extent, w.acquired, w.rendered, w.swapchain);
            self.begin()?;
            self.record(resource_id, dst, self.present_pass, framebuffer, extent)?;
            let d = &self.device;
            let presented;
            // SAFETY: the command buffer was begun above; handles are live.
            unsafe {
                d.end_command_buffer(self.cmd).map_err(vkr("end"))?;
                let cmds = [self.cmd];
                let waits = [acquired_sem];
                let stages = [vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT];
                let signals = [rendered_sem];
                let submit = [vk::SubmitInfo::default()
                    .wait_semaphores(&waits)
                    .wait_dst_stage_mask(&stages)
                    .command_buffers(&cmds)
                    .signal_semaphores(&signals)];
                d.queue_submit(self.queue, &submit, self.fence).map_err(vkr("submit"))?;
                let t2 = Instant::now();
                let swapchains = [swapchain];
                let indices = [index];
                presented = self.window.as_ref().map(|w| {
                    w.swapchain_fn.queue_present(
                        self.queue,
                        &vk::PresentInfoKHR::default()
                            .wait_semaphores(&signals)
                            .swapchains(&swapchains)
                            .image_indices(&indices),
                    )
                });
                timing.present = t2.elapsed();
                d.wait_for_fences(&[self.fence], true, 5_000_000_000).map_err(vkr("wait"))?;
                d.reset_fences(&[self.fence]).map_err(vkr("reset fence"))?;
            }
            timing.gpu = t1.elapsed().saturating_sub(timing.present);
            match presented {
                Some(Ok(_)) | None => {}
                Some(Err(vk::Result::ERROR_OUT_OF_DATE_KHR)) => {
                    let (width, height) = self.window_size();
                    self.resize(width, height)?;
                }
                Some(Err(r)) => return Err(Error::Vk("present", r)),
            }
            return Ok(timing);
        }
        Ok(timing)
    }

    /// The target's pixels after a `draw(.., readback: true)`: rows of B, G, R, A bytes.
    pub fn pixels(&self) -> Result<Vec<u8>, Error> {
        let t = &self.target;
        let len = u64::from(t.width) * u64::from(t.height) * 4;
        // SAFETY: host-visible coherent memory of at least `len` bytes, not in use by the GPU.
        unsafe {
            let p = self
                .device
                .map_memory(t.readback_memory, 0, len, vk::MemoryMapFlags::empty())
                .map_err(vkr("map readback"))?;
            let v = std::slice::from_raw_parts(p.cast::<u8>(), len as usize).to_vec();
            self.device.unmap_memory(t.readback_memory);
            Ok(v)
        }
    }

    pub fn size(&self) -> (u32, u32) {
        (self.target.width, self.target.height)
    }
}

fn color_range() -> vk::ImageSubresourceRange {
    vk::ImageSubresourceRange {
        aspect_mask: vk::ImageAspectFlags::COLOR,
        base_mip_level: 0,
        level_count: 1,
        base_array_layer: 0,
        layer_count: 1,
    }
}

impl Drop for Compositor {
    fn drop(&mut self) {
        let ids: Vec<u32> = self.guests.keys().copied().collect();
        // SAFETY: wait for the GPU, then destroy everything this compositor created (null
        // handles are ignored by Vulkan).
        unsafe {
            let _ = self.device.device_wait_idle();
            for id in ids {
                self.forget(id);
            }
            if let Some(mut w) = self.window.take() {
                for f in w.framebuffers.drain(..) {
                    self.device.destroy_framebuffer(f, None);
                }
                for v in w.views.drain(..) {
                    self.device.destroy_image_view(v, None);
                }
                w.swapchain_fn.destroy_swapchain(w.swapchain, None);
                w.surface_fn.destroy_surface(w.surface, None);
                self.device.destroy_semaphore(w.acquired, None);
                self.device.destroy_semaphore(w.rendered, None);
            }
            self.destroy_pipeline(&self.image_pipe);
            self.destroy_pipeline(&self.buffer_pipe);
            let d = &self.device;
            let t = &self.target;
            d.destroy_buffer(t.readback, None);
            d.free_memory(t.readback_memory, None);
            d.destroy_framebuffer(t.framebuffer, None);
            d.destroy_image_view(t.view, None);
            d.destroy_image(t.image, None);
            d.free_memory(t.memory, None);
            d.destroy_fence(self.fence, None);
            d.destroy_command_pool(self.commands, None);
            d.destroy_render_pass(self.render_pass, None);
            d.destroy_render_pass(self.present_pass, None);
            d.destroy_sampler(self.sampler, None);
            d.destroy_descriptor_pool(self.pool, None);
            d.destroy_device(None);
            self.instance.destroy_instance(None);
        }
    }
}

#[cfg(windows)]
fn close_handle(h: isize) {
    unsafe extern "system" {
        fn CloseHandle(h: isize) -> i32;
    }
    // SAFETY: the handle was handed to us to own; importing does not take ownership of it.
    unsafe { CloseHandle(h) };
}

#[cfg(not(windows))]
fn close_handle(_h: isize) {}
