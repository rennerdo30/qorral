//! Lists Vulkan devices with their UUIDs and selected extensions (diagnostics).
fn main() {
    use ash::vk;
    let entry = unsafe { ash::Entry::load() }.expect("Vulkan loader");
    let app = vk::ApplicationInfo::default().api_version(vk::API_VERSION_1_1);
    let exts = [ash::khr::surface::NAME.as_ptr(), ash::khr::win32_surface::NAME.as_ptr()];
    let inst = unsafe {
        entry.create_instance(
            &vk::InstanceCreateInfo::default().application_info(&app).enabled_extension_names(&exts),
            None,
        )
    }
    .unwrap();
    for pd in unsafe { inst.enumerate_physical_devices() }.unwrap() {
        let mut id = vk::PhysicalDeviceIDProperties::default();
        let (name, api, driver) = {
            let mut p2 = vk::PhysicalDeviceProperties2::default().push_next(&mut id);
            unsafe { inst.get_physical_device_properties2(pd, &mut p2) };
            let p = p2.properties;
            (p.device_name_as_c_str().unwrap().to_string_lossy().into_owned(), p.api_version, p.driver_version)
        };
        let uuid: String = id.device_uuid.iter().map(|b| format!("{b:02x}")).collect();
        println!(
            "{name} api {}.{}.{} driver {driver:#x} uuid {uuid}",
            vk::api_version_major(api),
            vk::api_version_minor(api),
            vk::api_version_patch(api)
        );
        let win32 = ash::khr::win32_surface::Instance::new(&entry, &inst);
        let families = unsafe { inst.get_physical_device_queue_family_properties(pd) };
        for (i, f) in families.iter().enumerate() {
            // SAFETY: a plain query; needs VK_KHR_win32_surface's function only.
            let present = unsafe { win32.get_physical_device_win32_presentation_support(pd, i as u32) };
            println!("  queue family {i}: {:?} x{} present {present}", f.queue_flags, f.queue_count);
        }
        for fmt in [vk::Format::B8G8R8A8_UNORM, vk::Format::R8G8B8A8_UNORM] {
            let p = unsafe { inst.get_physical_device_format_properties(pd, fmt) };
            println!(
                "  {fmt:?} optimal storage image {}",
                p.optimal_tiling_features.contains(vk::FormatFeatureFlags::STORAGE_IMAGE)
            );
        }
        let exts = unsafe { inst.enumerate_device_extension_properties(pd) }.unwrap();
        for e in exts {
            let n = e.extension_name_as_c_str().unwrap().to_string_lossy();
            if n.contains("compression")
                || n.contains("external_memory")
                || n.contains("dedicated")
                || n.contains("image_drm")
            {
                println!("  {n}");
            }
        }
    }
    unsafe { inst.destroy_instance(None) };
}
