//! Block identity and lighting addressed by physical arena slots. A raw quad
//! occupies four slots; a compact quad occupies one record slot. Both layouts
//! can share a page without their metadata addresses aliasing.
use super::upload::Uploads;
use crate::mesher::QuadState;
use std::collections::HashMap;
#[derive(Default)]
pub(super) struct TerrainMetadata {
    pages: HashMap<u32, wgpu::Buffer>,
}
pub(super) const WORDS: usize = 8;
pub(super) const BYTES: u64 = WORDS as u64 * 4;
/// A raw draw's first vertex slot, or a flat compact draw's record slot.
/// Shared with the Metal address oracle so the tested GLSL is the shipped input.
pub(super) const SLOT_GLSL: &str = "uint hz_metadata_slot(uint hz_vertex) {
    return hz_vertex >= 0x10000000u ? ((hz_vertex - 0x10000000u) >> 2u) : (hz_vertex & ~3u);
}
";

fn page_bytes(device: &wgpu::Device, capacity: u64) -> Option<u64> {
    let bytes = capacity.max(1).checked_mul(BYTES)?;
    let limits = device.limits();
    (bytes <= limits.max_buffer_size && bytes <= limits.max_storage_buffer_binding_size)
        .then_some(bytes)
}
pub(super) fn encode(state: QuadState) -> [u32; WORDS] {
    let mut words = [0; WORDS];
    words[0] = state.state;
    if state.state == u32::MAX {
        return words;
    }
    debug_assert!(state.block.iter().all(|&v| v < 16));
    words[1] = u32::from(state.block[0])
        | (u32::from(state.block[1]) << 4)
        | (u32::from(state.block[2]) << 8)
        | (u32::from(state.fluid) << 12);
    if let Some(light) = state.lighting {
        words[1] |= 1 << 13;
        words[2..6].copy_from_slice(&light.no_directional);
        words[6] = light.tint;
        words[7] = light.shaded_tint;
    }
    words
}

impl TerrainMetadata {
    /// `(page, first physical slot, capacity in slots)` addresses the position
    /// arena directly. The caller supplies one state per occupied slot.
    pub fn upload(
        &mut self,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        uploads: &mut Uploads,
        allocation: (u32, u64, u64),
        states: impl IntoIterator<Item = QuadState>,
    ) -> bool {
        let (page, start, capacity) = allocation;
        let Some(bytes) = page_bytes(device, capacity) else {
            return false;
        };
        let Some(remaining) = capacity.checked_sub(start) else {
            return false;
        };
        let words: Vec<[u32; WORDS]> = states
            .into_iter()
            .take(usize::try_from(remaining.saturating_add(1)).unwrap_or(usize::MAX))
            .map(encode)
            .collect();
        if words.len() as u64 > remaining {
            return false;
        }
        if self
            .pages
            .get(&page)
            .is_none_or(|buffer| buffer.size() < bytes)
        {
            let Some(new) = super::arena::try_create(
                device,
                "Terrain metadata",
                bytes,
                wgpu::BufferUsages::STORAGE
                    | wgpu::BufferUsages::COPY_DST
                    | wgpu::BufferUsages::COPY_SRC,
            ) else {
                return false;
            };
            if let Some(old) = self.pages.get(&page) {
                encoder.copy_buffer_to_buffer(old, 0, &new, 0, old.size());
            }
            self.pages.insert(page, new);
        }
        if !words.is_empty() {
            uploads.buffer(
                encoder,
                &self.pages[&page],
                start * BYTES,
                bytemuck::cast_slice(&words),
            );
        }
        true
    }
    /// Move shader identity/lighting with arena compaction; never leave quads bound
    /// to metadata from the previous occupant of their destination range.
    pub fn copy_range(
        &mut self,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        from: (u32, u64),
        to: (u32, u64),
        capacity: u64,
        vertices: u64,
    ) -> bool {
        let Some(source) = self.pages.get(&from.0).cloned() else {
            // A device without vertex storage has no sidecars or consumer.
            return true;
        };
        let Some(bytes) = page_bytes(device, capacity) else {
            return false;
        };
        if from
            .1
            .checked_add(vertices)
            .is_none_or(|end| end > source.size() / BYTES)
            || to.1.checked_add(vertices).is_none_or(|end| end > capacity)
        {
            return false;
        }

        if self
            .pages
            .get(&to.0)
            .is_none_or(|buffer| buffer.size() < bytes)
        {
            let Some(new) = super::arena::try_create(
                device,
                "Terrain metadata",
                bytes,
                wgpu::BufferUsages::STORAGE
                    | wgpu::BufferUsages::COPY_SRC
                    | wgpu::BufferUsages::COPY_DST,
            ) else {
                return false;
            };
            if let Some(old) = self.pages.get(&to.0) {
                encoder.copy_buffer_to_buffer(old, 0, &new, 0, old.size());
            }
            self.pages.insert(to.0, new);
        }
        encoder.copy_buffer_to_buffer(
            &source,
            from.1 * BYTES,
            &self.pages[&to.0],
            to.1 * BYTES,
            vertices * BYTES,
        );
        true
    }
    pub fn get(&self, page: u32) -> Option<&wgpu::Buffer> {
        self.pages.get(&page)
    }
    pub fn page(&self, page: u32) -> &wgpu::Buffer {
        self.pages
            .get(&page)
            .expect("quad metadata accompanies every terrain page")
    }
    pub fn retain(&mut self, live: impl Fn(u32) -> bool) -> Vec<wgpu::Buffer> {
        self.pages
            .extract_if(|&page, _| !live(page))
            .map(|(_, buffer)| buffer)
            .collect()
    }
    pub fn clear(&mut self) -> Vec<wgpu::Buffer> {
        self.pages.drain().map(|(_, buffer)| buffer).collect()
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn identities_and_origin_are_independent_of_shared_texture_layers() {
        assert_eq!(
            &encode(QuadState::new(40, [3, 7, 12], false))[..2],
            &[40, 3 | (7 << 4) | (12 << 8)]
        );
        assert_eq!(
            &encode(QuadState::new(41, [3, 7, 12], true))[..2],
            &[41, 3 | (7 << 4) | (12 << 8) | (1 << 12)]
        );
        assert_eq!(&encode(QuadState::UNKNOWN)[..2], &[u32::MAX, 0]);
    }
    #[test]
    #[ignore = "requires Metal; run explicitly"]
    fn quad_sources_survive_arena_page_growth_and_release_on_metal() {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::METAL,
            ..wgpu::InstanceDescriptor::new_without_display_handle()
        });
        let adapter = pollster::block_on(instance.request_adapter(&Default::default())).unwrap();
        let (device, queue) =
            pollster::block_on(adapter.request_device(&Default::default())).unwrap();
        let mut uploads = Uploads::new(&device);
        let mut metadata = TerrainMetadata::default();
        let mut encoder = device.create_command_encoder(&Default::default());
        let first = QuadState {
            state: 111,
            fluid: false,
            block: [3, 4, 5],
            lighting: None,
        };
        let second = QuadState {
            state: 222,
            fluid: true,
            block: [6, 7, 8],
            lighting: None,
        };
        assert!(metadata.upload(&device, &mut encoder, &mut uploads, (9, 4, 8), [first]));
        let original = metadata.page(9).clone();
        // Water-only geometry stays raw even when compact packing is enabled.
        // Use the same representation selector as Renderer::upload_mesh and
        // read the actual GPU sidecar below, after page growth and compaction.
        let raw = crate::mesher::SectionMesh {
            translucent: vec![crate::mesher::TerrainVertex::new([0.0; 3], [0.0; 2], 0, 0, 0); 4],
            translucent_states: vec![second],
            ..Default::default()
        };
        let states = raw.upload_states(true, false);
        assert!(metadata.upload(&device, &mut encoder, &mut uploads, (9, 8, 16), states));
        assert_ne!(&original, metadata.page(9));
        let read = device.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: 16 * BYTES,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        // Arena compaction moves both quads into another page before the old page is freed.
        assert!(metadata.copy_range(&device, &mut encoder, (9, 4), (10, 4), 16, 8));
        encoder.copy_buffer_to_buffer(metadata.page(10), 0, &read, 0, 16 * BYTES);
        uploads.finish(&encoder);
        queue.submit([encoder.finish()]);
        read.slice(..)
            .map_async(wgpu::MapMode::Read, |r| r.unwrap());
        device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
        let data = read.slice(..).get_mapped_range().unwrap();
        let words: &[[u32; WORDS]] = bytemuck::cast_slice(&data);
        assert_eq!(words[4], encode(first));
        assert_eq!(words[8], encode(second));
        drop(data);
        read.unmap();
        drop(metadata.retain(|page| page != 9 && page != 10));
        assert!(metadata.pages.is_empty());
        let mut encoder = device.create_command_encoder(&Default::default());
        assert!(metadata.upload(
            &device,
            &mut encoder,
            &mut uploads,
            (9, 0, 4),
            [QuadState::UNKNOWN]
        ));
        uploads.finish(&encoder);
        queue.submit([encoder.finish()]);
        device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
        drop(metadata.clear());
        assert!(metadata.pages.is_empty());
    }
}

#[cfg(test)]
#[path = "terrain_metadata/compact_tests.rs"]
mod compact_tests;
