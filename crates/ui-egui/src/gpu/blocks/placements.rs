//! Versioned placement buffers: retain geometry, upload sparse edit deltas, preserve old frames.
use super::*;

#[derive(Default)]
pub(super) struct Cache {
    pages: VecDeque<std::sync::Weak<Placements>>,
}

impl Cache {
    pub(super) fn upload(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, data: &InstanceData) -> Arc<Placements> {
        self.pages.retain(|page| page.strong_count() > 0);
        let old = self.pages.iter().rev().filter_map(std::sync::Weak::upgrade).find(|page| {
            Arc::ptr_eq(&page._geometry, &data.geometry) && page.handles.as_ref() == data.handles && page.transforms.len() == data.transforms.len()
        });
        let dirty = old.as_ref().map(|page| dirty_ranges(&page.transforms, &data.transforms));
        if let Some(page) = old.as_ref().filter(|page| dirty.as_ref().is_some_and(Vec::is_empty) && page.bounds == data.bounds) {
            return page.clone();
        }
        let buffer = if let Some(page) = old.as_ref().filter(|_| dirty.as_ref().is_some_and(Vec::is_empty)) {
            page.buffer.clone()
        } else if let (Some(page), Some(ranges)) = (&old, &dirty)
            && data.transforms.len() >= 4096
            && ranges.iter().map(std::ops::Range::len).sum::<usize>() <= data.transforms.len() / 4
            && ranges.len() <= 32
        {
            let buffer = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("cad_block_placement_delta"),
                size: data.transforms.len() as u64,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_SRC | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("cad_copy_unchanged_placements") });
            encoder.copy_buffer_to_buffer(&page.buffer, 0, &buffer, 0, data.transforms.len() as u64);
            // Submit the immutable base copy first. Queue writes are staged before the next
            // submission, so doing them before this submit would let the copy erase the edit.
            queue.submit([encoder.finish()]);
            for range in ranges {
                if let Some(bytes) = data.transforms.get(range.clone()) {
                    queue.write_buffer(&buffer, range.start as u64, bytes);
                }
            }
            buffer
        } else {
            device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("cad_block_placements"),
                contents: &data.transforms,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_SRC | wgpu::BufferUsages::COPY_DST,
            })
        };
        let page = Arc::new(Placements {
            buffer,
            bounds: data.bounds.clone(),
            _geometry: data.geometry.clone(),
            transforms: data.transforms.clone().into(),
            handles: data.handles.clone().into(),
            spatial: culling::placements::Index::new(&data.bounds),
        });
        while self.pages.len() >= 128 {
            self.pages.pop_front();
        }
        self.pages.push_back(Arc::downgrade(&page));
        page
    }
}

/// Record-aligned writes also satisfy WebGPU's four-byte copy alignment.
fn dirty_ranges(old: &[u8], new: &[u8]) -> Vec<std::ops::Range<usize>> {
    let mut ranges: Vec<std::ops::Range<usize>> = Vec::new();
    for (index, (a, b)) in old.as_chunks::<24>().0.iter().zip(new.as_chunks::<24>().0.iter()).enumerate() {
        if a == b {
            continue;
        }
        let start = index * 24;
        if let Some(last) = ranges.last_mut().filter(|last| last.end == start) {
            last.end += 24;
        } else {
            ranges.push(start..start + 24);
        }
    }
    ranges
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sparse_edits_upload_only_changed_records() {
        let old = vec![0; 24 * 1024];
        let mut new = old.clone();
        new[24 * 100 + 4] = 1;
        new[24 * 101 + 8] = 2;
        new[24 * 900] = 3;
        assert_eq!(dirty_ranges(&old, &new), vec![2400..2448, 21600..21624]);
        assert!(dirty_ranges(&old, &old).is_empty());
    }
}
