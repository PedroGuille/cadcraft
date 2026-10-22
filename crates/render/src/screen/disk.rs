//! Host-private, disposable ordinary geometry. Exact context/entity keys verify hash candidates.
//! No source drawing or recovery data is stored here; malformed entries always rebuild.
use super::*;
use cadcraft_geom::Vec2;
use std::{
    fs::{File, OpenOptions},
    hash::{Hash, Hasher},
    io::{self, BufWriter, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::OnceLock,
};

static DIRECTORY: OnceLock<PathBuf> = OnceLock::new();
const MAGIC: &[u8; 8] = b"CCGEO016";
const MAX_FILE: u64 = 32 << 20;
const MAX_FILES: usize = 16;
const MAX_RECORDS: usize = 16_384;
const MAX_KEY: usize = 64 << 10;
const MAX_GEOMETRY: usize = 256 << 10;

pub fn set_directory(path: PathBuf) {
    let _ = DIRECTORY.set(path);
}

struct Bounded {
    bytes: Vec<u8>,
    limit: usize,
}
impl Write for Bounded {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.limit.saturating_sub(self.bytes.len()) {
            return Err(io::Error::other("geometry key too large"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
fn json(value: &impl serde::Serialize, limit: usize) -> Option<Vec<u8>> {
    let mut out = Bounded { bytes: Vec::new(), limit };
    serde_json::to_writer(&mut out, value).ok()?;
    Some(out.bytes)
}
fn hash(bytes: &[u8]) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut h);
    h.finish()
}

pub(super) struct Disk {
    file: BufWriter<File>,
    index: HashMap<u64, Vec<u64>>,
    tail: u64,
    records: usize,
}

impl Disk {
    pub fn open(d: &Drawing) -> Option<Self> {
        if d.model.len() < 256 || d.refedit.is_some() {
            return None;
        }
        let directory = DIRECTORY.get()?;
        // These non-annotative, continuous ordinary entities never consume block/font data.
        let context = json(&(env!("CADCRAFT_GEOMETRY_CACHE_BUILD"), &d.header, &d.layers, &d.linetypes, &d.hidden_objects), 256 << 10)?;
        Self::open_in(directory, &context).ok()
    }

    fn open_in(directory: &Path, context: &[u8]) -> io::Result<Self> {
        std::fs::create_dir_all(directory)?;
        if !prune(directory) {
            return Err(io::Error::other("geometry cache directory is full"));
        }
        let path = directory.join(format!("{:016x}.ccg", hash(context)));
        let mut options = OpenOptions::new();
        options.read(true).append(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(path)?;
        // A second document/process may use the same context. It can rebuild while this writer
        // owns the disposable file; never interleave record streams or truncate another writer.
        file.try_lock().map_err(|e| io::Error::other(e.to_string()))?;
        let size = file.metadata()?.len();
        let mut expected = MAGIC.to_vec();
        expected.extend_from_slice(&(context.len() as u32).to_le_bytes());
        expected.extend_from_slice(context);
        let mut header = vec![0; expected.len()];
        if size > MAX_FILE || file.read_exact(&mut header).is_err() || header != expected {
            file.set_len(0)?;
            file.rewind()?;
            file.write_all(&expected)?;
        }
        let mut result = Self { file: BufWriter::with_capacity(64 << 10, file), index: HashMap::new(), tail: expected.len() as u64, records: 0 };
        let length = result.file.get_ref().metadata()?.len();
        while result.records < MAX_RECORDS && result.tail < length {
            let at = result.tail;
            let file = result.file.get_mut();
            file.seek(SeekFrom::Start(at))?;
            let mut head = [0; 24];
            if file.read_exact(&mut head).is_err() {
                break;
            }
            let mut r = Cursor(&head);
            let Some((key_hash, key, geometry, _)) =
                r.u64().zip(r.u32()).zip(r.u32()).zip(r.u64()).map(|(((h, k), g), s)| (h, k as usize, g as usize, s))
            else {
                break;
            };
            let end = at.saturating_add(24).saturating_add(key as u64).saturating_add(geometry as u64);
            if key > MAX_KEY || geometry > MAX_GEOMETRY || end > length || end > MAX_FILE {
                break;
            }
            let offsets = result.index.entry(key_hash).or_default();
            if offsets.len() < 4 {
                offsets.push(at);
            }
            result.records += 1;
            result.tail = end;
        }
        // A torn final write is disposable. Never interpret its length as an allocation request.
        if result.records < MAX_RECORDS {
            result.file.get_mut().set_len(result.tail)?;
        }
        result.file.get_mut().seek(SeekFrom::End(0))?;
        Ok(result)
    }

    pub fn bytes(&self) -> usize {
        self.index.capacity() * 64 + self.records * 8 + (64 << 10)
    }

    pub fn key(d: &Drawing, e: &Entity, o: &Options) -> Option<Vec<u8>> {
        if !e.common.xdata.is_empty() || !super::entities::eligible(e) {
            return None;
        }
        let continuous = e.common.linetype.eq_ignore_ascii_case("Continuous")
            || (e.common.linetype.eq_ignore_ascii_case("ByLayer")
                && d.layers.iter().find(|l| l.name == e.common.layer).is_some_and(|l| l.linetype.eq_ignore_ascii_case("Continuous")));
        if !continuous {
            return None;
        }
        let mut key = json(e, MAX_KEY)?;
        // JSON maps non-finite floats to null. Reject any lossy entity key instead of sharing
        // geometry between hostile values which happened to serialize alike.
        if serde_json::from_slice::<Entity>(&key).ok().as_ref() != Some(e) {
            return None;
        }
        let detail = json(
            &(o.tolerance.to_bits(), o.min_dash.to_bits(), o.view_height.to_bits(), o.fill, o.lineweights, o.transparency, o.text, o.skip_viewport),
            256,
        )?;
        if key.len().saturating_add(detail.len() + 1) > MAX_KEY {
            return None;
        }
        key.push(b'\n');
        key.extend_from_slice(&detail);
        Some(key)
    }

    pub fn get(&mut self, key: &[u8]) -> Option<DisplayList> {
        let offsets = self.index.get(&hash(key))?.clone();
        self.file.flush().ok()?;
        let mut answer = None;
        for at in offsets.into_iter().rev() {
            let file = self.file.get_mut();
            file.seek(SeekFrom::Start(at)).ok()?;
            let mut head = [0; 24];
            file.read_exact(&mut head).ok()?;
            let mut r = Cursor(&head);
            r.u64()?;
            let key_len = r.u32()? as usize;
            let len = r.u32()? as usize;
            let checksum = r.u64()?;
            if key_len != key.len() || len > MAX_GEOMETRY {
                continue;
            }
            let mut stored = vec![0; key_len];
            file.read_exact(&mut stored).ok()?;
            if stored != key {
                continue;
            }
            let mut bytes = vec![0; len];
            file.read_exact(&mut bytes).ok()?;
            if hash(&bytes) == checksum {
                answer = decode(&bytes);
                if answer.is_some() {
                    break;
                }
            }
        }
        self.file.get_mut().seek(SeekFrom::End(0)).ok()?;
        answer
    }

    pub fn insert(&mut self, key: &[u8], list: &DisplayList) {
        if self.records >= MAX_RECORDS || self.index.contains_key(&hash(key)) {
            return;
        }
        let Some(bytes) = encode(list) else { return };
        let length = 24 + key.len() + bytes.len();
        if key.len() > MAX_KEY || self.tail.saturating_add(length as u64) > MAX_FILE {
            return;
        }
        let mut record = Vec::with_capacity(length);
        record.extend_from_slice(&hash(key).to_le_bytes());
        record.extend_from_slice(&(key.len() as u32).to_le_bytes());
        record.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        record.extend_from_slice(&hash(&bytes).to_le_bytes());
        record.extend_from_slice(key);
        record.extend_from_slice(&bytes);
        if self.file.write_all(&record).is_ok() {
            self.index.entry(hash(key)).or_default().push(self.tail);
            self.tail += length as u64;
            self.records += 1;
        } else {
            self.records = MAX_RECORDS;
        }
    }

    pub fn flush(&mut self) {
        let _ = self.file.flush();
    }
}

fn prune(directory: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(directory) else { return false };
    let mut files: Vec<_> = entries
        .take(1025)
        .filter_map(Result::ok)
        .filter_map(|e| {
            let metadata = e.metadata().ok()?;
            (e.path().extension().is_some_and(|e| e == "ccg") && metadata.is_file()).then_some((metadata.modified().ok(), metadata.len(), e.path()))
        })
        .collect();
    if files.len() > 1024 {
        return false;
    }
    files.sort_by_key(|(time, _, _)| *time);
    let mut bytes: u64 = files.iter().map(|(_, size, _)| *size).sum();
    let mut count = files.len();
    for (_, size, path) in files {
        if bytes <= MAX_FILE * (MAX_FILES as u64 - 1) && count < MAX_FILES {
            break;
        }
        let unlocked = OpenOptions::new().read(true).write(true).open(&path).ok();
        let Some(file) = unlocked.filter(|file| file.try_lock().is_ok()) else { continue };
        if std::fs::remove_file(path).is_ok() {
            bytes = bytes.saturating_sub(size);
            count -= 1;
        }
        drop(file);
    }
    bytes <= MAX_FILE * (MAX_FILES as u64 - 1) && count < MAX_FILES
}

fn encode(list: &DisplayList) -> Option<Vec<u8>> {
    if list.expanded != 0 || !list.texts.is_empty() || !list.pens.is_empty() || !list.prim_layers.is_empty() || list.sheet.is_some() {
        return None;
    }
    let length = 16usize
        .checked_add(list.prims.len().checked_mul(28)?)?
        .checked_add(list.verts.len().checked_add(list.tris.len())?.checked_mul(16)?)?
        .checked_add(list.analytic.len().checked_mul(52)?)?;
    if length > MAX_GEOMETRY {
        return None;
    }
    let mut bytes = Vec::with_capacity(length);
    for count in [list.prims.len(), list.verts.len(), list.tris.len(), list.analytic.len()] {
        bytes.extend_from_slice(&u32::try_from(count).ok()?.to_le_bytes());
    }
    for p in &list.prims {
        let kind = match p.kind {
            crate::Kind::Polyline => 0,
            crate::Kind::Tris => 1,
            crate::Kind::Point => 2,
            _ => return None,
        };
        bytes.extend_from_slice(&[kind, p.color.0, p.color.1, p.color.2, u8::from(p.aci7), p.transparency, 0, 0]);
        bytes.extend_from_slice(&p.handle.0.to_le_bytes());
        bytes.extend_from_slice(&p.start.to_le_bytes());
        bytes.extend_from_slice(&p.len.to_le_bytes());
        bytes.extend_from_slice(&p.lw.to_le_bytes());
    }
    for p in list.verts.iter().chain(&list.tris) {
        if !p.is_finite() {
            return None;
        }
        for v in [p.x, p.y] {
            bytes.extend_from_slice(&v.to_le_bytes());
        }
    }
    for h in &list.analytic {
        bytes.extend_from_slice(&u32::try_from(h.prim).ok()?.to_le_bytes());
        for v in [h.center.x, h.center.y, h.radius, h.start, h.sweep, h.tolerance] {
            if !v.is_finite() {
                return None;
            }
            bytes.extend_from_slice(&v.to_le_bytes());
        }
    }
    Some(bytes)
}

struct Cursor<'a>(&'a [u8]);
impl Cursor<'_> {
    fn take<const N: usize>(&mut self) -> Option<[u8; N]> {
        let (head, tail) = self.0.split_at_checked(N)?;
        self.0 = tail;
        head.try_into().ok()
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take()?))
    }
    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take()?))
    }
    fn f64(&mut self) -> Option<f64> {
        let v = f64::from_le_bytes(self.take()?);
        v.is_finite().then_some(v)
    }
}
fn decode(bytes: &[u8]) -> Option<DisplayList> {
    let mut r = Cursor(bytes);
    let p = r.u32()? as usize;
    let v = r.u32()? as usize;
    let t = r.u32()? as usize;
    let a = r.u32()? as usize;
    let expected = 16usize.checked_add(p.checked_mul(28)?)?.checked_add(v.checked_add(t)?.checked_mul(16)?)?.checked_add(a.checked_mul(52)?)?;
    if expected != bytes.len() || expected > MAX_GEOMETRY {
        return None;
    }
    let mut list = DisplayList::default();
    for _ in 0..p {
        let [kind, red, green, blue, aci7, transparency, _, _] = r.take()?;
        let handle = cadcraft_doc::Handle(r.u64()?);
        let start = r.u32()?;
        let len = r.u32()?;
        let lw = f32::from_le_bytes(r.take()?);
        let kind = match kind {
            0 => crate::Kind::Polyline,
            1 if len.is_multiple_of(3) => crate::Kind::Tris,
            2 if len == 1 => crate::Kind::Point,
            _ => return None,
        };
        let limit = if kind == crate::Kind::Tris { t } else { v };
        if start.checked_add(len)? as usize > limit || !lw.is_finite() || lw < 0.0 || transparency > 90 || aci7 > 1 {
            return None;
        }
        list.prims.push(crate::DPrim { handle, start, len, lw, kind, color: cadcraft_color::Rgb(red, green, blue), transparency, aci7: aci7 != 0 });
    }
    for _ in 0..v {
        list.verts.push(Vec2::new(r.f64()?, r.f64()?));
    }
    for _ in 0..t {
        list.tris.push(Vec2::new(r.f64()?, r.f64()?));
    }
    for _ in 0..a {
        let prim = r.u32()? as usize;
        let center = Vec2::new(r.f64()?, r.f64()?);
        let radius = r.f64()?;
        let start = r.f64()?;
        let sweep = r.f64()?;
        let tolerance = r.f64()?;
        if prim >= p || radius <= 0.0 || tolerance <= 0.0 || list.analytic.last().is_some_and(|h| h.prim >= prim) {
            return None;
        }
        list.analytic.push(crate::ArcHint { prim, center, radius, start, sweep, tolerance });
    }
    list.bounds = Bounds2::from_points(list.verts.iter().chain(&list.tris).copied());
    Some(list)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn disk_geometry_roundtrip_checks_keys_corruption_and_hostile_lengths() {
        let mut d = Drawing::new_imperial();
        d.add(
            &Space::Model,
            Default::default(),
            EntityKind::Line(cadcraft_doc::Line { a: cadcraft_geom::Vec3::ZERO, b: cadcraft_geom::Vec3::new(5.0, 7.0, 0.0) }),
        )
        .unwrap();
        let list = crate::build(&d, &Space::Model, &Options::default());
        let encoded = encode(&list).unwrap();
        let decoded = decode(&encoded).unwrap();
        assert_eq!(decoded.verts, list.verts);
        assert_eq!(decoded.prims, list.prims);
        for end in 0..encoded.len() {
            assert!(decode(&encoded[..end]).is_none());
        }
        let mut hostile = encoded.clone();
        hostile[..4].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(decode(&hostile).is_none());
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!("cadcraft-geometry-test-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        {
            let mut disk = Disk::open_in(&dir, b"context").unwrap();
            disk.insert(b"entity/options", &list);
            disk.flush();
        }
        {
            let mut disk = Disk::open_in(&dir, b"context").unwrap();
            assert!(disk.get(b"changed").is_none());
            assert_eq!(disk.get(b"entity/options").unwrap().prims, list.prims);
        }
        {
            let mut disk = Disk::open_in(&dir, b"context").unwrap();
            let path = dir.join(format!("{:016x}.ccg", hash(b"context")));
            let mut file = OpenOptions::new().write(true).open(path).unwrap();
            file.seek(SeekFrom::End(-1)).unwrap();
            file.write_all(&[0xff]).unwrap();
            assert!(disk.get(b"entity/options").is_none());
        }
        let _ = std::fs::remove_dir_all(dir);
    }
}
