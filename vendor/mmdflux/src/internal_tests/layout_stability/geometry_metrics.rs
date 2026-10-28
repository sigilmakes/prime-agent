use super::mmds_metrics::COORD_EPS;
use crate::graph::geometry::{FPoint, FRect};

pub(crate) fn bend_count(path: &[FPoint]) -> usize {
    path.windows(3)
        .filter(|points| {
            let first = segment_axis(points[0], points[1]);
            let second = segment_axis(points[1], points[2]);
            first != Axis::Point && second != Axis::Point && first != second
        })
        .count()
}

pub(crate) fn polyline_length(path: &[FPoint]) -> f64 {
    path.windows(2)
        .map(|points| {
            let dx = points[1].x - points[0].x;
            let dy = points[1].y - points[0].y;
            dx.hypot(dy)
        })
        .sum()
}

pub(crate) fn route_envelope(path: &[FPoint]) -> Option<FRect> {
    let first = path.first()?;
    let mut min_x = first.x;
    let mut max_x = first.x;
    let mut min_y = first.y;
    let mut max_y = first.y;

    for point in path {
        min_x = min_x.min(point.x);
        max_x = max_x.max(point.x);
        min_y = min_y.min(point.y);
        max_y = max_y.max(point.y);
    }

    Some(FRect::new(min_x, min_y, max_x - min_x, max_y - min_y))
}

pub(crate) fn rects_overlap(a: FRect, b: FRect) -> bool {
    a.x < b.x + b.width && a.x + a.width > b.x && a.y < b.y + b.height && a.y + a.height > b.y
}

pub(crate) fn distance_point_to_path(point: FPoint, path: &[FPoint]) -> f64 {
    match path {
        [] => f64::INFINITY,
        [only] => distance(point, *only),
        _ => path
            .windows(2)
            .map(|segment| distance_point_to_segment(point, segment[0], segment[1]))
            .fold(f64::INFINITY, f64::min),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Axis {
    Horizontal,
    Vertical,
    Diagonal,
    Point,
}

fn segment_axis(start: FPoint, end: FPoint) -> Axis {
    let dx = end.x - start.x;
    let dy = end.y - start.y;
    if dx.abs() <= COORD_EPS && dy.abs() <= COORD_EPS {
        Axis::Point
    } else if dy.abs() <= COORD_EPS {
        Axis::Horizontal
    } else if dx.abs() <= COORD_EPS {
        Axis::Vertical
    } else {
        Axis::Diagonal
    }
}

fn distance_point_to_segment(point: FPoint, start: FPoint, end: FPoint) -> f64 {
    let dx = end.x - start.x;
    let dy = end.y - start.y;
    let length_squared = dx * dx + dy * dy;
    if length_squared <= COORD_EPS * COORD_EPS {
        return distance(point, start);
    }

    let t =
        (((point.x - start.x) * dx + (point.y - start.y) * dy) / length_squared).clamp(0.0, 1.0);
    let projection = FPoint::new(start.x + t * dx, start.y + t * dy);
    distance(point, projection)
}

fn distance(a: FPoint, b: FPoint) -> f64 {
    (a.x - b.x).hypot(a.y - b.y)
}
