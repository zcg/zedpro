use std::{
    borrow::Cow,
    ffi::c_void,
    marker::PhantomData,
    mem::{ManuallyDrop, size_of},
    ptr::{copy_nonoverlapping, null, null_mut},
    slice,
    sync::Arc,
    thread,
};

use anyhow::{Context, Result};
use collections::FxHashMap;
use etagere::BucketedAtlasAllocator;
use parking_lot::Mutex;
use util::ResultExt;
use windows::{
    Win32::{
        Foundation::{HWND, RECT},
        Graphics::{
            Direct3D::{
                D3D_PRIMITIVE_TOPOLOGY, D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST,
                D3D_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP,
            },
            Direct3D12::*,
            DirectComposition::{
                DCompositionCreateDevice2, IDCompositionDevice, IDCompositionTarget,
                IDCompositionVisual,
            },
            Dxgi::{Common::*, *},
        },
    },
    core::Interface,
};

use crate::{
    DirectXDevices, create_d3d12_device,
    directx_renderer::{
        DirectXRenderer, FontInfo,
        shader_resources::{RawShaderBytes, ShaderModule, ShaderTarget},
    },
};
use gpui::*;

const BUFFER_COUNT: usize = 3;
const RENDER_TARGET_FORMAT: DXGI_FORMAT = DXGI_FORMAT_B8G8R8A8_UNORM;
const PATH_MULTISAMPLE_COUNT: u32 = 4;
const RTV_DESCRIPTOR_COUNT: usize = BUFFER_COUNT + 1;
const SRV_DESCRIPTOR_COUNT: usize = 4096;

pub(crate) struct DirectX12Renderer {
    hwnd: HWND,
    atlas: Arc<DirectX12Atlas>,
    devices: DirectX12RendererDevices,
    resources: DirectX12RendererResources,
    globals: DirectX12GlobalElements,
    pipelines: DirectX12RenderPipelines,
    direct_composition: Option<DirectCompositionLayer>,
    font_info: &'static FontInfo,
    width: u32,
    height: u32,
    skip_draws: bool,
    srv_descriptor_cursor: usize,
}

struct DirectX12RendererDevices {
    adapter: IDXGIAdapter1,
    dxgi_factory: IDXGIFactory6,
    device: ID3D12Device,
    command_queue: ID3D12CommandQueue,
    executor: DirectX12ImmediateExecutor,
    root_signature: ID3D12RootSignature,
    rtv_heap: ID3D12DescriptorHeap,
    rtv_descriptor_size: usize,
    srv_heap: ID3D12DescriptorHeap,
    srv_descriptor_size: usize,
}

struct DirectX12RendererResources {
    swap_chain: IDXGISwapChain3,
    back_buffers: Vec<DirectX12BackBuffer>,
    path_intermediate_texture: ShaderTextureBinding,
    path_intermediate_resource: ID3D12Resource,
    path_intermediate_msaa_resource: ID3D12Resource,
    path_intermediate_msaa_rtv: D3D12_CPU_DESCRIPTOR_HANDLE,
    viewport: D3D12_VIEWPORT,
    scissor_rect: RECT,
}

struct DirectX12BackBuffer {
    resource: ID3D12Resource,
    rtv_handle: D3D12_CPU_DESCRIPTOR_HANDLE,
}

struct DirectCompositionLayer {
    comp_device: IDCompositionDevice,
    comp_target: IDCompositionTarget,
    comp_visual: IDCompositionVisual,
}

struct DirectX12GlobalElements {
    global_params_buffer: ConstantBuffer<GlobalParams>,
}

struct DirectX12RenderPipelines {
    shadow_pipeline: PipelineState<Shadow>,
    quad_pipeline: PipelineState<Quad>,
    path_rasterization_pipeline: PipelineState<PathRasterizationSprite>,
    path_sprite_pipeline: PipelineState<PathSprite>,
    underline_pipeline: PipelineState<Underline>,
    mono_sprites: PipelineState<MonochromeSprite>,
    subpixel_sprites: PipelineState<SubpixelSprite>,
    poly_sprites: PipelineState<PolychromeSprite>,
}

struct PipelineState<T> {
    label: &'static str,
    pipeline_state: ID3D12PipelineState,
    buffer: StructuredBuffer<T>,
}

struct ConstantBuffer<T> {
    resource: ID3D12Resource,
    mapped_ptr: *mut u8,
    _marker: PhantomData<T>,
}

struct StructuredBuffer<T> {
    resource: ID3D12Resource,
    mapped_ptr: *mut u8,
    capacity: usize,
    _marker: PhantomData<T>,
}

struct DirectX12ImmediateExecutor {
    command_allocator: ID3D12CommandAllocator,
    command_list: ID3D12GraphicsCommandList,
    fence: ID3D12Fence,
    next_fence_value: u64,
}

#[derive(Clone)]
struct ShaderTextureBinding {
    resource: ID3D12Resource,
    format: DXGI_FORMAT,
}

pub(crate) struct DirectX12Atlas(Mutex<DirectX12AtlasState>);

struct DirectX12AtlasState {
    device: ID3D12Device,
    command_queue: ID3D12CommandQueue,
    executor: DirectX12ImmediateExecutor,
    monochrome_textures: AtlasTextureList<DirectX12AtlasTexture>,
    polychrome_textures: AtlasTextureList<DirectX12AtlasTexture>,
    subpixel_textures: AtlasTextureList<DirectX12AtlasTexture>,
    tiles_by_key: FxHashMap<AtlasKey, AtlasTile>,
}

struct DirectX12AtlasTexture {
    id: AtlasTextureId,
    bytes_per_pixel: u32,
    allocator: BucketedAtlasAllocator,
    resource: ID3D12Resource,
    format: DXGI_FORMAT,
    live_atlas_keys: u32,
}

#[derive(Clone, Copy)]
#[repr(C)]
struct PathRasterizationSprite {
    xy_position: Point<ScaledPixels>,
    st_position: Point<f32>,
    color: Background,
    bounds: Bounds<ScaledPixels>,
}

#[derive(Clone, Copy)]
#[repr(C)]
struct PathSprite {
    bounds: Bounds<ScaledPixels>,
}

#[derive(Debug, Default)]
#[repr(C)]
struct GlobalParams {
    gamma_ratios: [f32; 4],
    viewport_size: [f32; 2],
    grayscale_enhanced_contrast: f32,
    subpixel_enhanced_contrast: f32,
}

impl DirectX12Renderer {
    pub(crate) fn new(
        hwnd: HWND,
        directx_devices: &DirectXDevices,
        disable_direct_composition: bool,
    ) -> Result<Self> {
        if disable_direct_composition {
            log::info!("Direct Composition is disabled for Direct3D 12.");
        }

        let devices = DirectX12RendererDevices::new(directx_devices, disable_direct_composition)
            .context("Creating Direct3D 12 devices")?;
        let atlas = Arc::new(DirectX12Atlas::new(&devices.device, &devices.command_queue)?);
        let resources =
            DirectX12RendererResources::new(&devices, 1, 1, hwnd, disable_direct_composition)
                .context("Creating Direct3D 12 resources")?;
        let globals = DirectX12GlobalElements::new(&devices.device)
            .context("Creating Direct3D 12 global elements")?;
        let pipelines = DirectX12RenderPipelines::new(&devices)
            .context("Creating Direct3D 12 render pipelines")?;

        let direct_composition = if disable_direct_composition {
            None
        } else {
            let composition =
                DirectCompositionLayer::new(hwnd)
                    .context("Creating DirectComposition for Direct3D 12")?;
            composition
                .set_swap_chain(&resources.swap_chain)
                .context("Binding Direct3D 12 swap chain to DirectComposition")?;
            Some(composition)
        };

        Ok(Self {
            hwnd,
            atlas,
            devices,
            resources,
            globals,
            pipelines,
            direct_composition,
            font_info: DirectXRenderer::get_font_info(),
            width: 1,
            height: 1,
            skip_draws: false,
            srv_descriptor_cursor: 0,
        })
    }

    pub(crate) fn sprite_atlas(&self) -> Arc<dyn PlatformAtlas> {
        self.atlas.clone()
    }

    pub(crate) fn handle_device_lost(&mut self, directx_devices: &DirectXDevices) -> Result<()> {
        let disable_direct_composition = self.direct_composition.take().is_none();

        let devices = DirectX12RendererDevices::new(directx_devices, disable_direct_composition)
            .context("Recreating Direct3D 12 devices")?;
        let resources = DirectX12RendererResources::new(
            &devices,
            self.width,
            self.height,
            self.hwnd,
            disable_direct_composition,
        )
        .context("Recreating Direct3D 12 resources")?;
        let globals = DirectX12GlobalElements::new(&devices.device)
            .context("Recreating Direct3D 12 global elements")?;
        let pipelines = DirectX12RenderPipelines::new(&devices)
            .context("Recreating Direct3D 12 render pipelines")?;
        let direct_composition = if disable_direct_composition {
            None
        } else {
            match DirectCompositionLayer::new(self.hwnd)
                .context("Recreating DirectComposition for Direct3D 12")
                .and_then(|composition| {
                    composition
                        .set_swap_chain(&resources.swap_chain)
                        .context("Rebinding Direct3D 12 swap chain to DirectComposition")?;
                    Ok(composition)
                }) {
                Ok(composition) => Some(composition),
                Err(error) => {
                    log::warn!(
                        "Direct3D 12 device recovery disabled DirectComposition after recreation failed: {error:#}"
                    );
                    None
                }
            }
        };

        self.atlas
            .handle_device_lost(&devices.device, &devices.command_queue)?;

        self.devices = devices;
        self.resources = resources;
        self.globals = globals;
        self.pipelines = pipelines;
        self.direct_composition = direct_composition;
        self.skip_draws = true;
        self.srv_descriptor_cursor = 0;
        Ok(())
    }

    pub(crate) fn draw(
        &mut self,
        scene: &Scene,
        background_appearance: WindowBackgroundAppearance,
    ) -> Result<()> {
        if self.skip_draws {
            return Ok(());
        }

        log::trace!(
            "Direct3D 12 scene stats: shadows={}, quads={}, underlines={}, mono={}, subpixel={}, poly={}, paths={}, surfaces={}",
            scene.shadows.len(),
            scene.quads.len(),
            scene.underlines.len(),
            scene.monochrome_sprites.len(),
            scene.subpixel_sprites.len(),
            scene.polychrome_sprites.len(),
            scene.paths.len(),
            scene.surfaces.len()
        );

        let clear_color = match background_appearance {
            WindowBackgroundAppearance::Opaque => [1.0, 1.0, 1.0, 1.0],
            _ => [0.0, 0.0, 0.0, 0.0],
        };

        self.pre_draw(clear_color)?;
        self.upload_scene_buffers(scene)?;

        for batch in scene.batches() {
            match batch {
                PrimitiveBatch::Shadows(range) => self.draw_shadows(range.start, range.len()),
                PrimitiveBatch::Quads(range) => self.draw_quads(range.start, range.len()),
                PrimitiveBatch::Paths(range) => {
                    let paths = &scene.paths[range];
                    self.draw_paths_to_intermediate(paths)?;
                    self.draw_paths_from_intermediate(paths)
                }
                PrimitiveBatch::Underlines(range) => self.draw_underlines(range.start, range.len()),
                PrimitiveBatch::MonochromeSprites { texture_id, range } => {
                    self.draw_monochrome_sprites(texture_id, range.start, range.len())
                }
                PrimitiveBatch::SubpixelSprites { texture_id, range } => {
                    self.draw_subpixel_sprites(texture_id, range.start, range.len())
                }
                PrimitiveBatch::PolychromeSprites { texture_id, range } => {
                    self.draw_polychrome_sprites(texture_id, range.start, range.len())
                }
                PrimitiveBatch::Surfaces(range) => self.draw_surfaces(&scene.surfaces[range]),
            }
            .context(format!(
                "scene too large: {} paths, {} shadows, {} quads, {} underlines, {} mono, {} subpixel, {} poly, {} surfaces",
                scene.paths.len(),
                scene.shadows.len(),
                scene.quads.len(),
                scene.underlines.len(),
                scene.monochrome_sprites.len(),
                scene.subpixel_sprites.len(),
                scene.polychrome_sprites.len(),
                scene.surfaces.len(),
            ))?;
        }

        self.present()
    }

    pub(crate) fn resize(&mut self, new_size: Size<DevicePixels>) -> Result<()> {
        let width = new_size.width.0.max(1) as u32;
        let height = new_size.height.0.max(1) as u32;
        if self.width == width && self.height == height {
            return Ok(());
        }

        self.devices.wait_for_gpu_idle()?;
        self.devices
            .executor
            .release_recorded_references()
            .context("Releasing Direct3D 12 command-list references before resize")?;
        if let Some(direct_composition) = &self.direct_composition {
            let new_resources = DirectX12RendererResources::new(
                &self.devices,
                width,
                height,
                self.hwnd,
                false,
            )
            .context("Recreating Direct3D 12 resources for DirectComposition resize")?;
            direct_composition
                .set_swap_chain(&new_resources.swap_chain)
                .context("Switching DirectComposition to resized Direct3D 12 swap chain")?;
            self.resources = new_resources;
        } else {
            self.resources
                .resize(&self.devices, width, height)
                .context("Resizing Direct3D 12 resources")?;
        }

        self.width = width;
        self.height = height;
        Ok(())
    }

    pub(crate) fn gpu_specs(&self) -> Result<GpuSpecs> {
        let desc = unsafe { self.devices.adapter.GetDesc1() }?;
        let is_software_emulated = (desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32) != 0;
        let device_name = String::from_utf16_lossy(&desc.Description)
            .trim_matches(char::from(0))
            .to_string();
        let driver_name = match desc.VendorId {
            0x10DE => "NVIDIA Corporation".to_string(),
            0x1002 => "AMD Corporation".to_string(),
            0x8086 => "Intel Corporation".to_string(),
            id => format!("Unknown Vendor (ID: {id:#X})"),
        };
        let driver_version = get_dxgi_driver_version(&self.devices.adapter)
            .context("Failed to get GPU driver info")
            .log_err()
            .unwrap_or("Unknown Driver".to_string());

        Ok(GpuSpecs {
            is_software_emulated,
            device_name,
            driver_name,
            driver_info: driver_version,
        })
    }

    pub(crate) fn mark_drawable(&mut self) {
        self.skip_draws = false;
    }

    fn pre_draw(&mut self, clear_color: [f32; 4]) -> Result<()> {
        self.globals.global_params_buffer.update(&GlobalParams {
            gamma_ratios: self.font_info.gamma_ratios,
            viewport_size: [self.resources.viewport.Width, self.resources.viewport.Height],
            grayscale_enhanced_contrast: self.font_info.grayscale_enhanced_contrast,
            subpixel_enhanced_contrast: self.font_info.subpixel_enhanced_contrast,
        });
        self.srv_descriptor_cursor = 0;

        let back_buffer_index =
            unsafe { self.resources.swap_chain.GetCurrentBackBufferIndex() as usize };
        let back_buffer = self
            .resources
            .back_buffers
            .get(back_buffer_index)
            .context("Direct3D 12 back buffer index is out of range")?;

        self.devices.executor.reset()?;
        let command_list = &self.devices.executor.command_list;
        unsafe {
            command_list.SetGraphicsRootSignature(&self.devices.root_signature);
            command_list.RSSetViewports(&[self.resources.viewport]);
            command_list.RSSetScissorRects(&[self.resources.scissor_rect]);
            command_list.SetDescriptorHeaps(&[Some(self.devices.srv_heap.clone())]);

            let to_render_target = transition_barrier(
                &back_buffer.resource,
                D3D12_RESOURCE_STATE_PRESENT,
                D3D12_RESOURCE_STATE_RENDER_TARGET,
            );
            command_list.ResourceBarrier(&[to_render_target]);
            command_list.OMSetRenderTargets(
                1,
                Some(&back_buffer.rtv_handle as *const _),
                false,
                None,
            );
            command_list.ClearRenderTargetView(back_buffer.rtv_handle, &clear_color, None);
        }
        Ok(())
    }

    fn present(&mut self) -> Result<()> {
        let back_buffer_index =
            unsafe { self.resources.swap_chain.GetCurrentBackBufferIndex() as usize };
        let back_buffer = self
            .resources
            .back_buffers
            .get(back_buffer_index)
            .context("Direct3D 12 back buffer index is out of range")?;

        unsafe {
            let to_present = transition_barrier(
                &back_buffer.resource,
                D3D12_RESOURCE_STATE_RENDER_TARGET,
                D3D12_RESOURCE_STATE_PRESENT,
            );
            self.devices
                .executor
                .command_list
                .ResourceBarrier(&[to_present]);
            self.devices
                .executor
                .command_list
                .Close()
                .context("Closing Direct3D 12 command list")?;
        }

        self.devices.execute_and_wait()?;

        unsafe {
            self.resources
                .swap_chain
                .Present(0, DXGI_PRESENT(0))
                .ok()
                .context("Presenting Direct3D 12 swap chain failed")?;
        }
        self.devices.wait_for_gpu_idle()?;
        Ok(())
    }

    fn upload_scene_buffers(&mut self, scene: &Scene) -> Result<()> {
        if !scene.shadows.is_empty() {
            self.pipelines
                .shadow_pipeline
                .update_buffer(&self.devices.device, &scene.shadows)?;
        }
        if !scene.quads.is_empty() {
            self.pipelines
                .quad_pipeline
                .update_buffer(&self.devices.device, &scene.quads)?;
        }
        if !scene.underlines.is_empty() {
            self.pipelines
                .underline_pipeline
                .update_buffer(&self.devices.device, &scene.underlines)?;
        }
        if !scene.monochrome_sprites.is_empty() {
            self.pipelines
                .mono_sprites
                .update_buffer(&self.devices.device, &scene.monochrome_sprites)?;
        }
        if !scene.subpixel_sprites.is_empty() {
            self.pipelines
                .subpixel_sprites
                .update_buffer(&self.devices.device, &scene.subpixel_sprites)?;
        }
        if !scene.polychrome_sprites.is_empty() {
            self.pipelines
                .poly_sprites
                .update_buffer(&self.devices.device, &scene.polychrome_sprites)?;
        }
        Ok(())
    }

    fn draw_shadows(&mut self, start: usize, len: usize) -> Result<()> {
        if len == 0 {
            return Ok(());
        }
        let pipeline_state = self.pipelines.shadow_pipeline.pipeline_state.clone();
        let gpu_va = self.pipelines.shadow_pipeline.buffer.gpu_virtual_address(start as u32);
        self.bind_draw_state(
            &pipeline_state,
            gpu_va,
            None,
            D3D_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP,
        )?;
        unsafe {
            self.devices
                .executor
                .command_list
                .DrawInstanced(4, len as u32, 0, 0);
        }
        Ok(())
    }

    fn draw_quads(&mut self, start: usize, len: usize) -> Result<()> {
        if len == 0 {
            return Ok(());
        }
        let pipeline_state = self.pipelines.quad_pipeline.pipeline_state.clone();
        let gpu_va = self.pipelines.quad_pipeline.buffer.gpu_virtual_address(start as u32);
        self.bind_draw_state(
            &pipeline_state,
            gpu_va,
            None,
            D3D_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP,
        )?;
        unsafe {
            self.devices
                .executor
                .command_list
                .DrawInstanced(4, len as u32, 0, 0);
        }
        Ok(())
    }

    fn draw_paths_to_intermediate(&mut self, paths: &[Path<ScaledPixels>]) -> Result<()> {
        if paths.is_empty() {
            return Ok(());
        }

        let mut vertices = Vec::new();
        for path in paths {
            vertices.extend(path.vertices.iter().map(|vertex| PathRasterizationSprite {
                xy_position: vertex.xy_position,
                st_position: vertex.st_position,
                color: path.color,
                bounds: path.clipped_bounds(),
            }));
        }
        self.pipelines
            .path_rasterization_pipeline
            .update_buffer(&self.devices.device, &vertices)?;

        unsafe {
            let command_list = &self.devices.executor.command_list;
            let to_msaa_rtv = transition_barrier(
                &self.resources.path_intermediate_msaa_resource,
                D3D12_RESOURCE_STATE_RESOLVE_SOURCE,
                D3D12_RESOURCE_STATE_RENDER_TARGET,
            );
            let to_path_resolve_dest = transition_barrier(
                &self.resources.path_intermediate_resource,
                D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE
                    | D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE,
                D3D12_RESOURCE_STATE_RESOLVE_DEST,
            );
            command_list.ResourceBarrier(&[to_msaa_rtv, to_path_resolve_dest]);
            command_list.OMSetRenderTargets(
                1,
                Some(&self.resources.path_intermediate_msaa_rtv as *const _),
                false,
                None,
            );
            command_list.ClearRenderTargetView(
                self.resources.path_intermediate_msaa_rtv,
                &[0.0; 4],
                None,
            );
        }

        let pipeline_state = self
            .pipelines
            .path_rasterization_pipeline
            .pipeline_state
            .clone();
        let gpu_va = self
            .pipelines
            .path_rasterization_pipeline
            .buffer
            .gpu_virtual_address(0);
        self.bind_draw_state(
            &pipeline_state,
            gpu_va,
            None,
            D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST,
        )?;
        unsafe {
            let command_list = &self.devices.executor.command_list;
            command_list.DrawInstanced(vertices.len() as u32, 1, 0, 0);
            let to_msaa_resolve_source = transition_barrier(
                &self.resources.path_intermediate_msaa_resource,
                D3D12_RESOURCE_STATE_RENDER_TARGET,
                D3D12_RESOURCE_STATE_RESOLVE_SOURCE,
            );
            command_list.ResourceBarrier(&[to_msaa_resolve_source]);
            command_list.ResolveSubresource(
                &self.resources.path_intermediate_resource,
                0,
                &self.resources.path_intermediate_msaa_resource,
                0,
                RENDER_TARGET_FORMAT,
            );
            let to_path_shader_read = transition_barrier(
                &self.resources.path_intermediate_resource,
                D3D12_RESOURCE_STATE_RESOLVE_DEST,
                D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE
                    | D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE,
            );
            command_list.ResourceBarrier(&[to_path_shader_read]);
            let current_back_buffer = self.current_back_buffer()?;
            command_list.OMSetRenderTargets(
                1,
                Some(&current_back_buffer.rtv_handle as *const _),
                false,
                None,
            );
        }

        Ok(())
    }

    fn draw_paths_from_intermediate(&mut self, paths: &[Path<ScaledPixels>]) -> Result<()> {
        let Some(first_path) = paths.first() else {
            return Ok(());
        };

        let sprites = if paths.last().unwrap().order == first_path.order {
            paths
                .iter()
                .map(|path| PathSprite {
                    bounds: path.clipped_bounds(),
                })
                .collect::<Vec<_>>()
        } else {
            let mut bounds = first_path.clipped_bounds();
            for path in paths.iter().skip(1) {
                bounds = bounds.union(&path.clipped_bounds());
            }
            vec![PathSprite { bounds }]
        };

        self.pipelines
            .path_sprite_pipeline
            .update_buffer(&self.devices.device, &sprites)?;
        let pipeline_state = self.pipelines.path_sprite_pipeline.pipeline_state.clone();
        let gpu_va = self.pipelines.path_sprite_pipeline.buffer.gpu_virtual_address(0);
        let texture = self.resources.path_intermediate_texture.clone();
        self.bind_draw_state(
            &pipeline_state,
            gpu_va,
            Some(&texture),
            D3D_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP,
        )?;
        unsafe {
            self.devices
                .executor
                .command_list
                .DrawInstanced(4, sprites.len() as u32, 0, 0);
        }
        Ok(())
    }

    fn draw_underlines(&mut self, start: usize, len: usize) -> Result<()> {
        if len == 0 {
            return Ok(());
        }
        let pipeline_state = self.pipelines.underline_pipeline.pipeline_state.clone();
        let gpu_va = self.pipelines.underline_pipeline.buffer.gpu_virtual_address(start as u32);
        self.bind_draw_state(
            &pipeline_state,
            gpu_va,
            None,
            D3D_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP,
        )?;
        unsafe {
            self.devices
                .executor
                .command_list
                .DrawInstanced(4, len as u32, 0, 0);
        }
        Ok(())
    }

    fn draw_monochrome_sprites(
        &mut self,
        texture_id: AtlasTextureId,
        start: usize,
        len: usize,
    ) -> Result<()> {
        if len == 0 {
            return Ok(());
        }
        let texture = self
            .atlas
            .get_texture_binding(texture_id)
            .context("Missing Direct3D 12 monochrome atlas texture")?;
        let pipeline_state = self.pipelines.mono_sprites.pipeline_state.clone();
        let gpu_va = self.pipelines.mono_sprites.buffer.gpu_virtual_address(start as u32);
        self.bind_draw_state(
            &pipeline_state,
            gpu_va,
            Some(&texture),
            D3D_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP,
        )?;
        unsafe {
            self.devices
                .executor
                .command_list
                .DrawInstanced(4, len as u32, 0, 0);
        }
        Ok(())
    }

    fn draw_subpixel_sprites(
        &mut self,
        texture_id: AtlasTextureId,
        start: usize,
        len: usize,
    ) -> Result<()> {
        if len == 0 {
            return Ok(());
        }
        let texture = self
            .atlas
            .get_texture_binding(texture_id)
            .context("Missing Direct3D 12 subpixel atlas texture")?;
        let pipeline_state = self.pipelines.subpixel_sprites.pipeline_state.clone();
        let gpu_va = self
            .pipelines
            .subpixel_sprites
            .buffer
            .gpu_virtual_address(start as u32);
        self.bind_draw_state(
            &pipeline_state,
            gpu_va,
            Some(&texture),
            D3D_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP,
        )?;
        unsafe {
            self.devices
                .executor
                .command_list
                .DrawInstanced(4, len as u32, 0, 0);
        }
        Ok(())
    }

    fn draw_polychrome_sprites(
        &mut self,
        texture_id: AtlasTextureId,
        start: usize,
        len: usize,
    ) -> Result<()> {
        if len == 0 {
            return Ok(());
        }
        let texture = self
            .atlas
            .get_texture_binding(texture_id)
            .context("Missing Direct3D 12 polychrome atlas texture")?;
        let pipeline_state = self.pipelines.poly_sprites.pipeline_state.clone();
        let gpu_va = self.pipelines.poly_sprites.buffer.gpu_virtual_address(start as u32);
        self.bind_draw_state(
            &pipeline_state,
            gpu_va,
            Some(&texture),
            D3D_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP,
        )?;
        unsafe {
            self.devices
                .executor
                .command_list
                .DrawInstanced(4, len as u32, 0, 0);
        }
        Ok(())
    }

    fn draw_surfaces(&mut self, surfaces: &[PaintSurface]) -> Result<()> {
        if surfaces.is_empty() {
            return Ok(());
        }
        Ok(())
    }

    fn bind_draw_state(
        &mut self,
        pipeline_state: &ID3D12PipelineState,
        structured_buffer_gpu_va: u64,
        texture: Option<&ShaderTextureBinding>,
        topology: D3D_PRIMITIVE_TOPOLOGY,
    ) -> Result<()> {
        let texture_handle = self.stage_texture_descriptor(texture)?;

        let command_list = &self.devices.executor.command_list;
        unsafe {
            command_list.SetPipelineState(pipeline_state);
            command_list.SetGraphicsRootSignature(&self.devices.root_signature);
            command_list.SetGraphicsRootConstantBufferView(
                0,
                self.globals.global_params_buffer.gpu_virtual_address(),
            );
            command_list.SetGraphicsRootShaderResourceView(1, structured_buffer_gpu_va);
            command_list.SetGraphicsRootDescriptorTable(2, texture_handle);
            command_list.IASetPrimitiveTopology(topology);
            command_list.RSSetViewports(&[self.resources.viewport]);
            command_list.RSSetScissorRects(&[self.resources.scissor_rect]);
        }
        Ok(())
    }

    fn stage_texture_descriptor(
        &mut self,
        texture: Option<&ShaderTextureBinding>,
    ) -> Result<D3D12_GPU_DESCRIPTOR_HANDLE> {
        let slot = self.srv_descriptor_cursor;
        anyhow::ensure!(
            slot < SRV_DESCRIPTOR_COUNT,
            "Direct3D 12 SRV descriptor heap exhausted while encoding draw commands"
        );
        self.srv_descriptor_cursor += 1;

        let cpu_handle = self.devices.srv_cpu_handle(slot);
        let gpu_handle = self.devices.srv_gpu_handle(slot);
        unsafe {
            if let Some(texture) = texture {
                self.devices.device.CreateShaderResourceView(
                    Some(&texture.resource),
                    Some(&texture_srv_desc(texture.format)),
                    cpu_handle,
                );
            } else {
                self.devices.device.CreateShaderResourceView(
                    None,
                    Some(&texture_srv_desc(DXGI_FORMAT_R8G8B8A8_UNORM)),
                    cpu_handle,
                );
            }
        }
        Ok(gpu_handle)
    }

fn current_back_buffer(&self) -> Result<&DirectX12BackBuffer> {
        let index = unsafe { self.resources.swap_chain.GetCurrentBackBufferIndex() as usize };
        self.resources
            .back_buffers
            .get(index)
            .context("Direct3D 12 back buffer index is out of range")
    }
}

impl DirectX12RendererDevices {
    fn new(directx_devices: &DirectXDevices, _disable_direct_composition: bool) -> Result<Self> {
        let adapter = directx_devices.adapter.clone();
        let dxgi_factory = directx_devices.dxgi_factory.clone();
        let device = if let Some(device) = directx_devices.d3d12_device() {
            device.clone()
        } else {
            create_d3d12_device(
                &adapter,
                directx_devices
                    .backend_probe
                    .d3d12_feature_level
                    .unwrap_or(windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_12_0),
            )?
        };

        let command_queue_desc = D3D12_COMMAND_QUEUE_DESC {
            Type: D3D12_COMMAND_LIST_TYPE_DIRECT,
            Priority: D3D12_COMMAND_QUEUE_PRIORITY_NORMAL.0,
            Flags: D3D12_COMMAND_QUEUE_FLAG_NONE,
            NodeMask: 0,
        };
        let command_queue: ID3D12CommandQueue = unsafe {
            device
                .CreateCommandQueue(&command_queue_desc)
                .context("Creating Direct3D 12 command queue")?
        };
        let executor = DirectX12ImmediateExecutor::new(&device)
            .context("Creating Direct3D 12 command execution objects")?;
        let root_signature =
            create_root_signature(&device).context("Creating Direct3D 12 root signature")?;

        let rtv_heap_desc = D3D12_DESCRIPTOR_HEAP_DESC {
            Type: D3D12_DESCRIPTOR_HEAP_TYPE_RTV,
            NumDescriptors: RTV_DESCRIPTOR_COUNT as u32,
            Flags: D3D12_DESCRIPTOR_HEAP_FLAG_NONE,
            NodeMask: 0,
        };
        let rtv_heap: ID3D12DescriptorHeap = unsafe {
            device
                .CreateDescriptorHeap(&rtv_heap_desc)
                .context("Creating Direct3D 12 RTV heap")?
        };

        let srv_heap_desc = D3D12_DESCRIPTOR_HEAP_DESC {
            Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
            NumDescriptors: SRV_DESCRIPTOR_COUNT as u32,
            Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
            NodeMask: 0,
        };
        let srv_heap: ID3D12DescriptorHeap = unsafe {
            device
                .CreateDescriptorHeap(&srv_heap_desc)
                .context("Creating Direct3D 12 SRV heap")?
        };

        Ok(Self {
            adapter,
            dxgi_factory,
            device: device.clone(),
            command_queue,
            executor,
            root_signature,
            rtv_heap,
            rtv_descriptor_size: unsafe {
                device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_RTV) as usize
            },
            srv_heap,
            srv_descriptor_size: unsafe {
                device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV)
                    as usize
            },
        })
    }

    fn execute_and_wait(&mut self) -> Result<()> {
        self.executor.execute_and_wait(&self.command_queue)
    }

    fn wait_for_gpu_idle(&mut self) -> Result<()> {
        self.executor.wait_for_gpu_idle(&self.command_queue)
    }

    fn rtv_handle(&self, slot: usize) -> D3D12_CPU_DESCRIPTOR_HANDLE {
        let start = unsafe { self.rtv_heap.GetCPUDescriptorHandleForHeapStart() };
        D3D12_CPU_DESCRIPTOR_HANDLE {
            ptr: start.ptr + self.rtv_descriptor_size * slot,
        }
    }

    fn srv_cpu_handle(&self, slot: usize) -> D3D12_CPU_DESCRIPTOR_HANDLE {
        let start = unsafe { self.srv_heap.GetCPUDescriptorHandleForHeapStart() };
        D3D12_CPU_DESCRIPTOR_HANDLE {
            ptr: start.ptr + self.srv_descriptor_size * slot,
        }
    }

    fn srv_gpu_handle(&self, slot: usize) -> D3D12_GPU_DESCRIPTOR_HANDLE {
        let start = unsafe { self.srv_heap.GetGPUDescriptorHandleForHeapStart() };
        D3D12_GPU_DESCRIPTOR_HANDLE {
            ptr: start.ptr + (self.srv_descriptor_size * slot) as u64,
        }
    }
}

impl DirectX12RendererResources {
    fn new(
        devices: &DirectX12RendererDevices,
        width: u32,
        height: u32,
        hwnd: HWND,
        disable_direct_composition: bool,
    ) -> Result<Self> {
        let swap_chain = create_swap_chain(
            &devices.dxgi_factory,
            &devices.command_queue,
            hwnd,
            width,
            height,
            disable_direct_composition,
        )
        .context("Creating Direct3D 12 swap chain")?;

        let back_buffers = create_back_buffers(devices, &swap_chain)
            .context("Creating Direct3D 12 back buffers")?;
        let path_intermediate_resource = create_texture_resource(
            &devices.device,
            width,
            height,
            RENDER_TARGET_FORMAT,
            1,
            D3D12_RESOURCE_FLAG_NONE,
            D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE
                | D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE,
        )
        .context("Creating Direct3D 12 path intermediate texture")?;
        let path_intermediate_msaa_resource = create_texture_resource(
            &devices.device,
            width,
            height,
            RENDER_TARGET_FORMAT,
            PATH_MULTISAMPLE_COUNT,
            D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
            D3D12_RESOURCE_STATE_RESOLVE_SOURCE,
        )
        .context("Creating Direct3D 12 path intermediate MSAA texture")?;
        let path_intermediate_msaa_rtv = devices.rtv_handle(BUFFER_COUNT);
        unsafe {
            devices.device.CreateRenderTargetView(
                &path_intermediate_msaa_resource,
                None,
                path_intermediate_msaa_rtv,
            );
        }

        Ok(Self {
            swap_chain,
            back_buffers,
            path_intermediate_texture: ShaderTextureBinding {
                resource: path_intermediate_resource.clone(),
                format: RENDER_TARGET_FORMAT,
            },
            path_intermediate_resource,
            path_intermediate_msaa_resource,
            path_intermediate_msaa_rtv,
            viewport: create_viewport(width as f32, height as f32),
            scissor_rect: create_scissor_rect(width, height),
        })
    }

    fn resize(
        &mut self,
        devices: &DirectX12RendererDevices,
        width: u32,
        height: u32,
    ) -> Result<()> {
        let old_back_buffers = std::mem::take(&mut self.back_buffers);
        drop(old_back_buffers);
        unsafe {
            if let Err(error) = self
                .swap_chain
                .ResizeBuffers(
                    BUFFER_COUNT as u32,
                    width,
                    height,
                    RENDER_TARGET_FORMAT,
                    DXGI_SWAP_CHAIN_FLAG(0),
                )
            {
                log::error!(
                    "Direct3D 12 ResizeBuffers failed while resizing swap chain to {}x{}: {error:#}",
                    width,
                    height
                );
                match create_back_buffers(devices, &self.swap_chain) {
                    Ok(back_buffers) => {
                        self.back_buffers = back_buffers;
                    }
                    Err(restore_error) => {
                        log::error!(
                            "Direct3D 12 failed to restore swap-chain back buffers after ResizeBuffers failed: {restore_error:#}"
                        );
                    }
                }
                return Err(error).context("Resizing Direct3D 12 swap chain");
            }
        }

        let new_back_buffers = create_back_buffers(devices, &self.swap_chain)
            .context("Recreating Direct3D 12 back buffers")?;
        let new_path_intermediate_resource = create_texture_resource(
            &devices.device,
            width,
            height,
            RENDER_TARGET_FORMAT,
            1,
            D3D12_RESOURCE_FLAG_NONE,
            D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE
                | D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE,
        )
        .context("Recreating Direct3D 12 path intermediate texture")?;
        let new_path_intermediate_texture = ShaderTextureBinding {
            resource: new_path_intermediate_resource.clone(),
            format: RENDER_TARGET_FORMAT,
        };
        let new_path_intermediate_msaa_resource = create_texture_resource(
            &devices.device,
            width,
            height,
            RENDER_TARGET_FORMAT,
            PATH_MULTISAMPLE_COUNT,
            D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
            D3D12_RESOURCE_STATE_RESOLVE_SOURCE,
        )
        .context("Recreating Direct3D 12 path intermediate MSAA texture")?;
        unsafe {
            devices.device.CreateRenderTargetView(
                &new_path_intermediate_msaa_resource,
                None,
                self.path_intermediate_msaa_rtv,
            );
        }
        self.back_buffers = new_back_buffers;
        self.path_intermediate_resource = new_path_intermediate_resource;
        self.path_intermediate_texture = new_path_intermediate_texture;
        self.path_intermediate_msaa_resource = new_path_intermediate_msaa_resource;
        self.viewport = create_viewport(width as f32, height as f32);
        self.scissor_rect = create_scissor_rect(width, height);
        Ok(())
    }
}

impl DirectCompositionLayer {
    fn new(hwnd: HWND) -> Result<Self> {
        let comp_device = unsafe {
            DCompositionCreateDevice2::<Option<&windows::core::IUnknown>, IDCompositionDevice>(None)?
        };
        let comp_target = unsafe { comp_device.CreateTargetForHwnd(hwnd, true) }?;
        let comp_visual = unsafe { comp_device.CreateVisual() }?;
        Ok(Self {
            comp_device,
            comp_target,
            comp_visual,
        })
    }

    fn set_swap_chain(&self, swap_chain: &IDXGISwapChain3) -> Result<()> {
        unsafe {
            self.comp_visual.SetContent(swap_chain)?;
            self.comp_target.SetRoot(&self.comp_visual)?;
            self.comp_device.Commit()?;
        }
        Ok(())
    }
}

impl DirectX12GlobalElements {
    fn new(device: &ID3D12Device) -> Result<Self> {
        Ok(Self {
            global_params_buffer: ConstantBuffer::new(device)
                .context("Creating Direct3D 12 global params buffer")?,
        })
    }
}

impl DirectX12RenderPipelines {
    fn new(devices: &DirectX12RendererDevices) -> Result<Self> {
        Ok(Self {
            shadow_pipeline: PipelineState::new(
                devices,
                "shadow_pipeline",
                ShaderModule::Shadow,
                4,
                create_default_blend_state(),
                1,
            )?,
            quad_pipeline: PipelineState::new(
                devices,
                "quad_pipeline",
                ShaderModule::Quad,
                64,
                create_default_blend_state(),
                1,
            )?,
            path_rasterization_pipeline: PipelineState::new(
                devices,
                "path_rasterization_pipeline",
                ShaderModule::PathRasterization,
                32,
                create_path_rasterization_blend_state(),
                PATH_MULTISAMPLE_COUNT,
            )?,
            path_sprite_pipeline: PipelineState::new(
                devices,
                "path_sprite_pipeline",
                ShaderModule::PathSprite,
                4,
                create_path_sprite_blend_state(),
                1,
            )?,
            underline_pipeline: PipelineState::new(
                devices,
                "underline_pipeline",
                ShaderModule::Underline,
                4,
                create_default_blend_state(),
                1,
            )?,
            mono_sprites: PipelineState::new(
                devices,
                "monochrome_sprite_pipeline",
                ShaderModule::MonochromeSprite,
                512,
                create_default_blend_state(),
                1,
            )?,
            subpixel_sprites: PipelineState::new(
                devices,
                "subpixel_sprite_pipeline",
                ShaderModule::SubpixelSprite,
                512,
                create_subpixel_blend_state(),
                1,
            )?,
            poly_sprites: PipelineState::new(
                devices,
                "polychrome_sprite_pipeline",
                ShaderModule::PolychromeSprite,
                16,
                create_default_blend_state(),
                1,
            )?,
        })
    }
}

impl<T> PipelineState<T> {
    fn new(
        devices: &DirectX12RendererDevices,
        label: &'static str,
        shader_module: ShaderModule,
        buffer_capacity: usize,
        blend_state: D3D12_BLEND_DESC,
        sample_count: u32,
    ) -> Result<Self> {
        let vertex = RawShaderBytes::new(shader_module, ShaderTarget::Vertex)?;
        let fragment = RawShaderBytes::new(shader_module, ShaderTarget::Fragment)?;
        let pipeline_state = create_graphics_pipeline_state(
            &devices.device,
            &devices.root_signature,
            vertex.as_bytes(),
            fragment.as_bytes(),
            blend_state,
            sample_count,
        )
        .with_context(|| format!("Creating Direct3D 12 pipeline state for {label}"))?;

        Ok(Self {
            label,
            pipeline_state,
            buffer: StructuredBuffer::new(&devices.device, buffer_capacity)
                .with_context(|| format!("Creating Direct3D 12 structured buffer for {label}"))?,
        })
    }

    fn update_buffer(&mut self, device: &ID3D12Device, data: &[T]) -> Result<()> {
        if self.buffer.capacity < data.len() {
            let new_capacity = data.len().max(1).next_power_of_two();
            log::debug!(
                "Updating {} buffer size from {} to {}",
                self.label,
                self.buffer.capacity,
                new_capacity
            );
            self.buffer = StructuredBuffer::new(device, new_capacity)?;
        }
        self.buffer.update(data);
        Ok(())
    }
}

impl<T> ConstantBuffer<T> {
    fn new(device: &ID3D12Device) -> Result<Self> {
        let size_in_bytes = align_up(
            size_of::<T>(),
            D3D12_CONSTANT_BUFFER_DATA_PLACEMENT_ALIGNMENT as usize,
        );
        let resource = create_upload_buffer_resource(device, size_in_bytes)?;
        let mapped_ptr = map_upload_resource(&resource)?;
        Ok(Self {
            resource,
            mapped_ptr,
            _marker: PhantomData,
        })
    }

    fn update(&self, value: &T) {
        unsafe {
            copy_nonoverlapping(
                (value as *const T).cast::<u8>(),
                self.mapped_ptr,
                size_of::<T>(),
            );
        }
    }

    fn gpu_virtual_address(&self) -> u64 {
        unsafe { self.resource.GetGPUVirtualAddress() }
    }
}

impl<T> StructuredBuffer<T> {
    fn new(device: &ID3D12Device, capacity: usize) -> Result<Self> {
        let capacity = capacity.max(1);
        let resource = create_upload_buffer_resource(device, size_of::<T>() * capacity)?;
        let mapped_ptr = map_upload_resource(&resource)?;
        Ok(Self {
            resource,
            mapped_ptr,
            capacity,
            _marker: PhantomData,
        })
    }

    fn update(&self, data: &[T]) {
        if data.is_empty() {
            return;
        }
        unsafe {
            copy_nonoverlapping(
                data.as_ptr().cast::<u8>(),
                self.mapped_ptr,
                std::mem::size_of_val(data),
            );
        }
    }

    fn gpu_virtual_address(&self, first_element: u32) -> u64 {
        unsafe {
            self.resource.GetGPUVirtualAddress()
                + (first_element as usize * size_of::<T>()) as u64
        }
    }
}

impl DirectX12ImmediateExecutor {
    fn new(device: &ID3D12Device) -> Result<Self> {
        let command_allocator = unsafe {
            device
                .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)
                .context("Creating Direct3D 12 command allocator")?
        };
        let command_list = unsafe {
            device
                .CreateCommandList::<_, _, ID3D12GraphicsCommandList>(
                    0,
                    D3D12_COMMAND_LIST_TYPE_DIRECT,
                    &command_allocator,
                    Option::<&ID3D12PipelineState>::None,
                )
                .context("Creating Direct3D 12 graphics command list")?
        };
        unsafe {
            command_list
                .Close()
                .context("Closing initial Direct3D 12 command list")?;
        }
        let fence = unsafe {
            device
                .CreateFence(0, D3D12_FENCE_FLAG_NONE)
                .context("Creating Direct3D 12 fence")?
        };

        Ok(Self {
            command_allocator,
            command_list,
            fence,
            next_fence_value: 1,
        })
    }

    fn reset(&self) -> Result<()> {
        unsafe {
            self.command_allocator
                .Reset()
                .context("Resetting Direct3D 12 command allocator")?;
            self.command_list
                .Reset(&self.command_allocator, None)
                .context("Resetting Direct3D 12 command list")?;
        }
        Ok(())
    }

    fn release_recorded_references(&self) -> Result<()> {
        unsafe {
            self.command_allocator
                .Reset()
                .context("Resetting Direct3D 12 command allocator before resize")?;
            self.command_list
                .Reset(&self.command_allocator, None)
                .context("Resetting Direct3D 12 command list before resize")?;
            self.command_list.ClearState(None);
            self.command_list
                .Close()
                .context("Closing Direct3D 12 command list before resize")?;
        }
        Ok(())
    }

    fn execute_and_wait(&mut self, command_queue: &ID3D12CommandQueue) -> Result<()> {
        let command_list: ID3D12CommandList = self
            .command_list
            .cast()
            .context("Casting Direct3D 12 graphics command list")?;
        unsafe {
            command_queue.ExecuteCommandLists(&[Some(command_list)]);
        }
        self.wait_for_gpu_idle(command_queue)
    }

    fn wait_for_gpu_idle(&mut self, command_queue: &ID3D12CommandQueue) -> Result<()> {
        let fence_value = self.next_fence_value;
        self.next_fence_value += 1;
        unsafe {
            command_queue
                .Signal(&self.fence, fence_value)
                .context("Signaling Direct3D 12 fence")?;
        }
        while unsafe { self.fence.GetCompletedValue() } < fence_value {
            thread::yield_now();
        }
        Ok(())
    }
}

impl DirectX12Atlas {
    fn new(device: &ID3D12Device, command_queue: &ID3D12CommandQueue) -> Result<Self> {
        Ok(Self(Mutex::new(DirectX12AtlasState {
            device: device.clone(),
            command_queue: command_queue.clone(),
            executor: DirectX12ImmediateExecutor::new(device)
                .context("Creating Direct3D 12 atlas uploader")?,
            monochrome_textures: Default::default(),
            polychrome_textures: Default::default(),
            subpixel_textures: Default::default(),
            tiles_by_key: Default::default(),
        })))
    }

    fn handle_device_lost(
        &self,
        device: &ID3D12Device,
        command_queue: &ID3D12CommandQueue,
    ) -> Result<()> {
        let mut state = self.0.lock();
        state.device = device.clone();
        state.command_queue = command_queue.clone();
        state.executor = DirectX12ImmediateExecutor::new(device)
            .context("Recreating Direct3D 12 atlas uploader")?;
        state.monochrome_textures = AtlasTextureList::default();
        state.polychrome_textures = AtlasTextureList::default();
        state.subpixel_textures = AtlasTextureList::default();
        state.tiles_by_key.clear();
        Ok(())
    }

    fn get_texture_binding(&self, id: AtlasTextureId) -> Option<ShaderTextureBinding> {
        let state = self.0.lock();
        state.texture(id).map(|texture| ShaderTextureBinding {
            resource: texture.resource.clone(),
            format: texture.format,
        })
    }
}

impl PlatformAtlas for DirectX12Atlas {
    fn get_or_insert_with<'a>(
        &self,
        key: &AtlasKey,
        build: &mut dyn FnMut() -> anyhow::Result<Option<(Size<DevicePixels>, Cow<'a, [u8]>)>>,
    ) -> anyhow::Result<Option<AtlasTile>> {
        let mut state = self.0.lock();
        if let Some(tile) = state.tiles_by_key.get(key) {
            return Ok(Some(tile.clone()));
        }

        let Some((size, bytes)) = build()? else {
            return Ok(None);
        };

        let tile = state
            .allocate(size, key.texture_kind())
            .ok_or_else(|| anyhow::anyhow!("failed to allocate Direct3D 12 atlas tile"))?;
        state.upload(tile.texture_id, tile.bounds, &bytes)?;
        state.tiles_by_key.insert(key.clone(), tile.clone());
        Ok(Some(tile))
    }

    fn remove(&self, key: &AtlasKey) {
        let mut state = self.0.lock();
        let Some(tile) = state.tiles_by_key.remove(key) else {
            return;
        };
        let textures = state.texture_list_mut(tile.texture_id.kind);
        let Some(slot) = textures.textures.get_mut(tile.texture_id.index as usize) else {
            return;
        };
        let Some(texture) = slot.as_mut() else {
            return;
        };
        texture.deallocate(tile.tile_id);
        texture.decrement_ref_count();
        if texture.is_unreferenced() {
            textures.free_list.push(texture.id.index as usize);
            *slot = None;
        }
    }
}

impl DirectX12AtlasState {
    fn allocate(
        &mut self,
        size: Size<DevicePixels>,
        texture_kind: AtlasTextureKind,
    ) -> Option<AtlasTile> {
        {
            let textures = self.texture_list_mut(texture_kind);
            if let Some(tile) = textures
                .iter_mut()
                .rev()
                .find_map(|texture| texture.allocate(size))
            {
                return Some(tile);
            }
        }

        let texture = self.push_texture(size, texture_kind)?;
        texture.allocate(size)
    }

    fn upload(
        &mut self,
        texture_id: AtlasTextureId,
        bounds: Bounds<DevicePixels>,
        bytes: &[u8],
    ) -> Result<()> {
        let texture = self
            .texture(texture_id)
            .context("Direct3D 12 atlas texture missing during upload")?;
        let upload_buffer_size =
            required_texture_upload_size(&self.device, bounds.size, texture.format)?;
        let upload_buffer = create_upload_buffer_resource(&self.device, upload_buffer_size)
            .context("Creating Direct3D 12 atlas upload buffer")?;
        write_texture_upload_buffer(
            &self.device,
            &upload_buffer,
            bounds.size,
            texture.bytes_per_pixel,
            bytes,
            texture.format,
        )?;

        self.executor.reset()?;
        let command_list = &self.executor.command_list;
        unsafe {
            let to_copy_dest = transition_barrier(
                &texture.resource,
                D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE
                    | D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE,
                D3D12_RESOURCE_STATE_COPY_DEST,
            );
            command_list.ResourceBarrier(&[to_copy_dest]);

            let footprint = texture_upload_footprint(&self.device, bounds.size, texture.format)?;
            let src = D3D12_TEXTURE_COPY_LOCATION {
                pResource: ManuallyDrop::new(Some(upload_buffer.clone())),
                Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                    PlacedFootprint: footprint,
                },
            };
            let dst = D3D12_TEXTURE_COPY_LOCATION {
                pResource: ManuallyDrop::new(Some(texture.resource.clone())),
                Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: 0 },
            };
            command_list.CopyTextureRegion(
                &dst,
                bounds.left().0 as u32,
                bounds.top().0 as u32,
                0,
                &src,
                None,
            );

            let to_shader_read = transition_barrier(
                &texture.resource,
                D3D12_RESOURCE_STATE_COPY_DEST,
                D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE
                    | D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE,
            );
            command_list.ResourceBarrier(&[to_shader_read]);
            command_list
                .Close()
                .context("Closing Direct3D 12 atlas upload command list")?;
        }

        self.executor.execute_and_wait(&self.command_queue)
    }

    fn push_texture(
        &mut self,
        min_size: Size<DevicePixels>,
        kind: AtlasTextureKind,
    ) -> Option<&mut DirectX12AtlasTexture> {
        const DEFAULT_ATLAS_SIZE: Size<DevicePixels> = Size {
            width: DevicePixels(1024),
            height: DevicePixels(1024),
        };
        const MAX_ATLAS_SIZE: Size<DevicePixels> = Size {
            width: DevicePixels(16384),
            height: DevicePixels(16384),
        };

        let size = min_size.min(&MAX_ATLAS_SIZE).max(&DEFAULT_ATLAS_SIZE);
        let (format, bytes_per_pixel) = match kind {
            AtlasTextureKind::Monochrome => (DXGI_FORMAT_R8_UNORM, 1),
            AtlasTextureKind::Polychrome => (DXGI_FORMAT_B8G8R8A8_UNORM, 4),
            AtlasTextureKind::Subpixel => (DXGI_FORMAT_R8G8B8A8_UNORM, 4),
        };
        let texture = create_texture_resource(
            &self.device,
            size.width.0 as u32,
            size.height.0 as u32,
            format,
            1,
            D3D12_RESOURCE_FLAG_NONE,
            D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE
                | D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE,
        )
        .ok()?;

        let textures = self.texture_list_mut(kind);
        let index = textures.free_list.pop();
        let atlas_texture = DirectX12AtlasTexture {
            id: AtlasTextureId {
                index: index.unwrap_or(textures.textures.len()) as u32,
                kind,
            },
            bytes_per_pixel,
            allocator: BucketedAtlasAllocator::new(device_size_to_etagere(size)),
            resource: texture,
            format,
            live_atlas_keys: 0,
        };
        if let Some(index) = index {
            textures.textures[index] = Some(atlas_texture);
            textures.textures.get_mut(index).unwrap().as_mut()
        } else {
            textures.textures.push(Some(atlas_texture));
            textures.textures.last_mut().unwrap().as_mut()
        }
    }

    fn texture_list_mut(
        &mut self,
        kind: AtlasTextureKind,
    ) -> &mut AtlasTextureList<DirectX12AtlasTexture> {
        match kind {
            AtlasTextureKind::Monochrome => &mut self.monochrome_textures,
            AtlasTextureKind::Polychrome => &mut self.polychrome_textures,
            AtlasTextureKind::Subpixel => &mut self.subpixel_textures,
        }
    }

    fn texture(&self, id: AtlasTextureId) -> Option<&DirectX12AtlasTexture> {
        match id.kind {
            AtlasTextureKind::Monochrome => self.monochrome_textures.textures[id.index as usize]
                .as_ref(),
            AtlasTextureKind::Polychrome => self.polychrome_textures.textures[id.index as usize]
                .as_ref(),
            AtlasTextureKind::Subpixel => self.subpixel_textures.textures[id.index as usize]
                .as_ref(),
        }
    }
}

impl DirectX12AtlasTexture {
    fn allocate(&mut self, size: Size<DevicePixels>) -> Option<AtlasTile> {
        let allocation = self.allocator.allocate(device_size_to_etagere(size))?;
        let tile = AtlasTile {
            texture_id: self.id,
            tile_id: allocation.id.into(),
            bounds: Bounds {
                origin: etagere_point_to_device(allocation.rectangle.min),
                size,
            },
            padding: 0,
        };
        self.live_atlas_keys += 1;
        Some(tile)
    }

    fn deallocate(&mut self, tile_id: TileId) {
        self.allocator.deallocate(tile_id.into());
    }

    fn decrement_ref_count(&mut self) {
        self.live_atlas_keys -= 1;
    }

    fn is_unreferenced(&self) -> bool {
        self.live_atlas_keys == 0
    }
}

fn create_swap_chain(
    dxgi_factory: &IDXGIFactory6,
    command_queue: &ID3D12CommandQueue,
    hwnd: HWND,
    width: u32,
    height: u32,
    disable_direct_composition: bool,
) -> Result<IDXGISwapChain3> {
    let desc = DXGI_SWAP_CHAIN_DESC1 {
        Width: width,
        Height: height,
        Format: RENDER_TARGET_FORMAT,
        Stereo: false.into(),
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
        BufferCount: BUFFER_COUNT as u32,
        Scaling: if disable_direct_composition {
            DXGI_SCALING_NONE
        } else {
            DXGI_SCALING_STRETCH
        },
        SwapEffect: DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
        AlphaMode: if disable_direct_composition {
            DXGI_ALPHA_MODE_IGNORE
        } else {
            DXGI_ALPHA_MODE_PREMULTIPLIED
        },
        Flags: 0,
    };

    let swap_chain = if disable_direct_composition {
        unsafe {
            dxgi_factory
                .CreateSwapChainForHwnd(
                    command_queue,
                    hwnd,
                    &desc,
                    None,
                    Option::<&IDXGIOutput>::None,
                )
                .context("Creating Direct3D 12 swap chain for hwnd")?
        }
    } else {
        unsafe {
            dxgi_factory
                .CreateSwapChainForComposition(command_queue, &desc, Option::<&IDXGIOutput>::None)
                .context("Creating Direct3D 12 swap chain for composition")?
        }
    };

    if disable_direct_composition {
        unsafe {
            dxgi_factory
                .MakeWindowAssociation(hwnd, DXGI_MWA_NO_ALT_ENTER)
                .context("Associating Direct3D 12 swap chain window")?;
        }
    }

    swap_chain
        .cast()
        .context("Casting Direct3D 12 swap chain to IDXGISwapChain3")
}

fn create_back_buffers(
    devices: &DirectX12RendererDevices,
    swap_chain: &IDXGISwapChain3,
) -> Result<Vec<DirectX12BackBuffer>> {
    let mut back_buffers = Vec::with_capacity(BUFFER_COUNT);
    for index in 0..BUFFER_COUNT {
        let resource: ID3D12Resource = unsafe {
            swap_chain
                .GetBuffer(index as u32)
                .context("Getting Direct3D 12 swap chain back buffer")?
        };
        let rtv_handle = devices.rtv_handle(index);
        unsafe {
            devices
                .device
                .CreateRenderTargetView(&resource, None, rtv_handle);
        }
        back_buffers.push(DirectX12BackBuffer {
            resource,
            rtv_handle,
        });
    }
    Ok(back_buffers)
}

fn create_root_signature(device: &ID3D12Device) -> Result<ID3D12RootSignature> {
    let texture_range = D3D12_DESCRIPTOR_RANGE {
        RangeType: D3D12_DESCRIPTOR_RANGE_TYPE_SRV,
        NumDescriptors: 1,
        BaseShaderRegister: 0,
        RegisterSpace: 0,
        OffsetInDescriptorsFromTableStart: 0,
    };
    let texture_table = D3D12_ROOT_DESCRIPTOR_TABLE {
        NumDescriptorRanges: 1,
        pDescriptorRanges: &texture_range,
    };
    let parameters = [
        D3D12_ROOT_PARAMETER {
            ParameterType: D3D12_ROOT_PARAMETER_TYPE_CBV,
            Anonymous: D3D12_ROOT_PARAMETER_0 {
                Descriptor: D3D12_ROOT_DESCRIPTOR {
                    ShaderRegister: 0,
                    RegisterSpace: 0,
                },
            },
            ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
        },
        D3D12_ROOT_PARAMETER {
            ParameterType: D3D12_ROOT_PARAMETER_TYPE_SRV,
            Anonymous: D3D12_ROOT_PARAMETER_0 {
                Descriptor: D3D12_ROOT_DESCRIPTOR {
                    ShaderRegister: 1,
                    RegisterSpace: 0,
                },
            },
            ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
        },
        D3D12_ROOT_PARAMETER {
            ParameterType: D3D12_ROOT_PARAMETER_TYPE_DESCRIPTOR_TABLE,
            Anonymous: D3D12_ROOT_PARAMETER_0 {
                DescriptorTable: texture_table,
            },
            ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
        },
    ];
    let static_sampler = D3D12_STATIC_SAMPLER_DESC {
        Filter: D3D12_FILTER_MIN_MAG_MIP_LINEAR,
        AddressU: D3D12_TEXTURE_ADDRESS_MODE_WRAP,
        AddressV: D3D12_TEXTURE_ADDRESS_MODE_WRAP,
        AddressW: D3D12_TEXTURE_ADDRESS_MODE_WRAP,
        MipLODBias: 0.0,
        MaxAnisotropy: 1,
        ComparisonFunc: D3D12_COMPARISON_FUNC_ALWAYS,
        BorderColor: D3D12_STATIC_BORDER_COLOR_TRANSPARENT_BLACK,
        MinLOD: 0.0,
        MaxLOD: D3D12_FLOAT32_MAX,
        ShaderRegister: 0,
        RegisterSpace: 0,
        ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
    };
    let desc = D3D12_ROOT_SIGNATURE_DESC {
        NumParameters: parameters.len() as u32,
        pParameters: parameters.as_ptr(),
        NumStaticSamplers: 1,
        pStaticSamplers: &static_sampler,
        Flags: D3D12_ROOT_SIGNATURE_FLAG_ALLOW_INPUT_ASSEMBLER_INPUT_LAYOUT,
    };

    let mut blob = None;
    let mut error_blob = None;
    let result = unsafe {
        D3D12SerializeRootSignature(
            &desc,
            D3D_ROOT_SIGNATURE_VERSION_1,
            &mut blob,
            Some(&mut error_blob),
        )
    };
    if let Err(error) = result {
        let details = error_blob
            .map(blob_to_string)
            .unwrap_or_else(|| error.message().to_string());
        anyhow::bail!("Serializing Direct3D 12 root signature failed: {details}");
    }
    let blob = blob.context("Missing Direct3D 12 root signature blob")?;

    let blob_bytes =
        unsafe { slice::from_raw_parts(blob.GetBufferPointer() as *const u8, blob.GetBufferSize()) };
    unsafe { device.CreateRootSignature(0, blob_bytes) }.context("Creating Direct3D 12 root signature")
}

fn create_graphics_pipeline_state(
    device: &ID3D12Device,
    root_signature: &ID3D12RootSignature,
    vertex_shader: &[u8],
    fragment_shader: &[u8],
    blend_state: D3D12_BLEND_DESC,
    sample_count: u32,
) -> Result<ID3D12PipelineState> {
    let mut rtv_formats = [DXGI_FORMAT_UNKNOWN; 8];
    rtv_formats[0] = RENDER_TARGET_FORMAT;

    let desc = D3D12_GRAPHICS_PIPELINE_STATE_DESC {
        pRootSignature: ManuallyDrop::new(Some(root_signature.clone())),
        VS: D3D12_SHADER_BYTECODE {
            pShaderBytecode: vertex_shader.as_ptr() as _,
            BytecodeLength: vertex_shader.len(),
        },
        PS: D3D12_SHADER_BYTECODE {
            pShaderBytecode: fragment_shader.as_ptr() as _,
            BytecodeLength: fragment_shader.len(),
        },
        BlendState: blend_state,
        SampleMask: u32::MAX,
        RasterizerState: D3D12_RASTERIZER_DESC {
            FillMode: D3D12_FILL_MODE_SOLID,
            CullMode: D3D12_CULL_MODE_NONE,
            FrontCounterClockwise: false.into(),
            DepthBias: D3D12_DEFAULT_DEPTH_BIAS as i32,
            DepthBiasClamp: D3D12_DEFAULT_DEPTH_BIAS_CLAMP,
            SlopeScaledDepthBias: D3D12_DEFAULT_SLOPE_SCALED_DEPTH_BIAS,
            DepthClipEnable: true.into(),
            MultisampleEnable: true.into(),
            AntialiasedLineEnable: false.into(),
            ForcedSampleCount: 0,
            ConservativeRaster: D3D12_CONSERVATIVE_RASTERIZATION_MODE_OFF,
        },
        DepthStencilState: D3D12_DEPTH_STENCIL_DESC {
            DepthEnable: false.into(),
            StencilEnable: false.into(),
            ..Default::default()
        },
        InputLayout: D3D12_INPUT_LAYOUT_DESC {
            pInputElementDescs: null(),
            NumElements: 0,
        },
        IBStripCutValue: D3D12_INDEX_BUFFER_STRIP_CUT_VALUE_DISABLED,
        PrimitiveTopologyType: D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE,
        NumRenderTargets: 1,
        RTVFormats: rtv_formats,
        DSVFormat: DXGI_FORMAT_UNKNOWN,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: sample_count,
            Quality: 0,
        },
        NodeMask: 0,
        CachedPSO: D3D12_CACHED_PIPELINE_STATE::default(),
        Flags: D3D12_PIPELINE_STATE_FLAG_NONE,
        ..Default::default()
    };

    unsafe {
        device
            .CreateGraphicsPipelineState(&desc)
            .context("Creating Direct3D 12 graphics pipeline state")
    }
}

fn create_upload_buffer_resource(
    device: &ID3D12Device,
    size_in_bytes: usize,
) -> Result<ID3D12Resource> {
    let heap_properties = D3D12_HEAP_PROPERTIES {
        Type: D3D12_HEAP_TYPE_UPLOAD,
        CPUPageProperty: D3D12_CPU_PAGE_PROPERTY_UNKNOWN,
        MemoryPoolPreference: D3D12_MEMORY_POOL_UNKNOWN,
        CreationNodeMask: 1,
        VisibleNodeMask: 1,
    };
    let resource_desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Alignment: 0,
        Width: size_in_bytes as u64,
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        Format: DXGI_FORMAT_UNKNOWN,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        Flags: D3D12_RESOURCE_FLAG_NONE,
    };

    let mut resource = None;
    unsafe {
        device
            .CreateCommittedResource(
                &heap_properties,
                D3D12_HEAP_FLAG_NONE,
                &resource_desc,
                D3D12_RESOURCE_STATE_GENERIC_READ,
                None,
                &mut resource,
            )
            .context("Creating Direct3D 12 upload buffer")?;
    }
    resource.context("Direct3D 12 upload buffer was not created")
}

fn create_texture_resource(
    device: &ID3D12Device,
    width: u32,
    height: u32,
    format: DXGI_FORMAT,
    sample_count: u32,
    flags: D3D12_RESOURCE_FLAGS,
    initial_state: D3D12_RESOURCE_STATES,
) -> Result<ID3D12Resource> {
    let heap_properties = D3D12_HEAP_PROPERTIES {
        Type: D3D12_HEAP_TYPE_DEFAULT,
        CPUPageProperty: D3D12_CPU_PAGE_PROPERTY_UNKNOWN,
        MemoryPoolPreference: D3D12_MEMORY_POOL_UNKNOWN,
        CreationNodeMask: 1,
        VisibleNodeMask: 1,
    };
    let resource_desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
        Alignment: 0,
        Width: width as u64,
        Height: height,
        DepthOrArraySize: 1,
        MipLevels: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: sample_count,
            Quality: 0,
        },
        Layout: D3D12_TEXTURE_LAYOUT_UNKNOWN,
        Flags: flags,
    };
    let clear_value = if flags.contains(D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET) {
        Some(D3D12_CLEAR_VALUE {
            Format: format,
            Anonymous: D3D12_CLEAR_VALUE_0 {
                Color: [0.0, 0.0, 0.0, 0.0],
            },
        })
    } else {
        None
    };

    let mut resource = None;
    unsafe {
        device
            .CreateCommittedResource(
                &heap_properties,
                D3D12_HEAP_FLAG_NONE,
                &resource_desc,
                initial_state,
                clear_value.as_ref().map(|value| value as *const _),
                &mut resource,
            )
            .context("Creating Direct3D 12 texture resource")?;
    }
    resource.context("Direct3D 12 texture resource was not created")
}

fn map_upload_resource(resource: &ID3D12Resource) -> Result<*mut u8> {
    let mut mapped_ptr: *mut c_void = null_mut();
    let read_range = D3D12_RANGE { Begin: 0, End: 0 };
    unsafe {
        resource
            .Map(0, Some(&read_range), Some(&mut mapped_ptr))
            .context("Mapping Direct3D 12 upload resource")?;
    }
    Ok(mapped_ptr.cast())
}

fn required_texture_upload_size(
    device: &ID3D12Device,
    size: Size<DevicePixels>,
    format: DXGI_FORMAT,
) -> Result<usize> {
    let footprint = texture_upload_footprint(device, size, format)?;
    Ok((footprint.Offset + footprint.Footprint.RowPitch as u64 * size.height.0 as u64) as usize)
}

fn texture_upload_footprint(
    device: &ID3D12Device,
    size: Size<DevicePixels>,
    format: DXGI_FORMAT,
) -> Result<D3D12_PLACED_SUBRESOURCE_FOOTPRINT> {
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
        Alignment: 0,
        Width: size.width.0 as u64,
        Height: size.height.0 as u32,
        DepthOrArraySize: 1,
        MipLevels: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Layout: D3D12_TEXTURE_LAYOUT_UNKNOWN,
        Flags: D3D12_RESOURCE_FLAG_NONE,
    };

    let mut footprint = D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default();
    let mut num_rows = 0;
    let mut row_size_in_bytes = 0u64;
    let mut total_bytes = 0u64;
    unsafe {
        device.GetCopyableFootprints(
            &desc,
            0,
            1,
            0,
            Some(&mut footprint),
            Some(&mut num_rows),
            Some(&mut row_size_in_bytes),
            Some(&mut total_bytes),
        );
    }
    if num_rows == 0 || total_bytes == 0 {
        anyhow::bail!("Failed to compute Direct3D 12 texture upload footprint");
    }
    Ok(footprint)
}

fn write_texture_upload_buffer(
    device: &ID3D12Device,
    upload_buffer: &ID3D12Resource,
    size: Size<DevicePixels>,
    bytes_per_pixel: u32,
    bytes: &[u8],
    format: DXGI_FORMAT,
) -> Result<()> {
    let footprint = texture_upload_footprint(device, size, format)?;
    let row_pitch = footprint.Footprint.RowPitch as usize;
    let row_count = size.height.0.max(0) as usize;
    let src_row_pitch = size.width.to_bytes(bytes_per_pixel as u8) as usize;
    let expected_len = src_row_pitch
        .checked_mul(row_count)
        .context("Computing Direct3D 12 source upload byte count")?;
    anyhow::ensure!(
        bytes.len() == expected_len,
        "Direct3D 12 upload byte count mismatch: expected {} bytes for {:?} at {} Bpp, got {}",
        expected_len,
        size,
        bytes_per_pixel,
        bytes.len()
    );
    anyhow::ensure!(
        row_pitch >= src_row_pitch,
        "Direct3D 12 upload row pitch {} is smaller than source row pitch {} for {:?}",
        row_pitch,
        src_row_pitch,
        size
    );
    if expected_len == 0 {
        return Ok(());
    }

    let mapped_ptr = map_upload_resource(upload_buffer)
        .context("Mapping Direct3D 12 texture upload buffer")?;
    anyhow::ensure!(
        !mapped_ptr.is_null(),
        "Direct3D 12 texture upload buffer map returned a null pointer"
    );

    let mapped_len = row_pitch
        .checked_mul(row_count)
        .context("Computing Direct3D 12 mapped upload byte count")?;
    unsafe {
        let mapped_bytes = slice::from_raw_parts_mut(mapped_ptr, mapped_len);
        mapped_bytes.fill(0);
        for (src_row, dst_row) in bytes
            .chunks_exact(src_row_pitch)
            .zip(mapped_bytes.chunks_exact_mut(row_pitch))
        {
            dst_row[..src_row_pitch].copy_from_slice(src_row);
        }
        upload_buffer.Unmap(0, None);
    }
    Ok(())
}

fn texture_srv_desc(format: DXGI_FORMAT) -> D3D12_SHADER_RESOURCE_VIEW_DESC {
    D3D12_SHADER_RESOURCE_VIEW_DESC {
        Format: format,
        ViewDimension: D3D12_SRV_DIMENSION_TEXTURE2D,
        Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
        Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
            Texture2D: D3D12_TEX2D_SRV {
                MostDetailedMip: 0,
                MipLevels: 1,
                PlaneSlice: 0,
                ResourceMinLODClamp: 0.0,
            },
        },
    }
}

fn create_default_blend_state() -> D3D12_BLEND_DESC {
    let mut desc = D3D12_BLEND_DESC::default();
    desc.RenderTarget[0] = D3D12_RENDER_TARGET_BLEND_DESC {
        BlendEnable: true.into(),
        LogicOpEnable: false.into(),
        SrcBlend: D3D12_BLEND_SRC_ALPHA,
        DestBlend: D3D12_BLEND_INV_SRC_ALPHA,
        BlendOp: D3D12_BLEND_OP_ADD,
        SrcBlendAlpha: D3D12_BLEND_ONE,
        DestBlendAlpha: D3D12_BLEND_ONE,
        BlendOpAlpha: D3D12_BLEND_OP_ADD,
        LogicOp: D3D12_LOGIC_OP_NOOP,
        RenderTargetWriteMask: D3D12_COLOR_WRITE_ENABLE_ALL.0 as u8,
    };
    desc
}

fn create_subpixel_blend_state() -> D3D12_BLEND_DESC {
    let mut desc = D3D12_BLEND_DESC::default();
    desc.RenderTarget[0] = D3D12_RENDER_TARGET_BLEND_DESC {
        BlendEnable: true.into(),
        LogicOpEnable: false.into(),
        SrcBlend: D3D12_BLEND_SRC1_COLOR,
        DestBlend: D3D12_BLEND_INV_SRC1_COLOR,
        BlendOp: D3D12_BLEND_OP_ADD,
        SrcBlendAlpha: D3D12_BLEND_ONE,
        DestBlendAlpha: D3D12_BLEND_ZERO,
        BlendOpAlpha: D3D12_BLEND_OP_ADD,
        LogicOp: D3D12_LOGIC_OP_NOOP,
        RenderTargetWriteMask: D3D12_COLOR_WRITE_ENABLE_ALL.0 as u8
            & !D3D12_COLOR_WRITE_ENABLE_ALPHA.0 as u8,
    };
    desc
}

fn create_path_rasterization_blend_state() -> D3D12_BLEND_DESC {
    let mut desc = D3D12_BLEND_DESC::default();
    desc.RenderTarget[0] = D3D12_RENDER_TARGET_BLEND_DESC {
        BlendEnable: true.into(),
        LogicOpEnable: false.into(),
        SrcBlend: D3D12_BLEND_ONE,
        DestBlend: D3D12_BLEND_INV_SRC_ALPHA,
        BlendOp: D3D12_BLEND_OP_ADD,
        SrcBlendAlpha: D3D12_BLEND_ONE,
        DestBlendAlpha: D3D12_BLEND_INV_SRC_ALPHA,
        BlendOpAlpha: D3D12_BLEND_OP_ADD,
        LogicOp: D3D12_LOGIC_OP_NOOP,
        RenderTargetWriteMask: D3D12_COLOR_WRITE_ENABLE_ALL.0 as u8,
    };
    desc
}

fn create_path_sprite_blend_state() -> D3D12_BLEND_DESC {
    let mut desc = D3D12_BLEND_DESC::default();
    desc.RenderTarget[0] = D3D12_RENDER_TARGET_BLEND_DESC {
        BlendEnable: true.into(),
        LogicOpEnable: false.into(),
        SrcBlend: D3D12_BLEND_ONE,
        DestBlend: D3D12_BLEND_INV_SRC_ALPHA,
        BlendOp: D3D12_BLEND_OP_ADD,
        SrcBlendAlpha: D3D12_BLEND_ONE,
        DestBlendAlpha: D3D12_BLEND_ONE,
        BlendOpAlpha: D3D12_BLEND_OP_ADD,
        LogicOp: D3D12_LOGIC_OP_NOOP,
        RenderTargetWriteMask: D3D12_COLOR_WRITE_ENABLE_ALL.0 as u8,
    };
    desc
}

fn transition_barrier(
    resource: &ID3D12Resource,
    before: D3D12_RESOURCE_STATES,
    after: D3D12_RESOURCE_STATES,
) -> D3D12_RESOURCE_BARRIER {
    D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
        Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
        Anonymous: D3D12_RESOURCE_BARRIER_0 {
            Transition: ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
                pResource: ManuallyDrop::new(Some(resource.clone())),
                Subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                StateBefore: before,
                StateAfter: after,
            }),
        },
    }
}

fn create_viewport(width: f32, height: f32) -> D3D12_VIEWPORT {
    D3D12_VIEWPORT {
        TopLeftX: 0.0,
        TopLeftY: 0.0,
        Width: width,
        Height: height,
        MinDepth: 0.0,
        MaxDepth: 1.0,
    }
}

fn create_scissor_rect(width: u32, height: u32) -> RECT {
    RECT {
        left: 0,
        top: 0,
        right: width as i32,
        bottom: height as i32,
    }
}

fn device_size_to_etagere(size: Size<DevicePixels>) -> etagere::Size {
    etagere::Size::new(size.width.into(), size.height.into())
}

fn etagere_point_to_device(value: etagere::Point) -> Point<DevicePixels> {
    Point {
        x: DevicePixels::from(value.x),
        y: DevicePixels::from(value.y),
    }
}

fn blob_to_string(blob: windows::Win32::Graphics::Direct3D::ID3DBlob) -> String {
    unsafe {
        let bytes = slice::from_raw_parts(
            blob.GetBufferPointer() as *const u8,
            blob.GetBufferSize(),
        );
        String::from_utf8_lossy(bytes).into_owned()
    }
}

fn align_up(value: usize, alignment: usize) -> usize {
    (value + alignment - 1) & !(alignment - 1)
}

fn get_dxgi_driver_version(adapter: &IDXGIAdapter1) -> Result<String> {
    let number = unsafe { adapter.CheckInterfaceSupport(&IDXGIDevice::IID as _) }?;
    Ok(format!(
        "{}.{}.{}.{}",
        number >> 48,
        (number >> 32) & 0xFFFF,
        (number >> 16) & 0xFFFF,
        number & 0xFFFF
    ))
}
