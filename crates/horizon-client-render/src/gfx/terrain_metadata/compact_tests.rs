//! Mixed raw/compact metadata addresses, checked by the shipped GLSL on Metal.
use crate::gfx::{
    terrain_metadata::{BYTES, SLOT_GLSL, TerrainMetadata, WORDS},
    upload::Uploads,
};
use crate::mesher::{QuadLighting, QuadState};
use wgpu::util::DeviceExt;

pub(crate) fn assert_mixed_metadata(device: &wgpu::Device, queue: &wgpu::Queue) {
    let state = |id, fluid| QuadState {
        state: id,
        fluid,
        block: [3, 4, 5],
        lighting: Some(QuadLighting {
            tint: 0xff123456,
            shaded_tint: 0xff654321,
            no_directional: [0x11445566, 0x22556677, 0x33667788, 0x44778899],
        }),
    };
    let mut metadata = TerrainMetadata::default();
    let mut uploads = Uploads::new(device);
    let mut encoder = device.create_command_encoder(&Default::default());
    assert!(metadata.upload(
        device,
        &mut encoder,
        &mut uploads,
        (1, 0, 4),
        (101..105).map(|id| state(id, false))
    ));
    let before = metadata.page(1).clone();
    assert!(metadata.upload(
        device,
        &mut encoder,
        &mut uploads,
        (1, 4, 8),
        [state(999, true); 4]
    ));
    assert_ne!(&before, metadata.page(1));
    assert!(metadata.copy_range(device, &mut encoder, (1, 0), (2, 8), 16, 8));
    assert!(metadata.upload(
        device,
        &mut encoder,
        &mut uploads,
        (2, 9, 16),
        [state(777, false)]
    ));
    // All corners of four packed records, then four raw vertices in the same page.
    let vertices: Vec<u32> = (0..4)
        .flat_map(|record| (0..4).map(move |corner| 0x10000000 + 4 * (8 + record) + corner))
        .chain(12..16)
        .collect();
    let input = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: None,
        contents: bytemuck::cast_slice(&vertices),
        usage: wgpu::BufferUsages::STORAGE,
    });
    let results = device.create_buffer(&wgpu::BufferDescriptor {
        label: None,
        size: vertices.len() as u64 * 16,
        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
        mapped_at_creation: false,
    });
    let code = format!(
        "#version 450\nlayout(local_size_x=1) in;\nlayout(set=0,binding=0) readonly buffer Meta {{uint words[];}};\nlayout(set=0,binding=1) readonly buffer Inputs {{uint vertices[];}};\nlayout(set=0,binding=2) buffer Results {{uvec4 output_values[];}};\n{}\nvoid main(){{uint i=gl_GlobalInvocationID.x;uint v=vertices[i];uint at=hz_metadata_slot(v)*{}u;output_values[i]=uvec4(words[at],words[at+1u],words[at+2u+(v&3u)],words[at+6u]);}}",
        SLOT_GLSL, WORDS
    );
    let module = naga::front::glsl::Frontend::default()
        .parse(
            &naga::front::glsl::Options {
                stage: naga::ShaderStage::Compute,
                defines: Default::default(),
            },
            &code,
        )
        .unwrap();
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("Shipped metadata-slot GLSL"),
        source: wgpu::ShaderSource::Naga(std::borrow::Cow::Owned(module)),
    });
    let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
        label: None,
        layout: None,
        module: &shader,
        entry_point: Some("main"),
        compilation_options: Default::default(),
        cache: None,
    });
    let group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: None,
        layout: &pipeline.get_bind_group_layout(0),
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: metadata.page(2).as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: input.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 2,
                resource: results.as_entire_binding(),
            },
        ],
    });
    {
        let mut pass = encoder.begin_compute_pass(&Default::default());
        pass.set_pipeline(&pipeline);
        pass.set_bind_group(0, &group, &[]);
        pass.dispatch_workgroups(vertices.len() as u32, 1, 1);
    }
    let read = device.create_buffer(&wgpu::BufferDescriptor {
        label: None,
        size: results.size(),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    encoder.copy_buffer_to_buffer(&results, 0, &read, 0, results.size());
    uploads.finish(&encoder);
    queue.submit([encoder.finish()]);
    read.slice(..)
        .map_async(wgpu::MapMode::Read, |result| result.unwrap());
    device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
    let mapped = read.slice(..).get_mapped_range().unwrap();
    let values: &[[u32; 4]] = bytemuck::cast_slice(&mapped);
    for (i, got) in values.iter().enumerate() {
        let expected = if i < 16 {
            [101, 777, 103, 104][i / 4]
        } else {
            999
        };
        let cell = 3 | 4 << 4 | 5 << 8 | u32::from(i >= 16) << 12 | 1 << 13;
        assert_eq!(
            *got,
            [
                expected,
                cell,
                [0x11445566, 0x22556677, 0x33667788, 0x44778899][i % 4],
                0xff123456
            ],
            "GPU vertex {}",
            vertices[i]
        );
    }
    println!(
        "20 actual GPU address/identity/cell/AO/tint checks passed after page growth, relocation and record reuse"
    );
    drop(mapped);
    read.unmap();
    let mut encoder = device.create_command_encoder(&Default::default());
    assert!(!metadata.upload(device, &mut encoder, &mut uploads, (3, 0, u64::MAX), []));
    assert!(!metadata.upload(
        device,
        &mut encoder,
        &mut uploads,
        (3, 8, 8),
        [state(1, false)]
    ));
    assert!(!metadata.copy_range(device, &mut encoder, (2, 15), (3, 0), 16, 8));
    let cap = device.limits().max_storage_buffer_binding_size / BYTES + 1;
    assert!(!metadata.upload(device, &mut encoder, &mut uploads, (3, 0, cap), []));
    assert!(metadata.get(3).is_none());
    println!("4 metadata overflow/binding/range refusals passed without GPU validation errors");
}

pub(crate) fn assert_arena_ranges(device: &wgpu::Device) {
    use crate::gfx::arena::{Arena, GRAIN};
    let mut arena = Arena::new(device, 24, GRAIN, true);
    let old = arena.page_vertices();
    arena.limit_sidecar(device, BYTES);
    let limits = device.limits();
    let expected = old.min(
        limits
            .max_buffer_size
            .min(limits.max_storage_buffer_binding_size)
            / BYTES
            / GRAIN
            * GRAIN,
    );
    assert_eq!(arena.page_vertices(), expected);
    let mut encoder = device.create_command_encoder(&Default::default());
    assert!(
        arena.alloc(device, &mut encoder, expected + 1).is_none(),
        "oversized geometry must be refused, never silently truncated"
    );
    assert!(arena.alloc_existing(expected + 1).is_none());
    assert!(arena.alloc_moved(expected + 1).is_none());
    let (page, at) = arena.alloc(device, &mut encoder, 4).unwrap();
    assert_eq!(at, 0);
    assert!(arena.buffer(page).size() / 24 * BYTES <= limits.max_storage_buffer_binding_size);
    arena.release(page, at, 4);
    assert!(
        arena.alloc_existing(expected + 1).is_none(),
        "existing pages must not truncate"
    );
    assert!(
        arena.alloc_moved(expected + 1).is_none(),
        "relocation must not truncate"
    );
    assert!(arena.alloc(device, &mut encoder, u64::MAX).is_none());
    assert!(arena.alloc_existing(u64::MAX).is_none());
    assert!(arena.alloc_moved(u64::MAX).is_none());
    assert_eq!(
        arena.alloc_existing(4),
        Some((page, at)),
        "rejected requests must leave the free range intact"
    );
    println!(
        "Actual arena sidecar capacity and oversized allocation checks passed at binding limit {}",
        limits.max_storage_buffer_binding_size
    );
}

#[cfg(test)]
#[test]
#[ignore = "requires Metal; run explicitly"]
fn mixed_layout_addresses_survive_growth_relocation_and_reuse_on_metal() {
    let _gpu = crate::gfx::test_gpu::lock();
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu::Backends::METAL,
        ..wgpu::InstanceDescriptor::new_without_display_handle()
    });
    let adapter = pollster::block_on(instance.request_adapter(&Default::default())).unwrap();
    let (device, queue) = pollster::block_on(adapter.request_device(&Default::default())).unwrap();
    assert_mixed_metadata(&device, &queue);
    let limits = wgpu::Limits {
        max_storage_buffer_binding_size: 8192,
        ..Default::default()
    };
    let (limited, _limited_queue) =
        pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            required_limits: limits,
            ..Default::default()
        }))
        .unwrap();
    assert_arena_ranges(&limited);
}
