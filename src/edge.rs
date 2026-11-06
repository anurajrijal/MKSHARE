//! Screen-edge switching logic that does not depend on the operating system.

use crate::proto::Side;
use std::time::{Duration, Instant};

/// After crossing, ignore the edge for a moment so the cursor does not bounce back.
pub const COOLDOWN: Duration = Duration::from_millis(400);

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

/// Is the point on (or past) the given edge of the screen?
pub fn at_edge(side: Side, r: &Rect, x: i32, y: i32) -> bool {
    match side {
        Side::Left => x <= r.x,
        Side::Right => x >= r.x + r.w - 1,
        Side::Top => y <= r.y,
        Side::Bottom => y >= r.y + r.h - 1,
    }
}

/// Position along the edge as a fraction 0..1 (vertical for left/right, horizontal for top/bottom).
pub fn along(side: Side, r: &Rect, x: i32, y: i32) -> f32 {
    let f = |v: i32, origin: i32, len: i32| ((v - origin) as f32 / (len - 1).max(1) as f32).clamp(0.0, 1.0);
    match side {
        Side::Left | Side::Right => f(y, r.y, r.h),
        Side::Top | Side::Bottom => f(x, r.x, r.w),
    }
}

/// Where (fractions of the screen) the cursor appears when it enters a screen through `side`.
pub fn enter_point(side: Side, along: f32) -> (f32, f32) {
    let a = along.clamp(0.0, 1.0);
    match side {
        Side::Left => (0.0, a),
        Side::Right => (1.0, a),
        Side::Top => (a, 0.0),
        Side::Bottom => (a, 1.0),
    }
}

/// Does a movement of (dx, dy) push towards the given edge?
pub fn outward(side: Side, dx: i32, dy: i32) -> bool {
    match side {
        Side::Left => dx < 0,
        Side::Right => dx > 0,
        Side::Top => dy < 0,
        Side::Bottom => dy > 0,
    }
}

/// A cursor position estimated from relative movements. Used where the real
/// position cannot be read (Linux). It is exact at the screen edges, because
/// both the real and the estimated cursor are clamped there.
pub struct VirtualCursor {
    rect: Rect,
    x: f64,
    y: f64,
}

impl VirtualCursor {
    pub fn new(rect: Rect) -> Self {
        VirtualCursor { rect, x: rect.x as f64 + rect.w as f64 / 2.0, y: rect.y as f64 + rect.h as f64 / 2.0 }
    }

    pub fn set_frac(&mut self, xf: f32, yf: f32) {
        self.x = self.rect.x as f64 + xf.clamp(0.0, 1.0) as f64 * (self.rect.w - 1) as f64;
        self.y = self.rect.y as f64 + yf.clamp(0.0, 1.0) as f64 * (self.rect.h - 1) as f64;
    }

    pub fn apply(&mut self, dx: i32, dy: i32) {
        let r = self.rect;
        self.x = (self.x + dx as f64).clamp(r.x as f64, (r.x + r.w - 1) as f64);
        self.y = (self.y + dy as f64).clamp(r.y as f64, (r.y + r.h - 1) as f64);
    }

    pub fn pos(&self) -> (i32, i32) {
        (self.x.round() as i32, self.y.round() as i32)
    }
}

/// Client-side edge state: decides when the cursor leaves the client's screen
/// through the edge that leads back to the server.
pub struct ClientEdge {
    back: Option<Side>,
    active: bool,
    leaving: bool,
    entered: Instant,
    rect: Option<Rect>,
    vc: Option<VirtualCursor>,
}

impl ClientEdge {
    pub fn new(rect: Option<Rect>) -> Self {
        ClientEdge {
            back: None,
            active: false,
            leaving: false,
            entered: Instant::now(),
            rect,
            vc: rect.map(VirtualCursor::new),
        }
    }

    pub fn set_config(&mut self, back: Option<Side>) {
        self.back = back;
    }

    pub fn set_active(&mut self, on: bool) {
        self.active = on;
        self.leaving = false;
        self.entered = Instant::now();
    }

    /// The server put the cursor at (x, y) (fractions of the screen).
    pub fn entered(&mut self, x: f32, y: f32) {
        self.entered = Instant::now();
        self.leaving = false;
        if let Some(vc) = self.vc.as_mut() {
            vc.set_frac(x, y);
        }
    }

    /// Call before a movement is applied. Returns the cursor position (fractions)
    /// when the cursor is on the return edge and is being pushed outwards.
    /// `real` is the true cursor position if the platform can provide it.
    pub fn check_leave(&mut self, dx: i32, dy: i32, real: Option<(i32, i32)>) -> Option<(f32, f32)> {
        let back = self.back?;
        if !self.active || self.leaving || self.entered.elapsed() < COOLDOWN || !outward(back, dx, dy) {
            return None;
        }
        let rect = self.rect?;
        let (x, y) = real.or_else(|| self.vc.as_ref().map(|v| v.pos()))?;
        if !at_edge(back, &rect, x, y) {
            return None;
        }
        self.leaving = true;
        let fx = ((x - rect.x) as f32 / (rect.w - 1).max(1) as f32).clamp(0.0, 1.0);
        let fy = ((y - rect.y) as f32 / (rect.h - 1).max(1) as f32).clamp(0.0, 1.0);
        Some((fx, fy))
    }

    /// Keep the estimated position up to date after a movement was applied.
    pub fn moved(&mut self, dx: i32, dy: i32) {
        if let Some(vc) = self.vc.as_mut() {
            vc.apply(dx, dy);
        }
    }

    pub fn can_track(&self) -> bool {
        self.rect.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const R: Rect = Rect { x: 0, y: 0, w: 1920, h: 1080 };

    #[test]
    fn edges() {
        assert!(at_edge(Side::Right, &R, 1919, 500));
        assert!(!at_edge(Side::Right, &R, 1918, 500));
        assert!(at_edge(Side::Left, &R, 0, 500));
        assert!(at_edge(Side::Top, &R, 10, 0));
        assert!(at_edge(Side::Bottom, &R, 10, 1079));
        // multi-monitor layout with a negative origin
        let r2 = Rect { x: -1920, y: 0, w: 3840, h: 1080 };
        assert!(at_edge(Side::Left, &r2, -1920, 5));
        assert!(at_edge(Side::Right, &r2, 1919, 5));
    }

    #[test]
    fn fractions_and_entry_points() {
        assert_eq!(along(Side::Right, &R, 1919, 0), 0.0);
        assert_eq!(along(Side::Right, &R, 1919, 1079), 1.0);
        assert!((along(Side::Top, &R, 960, 0) - 0.5).abs() < 0.01);
        // leaving the server's right edge at 25% height -> client's left edge at 25% height
        assert_eq!(enter_point(Side::Right.opposite(), 0.25), (0.0, 0.25));
        assert_eq!(enter_point(Side::Bottom.opposite(), 0.75), (0.75, 0.0));
        assert_eq!(enter_point(Side::Left, 2.0), (0.0, 1.0)); // clamped
    }

    #[test]
    fn outward_direction() {
        assert!(outward(Side::Left, -1, 0));
        assert!(!outward(Side::Left, 1, 0));
        assert!(outward(Side::Bottom, 0, 3));
        assert!(!outward(Side::Top, 0, 3));
    }

    #[test]
    fn virtual_cursor_clamps_and_resyncs() {
        let mut v = VirtualCursor::new(R);
        assert_eq!(v.pos(), (960, 540));
        v.apply(100_000, -100_000); // pushing into the top-right corner
        assert_eq!(v.pos(), (1919, 0));
        v.apply(-10, 20);
        assert_eq!(v.pos(), (1909, 20));
        v.set_frac(0.0, 1.0);
        assert_eq!(v.pos(), (0, 1079));
    }

    #[test]
    fn client_leaves_only_when_pushing_out_at_the_edge() {
        let mut c = ClientEdge::new(Some(R));
        c.set_config(Some(Side::Left)); // server is on the left of the client
        c.set_active(true);
        c.entered(0.0, 0.5);
        // right after entering: ignored (cooldown)
        assert!(c.check_leave(-5, 0, Some((0, 540))).is_none());
        std::thread::sleep(COOLDOWN + Duration::from_millis(50));
        // not at the edge
        assert!(c.check_leave(-5, 0, Some((300, 540))).is_none());
        // at the edge but moving away from it
        assert!(c.check_leave(5, 0, Some((0, 540))).is_none());
        // at the edge and pushing out: leaves, once
        let (x, y) = c.check_leave(-5, 0, Some((0, 540))).unwrap();
        assert_eq!(x, 0.0);
        assert!((y - 0.5).abs() < 0.01);
        assert!(c.check_leave(-5, 0, Some((0, 540))).is_none());
    }

    #[test]
    fn client_without_edge_config_never_leaves() {
        let mut c = ClientEdge::new(Some(R));
        c.set_active(true);
        std::thread::sleep(COOLDOWN + Duration::from_millis(50));
        assert!(c.check_leave(-5, 0, Some((0, 0))).is_none());
    }

    #[test]
    fn virtual_tracking_used_when_real_position_unknown() {
        let mut c = ClientEdge::new(Some(R));
        c.set_config(Some(Side::Left));
        c.set_active(true);
        c.entered(0.0, 0.5);
        std::thread::sleep(COOLDOWN + Duration::from_millis(50));
        c.moved(400, 0); // move into the screen
        assert!(c.check_leave(-5, 0, None).is_none());
        c.moved(-1000, 0); // pushed back to the left edge (clamped)
        assert!(c.check_leave(-5, 0, None).is_some());
    }
}
