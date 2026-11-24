//! The uniforms a shader pack's gbuffers programs read per draw (`HzDraw` at set 2, the
//! translator's `DRAW_UNIFORMS`): one slot per render stage of a uniform buffer, picked with a
//! dynamic offset at each draw.
pub use crate::render_geometry::EntitySpan;

use horizon_shaders::{RENDER_STAGES, render_stage};

/// `HzDraw`'s size: `renderStage`, `entityId`, `blockEntityId`, `currentRenderedItemId`,
/// `entityColor`.
const SIZE: u32 = 32;

pub(super) struct DrawUniforms {
    pub layout: wgpu::BindGroupLayout,
    group: wgpu::BindGroup,
    stride: u32,
}

impl DrawUniforms {
    pub fn new(device: &wgpu::Device) -> Self {
        let stride = SIZE.max(device.limits().min_uniform_buffer_offset_alignment);
        let mut contents = vec![0u8; stride as usize * RENDER_STAGES.len()];
        for (i, slot) in contents.chunks_mut(stride as usize).enumerate() {
            slot[..4].copy_from_slice(&(i as i32).to_le_bytes());
            // `blockEntityId` outside block entities, as Iris has it; `currentRenderedItemId`
            // outside items, as Iris 1.11.7 gives it to terrain, water and the sky (observed).
            slot[8..12].copy_from_slice(&(-1i32).to_le_bytes());
            slot[12..16].copy_from_slice(&(-1i32).to_le_bytes());
        }
        let buffer = wgpu::util::DeviceExt::create_buffer_init(
            device,
            &wgpu::util::BufferInitDescriptor {
                label: Some("Shader pack draws"),
                contents: &contents,
                usage: wgpu::BufferUsages::UNIFORM,
            },
        );
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("Shader pack draws"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: true,
                    min_binding_size: wgpu::BufferSize::new(u64::from(SIZE)),
                },
                count: None,
            }],
        });
        let group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("Shader pack draws"),
            layout: &layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                    buffer: &buffer,
                    offset: 0,
                    size: wgpu::BufferSize::new(u64::from(SIZE)),
                }),
            }],
        });
        Self {
            layout,
            group,
            stride,
        }
    }

    /// Draws that follow read `renderStage` = `MC_RENDER_STAGE_<stage>`, other values zero.
    pub fn bind(&self, pass: &mut wgpu::RenderPass<'_>, stage: &str) {
        pass.set_bind_group(2, &self.group, &[self.stride * render_stage(stage)]);
    }
}

#[cfg(test)]
mod tests {
    use super::EntitySpan;

    fn span(start: u32, id: i32) -> EntitySpan {
        EntitySpan {
            start,
            id,
            ..Default::default()
        }
    }

    #[test]
    fn item_ranges_keep_parent_flags_and_restore_ids_without_growing_uniforms() {
        let parent = EntitySpan {
            start: 0,
            stage: horizon_shaders::render_stage("ENTITIES"),
            id: 91,
            hidden: true,
            player: true,
            translucent: true,
            emissive: true,
            ..Default::default()
        };
        let mut spans = vec![parent];
        EntitySpan::item_range(&mut spans, 4, 8, 450);
        EntitySpan::item_range(&mut spans, 8, 12, 721);
        assert_eq!(
            spans[1],
            EntitySpan {
                start: 4,
                item_id: 450,
                ..parent
            }
        );
        assert_eq!(
            spans[3],
            EntitySpan {
                start: 8,
                item_id: 721,
                ..parent
            }
        );
        assert_eq!(
            spans.last().unwrap(),
            &EntitySpan {
                start: 12,
                ..parent
            }
        );
        let color = [0.2, 0.3, 0.4, 0.5];
        let bytes = spans[1].draw_bytes(color);
        assert_eq!(bytes.len(), 32);
        assert_eq!(i32::from_le_bytes(bytes[4..8].try_into().unwrap()), 91);
        assert_eq!(i32::from_le_bytes(bytes[12..16].try_into().unwrap()), 450);
        assert_eq!(
            f32::from_le_bytes(bytes[16..20].try_into().unwrap()),
            color[0]
        );
        let block = EntitySpan {
            stage: horizon_shaders::render_stage("BLOCK_ENTITIES"),
            ..spans[1]
        }
        .draw_bytes(color);
        assert_eq!(i32::from_le_bytes(block[4..8].try_into().unwrap()), 0);
        assert_eq!(i32::from_le_bytes(block[8..12].try_into().unwrap()), 91);
    }

    #[test]
    fn culling_item_vertices_preserves_the_enclosing_entity_marker() {
        let parent = span(4, 17);
        let mut spans = vec![span(0, 2), parent];
        EntitySpan::item_range(&mut spans, 4, 8, 80);
        EntitySpan::truncate(&mut spans, 4);
        assert_eq!(spans, [span(0, 2), parent]);
        EntitySpan::item_range(&mut spans, 4, 12, 90);
        assert_eq!(spans[2].id, 17);
        assert_eq!(spans[2].item_id, 90);
    }

    #[test]
    fn hide_splits_the_span_around_one_entity() {
        // Two cows in one span: the second (vertices 8..16) is hidden.
        let mut spans = vec![span(0, 7)];
        assert!(EntitySpan::hide(&mut spans, 8, 16));
        let hidden = EntitySpan {
            hidden: true,
            ..span(8, 7)
        };
        assert_eq!(spans, [span(0, 7), hidden, span(16, 7)]);
        // An entity with its own span and a part after it (armour) is hidden whole.
        let mut spans = vec![span(0, 1), span(4, 2), span(6, 3)];
        assert!(EntitySpan::hide(&mut spans, 4, 10));
        assert!(!spans[0].hidden && spans[1].hidden && spans[2].hidden);
        assert_eq!(spans[3], span(10, 3));
        assert!(!EntitySpan::hide(&mut Vec::new(), 0, 4));
    }
}
