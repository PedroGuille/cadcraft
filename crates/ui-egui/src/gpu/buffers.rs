//! Immutable ordinary mesh revisions. Sparse edits copy the base on the GPU, then upload deltas.
use super::*;
use std::collections::VecDeque;
use std::sync::Weak;

pub(super) struct Version {
    pub buffer: wgpu::Buffer,
    bytes: Mutex<Option<Arc<[u8]>>>,
    device: wgpu::Device,
    #[cfg(test)]
    copied: bool,
}
impl Version {
    pub fn cpu_bytes(&self) -> usize {
        self.bytes.lock().unwrap_or_else(std::sync::PoisonError::into_inner).as_ref().map_or(0, |bytes| bytes.len())
    }
}

pub(super) struct Cache {
    pages: VecDeque<(u64, Weak<Version>)>,
    limit: usize,
}

impl Default for Cache {
    fn default() -> Self {
        Self { pages: VecDeque::new(), limit: 16 << 20 }
    }
}

impl Cache {
    pub fn set_budget(&mut self, bytes: usize) {
        self.limit = bytes.min(16 << 20);
        self.trim(0);
        if bytes == 0 {
            self.pages.clear();
            self.pages.shrink_to_fit();
        }
    }

    fn trim(&mut self, reserve: usize) {
        let mut used = self.pages.iter().filter_map(|(_, page)| page.upgrade()).map(|page| page.cpu_bytes()).sum::<usize>();
        for (_, page) in &self.pages {
            if used.saturating_add(reserve) <= self.limit {
                break;
            }
            if let Some(page) = page.upgrade() {
                let old = page.bytes.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take();
                used = used.saturating_sub(old.map_or(0, |bytes| bytes.len()));
            }
        }
    }
    pub fn upload(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        key: u64,
        label: &str,
        bytes: &[u8],
        usage: wgpu::BufferUsages,
    ) -> Arc<Version> {
        self.pages.retain(|(_, page)| page.strong_count() > 0);
        let old = self.pages.iter().rev().filter(|(id, _)| *id == key).find_map(|(_, page)| {
            let page = page.upgrade()?;
            if page.device != *device {
                return None;
            }
            let data = page.bytes.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone()?;
            (data.len() == bytes.len()).then_some((page, data))
        });
        let ranges = old.as_ref().map(|(_, data)| dirty_ranges(data, bytes));
        if let Some((old, _)) = old.as_ref().filter(|_| ranges.as_ref().is_some_and(Vec::is_empty)) {
            return old.clone();
        }
        let usage = usage | wgpu::BufferUsages::COPY_SRC | wgpu::BufferUsages::COPY_DST;
        #[cfg(test)]
        let mut copied = false;
        let buffer = if let (Some((old, _)), Some(ranges)) = (&old, &ranges)
            && bytes.len() >= 4096
            && ranges.len() <= 64
            && ranges.iter().map(std::ops::Range::len).sum::<usize>() <= bytes.len() / 4
        {
            #[cfg(test)]
            {
                copied = true;
            }
            let buffer =
                device.create_buffer(&wgpu::BufferDescriptor { label: Some(label), size: bytes.len() as u64, usage, mapped_at_creation: false });
            let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("cad_copy_unchanged_mesh") });
            encoder.copy_buffer_to_buffer(&old.buffer, 0, &buffer, 0, bytes.len() as u64);
            queue.submit([encoder.finish()]);
            for range in ranges {
                if let Some(data) = bytes.get(range.clone()) {
                    queue.write_buffer(&buffer, range.start as u64, data);
                }
            }
            buffer
        } else {
            device.create_buffer_init(&wgpu::util::BufferInitDescriptor { label: Some(label), contents: bytes, usage })
        };
        self.trim(bytes.len().min(self.limit));
        let retained = (bytes.len() <= self.limit).then(|| Arc::<[u8]>::from(bytes));
        let version = Arc::new(Version {
            buffer,
            bytes: Mutex::new(retained),
            device: device.clone(),
            #[cfg(test)]
            copied,
        });
        while self.pages.len() >= 512 {
            if let Some((_, old)) = self.pages.pop_front()
                && let Some(old) = old.upgrade()
            {
                old.bytes.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take();
            }
        }
        self.pages.push_back((key, Arc::downgrade(&version)));
        version
    }
}

fn dirty_ranges(old: &[u8], new: &[u8]) -> Vec<std::ops::Range<usize>> {
    let mut ranges: Vec<std::ops::Range<usize>> = Vec::new();
    for (index, (a, b)) in old.as_chunks::<4>().0.iter().zip(new.as_chunks::<4>().0).enumerate() {
        if a == b {
            continue;
        }
        let start = index * 4;
        if let Some(last) = ranges.last_mut().filter(|last| last.end == start) {
            last.end += 4;
        } else {
            ranges.push(start..start + 4);
            if ranges.len() > 64 {
                return std::iter::once(0..new.len()).collect();
            }
        }
    }
    ranges
}

#[derive(Clone)]
pub(super) struct Reuse {
    pub key: u64,
    pub cache: Arc<Mutex<Cache>>,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn mesh_changes_use_four_byte_aligned_exact_ranges() {
        let old = vec![0; 8192];
        let mut new = old.clone();
        new[10] = 1;
        new[12] = 2;
        new[7901] = 3;
        assert_eq!(dirty_ranges(&old, &new), vec![8..16, 7900..7904]);
        assert!(dirty_ranges(&old, &old).is_empty());
    }

    #[test]
    fn sparse_gpu_mesh_updates_preserve_previous_snapshots_and_undo_bytes() {
        let rs = egui_kittest::wgpu::create_render_state(egui_kittest::wgpu::default_wgpu_setup(), Default::default());
        fn read(rs: &egui_wgpu::RenderState, source: &wgpu::Buffer) -> Vec<u8> {
            let target = rs.device.create_buffer(&wgpu::BufferDescriptor {
                label: None,
                size: source.size(),
                usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                mapped_at_creation: false,
            });
            let mut encoder = rs.device.create_command_encoder(&Default::default());
            encoder.copy_buffer_to_buffer(source, 0, &target, 0, source.size());
            rs.queue.submit([encoder.finish()]);
            target.slice(..).map_async(wgpu::MapMode::Read, |_| {});
            rs.device.poll(wgpu::PollType::Wait { submission_index: None, timeout: Some(std::time::Duration::from_secs(20)) }).unwrap();
            target.slice(..).get_mapped_range().unwrap().to_vec()
        }
        let mut cache = Cache::default();
        let bytes = vec![0; 8192];
        let first = cache.upload(&rs.device, &rs.queue, 7, "test", &bytes, wgpu::BufferUsages::VERTEX);
        let repeated = cache.upload(&rs.device, &rs.queue, 7, "test", &bytes, wgpu::BufferUsages::VERTEX);
        assert!(Arc::ptr_eq(&first, &repeated));
        let mut edited = bytes.clone();
        edited[400] = 17;
        let changed = cache.upload(&rs.device, &rs.queue, 7, "test", &edited, wgpu::BufferUsages::VERTEX);
        assert!(changed.copied);
        assert_eq!(read(&rs, &changed.buffer), edited);
        assert_eq!(read(&rs, &first.buffer), bytes);
        let undo = cache.upload(&rs.device, &rs.queue, 7, "test", &bytes, wgpu::BufferUsages::VERTEX);
        assert!(undo.copied);
        assert_eq!(read(&rs, &undo.buffer), bytes);
        assert_eq!(read(&rs, &changed.buffer), edited);
        cache.set_budget(0);
        assert_eq!(changed.cpu_bytes(), 0);
        assert_eq!(first.cpu_bytes(), 0);
        assert_eq!(read(&rs, &changed.buffer), edited);
    }
}
