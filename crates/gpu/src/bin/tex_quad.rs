//! Headless textured-quad harness for the sampled-texture backend path (doc-2 §C3/§C4).
//!
//! Renders a hardcoded RGBA8 checkerboard on a fullscreen quad through the exact
//! sampled-image sequence `AshBackend` uses — create a device-local `SAMPLED |
//! TRANSFER_DST` image, stage the detiled-linear pixels into it with the
//! `UNDEFINED -> TRANSFER_DST_OPTIMAL -> SHADER_READ_ONLY_OPTIMAL` transitions, create a
//! portable sampler (linear filter, repeat, no anisotropy/mips), declare a
//! `COMBINED_IMAGE_SAMPLER` binding on the pipeline's set-0 layout, and write the image
//! view + sampler into an allocated descriptor set before the draw. It then reads the
//! offscreen color target back and writes it as an RGBA PNG.
//!
//! `AshBackend` binds a swapchain/surface (a window), so a purely headless harness cannot
//! drive it directly; this self-contained offscreen renderer instead executes the same
//! image/sampler/descriptor verbs against a windowless instance/device (the pattern the
//! differential harness uses) so the checkerboard render can be judged headlessly (AC#2).
//!
//! Run:
//!   UNEMUPS4_TEX_PNG=/tmp/tex51.png cargo run -p ps4-gpu --bin tex_quad --release
//! (defaults to `tex_quad.png` in the cwd when the env var is unset.)

use std::ffi::CString;
use std::process::ExitCode;

use ash::vk;
use ps4_gpu::backend::write_rgba_png;
use rspirv::binary::Assemble;
use rspirv::dr::{Builder, Operand as DrOperand};
use rspirv::spirv;

/// Checkerboard side in texels and the size of each check square (in texels).
const TEX_DIM: u32 = 64;
const CHECK: u32 = 8;
/// Offscreen color-target side in pixels.
const RT_DIM: u32 = 256;
/// RGBA8 offscreen target format — matches the videoout target the real path renders into.
const RT_FORMAT: vk::Format = vk::Format::R8G8B8A8_UNORM;

fn main() -> ExitCode {
    match run() {
        Ok(path) => {
            println!("tex_quad: wrote checkerboard render to {path}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("tex_quad: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Build the hardcoded RGBA8 checkerboard: two colours in `CHECK`×`CHECK` squares. The
/// detiled-linear byte order the real upload path receives (row-major RGBA).
fn checkerboard() -> Vec<u8> {
    let mut px = Vec::with_capacity((TEX_DIM * TEX_DIM * 4) as usize);
    for y in 0..TEX_DIM {
        for x in 0..TEX_DIM {
            let even = ((x / CHECK) + (y / CHECK)).is_multiple_of(2);
            if even {
                px.extend_from_slice(&[230, 40, 40, 255]); // red-ish
            } else {
                px.extend_from_slice(&[40, 60, 230, 255]); // blue-ish
            }
        }
    }
    px
}

fn run() -> Result<String, String> {
    let out = std::env::var("UNEMUPS4_TEX_PNG").unwrap_or_else(|_| "tex_quad.png".to_string());
    let h = Harness::new()?;
    let pixels = unsafe { h.render_checkerboard()? };
    write_rgba_png(std::path::Path::new(&out), RT_DIM, RT_DIM, &pixels)
        .map_err(|e| format!("write png: {e}"))?;
    // Sanity: the render must not be a single flat colour (would mean the sample failed).
    let first = &pixels[0..4];
    let all_same = pixels.chunks_exact(4).all(|c| c == first);
    if all_same {
        return Err("rendered image is a single flat colour — texture sample failed".into());
    }
    Ok(out)
}

/// A windowless Vulkan instance/device + a graphics queue and command pool — the same
/// self-contained pattern the differential harness uses, so the sampled-texture verbs run
/// with no surface/swapchain.
struct Harness {
    _entry: ash::Entry,
    _instance: ash::Instance,
    device: ash::Device,
    queue: vk::Queue,
    command_pool: vk::CommandPool,
    mem_props: vk::PhysicalDeviceMemoryProperties,
}

impl Harness {
    fn new() -> Result<Self, String> {
        unsafe {
            let entry = ash::Entry::load().map_err(|e| format!("load Vulkan: {e}"))?;
            let app_name = CString::new("unemups4-tex-quad").unwrap();
            let app_info = vk::ApplicationInfo::default()
                .application_name(&app_name)
                .api_version(vk::API_VERSION_1_1);
            let ci = vk::InstanceCreateInfo::default().application_info(&app_info);
            let instance = entry
                .create_instance(&ci, None)
                .map_err(|e| format!("create instance: {e}"))?;

            let pdevices = instance
                .enumerate_physical_devices()
                .map_err(|e| format!("enumerate devices: {e}"))?;
            // Pick the first physical device that actually has a graphics queue family, not
            // just device 0 — on a machine whose primary device is compute-only (an offload/
            // headless GPU) with a graphics-capable device enumerated second, fixing on
            // device 0 would fail even though a usable device exists.
            let (pdevice, queue_family) = pdevices
                .iter()
                .find_map(|&pd| {
                    instance
                        .get_physical_device_queue_family_properties(pd)
                        .iter()
                        .position(|q| q.queue_flags.contains(vk::QueueFlags::GRAPHICS))
                        .map(|qf| (pd, qf as u32))
                })
                .ok_or("no Vulkan device with a graphics queue family")?;
            let mem_props = instance.get_physical_device_memory_properties(pdevice);

            let prio = [1.0f32];
            let qci = [vk::DeviceQueueCreateInfo::default()
                .queue_family_index(queue_family)
                .queue_priorities(&prio)];
            let dci = vk::DeviceCreateInfo::default().queue_create_infos(&qci);
            let device = instance
                .create_device(pdevice, &dci, None)
                .map_err(|e| format!("create device: {e}"))?;
            let queue = device.get_device_queue(queue_family, 0);

            let pool_ci = vk::CommandPoolCreateInfo::default()
                .queue_family_index(queue_family)
                .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER);
            let command_pool = device
                .create_command_pool(&pool_ci, None)
                .map_err(|e| format!("create command pool: {e}"))?;

            Ok(Harness {
                _entry: entry,
                _instance: instance,
                device,
                queue,
                command_pool,
                mem_props,
            })
        }
    }

    fn find_mem_type(&self, bits: u32, props: vk::MemoryPropertyFlags) -> Option<u32> {
        (0..self.mem_props.memory_type_count).find(|&i| {
            (bits & (1 << i)) != 0
                && self.mem_props.memory_types[i as usize]
                    .property_flags
                    .contains(props)
        })
    }

    unsafe fn shader_module(&self, spirv: &[u32]) -> Result<vk::ShaderModule, String> {
        let ci = vk::ShaderModuleCreateInfo::default().code(spirv);
        unsafe { self.device.create_shader_module(&ci, None) }
            .map_err(|e| format!("shader module: {e}"))
    }

    /// The whole textured draw: build the sampled image + sampler + combined image-sampler
    /// pipeline, upload the checkerboard, render a fullscreen quad sampling it, read back.
    unsafe fn render_checkerboard(&self) -> Result<Vec<u8>, String> {
        unsafe {
            // ---- sampled image (create + upload, mirrors the AshBackend sequence) -------
            let (image, view, image_mem) =
                self.create_sampled_image(TEX_DIM, TEX_DIM, RT_FORMAT)?;
            self.upload_image(image, TEX_DIM, TEX_DIM, &checkerboard())?;

            // ---- sampler (fixed portable defaults) --------------------------------------
            let sampler = self.create_sampler()?;

            // ---- offscreen color target + render pass -----------------------------------
            let (target, target_view, target_mem) = self.create_color_target(RT_DIM, RT_DIM)?;
            let render_pass = self.create_render_pass()?;
            let fb = self.framebuffer(render_pass, target_view, RT_DIM, RT_DIM)?;

            // ---- pipeline with a COMBINED_IMAGE_SAMPLER at set 0, binding 0 -------------
            let vs = self.shader_module(&build_textured_vs())?;
            let fs = self.shader_module(&build_textured_fs())?;
            let dsl = self.combined_sampler_layout(0)?;
            let layout = self.pipeline_layout(dsl)?;
            let pipeline = self.graphics_pipeline(render_pass, layout, vs, fs)?;

            // ---- descriptor set: write the image view + sampler at binding 0 ------------
            let (dpool, dset) = self.alloc_combined_set(dsl, view, sampler)?;

            // ---- record + submit the draw -----------------------------------------------
            self.submit_sync(|cb| {
                let clear = [vk::ClearValue {
                    color: vk::ClearColorValue {
                        float32: [0.0, 0.0, 0.0, 1.0],
                    },
                }];
                let rp = vk::RenderPassBeginInfo::default()
                    .render_pass(render_pass)
                    .framebuffer(fb)
                    .render_area(vk::Rect2D {
                        offset: vk::Offset2D { x: 0, y: 0 },
                        extent: vk::Extent2D {
                            width: RT_DIM,
                            height: RT_DIM,
                        },
                    })
                    .clear_values(&clear);
                self.device
                    .cmd_begin_render_pass(cb, &rp, vk::SubpassContents::INLINE);
                self.device
                    .cmd_bind_pipeline(cb, vk::PipelineBindPoint::GRAPHICS, pipeline);
                let vp = [vk::Viewport {
                    x: 0.0,
                    y: 0.0,
                    width: RT_DIM as f32,
                    height: RT_DIM as f32,
                    min_depth: 0.0,
                    max_depth: 1.0,
                }];
                let sc = [vk::Rect2D {
                    offset: vk::Offset2D { x: 0, y: 0 },
                    extent: vk::Extent2D {
                        width: RT_DIM,
                        height: RT_DIM,
                    },
                }];
                self.device.cmd_set_viewport(cb, 0, &vp);
                self.device.cmd_set_scissor(cb, 0, &sc);
                let sets = [dset];
                self.device.cmd_bind_descriptor_sets(
                    cb,
                    vk::PipelineBindPoint::GRAPHICS,
                    layout,
                    0,
                    &sets,
                    &[],
                );
                // Fullscreen triangle (3 verts, gl_VertexIndex-driven).
                self.device.cmd_draw(cb, 3, 1, 0, 0);
                self.device.cmd_end_render_pass(cb);
            })?;

            // ---- read the color target back to RGBA8 ------------------------------------
            let pixels = self.readback_rgba8(target, RT_DIM, RT_DIM)?;

            // Teardown (this harness runs once and exits; still be tidy).
            self.device.destroy_descriptor_pool(dpool, None);
            self.device.destroy_pipeline(pipeline, None);
            self.device.destroy_pipeline_layout(layout, None);
            self.device.destroy_descriptor_set_layout(dsl, None);
            self.device.destroy_shader_module(vs, None);
            self.device.destroy_shader_module(fs, None);
            self.device.destroy_framebuffer(fb, None);
            self.device.destroy_render_pass(render_pass, None);
            self.device.destroy_image_view(target_view, None);
            self.device.destroy_image(target, None);
            self.device.free_memory(target_mem, None);
            self.device.destroy_sampler(sampler, None);
            self.device.destroy_image_view(view, None);
            self.device.destroy_image(image, None);
            self.device.free_memory(image_mem, None);

            Ok(pixels)
        }
    }

    unsafe fn create_sampled_image(
        &self,
        w: u32,
        h: u32,
        format: vk::Format,
    ) -> Result<(vk::Image, vk::ImageView, vk::DeviceMemory), String> {
        unsafe {
            let ci = vk::ImageCreateInfo::default()
                .image_type(vk::ImageType::TYPE_2D)
                .format(format)
                .extent(vk::Extent3D {
                    width: w,
                    height: h,
                    depth: 1,
                })
                .mip_levels(1)
                .array_layers(1)
                .samples(vk::SampleCountFlags::TYPE_1)
                .tiling(vk::ImageTiling::OPTIMAL)
                .usage(vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_DST)
                .initial_layout(vk::ImageLayout::UNDEFINED);
            let image = self
                .device
                .create_image(&ci, None)
                .map_err(|e| format!("create image: {e}"))?;
            let req = self.device.get_image_memory_requirements(image);
            let mt = self
                .find_mem_type(req.memory_type_bits, vk::MemoryPropertyFlags::DEVICE_LOCAL)
                .ok_or("no device-local memory")?;
            let alloc = vk::MemoryAllocateInfo::default()
                .allocation_size(req.size)
                .memory_type_index(mt);
            let mem = self
                .device
                .allocate_memory(&alloc, None)
                .map_err(|e| format!("alloc image mem: {e}"))?;
            self.device
                .bind_image_memory(image, mem, 0)
                .map_err(|e| format!("bind image: {e}"))?;
            let view_ci = vk::ImageViewCreateInfo::default()
                .image(image)
                .view_type(vk::ImageViewType::TYPE_2D)
                .format(format)
                .subresource_range(color_range());
            let view = self
                .device
                .create_image_view(&view_ci, None)
                .map_err(|e| format!("create image view: {e}"))?;
            Ok((image, view, mem))
        }
    }

    /// Stage `pixels` into `image` and transition it to `SHADER_READ_ONLY_OPTIMAL` — the
    /// same UNDEFINED -> TRANSFER_DST -> SHADER_READ transition chain the backend records.
    unsafe fn upload_image(
        &self,
        image: vk::Image,
        w: u32,
        h: u32,
        pixels: &[u8],
    ) -> Result<(), String> {
        unsafe {
            let size = (w as u64) * (h as u64) * 4;
            let (staging, staging_mem) =
                self.host_buffer(size, vk::BufferUsageFlags::TRANSFER_SRC)?;
            let ptr = self
                .device
                .map_memory(staging_mem, 0, size, vk::MemoryMapFlags::empty())
                .map_err(|e| format!("map staging: {e}"))? as *mut u8;
            std::ptr::copy_nonoverlapping(pixels.as_ptr(), ptr, pixels.len().min(size as usize));
            self.device.unmap_memory(staging_mem);

            self.submit_sync(|cb| {
                let to_dst = vk::ImageMemoryBarrier::default()
                    .old_layout(vk::ImageLayout::UNDEFINED)
                    .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                    .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                    .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                    .image(image)
                    .subresource_range(color_range())
                    .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE);
                self.device.cmd_pipeline_barrier(
                    cb,
                    vk::PipelineStageFlags::TOP_OF_PIPE,
                    vk::PipelineStageFlags::TRANSFER,
                    vk::DependencyFlags::empty(),
                    &[],
                    &[],
                    &[to_dst],
                );
                let region = vk::BufferImageCopy::default()
                    .image_subresource(vk::ImageSubresourceLayers {
                        aspect_mask: vk::ImageAspectFlags::COLOR,
                        mip_level: 0,
                        base_array_layer: 0,
                        layer_count: 1,
                    })
                    .image_extent(vk::Extent3D {
                        width: w,
                        height: h,
                        depth: 1,
                    });
                self.device.cmd_copy_buffer_to_image(
                    cb,
                    staging,
                    image,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    &[region],
                );
                let to_shader = vk::ImageMemoryBarrier::default()
                    .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                    .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                    .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                    .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                    .image(image)
                    .subresource_range(color_range())
                    .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                    .dst_access_mask(vk::AccessFlags::SHADER_READ);
                self.device.cmd_pipeline_barrier(
                    cb,
                    vk::PipelineStageFlags::TRANSFER,
                    vk::PipelineStageFlags::FRAGMENT_SHADER,
                    vk::DependencyFlags::empty(),
                    &[],
                    &[],
                    &[to_shader],
                );
            })?;
            self.device.destroy_buffer(staging, None);
            self.device.free_memory(staging_mem, None);
            Ok(())
        }
    }

    unsafe fn create_sampler(&self) -> Result<vk::Sampler, String> {
        unsafe {
            let ci = vk::SamplerCreateInfo::default()
                .mag_filter(vk::Filter::LINEAR)
                .min_filter(vk::Filter::LINEAR)
                .mipmap_mode(vk::SamplerMipmapMode::NEAREST)
                .address_mode_u(vk::SamplerAddressMode::REPEAT)
                .address_mode_v(vk::SamplerAddressMode::REPEAT)
                .address_mode_w(vk::SamplerAddressMode::REPEAT)
                .anisotropy_enable(false)
                .max_lod(0.0);
            self.device
                .create_sampler(&ci, None)
                .map_err(|e| format!("create sampler: {e}"))
        }
    }

    unsafe fn combined_sampler_layout(
        &self,
        binding: u32,
    ) -> Result<vk::DescriptorSetLayout, String> {
        unsafe {
            let b = [vk::DescriptorSetLayoutBinding::default()
                .binding(binding)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::FRAGMENT)];
            let ci = vk::DescriptorSetLayoutCreateInfo::default().bindings(&b);
            self.device
                .create_descriptor_set_layout(&ci, None)
                .map_err(|e| format!("dsl: {e}"))
        }
    }

    unsafe fn alloc_combined_set(
        &self,
        dsl: vk::DescriptorSetLayout,
        view: vk::ImageView,
        sampler: vk::Sampler,
    ) -> Result<(vk::DescriptorPool, vk::DescriptorSet), String> {
        unsafe {
            let sizes = [vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .descriptor_count(1)];
            let ci = vk::DescriptorPoolCreateInfo::default()
                .max_sets(1)
                .pool_sizes(&sizes);
            let pool = self
                .device
                .create_descriptor_pool(&ci, None)
                .map_err(|e| format!("pool: {e}"))?;
            let layouts = [dsl];
            let ai = vk::DescriptorSetAllocateInfo::default()
                .descriptor_pool(pool)
                .set_layouts(&layouts);
            let dset = self
                .device
                .allocate_descriptor_sets(&ai)
                .map_err(|e| format!("alloc set: {e}"))?[0];
            let ii = [vk::DescriptorImageInfo::default()
                .sampler(sampler)
                .image_view(view)
                .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
            let write = [vk::WriteDescriptorSet::default()
                .dst_set(dset)
                .dst_binding(0)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .image_info(&ii)];
            self.device.update_descriptor_sets(&write, &[]);
            Ok((pool, dset))
        }
    }

    unsafe fn pipeline_layout(
        &self,
        dsl: vk::DescriptorSetLayout,
    ) -> Result<vk::PipelineLayout, String> {
        unsafe {
            let layouts = [dsl];
            let ci = vk::PipelineLayoutCreateInfo::default().set_layouts(&layouts);
            self.device
                .create_pipeline_layout(&ci, None)
                .map_err(|e| format!("pipeline layout: {e}"))
        }
    }

    unsafe fn create_color_target(
        &self,
        w: u32,
        h: u32,
    ) -> Result<(vk::Image, vk::ImageView, vk::DeviceMemory), String> {
        unsafe {
            let ci = vk::ImageCreateInfo::default()
                .image_type(vk::ImageType::TYPE_2D)
                .format(RT_FORMAT)
                .extent(vk::Extent3D {
                    width: w,
                    height: h,
                    depth: 1,
                })
                .mip_levels(1)
                .array_layers(1)
                .samples(vk::SampleCountFlags::TYPE_1)
                .tiling(vk::ImageTiling::OPTIMAL)
                .usage(vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::TRANSFER_SRC)
                .initial_layout(vk::ImageLayout::UNDEFINED);
            let image = self
                .device
                .create_image(&ci, None)
                .map_err(|e| format!("create target: {e}"))?;
            let req = self.device.get_image_memory_requirements(image);
            let mt = self
                .find_mem_type(req.memory_type_bits, vk::MemoryPropertyFlags::DEVICE_LOCAL)
                .ok_or("no device-local memory")?;
            let alloc = vk::MemoryAllocateInfo::default()
                .allocation_size(req.size)
                .memory_type_index(mt);
            let mem = self
                .device
                .allocate_memory(&alloc, None)
                .map_err(|e| format!("alloc target: {e}"))?;
            self.device
                .bind_image_memory(image, mem, 0)
                .map_err(|e| format!("bind target: {e}"))?;
            let view_ci = vk::ImageViewCreateInfo::default()
                .image(image)
                .view_type(vk::ImageViewType::TYPE_2D)
                .format(RT_FORMAT)
                .subresource_range(color_range());
            let view = self
                .device
                .create_image_view(&view_ci, None)
                .map_err(|e| format!("target view: {e}"))?;
            Ok((image, view, mem))
        }
    }

    unsafe fn create_render_pass(&self) -> Result<vk::RenderPass, String> {
        unsafe {
            let attach = [vk::AttachmentDescription::default()
                .format(RT_FORMAT)
                .samples(vk::SampleCountFlags::TYPE_1)
                .load_op(vk::AttachmentLoadOp::CLEAR)
                .store_op(vk::AttachmentStoreOp::STORE)
                .stencil_load_op(vk::AttachmentLoadOp::DONT_CARE)
                .stencil_store_op(vk::AttachmentStoreOp::DONT_CARE)
                .initial_layout(vk::ImageLayout::UNDEFINED)
                .final_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)];
            let color_ref = [vk::AttachmentReference::default()
                .attachment(0)
                .layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)];
            let subpass = [vk::SubpassDescription::default()
                .pipeline_bind_point(vk::PipelineBindPoint::GRAPHICS)
                .color_attachments(&color_ref)];
            // Make the color writes available AND visible to the transfer read in
            // `readback_rgba8` (a separate submit). The implicit subpass→EXTERNAL dependency
            // ends at BOTTOM_OF_PIPE / dstAccess=0, which orders execution but does not flush
            // the color writes for the transfer read; without this a cache-non-coherent driver
            // can read stale/clear pixels into the PNG. (Vulkan spec 8.1, synchronization.)
            let deps = [vk::SubpassDependency::default()
                .src_subpass(0)
                .dst_subpass(vk::SUBPASS_EXTERNAL)
                .src_stage_mask(vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT)
                .src_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE)
                .dst_stage_mask(vk::PipelineStageFlags::TRANSFER)
                .dst_access_mask(vk::AccessFlags::TRANSFER_READ)];
            let ci = vk::RenderPassCreateInfo::default()
                .attachments(&attach)
                .subpasses(&subpass)
                .dependencies(&deps);
            self.device
                .create_render_pass(&ci, None)
                .map_err(|e| format!("render pass: {e}"))
        }
    }

    unsafe fn framebuffer(
        &self,
        rp: vk::RenderPass,
        view: vk::ImageView,
        w: u32,
        h: u32,
    ) -> Result<vk::Framebuffer, String> {
        unsafe {
            let att = [view];
            let ci = vk::FramebufferCreateInfo::default()
                .render_pass(rp)
                .attachments(&att)
                .width(w)
                .height(h)
                .layers(1);
            self.device
                .create_framebuffer(&ci, None)
                .map_err(|e| format!("framebuffer: {e}"))
        }
    }

    unsafe fn graphics_pipeline(
        &self,
        rp: vk::RenderPass,
        layout: vk::PipelineLayout,
        vs: vk::ShaderModule,
        fs: vk::ShaderModule,
    ) -> Result<vk::Pipeline, String> {
        unsafe {
            let name = CString::new("main").unwrap();
            let stages = [
                vk::PipelineShaderStageCreateInfo::default()
                    .stage(vk::ShaderStageFlags::VERTEX)
                    .module(vs)
                    .name(&name),
                vk::PipelineShaderStageCreateInfo::default()
                    .stage(vk::ShaderStageFlags::FRAGMENT)
                    .module(fs)
                    .name(&name),
            ];
            let vertex_input = vk::PipelineVertexInputStateCreateInfo::default();
            let input_asm = vk::PipelineInputAssemblyStateCreateInfo::default()
                .topology(vk::PrimitiveTopology::TRIANGLE_LIST);
            let vp_state = vk::PipelineViewportStateCreateInfo::default()
                .viewport_count(1)
                .scissor_count(1);
            let dyn_states = [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR];
            let dyn_state =
                vk::PipelineDynamicStateCreateInfo::default().dynamic_states(&dyn_states);
            let raster = vk::PipelineRasterizationStateCreateInfo::default()
                .line_width(1.0)
                .cull_mode(vk::CullModeFlags::NONE)
                .front_face(vk::FrontFace::COUNTER_CLOCKWISE)
                .polygon_mode(vk::PolygonMode::FILL);
            let ms = vk::PipelineMultisampleStateCreateInfo::default()
                .rasterization_samples(vk::SampleCountFlags::TYPE_1);
            let blend_att = [vk::PipelineColorBlendAttachmentState::default()
                .color_write_mask(vk::ColorComponentFlags::RGBA)
                .blend_enable(false)];
            let blend = vk::PipelineColorBlendStateCreateInfo::default().attachments(&blend_att);
            let ci = [vk::GraphicsPipelineCreateInfo::default()
                .stages(&stages)
                .vertex_input_state(&vertex_input)
                .input_assembly_state(&input_asm)
                .viewport_state(&vp_state)
                .rasterization_state(&raster)
                .multisample_state(&ms)
                .color_blend_state(&blend)
                .dynamic_state(&dyn_state)
                .layout(layout)
                .render_pass(rp)
                .subpass(0)];
            self.device
                .create_graphics_pipelines(vk::PipelineCache::null(), &ci, None)
                .map_err(|(_, e)| format!("graphics pipeline: {e}"))
                .map(|v| v[0])
        }
    }

    unsafe fn host_buffer(
        &self,
        size: u64,
        usage: vk::BufferUsageFlags,
    ) -> Result<(vk::Buffer, vk::DeviceMemory), String> {
        unsafe {
            let bci = vk::BufferCreateInfo::default()
                .size(size)
                .usage(usage)
                .sharing_mode(vk::SharingMode::EXCLUSIVE);
            let buffer = self
                .device
                .create_buffer(&bci, None)
                .map_err(|e| format!("create buffer: {e}"))?;
            let req = self.device.get_buffer_memory_requirements(buffer);
            let mt = self
                .find_mem_type(
                    req.memory_type_bits,
                    vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
                )
                .ok_or("no host-visible memory")?;
            let alloc = vk::MemoryAllocateInfo::default()
                .allocation_size(req.size)
                .memory_type_index(mt);
            let mem = self
                .device
                .allocate_memory(&alloc, None)
                .map_err(|e| format!("alloc buffer mem: {e}"))?;
            self.device
                .bind_buffer_memory(buffer, mem, 0)
                .map_err(|e| format!("bind buffer: {e}"))?;
            Ok((buffer, mem))
        }
    }

    unsafe fn submit_sync(&self, record: impl FnOnce(vk::CommandBuffer)) -> Result<(), String> {
        unsafe {
            let ai = vk::CommandBufferAllocateInfo::default()
                .command_pool(self.command_pool)
                .level(vk::CommandBufferLevel::PRIMARY)
                .command_buffer_count(1);
            let cb = self
                .device
                .allocate_command_buffers(&ai)
                .map_err(|e| format!("alloc cb: {e}"))?[0];
            let begin = vk::CommandBufferBeginInfo::default()
                .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
            self.device
                .begin_command_buffer(cb, &begin)
                .map_err(|e| format!("begin cb: {e}"))?;
            record(cb);
            self.device
                .end_command_buffer(cb)
                .map_err(|e| format!("end cb: {e}"))?;
            let fence = self
                .device
                .create_fence(&vk::FenceCreateInfo::default(), None)
                .map_err(|e| format!("fence: {e}"))?;
            let cbs = [cb];
            let submit = [vk::SubmitInfo::default().command_buffers(&cbs)];
            self.device
                .queue_submit(self.queue, &submit, fence)
                .map_err(|e| format!("submit: {e}"))?;
            self.device
                .wait_for_fences(&[fence], true, 5_000_000_000)
                .map_err(|e| format!("wait fence: {e}"))?;
            self.device.destroy_fence(fence, None);
            self.device.free_command_buffers(self.command_pool, &cbs);
            Ok(())
        }
    }

    /// Copy the color target into a host buffer and read it back as RGBA8 bytes.
    unsafe fn readback_rgba8(&self, target: vk::Image, w: u32, h: u32) -> Result<Vec<u8>, String> {
        unsafe {
            let size = (w as u64) * (h as u64) * 4;
            let (buffer, mem) = self.host_buffer(size, vk::BufferUsageFlags::TRANSFER_DST)?;
            self.submit_sync(|cb| {
                let region = vk::BufferImageCopy::default()
                    .image_subresource(vk::ImageSubresourceLayers {
                        aspect_mask: vk::ImageAspectFlags::COLOR,
                        mip_level: 0,
                        base_array_layer: 0,
                        layer_count: 1,
                    })
                    .image_extent(vk::Extent3D {
                        width: w,
                        height: h,
                        depth: 1,
                    });
                self.device.cmd_copy_image_to_buffer(
                    cb,
                    target,
                    vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                    buffer,
                    &[region],
                );
            })?;
            let ptr = self
                .device
                .map_memory(mem, 0, size, vk::MemoryMapFlags::empty())
                .map_err(|e| format!("map readback: {e}"))? as *const u8;
            let out = std::slice::from_raw_parts(ptr, size as usize).to_vec();
            self.device.unmap_memory(mem);
            self.device.destroy_buffer(buffer, None);
            self.device.free_memory(mem, None);
            Ok(out)
        }
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

/// A vertex shader emitting a fullscreen triangle (3 verts, `gl_VertexIndex`-driven) and a
/// per-vertex UV at `Location=0` so the fragment shader samples the whole texture across the
/// target. Only the portable `Shader` capability is declared (decision-3).
fn build_textured_vs() -> Vec<u32> {
    let mut b = Builder::new();
    b.set_version(1, 3);
    b.capability(spirv::Capability::Shader);
    b.memory_model(spirv::AddressingModel::Logical, spirv::MemoryModel::GLSL450);

    let void = b.type_void();
    let f32t = b.type_float(32, None);
    let i32t = b.type_int(32, 1);
    let v2f32 = b.type_vector(f32t, 2);
    let v4f32 = b.type_vector(f32t, 4);
    let fn_void = b.type_function(void, []);

    let ptr_in_i32 = b.type_pointer(None, spirv::StorageClass::Input, i32t);
    let vertex_index = b.variable(ptr_in_i32, None, spirv::StorageClass::Input, None);
    b.decorate(
        vertex_index,
        spirv::Decoration::BuiltIn,
        [DrOperand::BuiltIn(spirv::BuiltIn::VertexIndex)],
    );

    let per_vertex = b.type_struct([v4f32]);
    b.decorate(per_vertex, spirv::Decoration::Block, []);
    b.member_decorate(
        per_vertex,
        0,
        spirv::Decoration::BuiltIn,
        [DrOperand::BuiltIn(spirv::BuiltIn::Position)],
    );
    let ptr_out_pv = b.type_pointer(None, spirv::StorageClass::Output, per_vertex);
    let pv_var = b.variable(ptr_out_pv, None, spirv::StorageClass::Output, None);

    let ptr_out_v2 = b.type_pointer(None, spirv::StorageClass::Output, v2f32);
    let uv_out = b.variable(ptr_out_v2, None, spirv::StorageClass::Output, None);
    b.decorate(
        uv_out,
        spirv::Decoration::Location,
        [DrOperand::LiteralBit32(0)],
    );

    let c0 = b.constant_bit32(f32t, 0.0f32.to_bits());
    let c1 = b.constant_bit32(f32t, 1.0f32.to_bits());
    let cn1 = b.constant_bit32(f32t, (-1.0f32).to_bits());
    let c2 = b.constant_bit32(f32t, 2.0f32.to_bits());
    let c3 = b.constant_bit32(f32t, 3.0f32.to_bits());
    let u32t = b.type_int(32, 0);
    let u0 = b.constant_bit32(u32t, 0);
    let cidx1 = b.constant_bit32(i32t, 1);
    let cidx2 = b.constant_bit32(i32t, 2);
    let bool_ty = b.type_bool();

    let main = b.id();
    b.begin_function(void, Some(main), spirv::FunctionControl::NONE, fn_void)
        .unwrap();
    b.begin_block(None).unwrap();

    let idx = b.load(i32t, None, vertex_index, None, []).unwrap();
    let is2 = b.i_equal(bool_ty, None, idx, cidx2).unwrap();
    let is1 = b.i_equal(bool_ty, None, idx, cidx1).unwrap();
    // Big-triangle clip positions: x = is2?3:-1, y = is1?3:-1.
    let x = b.select(f32t, None, is2, c3, cn1).unwrap();
    let y = b.select(f32t, None, is1, c3, cn1).unwrap();
    let pos = b.composite_construct(v4f32, None, [x, y, c0, c1]).unwrap();
    let ptr_out_v4_pos = b.type_pointer(None, spirv::StorageClass::Output, v4f32);
    let ptr_pos = b.access_chain(ptr_out_v4_pos, None, pv_var, [u0]).unwrap();
    b.store(ptr_pos, pos, None, []).unwrap();

    // UV = (pos.xy * 0.5 + 0.5): maps the [-1,3] clip range to [0,2] texcoords, so the
    // visible [-1,1] region samples [0,1] — the full checkerboard across the target.
    let half = b.constant_bit32(f32t, 0.5f32.to_bits());
    let _ = c2;
    let ux = b.f_mul(f32t, None, x, half).unwrap();
    let ux = b.f_add(f32t, None, ux, half).unwrap();
    let uy = b.f_mul(f32t, None, y, half).unwrap();
    let uy = b.f_add(f32t, None, uy, half).unwrap();
    let uv = b.composite_construct(v2f32, None, [ux, uy]).unwrap();
    b.store(uv_out, uv, None, []).unwrap();

    b.ret().unwrap();
    b.end_function().unwrap();

    b.entry_point(
        spirv::ExecutionModel::Vertex,
        main,
        "main",
        [vertex_index, pv_var, uv_out],
    );
    b.module().assemble()
}

/// A fragment shader sampling a combined image-sampler (set 0, binding 0) at the
/// interpolated `Location=0` UV, writing the texel to `Location=0`. Only the portable
/// `Shader` capability (decision-3).
fn build_textured_fs() -> Vec<u32> {
    let mut b = Builder::new();
    b.set_version(1, 3);
    b.capability(spirv::Capability::Shader);
    b.memory_model(spirv::AddressingModel::Logical, spirv::MemoryModel::GLSL450);

    let void = b.type_void();
    let f32t = b.type_float(32, None);
    let v2f32 = b.type_vector(f32t, 2);
    let v4f32 = b.type_vector(f32t, 4);
    let fn_void = b.type_function(void, []);

    let ptr_in_v2 = b.type_pointer(None, spirv::StorageClass::Input, v2f32);
    let uv_in = b.variable(ptr_in_v2, None, spirv::StorageClass::Input, None);
    b.decorate(
        uv_in,
        spirv::Decoration::Location,
        [DrOperand::LiteralBit32(0)],
    );

    let ptr_out_v4 = b.type_pointer(None, spirv::StorageClass::Output, v4f32);
    let color_out = b.variable(ptr_out_v4, None, spirv::StorageClass::Output, None);
    b.decorate(
        color_out,
        spirv::Decoration::Location,
        [DrOperand::LiteralBit32(0)],
    );

    // Combined image-sampler at set 0, binding 0.
    let image_ty = b.type_image(
        f32t,
        spirv::Dim::Dim2D,
        0,
        0,
        0,
        1,
        spirv::ImageFormat::Unknown,
        None,
    );
    let sampled_ty = b.type_sampled_image(image_ty);
    let ptr_uc = b.type_pointer(None, spirv::StorageClass::UniformConstant, sampled_ty);
    let tex = b.variable(ptr_uc, None, spirv::StorageClass::UniformConstant, None);
    b.decorate(
        tex,
        spirv::Decoration::DescriptorSet,
        [DrOperand::LiteralBit32(0)],
    );
    b.decorate(
        tex,
        spirv::Decoration::Binding,
        [DrOperand::LiteralBit32(0)],
    );

    let main = b.id();
    b.begin_function(void, Some(main), spirv::FunctionControl::NONE, fn_void)
        .unwrap();
    b.begin_block(None).unwrap();
    let uv = b.load(v2f32, None, uv_in, None, []).unwrap();
    let si = b.load(sampled_ty, None, tex, None, []).unwrap();
    let texel = b
        .image_sample_implicit_lod(v4f32, None, si, uv, None, [])
        .unwrap();
    b.store(color_out, texel, None, []).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();

    // SPIR-V spec (2.16.1, "Universal Validation Rules"): before version 1.4 the
    // OpEntryPoint interface must list only Input/Output variables. `tex` is a
    // UniformConstant, so it stays off the interface here (module is 1.3, above);
    // it would only be required from 1.4 onward.
    b.entry_point(
        spirv::ExecutionModel::Fragment,
        main,
        "main",
        [uv_in, color_out],
    );
    b.execution_mode(main, spirv::ExecutionMode::OriginUpperLeft, []);
    b.module().assemble()
}
