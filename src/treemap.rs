//! Squarified treemap layout (Bruls, Huizing, van Wijk).
//!
//! Given weighted items and a rectangle, produce a rectangle per item that
//! tiles the area with aspect ratios kept as close to square as possible.

use eframe::egui::{Rect, pos2, vec2};

/// Lay out `items` (index, weight) inside `rect`. Zero-weight items are dropped.
pub fn squarified(items: &[(usize, u64)], rect: Rect) -> Vec<(usize, Rect)> {
    let mut out = Vec::new();
    let total: f64 = items.iter().map(|(_, s)| *s as f64).sum();
    if total <= 0.0 || rect.width() <= 1.0 || rect.height() <= 1.0 {
        return out;
    }
    let area = rect.width() as f64 * rect.height() as f64;
    let scaled: Vec<(usize, f64)> = items
        .iter()
        .filter(|(_, s)| *s > 0)
        .map(|(i, s)| (*i, *s as f64 / total * area))
        .collect();
    squarify(&scaled, rect, &mut out);
    out
}

/// Worst (largest) aspect ratio in a candidate row laid along `side`.
fn worst(row: &[(usize, f64)], sum: f64, side: f64) -> f64 {
    if sum <= 0.0 || side <= 0.0 {
        return f64::INFINITY;
    }
    let thickness = sum / side;
    let mut w = 1.0_f64;
    for (_, a) in row {
        if *a <= 0.0 {
            continue;
        }
        let length = a / thickness;
        let r = (thickness / length).max(length / thickness);
        if r > w {
            w = r;
        }
    }
    w
}

fn squarify(items: &[(usize, f64)], mut rect: Rect, out: &mut Vec<(usize, Rect)>) {
    let mut i = 0;
    while i < items.len() {
        let side = rect.width().min(rect.height()) as f64;
        let mut end = i + 1;
        let mut sum = items[i].1;
        let mut best = worst(&items[i..end], sum, side);
        while end < items.len() {
            let nsum = sum + items[end].1;
            let w = worst(&items[i..end + 1], nsum, side);
            if w > best {
                break;
            }
            best = w;
            sum = nsum;
            end += 1;
        }
        rect = lay_row(&items[i..end], sum, rect, out);
        i = end;
    }
}

/// Place one row across the shorter side of `rect`; return the remaining rect.
fn lay_row(row: &[(usize, f64)], sum: f64, rect: Rect, out: &mut Vec<(usize, Rect)>) -> Rect {
    let w = rect.width() as f64;
    let h = rect.height() as f64;
    if sum <= 0.0 {
        return rect;
    }
    if w <= h {
        let t = (sum / w) as f32;
        let mut cx = rect.min.x;
        for (idx, a) in row {
            let seg = (*a / sum * w) as f32;
            out.push((
                *idx,
                Rect::from_min_size(pos2(cx, rect.min.y), vec2(seg, t)),
            ));
            cx += seg;
        }
        Rect::from_min_max(pos2(rect.min.x, rect.min.y + t), rect.max)
    } else {
        let t = (sum / h) as f32;
        let mut cy = rect.min.y;
        for (idx, a) in row {
            let seg = (*a / sum * h) as f32;
            out.push((
                *idx,
                Rect::from_min_size(pos2(rect.min.x, cy), vec2(t, seg)),
            ));
            cy += seg;
        }
        Rect::from_min_max(pos2(rect.min.x + t, rect.min.y), rect.max)
    }
}

#[cfg(test)]
mod tests {
    use super::squarified;
    use eframe::egui::{Rect, pos2, vec2};

    #[test]
    fn tiles_stay_inside_and_cover() {
        let rect = Rect::from_min_size(pos2(0.0, 0.0), vec2(400.0, 300.0));
        let items = [(0, 50u64), (1, 30), (2, 15), (3, 5)];
        let tiles = squarified(&items, rect);
        assert_eq!(tiles.len(), 4);
        let area = |r: &Rect| r.width() * r.height();
        let mut covered = 0.0f32;
        for (_, r) in &tiles {
            let inside = r.min.x >= rect.min.x - 0.01
                && r.min.y >= rect.min.y - 0.01
                && r.max.x <= rect.max.x + 0.01
                && r.max.y <= rect.max.y + 0.01;
            assert!(inside, "tile escaped the bounds: {r:?}");
            covered += area(r);
        }
        // Tiles should cover essentially the whole rectangle.
        assert!(
            (covered - area(&rect)).abs() < 1.0,
            "covered {covered} of {}",
            area(&rect)
        );
    }

    #[test]
    fn empty_and_zero_weight() {
        let rect = Rect::from_min_size(pos2(0.0, 0.0), vec2(100.0, 100.0));
        assert!(squarified(&[], rect).is_empty());
        assert!(squarified(&[(0, 0)], rect).is_empty());
    }
}
