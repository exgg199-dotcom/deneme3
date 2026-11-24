//! The current checked shader API reaches its post, tonemap and linear runtime pipelines.
use super::*;
use horizon_shader_api::{MANIFEST, Pack, check};

fn checked(api: &str, extra: &str, tonemap: bool) -> Report {
    let stage = if tonemap { "[tonemap]" } else { "[[post]]" };
    let source = if tonemap {
        "fn tonemap(input: TonemapInput) -> vec3<f32> { return input.color; }"
    } else {
        // Return known linear light and read a field of the current frame.
        "fn post(input: PostInput) -> vec4<f32> { return vec4<f32>(pow(vec3<f32>(16.0/255.0,128.0/255.0,240.0/255.0),vec3<f32>(2.2)),1.0+frame.time*0.0); }"
    };
    let manifest = format!(
        "schema=1\nid=\"runtime_control\"\nname=\"Runtime control\"\nversion=\"1\"\napi=\"{api}\"\n{stage}\nfile=\"stage.wgsl\"\n{extra}"
    );
    let files = [(MANIFEST, manifest.as_str()), ("stage.wgsl", source)]
        .map(|(name, source)| (name.to_owned(), source.as_bytes().to_vec()))
        .into();
    check(&Pack::from_files(files).unwrap()).unwrap()
}

#[test]
fn checked_current_api_builds_post_tonemap_and_linear_variants() {
    let _gpu = crate::gfx::test_gpu::lock();
    let instance = crate::gfx::test_gpu::instance();
    let Ok(adapter) = pollster::block_on(instance.request_adapter(&Default::default())) else {
        return;
    };
    eprintln!("Native API runtime adapter: {:?}", adapter.get_info());
    let (device, queue) = pollster::block_on(adapter.request_device(&Default::default())).unwrap();
    let empty = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("No terrain in the post-only API control"),
        entries: &[],
    });
    let terrain = TerrainShape {
        globals: &empty,
        flat: None,
        looks: None,
        sprites: [1.0; 2],
        sprite_views: &[],
        format: wgpu::TextureFormat::Rgba8Unorm,
        linear: false,
        sprite_tags: &[],
        sprite_materials: &[],
        entities: &empty,
        retained_entities: &empty,
        weather_groups: [&empty, &empty],
        pbr: None,
    };
    let (mut current, labels) = NativePack::new(
        &device,
        &queue,
        &terrain,
        checked(horizon_shader_api::API, "", false),
        &[],
        Default::default(),
    )
    .unwrap();
    assert!(!current.post.labels().is_empty());
    assert_eq!(labels, ["stage.wgsl"]);
    for (extra, tonemap, linear) in [
        ("", true, false),
        ("", false, false),
        ("[frame]\nblend=\"linear\"\n", false, true),
    ] {
        let report = checked(horizon_shader_api::API, extra, tonemap);
        assert!(!report.stages.is_empty());
        let candidate_shape = TerrainShape { linear, ..terrain };
        let (built, labels) = NativePack::new(
            &device,
            &queue,
            &candidate_shape,
            report,
            &[],
            Default::default(),
        )
        .unwrap();
        assert_eq!(built.capabilities().tone_curve, tonemap);
        assert_eq!(labels, ["stage.wgsl"]);
    }

    // The ordinary post stage still executes after constructing the other variants.
    // Frame data has SDR headroom; zeroed uniform storage would clip every pixel.
    let inputs = horizon_shader_api::Frame {
        headroom: 1.0,
        ..Default::default()
    };
    queue.write_buffer(&current.frame.buffer, 0, bytemuck::bytes_of(&inputs));
    let texture = |format, extra| {
        device.create_texture(&wgpu::TextureDescriptor {
            label: Some("Native API runtime pixel control"),
            size: wgpu::Extent3d {
                width: 4,
                height: 4,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                | wgpu::TextureUsages::TEXTURE_BINDING
                | extra,
            view_formats: &[],
        })
    };
    let color = texture(
        wgpu::TextureFormat::Rgba8Unorm,
        wgpu::TextureUsages::empty(),
    );
    let depth = texture(
        wgpu::TextureFormat::Depth32Float,
        wgpu::TextureUsages::empty(),
    );
    let screen = texture(
        wgpu::TextureFormat::Rgba8Unorm,
        wgpu::TextureUsages::COPY_SRC,
    );
    let color_view = color.create_view(&Default::default());
    let depth_view = depth.create_view(&Default::default());
    let screen_view = screen.create_view(&Default::default());
    let mut encoder = device.create_command_encoder(&Default::default());
    encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some("Native API runtime input clear"),
        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
            view: &color_view,
            depth_slice: None,
            resolve_target: None,
            ops: wgpu::Operations {
                load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                store: wgpu::StoreOp::Store,
            },
        })],
        depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
            view: &depth_view,
            depth_ops: Some(wgpu::Operations {
                load: wgpu::LoadOp::Clear(0.5),
                store: wgpu::StoreOp::Store,
            }),
            stencil_ops: None,
        }),
        ..Default::default()
    });
    current.post.run(
        &device,
        &mut encoder,
        (&color_view, &depth_view, &current.frame.surface_view),
        &screen_view,
        [4, 4],
        None,
        None,
    );
    let stride = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let read = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("Native API runtime pixels"),
        size: u64::from(stride * 4),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    encoder.copy_texture_to_buffer(
        screen.as_image_copy(),
        wgpu::TexelCopyBufferInfo {
            buffer: &read,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(stride),
                rows_per_image: Some(4),
            },
        },
        screen.size(),
    );
    queue.submit([encoder.finish()]);
    read.slice(..)
        .map_async(wgpu::MapMode::Read, |result| result.unwrap());
    device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
    let pixels = read.slice(..).get_mapped_range().unwrap();
    for y in 0..4 {
        for x in 0..4 {
            let at = y * stride as usize + x * 4;
            let got = &pixels[at..at + 4];
            assert!(
                got.iter()
                    .zip([16u8, 128, 240, 255])
                    .all(|(&g, w)| g.abs_diff(w) <= 1),
                "{x},{y}: {got:?}"
            );
        }
    }
}
