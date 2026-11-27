use std::cell::Cell;
use std::fmt;
use std::rc::Rc;

use rune::{Color, Cx, Drag, El, GesturePhase, Key, KeyEvent, Live, Mods, Phase, Tx, WheelEvent, col, el};
use rune_kit::Kit;

use crate::state::AppState;

const BAR_HEIGHT: f32 = 12.0;
const TRACK_PAD_X: f32 = 4.0;
const THUMB_HEIGHT: f32 = 6.0;
const THUMB_Y: f32 = 3.0;
const MIN_THUMB_WIDTH: f32 = 32.0;
const OVERFLOW_EPS: f32 = 0.5;
const KEY_STEP_PX: f32 = 120.0;
const PHASED_WHEEL_IDLE: f64 = 1.0;
const UNPHASED_WHEEL_IDLE: f64 = 0.1;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
enum EmptyWheelLatch {
    #[default]
    Idle,
    Armed {
        at: f64,
    },
    LatchedX {
        at: f64,
    },
    EndedX {
        at: f64,
    },
    UnphasedX {
        at: f64,
        mods: Mods,
    },
}

struct HScrollInner {
    viewport_x: Cell<f32>,
    viewport_w: Cell<f32>,
    content_w: Cell<f32>,
    scroll_x: Cell<f32>,
    has_rows: Cell<bool>,
    empty_wheel: Cell<EmptyWheelLatch>,
    hovered: Cell<bool>,
    drag: Cell<Option<(f32, f32)>>,
    rest_fill: Cell<Color>,
    hover_fill: Cell<Color>,
    thumb_rect: Live<[f32; 4]>,
    thumb_fill: Live<Color>,
}

/// Live horizontal scroll metrics and thumb state for a virtualized `rune_kit::Table`.
///
/// Tracks viewport/row geometry via `tx.live_only()` layout observers so dragging,
/// trackpad swiping, Shift+wheel scrolling, and arrow navigation never trigger view rebuilds.
#[derive(Clone)]
pub struct TableHScroll {
    inner: Rc<HScrollInner>,
}

impl Default for TableHScroll {
    fn default() -> Self {
        Self {
            inner: Rc::new(HScrollInner {
                viewport_x: Cell::new(0.0),
                viewport_w: Cell::new(0.0),
                content_w: Cell::new(0.0),
                scroll_x: Cell::new(0.0),
                has_rows: Cell::new(false),
                empty_wheel: Cell::new(EmptyWheelLatch::Idle),
                hovered: Cell::new(false),
                drag: Cell::new(None),
                rest_fill: Cell::new(Color::TRANSPARENT),
                hover_fill: Cell::new(Color::TRANSPARENT),
                thumb_rect: Live::new([TRACK_PAD_X, THUMB_Y, 0.0, THUMB_HEIGHT]),
                thumb_fill: Live::new(Color::TRANSPARENT),
            }),
        }
    }
}

impl fmt::Debug for TableHScroll {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TableHScroll").finish_non_exhaustive()
    }
}

impl TableHScroll {
    /// Attaches a live layout observer to a mounted table row so horizontal scroll
    /// deltas update the scrollbar thumb without scheduling a view commit.
    pub fn attach_row(&self, row_el: El) -> El {
        let hscroll = self.clone();
        row_el.on_layout_any(move |rect, tx| {
            tx.live_only();
            hscroll.observe_row(rect);
        })
    }

    fn max_x(&self) -> f32 {
        (self.inner.content_w.get() - self.inner.viewport_w.get()).max(0.0)
    }

    fn viewport_w(&self) -> f32 {
        self.inner.viewport_w.get()
    }

    fn scroll_x(&self) -> f32 {
        self.inner.scroll_x.get()
    }

    fn has_rows(&self) -> bool {
        self.inner.has_rows.get()
    }

    fn track_w(&self) -> f32 {
        (self.inner.viewport_w.get() - 2.0 * TRACK_PAD_X).max(0.0)
    }

    fn thumb_geometry(&self, track_w: f32) -> Option<(f32, f32, f32, f32)> {
        let max_x = self.max_x();
        let viewport_w = self.inner.viewport_w.get();
        let content_w = self.inner.content_w.get();
        if max_x <= OVERFLOW_EPS || track_w <= 1.0 || content_w <= 0.0 {
            return None;
        }
        let thumb_w = (track_w * (viewport_w / content_w)).clamp(MIN_THUMB_WIDTH.min(track_w), track_w);
        let travel = (track_w - thumb_w).max(0.0);
        let frac = (self.inner.scroll_x.get() / max_x).clamp(0.0, 1.0);
        let thumb_x = travel * frac;
        Some((thumb_x, thumb_w, travel, max_x))
    }

    fn sync_thumb_rect(&self) {
        let rect = match self.thumb_geometry(self.track_w()) {
            Some((thumb_x, thumb_w, _, _)) => [TRACK_PAD_X + thumb_x, THUMB_Y, thumb_w, THUMB_HEIGHT],
            None => [TRACK_PAD_X, THUMB_Y, 0.0, THUMB_HEIGHT],
        };
        self.inner.thumb_rect.snap(rect);
    }

    fn sync_thumb_fill(&self) {
        let color = if self.inner.hovered.get() || self.inner.drag.get().is_some() {
            self.inner.hover_fill.get()
        } else {
            self.inner.rest_fill.get()
        };
        self.inner.thumb_fill.snap(color);
    }

    fn sync_view_metrics(
        &self,
        viewport_x: f32,
        viewport_w: f32,
        content_w: f32,
        has_rows: bool,
        rest_fill: Color,
        hover_fill: Color,
    ) {
        if viewport_w > 0.0 {
            self.inner.viewport_x.set(viewport_x);
        }
        self.inner.viewport_w.set(viewport_w);
        self.inner.content_w.set(content_w);
        self.inner.has_rows.set(has_rows);
        if has_rows {
            self.inner.empty_wheel.set(EmptyWheelLatch::Idle);
        }
        let max_x = self.max_x();
        self.inner.scroll_x.set(self.inner.scroll_x.get().clamp(0.0, max_x));
        self.inner.rest_fill.set(rest_fill);
        self.inner.hover_fill.set(hover_fill);
        self.sync_thumb_rect();
        self.sync_thumb_fill();
    }

    fn observe_viewport(&self, rect: [f32; 4]) {
        let prev_x = self.inner.viewport_x.get();
        let prev_w = self.inner.viewport_w.get();
        if (prev_x - rect[0]).abs() <= 0.01 && (prev_w - rect[2]).abs() <= 0.01 {
            return;
        }
        self.inner.viewport_x.set(rect[0]);
        self.inner.viewport_w.set(rect[2]);
        let max_x = self.max_x();
        self.inner.scroll_x.set(self.inner.scroll_x.get().clamp(0.0, max_x));
        self.sync_thumb_rect();
    }

    fn observe_row(&self, rect: [f32; 4]) {
        let prev_content_w = self.inner.content_w.get();
        let next_content_w = if rect[2] > 0.0 { rect[2] } else { prev_content_w };
        let max_x = (next_content_w - self.inner.viewport_w.get()).max(0.0);
        let next_scroll_x = (self.inner.viewport_x.get() - rect[0]).clamp(0.0, max_x);
        let prev_scroll_x = self.inner.scroll_x.get();
        if (prev_content_w - next_content_w).abs() <= 0.01 && (prev_scroll_x - next_scroll_x).abs() <= 0.01 {
            return;
        }
        self.inner.content_w.set(next_content_w);
        self.inner.scroll_x.set(next_scroll_x);
        self.sync_thumb_rect();
    }

    fn set_hovered(&self, hovered: bool) {
        if self.inner.hovered.replace(hovered) != hovered {
            self.sync_thumb_fill();
        }
    }

    fn scroll_to(&self, target_x: f32, viewport_key: &str, tx: &mut Tx) {
        let clamped = target_x.clamp(0.0, self.max_x());
        self.inner.scroll_x.set(clamped);
        self.sync_thumb_rect();
        tx.scroll_to_x(viewport_key, clamped);
    }

    fn handle_wheel(&self, ev: WheelEvent, viewport_key: &str, tx: &mut Tx) -> bool {
        if ev.touch || !ev.dx.is_finite() || !ev.dy.is_finite() || self.max_x() <= OVERFLOW_EPS {
            return false;
        }
        let shift_vertical_native = ev.mods.shift
            && !ev.mods.ctrl
            && ev.dx == 0.0
            && ev.dy != 0.0
            && (!cfg!(target_os = "macos") || !ev.precise);
        let shift_vertical_precise = ev.mods.shift
            && !ev.mods.ctrl
            && !ev.mods.cmd
            && !ev.mods.alt
            && !ev.mods.logo
            && ev.dx == 0.0
            && ev.dy != 0.0
            && ev.precise;

        if self.has_rows() {
            self.inner.empty_wheel.set(EmptyWheelLatch::Idle);
            if !shift_vertical_precise {
                return false;
            }
            tx.live_only();
            self.scroll_to(self.scroll_x() - ev.dy, viewport_key, tx);
            return true;
        }

        // With zero rows mounted to report row layout moves, latch phased and unphased
        // horizontal swipes here so slightly diagonal middle deltas never fall through to
        // vertical lock, and apply the Shift conversion consistently across every phase.
        let now = tx.clock();
        let latch = self.inner.empty_wheel.get();
        let (eff_dx, eff_dy) = if shift_vertical_precise || shift_vertical_native {
            (ev.dy, 0.0)
        } else {
            (ev.dx, ev.dy)
        };
        let unlatched_delta = (eff_dx != 0.0 && eff_dx.abs() > eff_dy.abs()).then_some(eff_dx);

        match ev.phase {
            GesturePhase::Began => {
                if ev.momentum
                    && matches!(
                        latch,
                        EmptyWheelLatch::EndedX { at } | EmptyWheelLatch::LatchedX { at }
                            if now - at <= PHASED_WHEEL_IDLE
                    )
                {
                    self.inner.empty_wheel.set(EmptyWheelLatch::LatchedX { at: now });
                    tx.live_only();
                    if eff_dx != 0.0 {
                        self.scroll_to(self.scroll_x() - eff_dx, viewport_key, tx);
                    }
                    true
                } else if eff_dx == 0.0 && eff_dy == 0.0 {
                    self.inner.empty_wheel.set(EmptyWheelLatch::Armed { at: now });
                    false
                } else if let Some(delta) = unlatched_delta {
                    self.inner.empty_wheel.set(EmptyWheelLatch::LatchedX { at: now });
                    tx.live_only();
                    self.scroll_to(self.scroll_x() - delta, viewport_key, tx);
                    true
                } else {
                    self.inner.empty_wheel.set(EmptyWheelLatch::Idle);
                    false
                }
            }
            GesturePhase::Moved => match latch {
                EmptyWheelLatch::LatchedX { at } if now - at <= PHASED_WHEEL_IDLE => {
                    self.inner.empty_wheel.set(EmptyWheelLatch::LatchedX { at: now });
                    tx.live_only();
                    if eff_dx != 0.0 {
                        self.scroll_to(self.scroll_x() - eff_dx, viewport_key, tx);
                    }
                    true
                }
                EmptyWheelLatch::EndedX { at } if ev.momentum && now - at <= PHASED_WHEEL_IDLE => {
                    self.inner.empty_wheel.set(EmptyWheelLatch::LatchedX { at: now });
                    tx.live_only();
                    if eff_dx != 0.0 {
                        self.scroll_to(self.scroll_x() - eff_dx, viewport_key, tx);
                    }
                    true
                }
                EmptyWheelLatch::UnphasedX { at, mods }
                    if now - at <= UNPHASED_WHEEL_IDLE && mods == ev.mods && eff_dx != 0.0 =>
                {
                    self.inner
                        .empty_wheel
                        .set(EmptyWheelLatch::UnphasedX { at: now, mods: ev.mods });
                    tx.live_only();
                    self.scroll_to(self.scroll_x() - eff_dx, viewport_key, tx);
                    true
                }
                EmptyWheelLatch::Armed { at } if now - at <= PHASED_WHEEL_IDLE => {
                    if eff_dx == 0.0 && eff_dy == 0.0 {
                        return false;
                    }
                    if let Some(delta) = unlatched_delta {
                        self.inner.empty_wheel.set(EmptyWheelLatch::LatchedX { at: now });
                        tx.live_only();
                        self.scroll_to(self.scroll_x() - delta, viewport_key, tx);
                        true
                    } else {
                        self.inner.empty_wheel.set(EmptyWheelLatch::Idle);
                        false
                    }
                }
                _ => {
                    if let Some(delta) = unlatched_delta {
                        self.inner
                            .empty_wheel
                            .set(EmptyWheelLatch::UnphasedX { at: now, mods: ev.mods });
                        tx.live_only();
                        self.scroll_to(self.scroll_x() - delta, viewport_key, tx);
                        true
                    } else {
                        self.inner.empty_wheel.set(EmptyWheelLatch::Idle);
                        false
                    }
                }
            },
            GesturePhase::Ended => {
                if matches!(latch, EmptyWheelLatch::LatchedX { at } if now - at <= PHASED_WHEEL_IDLE) {
                    self.inner.empty_wheel.set(if ev.momentum {
                        EmptyWheelLatch::Idle
                    } else {
                        EmptyWheelLatch::EndedX { at: now }
                    });
                    tx.live_only();
                    if eff_dx != 0.0 {
                        self.scroll_to(self.scroll_x() - eff_dx, viewport_key, tx);
                    }
                    true
                } else {
                    self.inner.empty_wheel.set(EmptyWheelLatch::Idle);
                    false
                }
            }
        }
    }

    fn handle_drag(&self, viewport_key: &str, d: &mut Drag, tx: &mut Tx) {
        let track_w = if d.w > 2.0 * TRACK_PAD_X + 1.0 {
            d.w - 2.0 * TRACK_PAD_X
        } else {
            self.track_w()
        };
        let Some((thumb_x, thumb_w, travel, max_x)) = self.thumb_geometry(track_w) else {
            self.inner.drag.set(None);
            self.sync_thumb_fill();
            return;
        };
        let local_x = d.x - TRACK_PAD_X;

        match d.phase {
            Phase::Down => {
                let start_scroll_x = self.inner.scroll_x.get();
                let on_thumb = (thumb_x..=thumb_x + thumb_w).contains(&local_x);
                let grab = if on_thumb {
                    local_x - thumb_x
                } else {
                    thumb_w * 0.5
                };
                self.inner.drag.set(Some((grab, start_scroll_x)));
                self.sync_thumb_fill();
                if !on_thumb {
                    let target_scroll = scroll_from_thumb_x(local_x - grab, travel, max_x);
                    self.scroll_to(target_scroll, viewport_key, tx);
                }
            }
            Phase::Move => {
                let Some((grab, _)) = self.inner.drag.get() else {
                    return;
                };
                let target_scroll = scroll_from_thumb_x(local_x - grab, travel, max_x);
                self.scroll_to(target_scroll, viewport_key, tx);
            }
            Phase::Up => {
                let prev = self.inner.drag.take();
                if d.cancelled()
                    && let Some((_, start_scroll_x)) = prev
                {
                    self.scroll_to(start_scroll_x, viewport_key, tx);
                }
                self.sync_thumb_fill();
            }
        }
    }
}

fn scroll_from_thumb_x(thumb_x: f32, travel: f32, max_x: f32) -> f32 {
    if travel <= 0.0 || max_x <= 0.0 {
        return 0.0;
    }
    (thumb_x.clamp(0.0, travel) / travel) * max_x
}

/// Wraps a `rune_kit::Table` element with horizontal keyboard navigation,
/// macOS Shift+precise-wheel support, zero-row wheel synchronization, and an
/// overflow-only bottom scrollbar.
pub fn wrap_table_hscroll(
    table_key: &str,
    hscroll: &TableHScroll,
    table_el: El,
    has_rows: bool,
    cx: &Cx<AppState>,
    k: &Kit,
) -> El {
    let viewport_key = format!("{table_key}/viewport");
    let content_key = format!("{table_key}/viewport/content");

    // Read static layout rects (`cx.layout_with`) rather than `window_layout` so
    // scrolling never triggers view commits or Rune's CommitMeter warnings.
    let view_w = cx.layout_with(&viewport_key, |r| r[2]).unwrap_or(0.0);
    let view_x = cx.layout_with(&viewport_key, |r| r[0]).unwrap_or(0.0);
    let content_w = cx.layout_with(&content_key, |r| r[2]).unwrap_or(0.0);

    let rest_fill = k.alpha(k.c.text_muted, if k.dark { 0.42 } else { 0.36 });
    let hover_fill = k.alpha(k.c.text_muted, if k.dark { 0.72 } else { 0.62 });
    hscroll.sync_view_metrics(view_x, view_w, content_w, has_rows, rest_fill, hover_fill);

    let vp_obs = hscroll.clone();
    let table_el = table_el.on_layout_any(move |rect, tx| {
        tx.live_only();
        vp_obs.observe_viewport(rect);
    });

    let wheel_hscroll = hscroll.clone();
    let wheel_vp_key = viewport_key.clone();
    let key_hscroll = hscroll.clone();
    let key_vp_key = viewport_key.clone();

    let mut shell = col()
        .key(format!("{table_key}-scroll-shell"))
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        // Rune converts discrete Shift+wheel on macOS into horizontal scroll, but leaves
        // precise Shift+wheel vertical. When no rows are mounted to report row layout moves,
        // every wheel gesture Rune would scroll horizontally (including modified/phased
        // horizontal swipes and native Shift+wheel) is routed here so `scroll_x` stays synced.
        .on_wheel(move |_s: &mut AppState, ev: WheelEvent, tx: &mut Tx| {
            wheel_hscroll.handle_wheel(ev, &wheel_vp_key, tx)
        })
        .on_key_any(move |ev: &KeyEvent, tx: &mut Tx| {
            if ev.mods.cmd
                || ev.mods.ctrl
                || ev.mods.alt
                || ev.mods.logo
                || key_hscroll.max_x() <= OVERFLOW_EPS
            {
                return false;
            }
            let dir = match ev.key {
                Key::Left => -1.0,
                Key::Right => 1.0,
                _ => return false,
            };
            let step = if ev.mods.shift {
                (key_hscroll.viewport_w() * 0.8).max(160.0)
            } else {
                KEY_STEP_PX
            };
            tx.live_only();
            let next = key_hscroll.scroll_x() + dir * step;
            key_hscroll.scroll_to(next, &key_vp_key, tx);
            true
        })
        .child(table_el);

    if view_w > 0.0 && content_w > view_w + OVERFLOW_EPS {
        let hover_hscroll = hscroll.clone();
        let drag_hscroll = hscroll.clone();
        let drag_vp_key = viewport_key.clone();

        let thumb = el()
            .key(format!("{table_key}-hscroll-thumb"))
            .absolute(0.0, 0.0)
            .live_rect(&hscroll.inner.thumb_rect)
            .live_fill(&hscroll.inner.thumb_fill)
            .radius(THUMB_HEIGHT * 0.5);

        let track = el()
            .key(format!("{table_key}-hscroll-track"))
            .role("scrollbar")
            .label("Horizontal scroll")
            .preserve_focus_on_press(true)
            .w_full()
            .h_full()
            .wheel_scrolls(viewport_key)
            .on_hover_change_any(move |inside, tx| {
                tx.live_only();
                hover_hscroll.set_hovered(inside);
            })
            .on_drag_any(move |d: &mut Drag, tx: &mut Tx| {
                tx.live_only();
                drag_hscroll.handle_drag(&drag_vp_key, d, tx);
            })
            .child(thumb);

        let bar = el()
            .key(format!("{table_key}-hscroll-bar"))
            .preserve_focus_on_press(true)
            .w_full()
            .h(BAR_HEIGHT)
            .fill(k.c.layer_01)
            .border_sides([1.0, 0.0, 0.0, 0.0], k.c.border_muted)
            .child(track);

        shell = shell.child(bar);
    }

    shell
}
