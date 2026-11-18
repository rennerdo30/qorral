//! Lists Vulkan devices with their UUIDs and selected extensions (diagnostics).
fn main() {
    use ash::vk;
    let entry = unsafe { ash::Entry::load() }.expect("Vulkan loader");
    let app = vk::ApplicationInfo::default().api_version(vk::API_VERSION_1_1);
    let inst =
        unsafe { entry.create_instance(&vk::InstanceCreateInfo::default().application_info(&app), None) }.unwrap();
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
