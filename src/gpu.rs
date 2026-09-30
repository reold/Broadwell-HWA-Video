pub struct Pipelines {
    pub grade_pipeline: wgpu::ComputePipeline,
    pub grade_bgl: wgpu::BindGroupLayout,
    #[allow(dead_code)]
    pub out_texture: wgpu::Texture,
    pub out_view: wgpu::TextureView,
    pub blit_pipeline: wgpu::RenderPipeline,
    pub blit_bg: wgpu::BindGroup,
}

pub fn build_pipelines(
    host: &grafting::HostWgpuContext,
    surface_format: wgpu::TextureFormat,
    vw: u32,
    vh: u32,
) -> Pipelines {
    let out_texture = host.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("graded-output"),
        size: wgpu::Extent3d {
            width: vw,
            height: vh,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::STORAGE_BINDING | wgpu::TextureUsages::TEXTURE_BINDING,
        view_formats: &[],
    });
    let out_view = out_texture.create_view(&wgpu::TextureViewDescriptor::default());

    // ---- grade pipeline ----
    let grade_shader = host
        .device
        .create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("grade-shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("grade.wgsl").into()),
        });
    let grade_bgl = host
        .device
        .create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("grade-bgl"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: false },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::StorageTexture {
                        access: wgpu::StorageTextureAccess::WriteOnly,
                        format: wgpu::TextureFormat::Rgba8Unorm,
                        view_dimension: wgpu::TextureViewDimension::D2,
                    },
                    count: None,
                },
            ],
        });
    let grade_pl = host
        .device
        .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("grade-pl"),
            bind_group_layouts: &[Some(&grade_bgl)],
            immediate_size: 0,
        });
    let grade_pipeline = host
        .device
        .create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("grade-pipeline"),
            layout: Some(&grade_pl),
            module: &grade_shader,
            entry_point: Some("main"),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            cache: None,
        });

    // ---- blit pipeline ----
    let blit_shader = host
        .device
        .create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("blit-shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("blit.wgsl").into()),
        });
    let blit_bgl = host
        .device
        .create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("blit-bgl"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
            ],
        });
    let blit_pl = host
        .device
        .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("blit-pl"),
            bind_group_layouts: &[Some(&blit_bgl)],
            immediate_size: 0,
        });
    let blit_pipeline = host
        .device
        .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("blit-pipeline"),
            layout: Some(&blit_pl),
            vertex: wgpu::VertexState {
                module: &blit_shader,
                entry_point: Some("vs"),
                buffers: &[],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &blit_shader,
                entry_point: Some("fs"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: surface_format,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

    let sampler = host.device.create_sampler(&wgpu::SamplerDescriptor {
        label: Some("blit-sampler"),
        mag_filter: wgpu::FilterMode::Linear,
        min_filter: wgpu::FilterMode::Linear,
        ..Default::default()
    });
    let blit_bg = host.device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("blit-bg"),
        layout: &blit_bgl,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::Sampler(&sampler),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::TextureView(&out_view),
            },
        ],
    });

    Pipelines {
        grade_pipeline,
        grade_bgl,
        out_texture,
        out_view,
        blit_pipeline,
        blit_bg,
    }
}

/// Stable identity of a VAAPI pool slot, derived from fstat on the underlying
/// GEM object. The pointer-based key was tried and rejected because skipping
/// the DMA-BUF acquire barrier per frame caused tearing.
#[derive(Clone, Copy, Hash, Eq, PartialEq)]
pub struct CacheKey {
    pub dev: u64,
    pub ino: u64,
    pub offset: u64,
    pub stride: u64,
    pub modifier: u64,
    pub format: u32,
}

pub struct CachedTexture {
    #[allow(dead_code)]
    pub texture: wgpu::Texture,
    pub view: wgpu::TextureView,
}
