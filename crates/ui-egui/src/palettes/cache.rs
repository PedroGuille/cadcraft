//! Cache unchanged palette models, never interactive widgets or their event handling. Weak
//! entity identities detect in-place previews/undo without retaining a drawing or forcing COW.
use cadcraft_color::Color;
use cadcraft_doc::{Drawing, Entity, EntityKind, Handle};
use std::sync::{Arc, Weak};

type Layer = (String, bool, bool, bool, Color);

pub(crate) struct Tables {
    pub layers: Vec<Layer>,
    pub layer_names: Vec<String>,
    pub linetypes: Vec<String>,
    pub text_styles: Vec<String>,
    pub dim_styles: Vec<String>,
    pub ml_styles: Vec<String>,
    pub tb_styles: Vec<String>,
}

impl Tables {
    fn matches(&self, d: &Drawing) -> bool {
        self.layers.len() == d.layers.len()
            && self
                .layers
                .iter()
                .zip(d.layers.iter())
                .all(|((n, on, frozen, locked, c), l)| *n == l.name && *on == l.on && *frozen == l.frozen && *locked == l.locked && *c == l.color)
            && names_match(&self.linetypes, d.linetypes.iter().map(|l| l.name.as_str()))
            && names_match(&self.text_styles, d.text_styles.iter().map(|l| l.name.as_str()))
            && names_match(&self.dim_styles, d.dim_styles.iter().map(|l| l.name.as_str()))
            && names_match(&self.ml_styles, d.mleader_styles.iter().map(|l| l.name.as_str()))
            && names_match(&self.tb_styles, d.table_styles.iter().map(|l| l.name.as_str()))
    }

    fn new(d: &Drawing) -> Self {
        Self {
            layers: d.layers.iter().map(|l| (l.name.clone(), l.on, l.frozen, l.locked, l.color)).collect(),
            layer_names: d.layers.iter().map(|l| l.name.clone()).collect(),
            linetypes: d.linetypes.iter().map(|l| l.name.clone()).collect(),
            text_styles: d.text_styles.iter().map(|l| l.name.clone()).collect(),
            dim_styles: d.dim_styles.iter().map(|l| l.name.clone()).collect(),
            ml_styles: d.mleader_styles.iter().map(|l| l.name.clone()).collect(),
            tb_styles: d.table_styles.iter().map(|l| l.name.clone()).collect(),
        }
    }

    fn bytes(&self) -> usize {
        self.layers
            .capacity()
            .saturating_mul(std::mem::size_of::<Layer>())
            .saturating_add(self.layers.iter().map(|l| l.0.capacity()).sum::<usize>())
            .saturating_add(
                [&self.layer_names, &self.linetypes, &self.text_styles, &self.dim_styles, &self.ml_styles, &self.tb_styles]
                    .into_iter()
                    .fold(0usize, |n, names| n.saturating_add(strings_bytes(names))),
            )
    }
}

fn names_match<'a>(names: &[String], source: impl Iterator<Item = &'a str>) -> bool {
    names.iter().map(String::as_str).eq(source)
}

fn strings_bytes(names: &Vec<String>) -> usize {
    names.capacity().saturating_mul(std::mem::size_of::<String>()).saturating_add(names.iter().fold(0usize, |n, s| n.saturating_add(s.capacity())))
}

pub(crate) struct Selection {
    handles: Arc<Vec<Handle>>,
    sources: Vec<Option<Weak<Entity>>>,
    pub types: Vec<(&'static str, Vec<String>)>,
    pub ids: Vec<String>,
    pub common: [String; 6],
    pub polyline: Option<(f64, Option<f64>)>,
}

impl Selection {
    fn matches(&self, d: &Drawing, handles: &Arc<Vec<Handle>>) -> bool {
        Arc::ptr_eq(&self.handles, handles)
            && self.sources.iter().zip(handles.iter()).all(|(old, h)| match (old, d.entity(*h)) {
                (Some(old), Some(now)) => old.as_ptr() == Arc::as_ptr(now),
                (None, None) => true,
                _ => false,
            })
    }

    fn new(d: &Drawing, handles: Arc<Vec<Handle>>) -> Self {
        let sources = handles.iter().map(|h| d.entity(*h).map(Arc::downgrade)).collect();
        let mut types: Vec<(&'static str, Vec<String>)> = Vec::new();
        for h in handles.iter() {
            if let Some(e) = d.entity(*h) {
                let kind = e.kind.type_name();
                match types.iter_mut().find(|(t, _)| *t == kind) {
                    Some((_, hs)) => hs.push(h.hex()),
                    None => types.push((kind, vec![h.hex()])),
                }
            }
        }
        let same = |field: &dyn Fn(&Entity) -> String| {
            let mut values = handles.iter().filter_map(|h| d.entity(*h)).map(|e| field(e));
            match values.next() {
                Some(first) if values.all(|v| v == first) => first,
                _ => "*VARIES*".into(),
            }
        };
        let common = [
            same(&|e| e.common.color.name()),
            same(&|e| e.common.layer.clone()),
            same(&|e| e.common.linetype.clone()),
            same(&|e| format!("{:.4}", e.common.ltscale)),
            same(&|e| e.common.lineweight.name()),
            same(&|e| e.common.transparency.name()),
        ];
        let polyline = if handles.len() == 1 {
            handles.first().and_then(|h| d.entity(*h)).and_then(|e| match &e.kind {
                EntityKind::LwPolyline(p) => {
                    let g = cadcraft_geom::Polyline { vertices: p.vertices.clone(), closed: p.closed };
                    Some((g.len(), p.closed.then(|| g.area().abs())))
                }
                _ => None,
            })
        } else {
            None
        };
        let ids = handles.iter().map(|h| h.hex()).collect();
        Self { handles, sources, types, ids, common, polyline }
    }

    fn bytes(&self) -> usize {
        strings_bytes(&self.ids)
            .saturating_add(self.types.iter().fold(0usize, |n, (_, hs)| n.saturating_add(strings_bytes(hs))))
            .saturating_add(self.sources.capacity().saturating_mul(std::mem::size_of::<Option<Weak<Entity>>>()))
            .saturating_add(self.common.iter().fold(0usize, |n, s| n.saturating_add(s.capacity())))
    }
}

pub(crate) struct Cache {
    tables: Option<Arc<Tables>>,
    selection: Option<Arc<Selection>>,
    limit: usize,
}

impl Default for Cache {
    fn default() -> Self {
        Self { tables: None, selection: None, limit: 2 << 20 }
    }
}

impl Cache {
    pub fn clear(&mut self) {
        self.tables = None;
        self.selection = None;
    }

    pub fn bytes(&self) -> usize {
        self.tables.as_ref().map_or(0, |t| t.bytes()).saturating_add(self.selection.as_ref().map_or(0, |s| s.bytes()))
    }

    pub fn set_budget(&mut self, bytes: usize) {
        self.limit = bytes;
        if self.bytes() > bytes {
            self.tables = None;
            self.selection = None;
        }
    }

    pub fn tables(&mut self, d: &Drawing) -> Arc<Tables> {
        if let Some(t) = &self.tables
            && t.matches(d)
        {
            return t.clone();
        }
        let t = Arc::new(Tables::new(d));
        if t.bytes().saturating_add(self.selection.as_ref().map_or(0, |s| s.bytes())) > self.limit {
            self.selection = None;
        }
        self.tables = (t.bytes() <= self.limit).then(|| t.clone());
        t
    }

    pub fn selection(&mut self, d: &Drawing, handles: &Arc<Vec<Handle>>) -> Arc<Selection> {
        if let Some(s) = &self.selection
            && s.matches(d, handles)
        {
            return s.clone();
        }
        let s = Arc::new(Selection::new(d, handles.clone()));
        if s.bytes().saturating_add(self.tables.as_ref().map_or(0, |t| t.bytes())) > self.limit {
            self.tables = None;
        }
        self.selection = (s.bytes() <= self.limit).then(|| s.clone());
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cadcraft_doc::{Common, Line, Space};
    use cadcraft_geom::Vec3;

    #[test]
    fn palette_models_reuse_unchanged_data_and_refresh_after_preview_edits_and_undo() {
        let mut d = Drawing::new_imperial();
        let h = d.add(&Space::Model, Common::default(), EntityKind::Line(Line { a: Vec3::ZERO, b: Vec3::new(1.0, 0.0, 0.0) })).unwrap();
        let handles = Arc::new(vec![h]);
        let mut cache = Cache::default();
        let before = cache.selection(&d, &handles);
        assert!(Arc::ptr_eq(&before, &cache.selection(&d, &handles)));
        let tables = cache.tables(&d);
        assert!(Arc::ptr_eq(&tables, &cache.tables(&d)));
        let saved = d.clone();
        d.model.modify(h, |e| e.common.color = Color::Index(1));
        let edited = cache.selection(&d, &handles);
        assert!(!Arc::ptr_eq(&before, &edited));
        assert_ne!(before.common[0], edited.common[0]);
        assert!(Arc::ptr_eq(&tables, &cache.tables(&d)), "entity edits preserve unchanged layer/table data");
        let undo = cache.selection(&saved, &handles);
        assert_eq!(undo.common, before.common);
        d.layers[0].locked = true;
        let locked = cache.tables(&d);
        assert!(!Arc::ptr_eq(&tables, &locked));
        assert!(locked.layers[0].3);
        cache.set_budget(0);
        assert_eq!(cache.bytes(), 0);
        assert_eq!(cache.selection(&d, &handles).common, edited.common);
        assert_eq!(cache.bytes(), 0);
    }
}
