use ash::khr::surface::Instance as Surface;
use ash::khr::swapchain::Device as Swapchain;
use ash::{Device, Entry, Instance, vk};
use raw_window_handle::{HasDisplayHandle, HasWindowHandle};
use std::ffi::CString;
use std::io::Cursor;
use winit::window::Window;

// Many fields are owned Vulkan handles that are never read back directly: they
// must stay alive for the lifetime of the context (and drop in order), so the
// dead_code lint is silenced for the whole struct.
#[allow(dead_code)]
pub struct VulkanContext {
    pub entry: Entry,
    pub instance: Instance,
    pub surface_loader: Surface,
    pub surface: vk::SurfaceKHR,
    pub physical_device: vk::PhysicalDevice,
    pub device: Device,
    pub queue: vk::Queue,
    pub queue_family_index: u32,

    pub swapchain_loader: Swapchain,
    pub swapchain: vk::SwapchainKHR,
    pub swapchain_images: Vec<vk::Image>,
    pub swapchain_image_views: Vec<vk::ImageView>,
    pub swapchain_format: vk::Format,
    pub swapchain_extent: vk::Extent2D,

    pub render_pass: vk::RenderPass,
    pub pipeline_layout: vk::PipelineLayout,
    pub pipeline: vk::Pipeline,
    pub framebuffers: Vec<vk::Framebuffer>,

    pub command_pool: vk::CommandPool,
    pub command_buffer: vk::CommandBuffer,

    pub image_available_semaphore: vk::Semaphore,
    pub render_finished_semaphore: vk::Semaphore,
    pub in_flight_fence: vk::Fence,

    pub texture_image: vk::Image,
    pub texture_mem: vk::DeviceMemory,
    pub texture_view: vk::ImageView,
    pub sampler: vk::Sampler,
    pub descriptor_pool: vk::DescriptorPool,
    pub descriptor_set: vk::DescriptorSet,

    pub staging_buffer: vk::Buffer,
    pub staging_mem: vk::DeviceMemory,
    pub staging_ptr: *mut u8,

    /// VK_EXT_external_memory_host wrapper, present only when the extension is
    /// enabled (available on the device AND not disabled via
    /// `UNEMUPS4_NO_EXTMEMHOST`). When `Some`, guest framebuffers can be imported
    /// zero-copy (see `try_import_host_buffer`); when `None` the staging-copy path
    /// is used.
    pub ext_mem_host: Option<ash::ext::external_memory_host::Device>,
    /// `minImportedHostPointerAlignment` reported by the device. Host pointers and
    /// import sizes must be aligned to this to be importable. Meaningful only when
    /// `ext_mem_host` is `Some`.
    pub min_import_alignment: u64,
}

/// A guest framebuffer imported zero-copy as a `VkBuffer` over host pages via
/// VK_EXT_external_memory_host. The GPU reads guest memory directly through this
/// buffer, so the per-frame staging memcpy is skipped for the owning
/// `(handle, index)`. Resources leak on process exit (matching the rest of
/// `VulkanContext`); they are only explicitly freed when a buffer key is
/// re-registered at a new host pointer.
pub struct ImportedBuf {
    pub buffer: vk::Buffer,
    pub memory: vk::DeviceMemory,
    /// The host pointer this import was bound to; used to detect a re-register at
    /// a different address so the stale import can be freed.
    pub host_ptr: *const u8,
}

// Grouped return values for the builders below; each is destructured straight
// into VulkanContext, so nothing here is actually dead.
struct SwapchainBundle {
    loader: Swapchain,
    handle: vk::SwapchainKHR,
    images: Vec<vk::Image>,
    image_views: Vec<vk::ImageView>,
    format: vk::Format,
    extent: vk::Extent2D,
}

struct PipelineBundle {
    descriptor_set_layout: vk::DescriptorSetLayout,
    pipeline_layout: vk::PipelineLayout,
    pipeline: vk::Pipeline,
}

struct TextureBundle {
    staging_buffer: vk::Buffer,
    staging_mem: vk::DeviceMemory,
    staging_ptr: *mut u8,
    image: vk::Image,
    mem: vk::DeviceMemory,
    view: vk::ImageView,
    sampler: vk::Sampler,
    descriptor_pool: vk::DescriptorPool,
    descriptor_set: vk::DescriptorSet,
}

type VkResult<T> = Result<T, Box<dyn std::error::Error>>;

impl VulkanContext {
    pub unsafe fn new(window: &Window) -> VkResult<Self> {
        let (entry, instance) = unsafe { Self::create_instance(window)? };
        let (surface_loader, surface) = unsafe { Self::create_surface(&entry, &instance, window)? };
        let (pdevice, queue_family_index) =
            unsafe { Self::pick_device(&instance, &surface_loader, surface)? };
        let (device, queue, ext_host_enabled) =
            unsafe { Self::create_device(&instance, pdevice, queue_family_index)? };
        let sc = unsafe {
            Self::create_swapchain(&instance, &device, pdevice, surface, &surface_loader)?
        };
        let (command_pool, command_buffer) =
            unsafe { Self::create_command_pool(&device, queue_family_index)? };
        let render_pass = unsafe { Self::create_render_pass(&device, sc.format)? };
        let pipe = unsafe { Self::create_pipeline(&device, render_pass)? };
        let framebuffers =
            unsafe { Self::create_framebuffers(&device, render_pass, &sc.image_views, sc.extent)? };
        let (image_available, render_finished, in_flight) = unsafe { Self::create_sync(&device)? };
        let tex = unsafe {
            Self::create_texture(&instance, &device, pdevice, pipe.descriptor_set_layout)?
        };

        // VK_EXT_external_memory_host: when enabled, build the wrapper and
        // query the minimum importable host-pointer alignment, then log which
        // present path the flip loop will take. When absent, the staging-copy path
        // is used unchanged.
        let (ext_mem_host, min_import_alignment) = if ext_host_enabled {
            let ext = ash::ext::external_memory_host::Device::new(&instance, &device);
            // SAFETY: `pdevice` is a valid physical device from this instance; the
            // properties2 chain is a correctly typed, initialized struct with its
            // s_type set and lifetime tied to the local `props`.
            let alignment = unsafe {
                let mut host_props = vk::PhysicalDeviceExternalMemoryHostPropertiesEXT::default();
                let mut props2 =
                    vk::PhysicalDeviceProperties2::default().push_next(&mut host_props);
                instance.get_physical_device_properties2(pdevice, &mut props2);
                host_props.min_imported_host_pointer_alignment
            };
            tracing::info!(
                "Present path: ZERO-COPY (VK_EXT_external_memory_host enabled, \
                 minImportedHostPointerAlignment={}); guest framebuffers whose host \
                 pointer is alignable will skip the staging memcpy",
                alignment
            );
            (Some(ext), alignment)
        } else {
            tracing::info!(
                "Present path: STAGING-COPY (VK_EXT_external_memory_host unavailable \
                 or disabled via UNEMUPS4_NO_EXTMEMHOST); per-frame guest framebuffer \
                 memcpy in use"
            );
            (None, 0)
        };

        tracing::info!("Vulkan context initialized");
        Ok(VulkanContext {
            entry,
            instance,
            surface_loader,
            surface,
            physical_device: pdevice,
            device,
            queue,
            queue_family_index,
            swapchain_loader: sc.loader,
            swapchain: sc.handle,
            swapchain_images: sc.images,
            swapchain_image_views: sc.image_views,
            swapchain_format: sc.format,
            swapchain_extent: sc.extent,
            render_pass,
            pipeline_layout: pipe.pipeline_layout,
            pipeline: pipe.pipeline,
            framebuffers,
            command_pool,
            command_buffer,
            image_available_semaphore: image_available,
            render_finished_semaphore: render_finished,
            in_flight_fence: in_flight,
            texture_image: tex.image,
            texture_mem: tex.mem,
            texture_view: tex.view,
            sampler: tex.sampler,
            staging_buffer: tex.staging_buffer,
            staging_mem: tex.staging_mem,
            staging_ptr: tex.staging_ptr,
            descriptor_pool: tex.descriptor_pool,
            descriptor_set: tex.descriptor_set,
            ext_mem_host,
            min_import_alignment,
        })
    }

    unsafe fn create_instance(window: &Window) -> VkResult<(Entry, Instance)> {
        unsafe {
            tracing::debug!("Loading Vulkan library");
            let entry = Entry::load()?;
            let app_name = CString::new("UnemuPS4")?;

            // no validation layers
            let layer_names: Vec<*const i8> = Vec::new();
            let layers_ptr = if layer_names.is_empty() {
                std::ptr::null()
            } else {
                layer_names.as_ptr()
            };

            let extensions =
                ash_window::enumerate_required_extensions(window.display_handle()?.as_raw())?
                    .to_vec();

            let app_info = vk::ApplicationInfo {
                s_type: vk::StructureType::APPLICATION_INFO,
                p_application_name: app_name.as_ptr(),
                api_version: vk::API_VERSION_1_3,
                ..Default::default()
            };

            let create_info = vk::InstanceCreateInfo {
                s_type: vk::StructureType::INSTANCE_CREATE_INFO,
                p_application_info: &app_info,
                pp_enabled_layer_names: layers_ptr,
                enabled_layer_count: 0,
                pp_enabled_extension_names: extensions.as_ptr(),
                enabled_extension_count: extensions.len() as u32,
                ..Default::default()
            };

            tracing::debug!("Creating instance");
            let instance = entry.create_instance(&create_info, None)?;
            Ok((entry, instance))
        }
    }

    unsafe fn create_surface(
        entry: &Entry,
        instance: &Instance,
        window: &Window,
    ) -> VkResult<(Surface, vk::SurfaceKHR)> {
        unsafe {
            tracing::debug!("Creating surface");
            let surface = ash_window::create_surface(
                entry,
                instance,
                window.display_handle()?.as_raw(),
                window.window_handle()?.as_raw(),
                None,
            )?;
            let surface_loader = Surface::new(entry, instance);
            Ok((surface_loader, surface))
        }
    }

    unsafe fn pick_device(
        instance: &Instance,
        surface_loader: &Surface,
        surface: vk::SurfaceKHR,
    ) -> VkResult<(vk::PhysicalDevice, u32)> {
        unsafe {
            tracing::debug!("Selecting physical device");
            let pdevices = instance.enumerate_physical_devices()?;
            let picked = pdevices
                .iter()
                .map(|&p| {
                    let queues = instance.get_physical_device_queue_family_properties(p);
                    (
                        p,
                        queues
                            .into_iter()
                            .enumerate()
                            .find(|(i, q)| {
                                q.queue_flags.contains(vk::QueueFlags::GRAPHICS)
                                    && surface_loader
                                        .get_physical_device_surface_support(p, *i as u32, surface)
                                        .unwrap_or(false)
                            })
                            .map(|(i, _)| i as u32),
                    )
                })
                .find(|(_, q)| q.is_some())
                .map(|(p, q)| (p, q.unwrap()))
                .ok_or("No suitable GPU found")?;
            Ok(picked)
        }
    }

    /// Creates the logical device. Always enables `VK_KHR_swapchain`. When
    /// available and not disabled via `UNEMUPS4_NO_EXTMEMHOST`, also enables
    /// `VK_EXT_external_memory_host` (plus its dependency `VK_KHR_external_memory`)
    /// so guest framebuffers can be imported zero-copy. The returned
    /// bool reports whether the external-memory-host extension was enabled.
    unsafe fn create_device(
        instance: &Instance,
        pdevice: vk::PhysicalDevice,
        queue_family_index: u32,
    ) -> VkResult<(Device, vk::Queue, bool)> {
        unsafe {
            tracing::debug!("Creating logical device");
            let priorities = [1.0];
            let queue_info = [vk::DeviceQueueCreateInfo {
                s_type: vk::StructureType::DEVICE_QUEUE_CREATE_INFO,
                queue_family_index,
                queue_count: 1,
                p_queue_priorities: priorities.as_ptr(),
                ..Default::default()
            }];

            // Probe device extensions to decide whether the zero-copy import path is
            // available. Gate additionally on an env override so the maintainer can
            // force the staging-copy fallback for A/B comparison.
            let ext_disabled = std::env::var_os("UNEMUPS4_NO_EXTMEMHOST").is_some();
            let available = instance.enumerate_device_extension_properties(pdevice)?;
            let has_ext = |name: &std::ffi::CStr| {
                available.iter().any(|e| {
                    // SAFETY: `extension_name` is a NUL-terminated fixed-size C string
                    // populated by the driver.
                    let n = std::ffi::CStr::from_ptr(e.extension_name.as_ptr());
                    n == name
                })
            };
            let ext_host_name = ash::ext::external_memory_host::NAME;
            let ext_mem_name = ash::khr::external_memory::NAME;
            let enable_ext_host = !ext_disabled && has_ext(ext_host_name) && has_ext(ext_mem_name);

            let swapchain_ext_name = CString::new("VK_KHR_swapchain")?;
            let mut device_extensions = vec![swapchain_ext_name.as_ptr()];
            if enable_ext_host {
                // VK_EXT_external_memory_host requires VK_KHR_external_memory.
                device_extensions.push(ext_mem_name.as_ptr());
                device_extensions.push(ext_host_name.as_ptr());
            }

            let device_create_info = vk::DeviceCreateInfo {
                s_type: vk::StructureType::DEVICE_CREATE_INFO,
                queue_create_info_count: 1,
                p_queue_create_infos: queue_info.as_ptr(),
                enabled_extension_count: device_extensions.len() as u32,
                pp_enabled_extension_names: device_extensions.as_ptr(),
                ..Default::default()
            };

            let device = instance.create_device(pdevice, &device_create_info, None)?;
            let queue = device.get_device_queue(queue_family_index, 0);
            Ok((device, queue, enable_ext_host))
        }
    }

    unsafe fn create_swapchain(
        instance: &Instance,
        device: &Device,
        pdevice: vk::PhysicalDevice,
        surface: vk::SurfaceKHR,
        surface_loader: &Surface,
    ) -> VkResult<SwapchainBundle> {
        unsafe {
            tracing::debug!("Creating swapchain");
            let caps = surface_loader.get_physical_device_surface_capabilities(pdevice, surface)?;
            let format = surface_loader.get_physical_device_surface_formats(pdevice, surface)?[0];
            let mut extent = caps.current_extent;

            // wayland hands back 0xFFFFFFFF or 0x0 here; force a sane size
            if extent.width == u32::MAX || extent.width == 0 {
                tracing::warn!(
                    "Surface returned invalid extent {:?}, forcing 1920x1080",
                    extent
                );
                extent.width = 1920;
                extent.height = 1080;
            }

            let swapchain_loader = Swapchain::new(instance, device);
            let swapchain_info = vk::SwapchainCreateInfoKHR {
                s_type: vk::StructureType::SWAPCHAIN_CREATE_INFO_KHR,
                surface,
                min_image_count: 2,
                image_format: format.format,
                image_color_space: format.color_space,
                image_extent: extent,
                image_array_layers: 1,
                // TRANSFER_SRC lets the env-gated PNG dump (UNEMUPS4_DUMP_PNG) copy the
                // presented image back to a host buffer; universally supported for
                // swapchain images and free when unused.
                image_usage: vk::ImageUsageFlags::COLOR_ATTACHMENT
                    | vk::ImageUsageFlags::TRANSFER_SRC,
                image_sharing_mode: vk::SharingMode::EXCLUSIVE,
                pre_transform: caps.current_transform,
                composite_alpha: vk::CompositeAlphaFlagsKHR::OPAQUE,
                present_mode: vk::PresentModeKHR::FIFO,
                ..Default::default()
            };

            let swapchain = swapchain_loader.create_swapchain(&swapchain_info, None)?;
            let swapchain_images = swapchain_loader.get_swapchain_images(swapchain)?;
            let swapchain_image_views: Vec<vk::ImageView> = swapchain_images
                .iter()
                .map(|&img| {
                    let info = vk::ImageViewCreateInfo {
                        s_type: vk::StructureType::IMAGE_VIEW_CREATE_INFO,
                        image: img,
                        view_type: vk::ImageViewType::TYPE_2D,
                        format: format.format,
                        subresource_range: vk::ImageSubresourceRange {
                            aspect_mask: vk::ImageAspectFlags::COLOR,
                            base_mip_level: 0,
                            level_count: 1,
                            base_array_layer: 0,
                            layer_count: 1,
                        },
                        ..Default::default()
                    };
                    device.create_image_view(&info, None).unwrap()
                })
                .collect();

            Ok(SwapchainBundle {
                loader: swapchain_loader,
                handle: swapchain,
                images: swapchain_images,
                image_views: swapchain_image_views,
                format: format.format,
                extent,
            })
        }
    }

    unsafe fn create_command_pool(
        device: &Device,
        queue_family_index: u32,
    ) -> VkResult<(vk::CommandPool, vk::CommandBuffer)> {
        unsafe {
            tracing::debug!("Creating command pool");
            let pool_info = vk::CommandPoolCreateInfo {
                s_type: vk::StructureType::COMMAND_POOL_CREATE_INFO,
                queue_family_index,
                flags: vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER,
                ..Default::default()
            };
            let command_pool = device.create_command_pool(&pool_info, None)?;

            let alloc_info = vk::CommandBufferAllocateInfo {
                s_type: vk::StructureType::COMMAND_BUFFER_ALLOCATE_INFO,
                command_pool,
                level: vk::CommandBufferLevel::PRIMARY,
                command_buffer_count: 1,
                ..Default::default()
            };
            let command_buffer = device.allocate_command_buffers(&alloc_info)?[0];
            Ok((command_pool, command_buffer))
        }
    }

    unsafe fn create_render_pass(device: &Device, format: vk::Format) -> VkResult<vk::RenderPass> {
        unsafe {
            tracing::debug!("Creating render pass");
            let attachments = [vk::AttachmentDescription {
                format,
                samples: vk::SampleCountFlags::TYPE_1,
                load_op: vk::AttachmentLoadOp::CLEAR,
                store_op: vk::AttachmentStoreOp::STORE,
                final_layout: vk::ImageLayout::PRESENT_SRC_KHR,
                initial_layout: vk::ImageLayout::UNDEFINED,
                stencil_load_op: vk::AttachmentLoadOp::DONT_CARE,
                stencil_store_op: vk::AttachmentStoreOp::DONT_CARE,
                flags: vk::AttachmentDescriptionFlags::empty(),
            }];
            let color_ref = [vk::AttachmentReference {
                attachment: 0,
                layout: vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
            }];
            let subpass = [vk::SubpassDescription {
                flags: vk::SubpassDescriptionFlags::empty(),
                pipeline_bind_point: vk::PipelineBindPoint::GRAPHICS,
                color_attachment_count: 1,
                p_color_attachments: color_ref.as_ptr(),
                input_attachment_count: 0,
                p_input_attachments: std::ptr::null(),
                p_resolve_attachments: std::ptr::null(),
                p_depth_stencil_attachment: std::ptr::null(),
                preserve_attachment_count: 0,
                p_preserve_attachments: std::ptr::null(),
                ..Default::default()
            }];
            let render_pass_info = vk::RenderPassCreateInfo {
                s_type: vk::StructureType::RENDER_PASS_CREATE_INFO,
                attachment_count: 1,
                p_attachments: attachments.as_ptr(),
                subpass_count: 1,
                p_subpasses: subpass.as_ptr(),
                dependency_count: 0,
                p_dependencies: std::ptr::null(),
                ..Default::default()
            };
            Ok(device.create_render_pass(&render_pass_info, None)?)
        }
    }

    unsafe fn create_pipeline(
        device: &Device,
        render_pass: vk::RenderPass,
    ) -> VkResult<PipelineBundle> {
        unsafe {
            tracing::debug!("Loading shaders");
            let vert_code = include_bytes!("../shaders/vert.spv");
            let frag_code = include_bytes!("../shaders/frag.spv");
            let vert_module = Self::create_shader_module(device, vert_code);
            let frag_module = Self::create_shader_module(device, frag_code);

            let main_name = c"main";
            let stages = [
                vk::PipelineShaderStageCreateInfo {
                    s_type: vk::StructureType::PIPELINE_SHADER_STAGE_CREATE_INFO,
                    stage: vk::ShaderStageFlags::VERTEX,
                    module: vert_module,
                    p_name: main_name.as_ptr(),
                    ..Default::default()
                },
                vk::PipelineShaderStageCreateInfo {
                    s_type: vk::StructureType::PIPELINE_SHADER_STAGE_CREATE_INFO,
                    stage: vk::ShaderStageFlags::FRAGMENT,
                    module: frag_module,
                    p_name: main_name.as_ptr(),
                    ..Default::default()
                },
            ];

            tracing::debug!("Creating pipeline");
            let vertex_input = vk::PipelineVertexInputStateCreateInfo {
                s_type: vk::StructureType::PIPELINE_VERTEX_INPUT_STATE_CREATE_INFO,
                ..Default::default()
            };
            let input_assembly = vk::PipelineInputAssemblyStateCreateInfo {
                s_type: vk::StructureType::PIPELINE_INPUT_ASSEMBLY_STATE_CREATE_INFO,
                topology: vk::PrimitiveTopology::TRIANGLE_LIST,
                ..Default::default()
            };
            let viewport_state = vk::PipelineViewportStateCreateInfo {
                s_type: vk::StructureType::PIPELINE_VIEWPORT_STATE_CREATE_INFO,
                viewport_count: 1,
                scissor_count: 1,
                ..Default::default()
            };
            let rasterizer = vk::PipelineRasterizationStateCreateInfo {
                s_type: vk::StructureType::PIPELINE_RASTERIZATION_STATE_CREATE_INFO,
                line_width: 1.0,
                cull_mode: vk::CullModeFlags::NONE,
                front_face: vk::FrontFace::CLOCKWISE,
                ..Default::default()
            };
            let multisampling = vk::PipelineMultisampleStateCreateInfo {
                s_type: vk::StructureType::PIPELINE_MULTISAMPLE_STATE_CREATE_INFO,
                rasterization_samples: vk::SampleCountFlags::TYPE_1,
                ..Default::default()
            };
            let color_blend_attachment = [vk::PipelineColorBlendAttachmentState {
                color_write_mask: vk::ColorComponentFlags::RGBA,
                blend_enable: 0,
                ..Default::default()
            }];
            let color_blending = vk::PipelineColorBlendStateCreateInfo {
                s_type: vk::StructureType::PIPELINE_COLOR_BLEND_STATE_CREATE_INFO,
                attachment_count: 1,
                p_attachments: color_blend_attachment.as_ptr(),
                ..Default::default()
            };
            let dynamic_states = [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR];
            let dynamic_state = vk::PipelineDynamicStateCreateInfo {
                s_type: vk::StructureType::PIPELINE_DYNAMIC_STATE_CREATE_INFO,
                dynamic_state_count: dynamic_states.len() as u32,
                p_dynamic_states: dynamic_states.as_ptr(),
                ..Default::default()
            };

            let bindings = [vk::DescriptorSetLayoutBinding {
                binding: 0,
                descriptor_type: vk::DescriptorType::COMBINED_IMAGE_SAMPLER,
                descriptor_count: 1,
                stage_flags: vk::ShaderStageFlags::FRAGMENT,
                ..Default::default()
            }];
            let layout_info = vk::DescriptorSetLayoutCreateInfo {
                s_type: vk::StructureType::DESCRIPTOR_SET_LAYOUT_CREATE_INFO,
                binding_count: 1,
                p_bindings: bindings.as_ptr(),
                ..Default::default()
            };
            let set_layout = device.create_descriptor_set_layout(&layout_info, None)?;

            let set_layouts = [set_layout];
            let pipeline_layout_info = vk::PipelineLayoutCreateInfo {
                s_type: vk::StructureType::PIPELINE_LAYOUT_CREATE_INFO,
                set_layout_count: 1,
                p_set_layouts: set_layouts.as_ptr(),
                ..Default::default()
            };
            let pipeline_layout = device.create_pipeline_layout(&pipeline_layout_info, None)?;

            let pipeline_info = vk::GraphicsPipelineCreateInfo {
                s_type: vk::StructureType::GRAPHICS_PIPELINE_CREATE_INFO,
                stage_count: 2,
                p_stages: stages.as_ptr(),
                p_vertex_input_state: &vertex_input,
                p_input_assembly_state: &input_assembly,
                p_viewport_state: &viewport_state,
                p_rasterization_state: &rasterizer,
                p_multisample_state: &multisampling,
                p_color_blend_state: &color_blending,
                p_dynamic_state: &dynamic_state,
                layout: pipeline_layout,
                render_pass,
                subpass: 0,
                ..Default::default()
            };
            let pipeline = device
                .create_graphics_pipelines(vk::PipelineCache::null(), &[pipeline_info], None)
                .unwrap()[0];

            device.destroy_shader_module(vert_module, None);
            device.destroy_shader_module(frag_module, None);

            Ok(PipelineBundle {
                descriptor_set_layout: set_layout,
                pipeline_layout,
                pipeline,
            })
        }
    }

    unsafe fn create_framebuffers(
        device: &Device,
        render_pass: vk::RenderPass,
        image_views: &[vk::ImageView],
        extent: vk::Extent2D,
    ) -> VkResult<Vec<vk::Framebuffer>> {
        unsafe {
            tracing::debug!("Creating framebuffers");
            let framebuffers = image_views
                .iter()
                .map(|&view| {
                    let attachments = [view];
                    let info = vk::FramebufferCreateInfo {
                        s_type: vk::StructureType::FRAMEBUFFER_CREATE_INFO,
                        render_pass,
                        attachment_count: 1,
                        p_attachments: attachments.as_ptr(),
                        width: extent.width,
                        height: extent.height,
                        layers: 1,
                        ..Default::default()
                    };
                    device.create_framebuffer(&info, None).unwrap()
                })
                .collect();
            Ok(framebuffers)
        }
    }

    unsafe fn create_sync(device: &Device) -> VkResult<(vk::Semaphore, vk::Semaphore, vk::Fence)> {
        unsafe {
            tracing::debug!("Creating sync objects");
            let sem_info = vk::SemaphoreCreateInfo {
                s_type: vk::StructureType::SEMAPHORE_CREATE_INFO,
                ..Default::default()
            };
            let fence_info = vk::FenceCreateInfo {
                s_type: vk::StructureType::FENCE_CREATE_INFO,
                flags: vk::FenceCreateFlags::SIGNALED,
                ..Default::default()
            };

            let image_available = device.create_semaphore(&sem_info, None)?;
            let render_finished = device.create_semaphore(&sem_info, None)?;
            let in_flight = device.create_fence(&fence_info, None)?;
            Ok((image_available, render_finished, in_flight))
        }
    }

    unsafe fn create_texture(
        instance: &Instance,
        device: &Device,
        pdevice: vk::PhysicalDevice,
        set_layout: vk::DescriptorSetLayout,
    ) -> VkResult<TextureBundle> {
        unsafe {
            tracing::debug!("Allocating resources");
            let buffer_size = 1920 * 1080 * 4;
            let (staging_buf, staging_mem) = Self::create_buffer(
                instance,
                device,
                pdevice,
                buffer_size,
                vk::BufferUsageFlags::TRANSFER_SRC,
                vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
            )?;
            let staging_ptr =
                device.map_memory(staging_mem, 0, buffer_size, vk::MemoryMapFlags::empty())?
                    as *mut u8;

            // COLOR_ATTACHMENT is added so the phase-3.5 embedded draw can
            // render directly into this videoout image before the present path blits
            // it to the swapchain; TRANSFER_DST/SAMPLED remain for the softgpu path.
            let (tex_img, tex_mem) = Self::create_image(
                instance,
                device,
                pdevice,
                1920,
                1080,
                vk::Format::R8G8B8A8_UNORM,
                vk::ImageUsageFlags::TRANSFER_DST
                    | vk::ImageUsageFlags::SAMPLED
                    | vk::ImageUsageFlags::COLOR_ATTACHMENT,
            )?;
            let view_info = vk::ImageViewCreateInfo {
                s_type: vk::StructureType::IMAGE_VIEW_CREATE_INFO,
                image: tex_img,
                view_type: vk::ImageViewType::TYPE_2D,
                format: vk::Format::R8G8B8A8_UNORM,
                subresource_range: vk::ImageSubresourceRange {
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    base_mip_level: 0,
                    level_count: 1,
                    base_array_layer: 0,
                    layer_count: 1,
                },
                ..Default::default()
            };
            let tex_view = device.create_image_view(&view_info, None)?;

            let sampler_info = vk::SamplerCreateInfo {
                s_type: vk::StructureType::SAMPLER_CREATE_INFO,
                mag_filter: vk::Filter::NEAREST,
                min_filter: vk::Filter::NEAREST,
                ..Default::default()
            };
            let sampler = device.create_sampler(&sampler_info, None)?;

            // descriptor set: pool, set, write texture
            let pool_sizes = [vk::DescriptorPoolSize {
                ty: vk::DescriptorType::COMBINED_IMAGE_SAMPLER,
                descriptor_count: 1,
            }];
            let pool_info = vk::DescriptorPoolCreateInfo {
                s_type: vk::StructureType::DESCRIPTOR_POOL_CREATE_INFO,
                pool_size_count: 1,
                p_pool_sizes: pool_sizes.as_ptr(),
                max_sets: 1,
                ..Default::default()
            };
            let descriptor_pool = device.create_descriptor_pool(&pool_info, None)?;

            let alloc_info = vk::DescriptorSetAllocateInfo {
                s_type: vk::StructureType::DESCRIPTOR_SET_ALLOCATE_INFO,
                descriptor_pool,
                descriptor_set_count: 1,
                p_set_layouts: &set_layout,
                ..Default::default()
            };
            let descriptor_set = device.allocate_descriptor_sets(&alloc_info)?[0];

            let image_info = [vk::DescriptorImageInfo {
                sampler,
                image_view: tex_view,
                image_layout: vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
            }];
            let write_sets = [vk::WriteDescriptorSet {
                s_type: vk::StructureType::WRITE_DESCRIPTOR_SET,
                dst_set: descriptor_set,
                dst_binding: 0,
                dst_array_element: 0,
                descriptor_count: 1,
                descriptor_type: vk::DescriptorType::COMBINED_IMAGE_SAMPLER,
                p_image_info: image_info.as_ptr(),
                ..Default::default()
            }];
            device.update_descriptor_sets(&write_sets, &[]);

            Ok(TextureBundle {
                staging_buffer: staging_buf,
                staging_mem,
                staging_ptr,
                image: tex_img,
                mem: tex_mem,
                view: tex_view,
                sampler,
                descriptor_pool,
                descriptor_set,
            })
        }
    }

    unsafe fn create_shader_module(device: &Device, code: &[u8]) -> vk::ShaderModule {
        unsafe {
            let mut cursor = Cursor::new(code);
            let code_u32 = ash::util::read_spv(&mut cursor).expect("Failed to read SPIR-V");

            let info = vk::ShaderModuleCreateInfo {
                s_type: vk::StructureType::SHADER_MODULE_CREATE_INFO,
                code_size: code_u32.len() * 4,
                p_code: code_u32.as_ptr(),
                ..Default::default()
            };
            device.create_shader_module(&info, None).unwrap()
        }
    }

    pub(crate) unsafe fn create_buffer(
        instance: &Instance,
        device: &Device,
        pdevice: vk::PhysicalDevice,
        size: u64,
        usage: vk::BufferUsageFlags,
        props: vk::MemoryPropertyFlags,
    ) -> Result<(vk::Buffer, vk::DeviceMemory), vk::Result> {
        unsafe {
            let info = vk::BufferCreateInfo {
                s_type: vk::StructureType::BUFFER_CREATE_INFO,
                size,
                usage,
                sharing_mode: vk::SharingMode::EXCLUSIVE,
                ..Default::default()
            };
            let buffer = device.create_buffer(&info, None)?;
            let reqs = device.get_buffer_memory_requirements(buffer);
            let mem_props = instance.get_physical_device_memory_properties(pdevice);
            let type_index = (0..mem_props.memory_type_count)
                .find(|&i| {
                    (reqs.memory_type_bits & (1 << i)) != 0
                        && (mem_props.memory_types[i as usize].property_flags & props) == props
                })
                .unwrap();

            let alloc_info = vk::MemoryAllocateInfo {
                s_type: vk::StructureType::MEMORY_ALLOCATE_INFO,
                allocation_size: reqs.size,
                memory_type_index: type_index,
                ..Default::default()
            };
            let mem = device.allocate_memory(&alloc_info, None)?;
            device.bind_buffer_memory(buffer, mem, 0)?;
            Ok((buffer, mem))
        }
    }

    unsafe fn create_image(
        instance: &Instance,
        device: &Device,
        pdevice: vk::PhysicalDevice,
        w: u32,
        h: u32,
        format: vk::Format,
        usage: vk::ImageUsageFlags,
    ) -> Result<(vk::Image, vk::DeviceMemory), vk::Result> {
        unsafe {
            let info = vk::ImageCreateInfo {
                s_type: vk::StructureType::IMAGE_CREATE_INFO,
                image_type: vk::ImageType::TYPE_2D,
                extent: vk::Extent3D {
                    width: w,
                    height: h,
                    depth: 1,
                },
                mip_levels: 1,
                array_layers: 1,
                format,
                tiling: vk::ImageTiling::OPTIMAL,
                initial_layout: vk::ImageLayout::UNDEFINED,
                usage,
                samples: vk::SampleCountFlags::TYPE_1,
                ..Default::default()
            };
            let image = device.create_image(&info, None)?;
            let reqs = device.get_image_memory_requirements(image);
            let mem_props = instance.get_physical_device_memory_properties(pdevice);
            let type_index = (0..mem_props.memory_type_count)
                .find(|&i| {
                    (reqs.memory_type_bits & (1 << i)) != 0
                        && (mem_props.memory_types[i as usize].property_flags
                            & vk::MemoryPropertyFlags::DEVICE_LOCAL)
                            == vk::MemoryPropertyFlags::DEVICE_LOCAL
                })
                .unwrap();

            let alloc_info = vk::MemoryAllocateInfo {
                s_type: vk::StructureType::MEMORY_ALLOCATE_INFO,
                allocation_size: reqs.size,
                memory_type_index: type_index,
                ..Default::default()
            };
            let mem = device.allocate_memory(&alloc_info, None)?;
            device.bind_image_memory(image, mem, 0)?;
            Ok((image, mem))
        }
    }

    /// Attempts to import `size` bytes of host memory at `host_ptr` as a
    /// TRANSFER_SRC `VkBuffer` via VK_EXT_external_memory_host, so the GPU can copy
    /// the guest framebuffer to the display texture with no CPU-side memcpy.
    ///
    /// Returns `None` (caller falls back to the staging copy) when the extension
    /// is disabled, when `host_ptr` is not aligned to
    /// `min_import_alignment`, or when any Vulkan call fails. On success the
    /// returned buffer/memory reference the *guest's own pages*: the caller must
    /// not let the guest overwrite them while a GPU transfer reading the buffer is
    /// in flight (handled in the flip path by deferring the guest vsync signal
    /// until after submit on the zero-copy path).
    ///
    /// # Safety
    /// `host_ptr` must point to at least `size` bytes of memory that stays mapped
    /// and valid for the lifetime of the returned import (it is the identity-mapped
    /// guest framebuffer, which lives for the whole run). The import must be
    /// destroyed before that memory is unmapped/freed.
    pub unsafe fn try_import_host_buffer(
        &self,
        host_ptr: *const u8,
        size: u64,
    ) -> Option<ImportedBuf> {
        let ext = self.ext_mem_host.as_ref()?;
        let align = self.min_import_alignment;
        if align == 0 || !(host_ptr as usize as u64).is_multiple_of(align) {
            // Guest framebuffers are malloc'd and frequently not page-aligned;
            // falling back for this buffer is expected, not an error.
            tracing::debug!(
                "Zero-copy import skipped: host ptr {:p} not aligned to {}",
                host_ptr,
                align
            );
            return None;
        }
        // Round the import size up to the required alignment.
        let import_size = size.div_ceil(align) * align;
        let handle_type = vk::ExternalMemoryHandleTypeFlags::HOST_ALLOCATION_EXT;

        unsafe {
            // Query which memory types can back this host pointer.
            // SAFETY: `ext` was built from this device; `host_ptr` is valid; the
            // properties struct is default-initialized with its s_type set.
            let mut props = vk::MemoryHostPointerPropertiesEXT::default();
            let res = (ext.fp().get_memory_host_pointer_properties_ext)(
                self.device.handle(),
                handle_type,
                host_ptr as *const std::ffi::c_void,
                &mut props,
            );
            if res != vk::Result::SUCCESS {
                tracing::debug!("get_memory_host_pointer_properties failed: {:?}", res);
                return None;
            }
            if props.memory_type_bits == 0 {
                return None;
            }

            // Create a buffer flagged for external host-allocation import.
            let mut ext_buf_info =
                vk::ExternalMemoryBufferCreateInfo::default().handle_types(handle_type);
            let buffer_info = vk::BufferCreateInfo::default()
                .size(import_size)
                .usage(vk::BufferUsageFlags::TRANSFER_SRC)
                .sharing_mode(vk::SharingMode::EXCLUSIVE)
                .push_next(&mut ext_buf_info);
            let buffer = match self.device.create_buffer(&buffer_info, None) {
                Ok(b) => b,
                Err(e) => {
                    tracing::debug!("zero-copy create_buffer failed: {:?}", e);
                    return None;
                }
            };
            let reqs = self.device.get_buffer_memory_requirements(buffer);

            // Pick a memory type satisfying both the buffer requirements and the
            // host-pointer properties. Guest framebuffer pages are host-cached, so
            // prefer a HOST_VISIBLE|HOST_COHERENT type: coherent import means guest
            // writes become visible to the GPU transfer without an explicit flush
            // (which is not possible on imported host memory) — avoids stale-frame
            // artifacts (visibility point). Fall back to any usable type if
            // no coherent one is offered (the driver still reports only types valid
            // for this host allocation).
            let usable_bits = props.memory_type_bits & reqs.memory_type_bits;
            let mem_props = self
                .instance
                .get_physical_device_memory_properties(self.physical_device);
            let want =
                vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT;
            let is_usable = |i: u32| (usable_bits & (1 << i)) != 0;
            let type_index = (0..mem_props.memory_type_count)
                .find(|&i| {
                    is_usable(i)
                        && mem_props.memory_types[i as usize]
                            .property_flags
                            .contains(want)
                })
                .or_else(|| (0..mem_props.memory_type_count).find(|&i| is_usable(i)));
            let Some(type_index) = type_index else {
                self.device.destroy_buffer(buffer, None);
                return None;
            };

            // Import the host pages as device memory. `import_size` must be >=
            // reqs.size; take the max to stay safe.
            let alloc_size = import_size.max(reqs.size);
            let mut import_info = vk::ImportMemoryHostPointerInfoEXT::default()
                .handle_type(handle_type)
                .host_pointer(host_ptr as *mut std::ffi::c_void);
            let alloc_info = vk::MemoryAllocateInfo::default()
                .allocation_size(alloc_size)
                .memory_type_index(type_index)
                .push_next(&mut import_info);
            let memory = match self.device.allocate_memory(&alloc_info, None) {
                Ok(m) => m,
                Err(e) => {
                    tracing::debug!("zero-copy import allocate_memory failed: {:?}", e);
                    self.device.destroy_buffer(buffer, None);
                    return None;
                }
            };
            if let Err(e) = self.device.bind_buffer_memory(buffer, memory, 0) {
                tracing::debug!("zero-copy bind_buffer_memory failed: {:?}", e);
                self.device.free_memory(memory, None);
                self.device.destroy_buffer(buffer, None);
                return None;
            }

            tracing::info!(
                "Zero-copy: imported guest framebuffer at {:p} ({} bytes) as VkBuffer",
                host_ptr,
                alloc_size
            );
            Some(ImportedBuf {
                buffer,
                memory,
                host_ptr,
            })
        }
    }

    /// Destroys an imported buffer's Vulkan resources. Called when a buffer key is
    /// re-registered at a different host pointer, so stale imports don't
    /// accumulate. The general leak-on-exit convention still applies to imports
    /// live at process exit.
    ///
    /// # Safety
    /// No GPU work referencing `imported.buffer` may be in flight.
    pub unsafe fn destroy_imported_buffer(&self, imported: &ImportedBuf) {
        unsafe {
            self.device.destroy_buffer(imported.buffer, None);
            self.device.free_memory(imported.memory, None);
        }
    }

    /// Allocate a host-visible, host-coherent linear buffer of `size` bytes for the
    /// resource cache's copy path (doc-4 §8.2). Usage covers the linear buffer kinds
    /// the phase-3.5 cache handles (vertex / index / uniform). Host-visible + coherent
    /// so [`Self::upload_cache_buffer`] can memcpy straight in without a staging hop;
    /// the corpus is tiny, so a device-local + staging split is deferred (doc-4 §8.6).
    /// Leak-on-exit like the rest of the Vulkan state.
    ///
    /// # Safety
    /// `self.device`/`instance`/`physical_device` must be live and owned by the caller.
    pub unsafe fn create_cache_buffer(&self, size: u64) -> (vk::Buffer, vk::DeviceMemory) {
        unsafe {
            let usage = vk::BufferUsageFlags::VERTEX_BUFFER
                | vk::BufferUsageFlags::INDEX_BUFFER
                | vk::BufferUsageFlags::UNIFORM_BUFFER
                | vk::BufferUsageFlags::STORAGE_BUFFER
                | vk::BufferUsageFlags::TRANSFER_DST;
            let props =
                vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT;
            Self::create_buffer(
                &self.instance,
                &self.device,
                self.physical_device,
                size.max(1),
                usage,
                props,
            )
            .expect("cache buffer allocation")
        }
    }

    /// Copy `bytes` into a cache buffer's memory at `offset`, via a transient map of
    /// the host-coherent allocation (doc-4 §8.2 copy path). Host-coherent, so no
    /// explicit flush is needed.
    ///
    /// # Safety
    /// `mem` must be a host-visible allocation from [`Self::create_cache_buffer`] with
    /// at least `offset + bytes.len()` bytes; `self.device` must be live.
    pub unsafe fn upload_cache_buffer(&self, mem: vk::DeviceMemory, offset: u64, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        unsafe {
            let ptr = self
                .device
                .map_memory(mem, offset, bytes.len() as u64, vk::MemoryMapFlags::empty())
                .expect("map cache buffer") as *mut u8;
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, bytes.len());
            self.device.unmap_memory(mem);
        }
    }

    /// Allocate a device-local sampled `w`×`h` image in `format` plus a 2D color view
    /// (doc-4 §C3/§C4). Usage covers `SAMPLED` (a pixel shader reads it through a combined
    /// image-sampler) and `TRANSFER_DST` (its pixels arrive via the staging copy in
    /// [`Self::upload_image`]). Portability subset: an uncompressed single-mip 2D image, no
    /// anisotropy/compression. Leak-on-exit like the rest of the Vulkan state.
    ///
    /// # Safety
    /// `self.device`/`instance`/`physical_device` must be live and owned by the caller.
    pub unsafe fn create_sampled_image(
        &self,
        w: u32,
        h: u32,
        format: vk::Format,
    ) -> (vk::Image, vk::ImageView, vk::DeviceMemory) {
        unsafe {
            let (image, mem) = Self::create_image(
                &self.instance,
                &self.device,
                self.physical_device,
                w.max(1),
                h.max(1),
                format,
                vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_DST,
            )
            .expect("sampled image allocation");
            let view_info = vk::ImageViewCreateInfo {
                s_type: vk::StructureType::IMAGE_VIEW_CREATE_INFO,
                image,
                view_type: vk::ImageViewType::TYPE_2D,
                format,
                subresource_range: vk::ImageSubresourceRange {
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    base_mip_level: 0,
                    level_count: 1,
                    base_array_layer: 0,
                    layer_count: 1,
                },
                ..Default::default()
            };
            let view = self
                .device
                .create_image_view(&view_info, None)
                .expect("sampled image view");
            (image, view, mem)
        }
    }

    /// Stage `pixels` (detiled linear RGBA, `w*h*4` bytes) into a sampled `image` and leave
    /// it `SHADER_READ_ONLY_OPTIMAL` for a combined image-sampler read (doc-4 §C3). A
    /// host-visible staging buffer holds the pixels; one command buffer transitions
    /// `UNDEFINED -> TRANSFER_DST_OPTIMAL`, copies the buffer into the image, then
    /// transitions `TRANSFER_DST_OPTIMAL -> SHADER_READ_ONLY_OPTIMAL`. Submitted with a
    /// transient fence + wait (uploads are rare, not a per-draw hot path). All core Vulkan
    /// 1.0, portable subset (decision-3).
    ///
    /// # Safety
    /// `image` must be a sampled image from [`Self::create_sampled_image`] of at least
    /// `w`×`h`; `self.device` must be live and owned by the caller.
    pub unsafe fn upload_image(&self, image: vk::Image, w: u32, h: u32, pixels: &[u8]) {
        let size = (w as u64) * (h as u64) * 4;
        if size == 0 {
            return;
        }
        unsafe {
            let (staging, staging_mem) = Self::create_buffer(
                &self.instance,
                &self.device,
                self.physical_device,
                size,
                vk::BufferUsageFlags::TRANSFER_SRC,
                vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
            )
            .expect("image staging buffer");
            let n = (pixels.len() as u64).min(size);
            let ptr = self
                .device
                .map_memory(staging_mem, 0, size, vk::MemoryMapFlags::empty())
                .expect("map image staging") as *mut u8;
            std::ptr::copy_nonoverlapping(pixels.as_ptr(), ptr, n as usize);
            self.device.unmap_memory(staging_mem);

            let alloc = vk::CommandBufferAllocateInfo {
                s_type: vk::StructureType::COMMAND_BUFFER_ALLOCATE_INFO,
                command_pool: self.command_pool,
                level: vk::CommandBufferLevel::PRIMARY,
                command_buffer_count: 1,
                ..Default::default()
            };
            let cb = self.device.allocate_command_buffers(&alloc).unwrap()[0];
            let begin = vk::CommandBufferBeginInfo {
                s_type: vk::StructureType::COMMAND_BUFFER_BEGIN_INFO,
                flags: vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT,
                ..Default::default()
            };
            self.device.begin_command_buffer(cb, &begin).unwrap();

            let sub = vk::ImageSubresourceRange {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                base_mip_level: 0,
                level_count: 1,
                base_array_layer: 0,
                layer_count: 1,
            };
            let to_dst = vk::ImageMemoryBarrier {
                s_type: vk::StructureType::IMAGE_MEMORY_BARRIER,
                old_layout: vk::ImageLayout::UNDEFINED,
                new_layout: vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                src_queue_family_index: vk::QUEUE_FAMILY_IGNORED,
                dst_queue_family_index: vk::QUEUE_FAMILY_IGNORED,
                image,
                subresource_range: sub,
                src_access_mask: vk::AccessFlags::empty(),
                dst_access_mask: vk::AccessFlags::TRANSFER_WRITE,
                ..Default::default()
            };
            self.device.cmd_pipeline_barrier(
                cb,
                vk::PipelineStageFlags::TOP_OF_PIPE,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[to_dst],
            );

            let region = vk::BufferImageCopy {
                image_subresource: vk::ImageSubresourceLayers {
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    mip_level: 0,
                    base_array_layer: 0,
                    layer_count: 1,
                },
                image_extent: vk::Extent3D {
                    width: w,
                    height: h,
                    depth: 1,
                },
                ..Default::default()
            };
            self.device.cmd_copy_buffer_to_image(
                cb,
                staging,
                image,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &[region],
            );

            let to_shader = vk::ImageMemoryBarrier {
                s_type: vk::StructureType::IMAGE_MEMORY_BARRIER,
                old_layout: vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                new_layout: vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                src_queue_family_index: vk::QUEUE_FAMILY_IGNORED,
                dst_queue_family_index: vk::QUEUE_FAMILY_IGNORED,
                image,
                subresource_range: sub,
                src_access_mask: vk::AccessFlags::TRANSFER_WRITE,
                dst_access_mask: vk::AccessFlags::SHADER_READ,
                ..Default::default()
            };
            self.device.cmd_pipeline_barrier(
                cb,
                vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::FRAGMENT_SHADER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[to_shader],
            );

            self.device.end_command_buffer(cb).unwrap();
            let fence = self
                .device
                .create_fence(&vk::FenceCreateInfo::default(), None)
                .unwrap();
            let cbs = [cb];
            let submit = vk::SubmitInfo {
                s_type: vk::StructureType::SUBMIT_INFO,
                command_buffer_count: 1,
                p_command_buffers: cbs.as_ptr(),
                ..Default::default()
            };
            self.device
                .queue_submit(self.queue, &[submit], fence)
                .unwrap();
            self.device
                .wait_for_fences(&[fence], true, u64::MAX)
                .unwrap();
            self.device.destroy_fence(fence, None);
            self.device.free_command_buffers(self.command_pool, &cbs);
            self.device.destroy_buffer(staging, None);
            self.device.free_memory(staging_mem, None);
        }
    }

    /// Create a `vk::Sampler` from the portable filter/address parameters (doc-4 §C4).
    /// Anisotropy and mip-based LOD are off (portability subset, decision-3), so no device
    /// feature is required. Leak-on-exit.
    ///
    /// # Safety
    /// `self.device` must be live and owned by the caller.
    pub unsafe fn create_sampler(
        &self,
        mag: vk::Filter,
        min: vk::Filter,
        address: vk::SamplerAddressMode,
    ) -> vk::Sampler {
        unsafe {
            let info = vk::SamplerCreateInfo {
                s_type: vk::StructureType::SAMPLER_CREATE_INFO,
                mag_filter: mag,
                min_filter: min,
                mipmap_mode: vk::SamplerMipmapMode::NEAREST,
                address_mode_u: address,
                address_mode_v: address,
                address_mode_w: address,
                anisotropy_enable: vk::FALSE,
                max_anisotropy: 1.0,
                min_lod: 0.0,
                max_lod: 0.0,
                border_color: vk::BorderColor::INT_OPAQUE_BLACK,
                ..Default::default()
            };
            self.device.create_sampler(&info, None).expect("sampler")
        }
    }
}
