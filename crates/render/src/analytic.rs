//! Optional interactive GPU hints. Ordinary vertices remain available to software drawing,
//! picking and exports, and unsupported transforms simply omit the hint.

use cadcraft_doc::EntityKind;
use cadcraft_geom::{Mat3, Vec2};

#[derive(Clone, Copy, Debug)]
pub struct ArcHint {
    pub prim: usize,
    pub center: Vec2,
    pub radius: f64,
    pub start: f64,
    pub sweep: f64,
    pub tolerance: f64,
}

impl ArcHint {
    pub(crate) fn new(kind: &EntityKind, xf: Mat3, prim: usize, tolerance: f64) -> Option<Self> {
        let (center, radius, start, sweep) = match kind {
            EntityKind::Circle(c) => (c.center.xy(), c.radius.abs(), 0.0, std::f64::consts::TAU),
            EntityKind::Arc(a) => {
                let arc = cadcraft_geom::Arc::new(a.center.xy(), a.radius, a.start, a.end);
                (arc.center, arc.radius, arc.start, arc.sweep())
            }
            _ => return None,
        };
        let mut hint = Self { prim, center, radius, start, sweep, tolerance: 0.0 }.transformed(xf, prim)?;
        hint.tolerance = tolerance;
        // A full bounding quad is economical only for small symbols. Large curves retain the
        // segment renderer, avoiding large areas of discarded fragments during navigation.
        let size = hint.radius / tolerance;
        (tolerance.is_finite() && (6.0..=64.0).contains(&size) && hint.sweep >= std::f64::consts::FRAC_PI_2).then_some(hint)
    }

    pub fn transformed(self, xf: Mat3, prim: usize) -> Option<Self> {
        let x = Vec2::new(xf.a, xf.b);
        let y = Vec2::new(xf.c, xf.d);
        let scale = x.len();
        let error = scale * scale * 1e-10;
        if !scale.is_finite() || scale <= 0.0 || (x.dot(y)).abs() > error || (x.dot(x) - y.dot(y)).abs() > error {
            return None;
        }
        let mut direction = Vec2::new(self.start.cos(), self.start.sin());
        if xf.a * xf.d - xf.b * xf.c < 0.0 {
            let end = self.start + self.sweep;
            direction = Vec2::new(end.cos(), end.sin());
        }
        direction = Vec2::new(xf.a * direction.x + xf.c * direction.y, xf.b * direction.x + xf.d * direction.y);
        let center = xf.apply(self.center);
        let radius = self.radius * scale;
        (center.is_finite() && radius.is_finite() && radius > 0.0 && direction.is_finite()).then_some(Self {
            prim,
            center,
            radius,
            start: direction.y.atan2(direction.x),
            tolerance: self.tolerance * scale,
            ..self
        })
    }
}
