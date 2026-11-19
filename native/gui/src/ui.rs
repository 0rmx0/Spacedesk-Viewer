//! Minimal immediate-mode UI for the home menu: labels, buttons, text fields,
//! checkboxes and dropdowns, drawn with the GL renderer. Interaction state
//! (mouse position/button, queued characters/keys) is fed once per frame via
//! [`Ui::begin`]; widgets then both react and draw.

use crate::gl::Renderer;

pub const COL_BG: [f32; 4] = [0.09, 0.10, 0.13, 1.0];
pub const COL_PANEL: [f32; 4] = [0.13, 0.14, 0.18, 1.0];
pub const COL_FIELD: [f32; 4] = [0.05, 0.06, 0.08, 1.0];
pub const COL_BORDER: [f32; 4] = [0.35, 0.38, 0.45, 1.0];
pub const COL_BORDER_FOCUS: [f32; 4] = [0.30, 0.55, 0.95, 1.0];
pub const COL_BTN: [f32; 4] = [0.20, 0.24, 0.32, 1.0];
pub const COL_BTN_HOT: [f32; 4] = [0.28, 0.34, 0.46, 1.0];
pub const COL_BTN_PRESS: [f32; 4] = [0.16, 0.19, 0.26, 1.0];
pub const COL_ACCENT: [f32; 4] = [0.20, 0.50, 0.95, 1.0];
pub const COL_TEXT: [f32; 4] = [0.92, 0.93, 0.96, 1.0];
pub const COL_TEXT_DIM: [f32; 4] = [0.55, 0.58, 0.65, 1.0];
pub const COL_OK: [f32; 4] = [0.35, 0.85, 0.45, 1.0];
pub const COL_ERR: [f32; 4] = [0.95, 0.35, 0.30, 1.0];

pub const FIELD_H: f32 = 26.0;

struct PopupDraw {
    x: f32,
    y: f32,
    w: f32,
    options: &'static [&'static str],
    sel: usize,
}

pub struct Ui {
    pub mouse: (f32, f32),
    pub down: bool,
    prev_down: bool,
    pressed: u32,
    released: u32,
    pub focus: u32,
    pub open_popup: u32,
    /// rect (x, y, w, h) of the open popup's item list, used to block
    /// hit-testing of widgets underneath the popup
    popup_rect: (f32, f32, f32, f32),
    popups: Vec<PopupDraw>,
    pub chars: Vec<char>,
    pub backspace: bool,
    pub enter: bool,
    pub frame: u64,
    hot: u32,
}

impl Ui {
    pub fn new() -> Ui {
        Ui {
            mouse: (0.0, 0.0),
            down: false,
            prev_down: false,
            pressed: 0,
            released: 0,
            focus: 0,
            open_popup: 0,
            popup_rect: (0.0, 0.0, 0.0, 0.0),
            popups: Vec::new(),
            chars: Vec::new(),
            backspace: false,
            enter: false,
            frame: 0,
            hot: 0,
        }
    }

    /// Feed per-frame input. `clicked` is derived from the button transition.
    pub fn begin(
        &mut self,
        mouse: (f32, f32),
        down: bool,
        chars: Vec<char>,
        backspace: bool,
        enter: bool,
    ) {
        let clicked = self.prev_down && !down;
        self.released = if clicked { self.pressed } else { 0 };
        if clicked {
            self.focus = 0;
        }
        self.popups.clear();
        if !down {
            self.pressed = 0;
        }
        self.prev_down = down;
        self.mouse = mouse;
        self.down = down;
        self.chars = chars;
        self.backspace = backspace;
        self.enter = enter;
        self.hot = 0;
        self.frame += 1;
    }

    pub fn close_popup(&mut self) {
        self.open_popup = 0;
        self.popup_rect = (0.0, 0.0, 0.0, 0.0);
    }

    fn hit(&mut self, id: u32, x: f32, y: f32, w: f32, h: f32) -> bool {
        // while a popup is open, widgets underneath it must not capture input
        if self.open_popup != 0 && id != self.open_popup && id / 1000 != self.open_popup {
            let (px, py, pw, ph) = self.popup_rect;
            if self.mouse.0 >= px
                && self.mouse.0 < px + pw
                && self.mouse.1 >= py
                && self.mouse.1 < py + ph
            {
                return false;
            }
        }
        let inside =
            self.mouse.0 >= x && self.mouse.0 < x + w && self.mouse.1 >= y && self.mouse.1 < y + h;
        if inside && self.hot == 0 {
            self.hot = id;
        }
        if inside && self.down {
            self.pressed = id;
        }
        inside
    }

    fn take_click(&mut self, id: u32, inside: bool) -> bool {
        if self.released == id && inside {
            self.released = 0;
            true
        } else {
            false
        }
    }

    /// Returns true on click.
    #[allow(clippy::too_many_arguments)]
    pub fn button(
        &mut self,
        r: &Renderer,
        id: u32,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        label: &str,
        accent: bool,
    ) -> bool {
        let inside = self.hit(id, x, y, w, h);
        let clicked = self.take_click(id, inside);
        let bg = if self.pressed == id && inside {
            COL_BTN_PRESS
        } else if self.hot == id {
            COL_BTN_HOT
        } else if accent {
            COL_ACCENT
        } else {
            COL_BTN
        };
        unsafe {
            r.draw_rect(x, y, w, h, bg);
            r.draw_rect(x, y, w, 1.0, COL_BORDER);
            r.draw_rect(x, y + h - 1.0, w, 1.0, COL_BORDER);
            r.draw_rect(x, y, 1.0, h, COL_BORDER);
            r.draw_rect(x + w - 1.0, y, 1.0, h, COL_BORDER);
            let tw = r.text_width(1.0, label);
            r.draw_text(x + (w - tw) / 2.0, y + h / 2.0 + 7.0, 1.0, label, COL_TEXT);
        }
        clicked
    }

    /// Returns true when Enter was pressed while focused.
    #[allow(clippy::too_many_arguments)]
    pub fn text_field(
        &mut self,
        r: &Renderer,
        id: u32,
        x: f32,
        y: f32,
        w: f32,
        buf: &mut String,
        numeric: bool,
        placeholder: &str,
    ) -> bool {
        let inside = self.hit(id, x, y, w, FIELD_H);
        if self.take_click(id, inside) {
            self.focus = id;
        }
        let focused = self.focus == id;
        if focused {
            for c in self.chars.drain(..) {
                if c.is_control() {
                    continue;
                }
                if numeric && !c.is_ascii_digit() {
                    continue;
                }
                if buf.len() < 40 {
                    buf.push(c);
                }
            }
            if self.backspace {
                buf.pop();
            }
        }
        let enter = focused && self.enter;
        unsafe {
            r.draw_rect(x, y, w, FIELD_H, COL_FIELD);
            let border = if focused {
                COL_BORDER_FOCUS
            } else {
                COL_BORDER
            };
            r.draw_rect(x, y, w, 1.0, border);
            r.draw_rect(x, y + FIELD_H - 1.0, w, 1.0, border);
            r.draw_rect(x, y, 1.0, FIELD_H, border);
            r.draw_rect(x + w - 1.0, y, 1.0, FIELD_H, border);
            let shown = if buf.is_empty() {
                placeholder.to_string()
            } else {
                buf.clone()
            };
            let col = if buf.is_empty() {
                COL_TEXT_DIM
            } else {
                COL_TEXT
            };
            r.draw_text(x + 7.0, y + FIELD_H / 2.0 + 7.0, 1.0, &shown, col);
            if focused && self.frame % 60 < 35 {
                let tw = r.text_width(1.0, buf);
                r.draw_rect(x + 8.0 + tw, y + 5.0, 2.0, FIELD_H - 10.0, COL_TEXT);
            }
        }
        enter
    }

    pub fn checkbox(
        &mut self,
        r: &Renderer,
        id: u32,
        x: f32,
        y: f32,
        label: &str,
        checked: &mut bool,
    ) {
        let inside = self.hit(id, x, y, 18.0, 18.0);
        if self.take_click(id, inside) {
            *checked = !*checked;
        }
        unsafe {
            r.draw_rect(x, y, 18.0, 18.0, COL_FIELD);
            let border = if self.hot == id {
                COL_BORDER_FOCUS
            } else {
                COL_BORDER
            };
            r.draw_rect(x, y, 18.0, 1.0, border);
            r.draw_rect(x, y + 17.0, 18.0, 1.0, border);
            r.draw_rect(x, y, 1.0, 18.0, border);
            r.draw_rect(x + 17.0, y, 1.0, 18.0, border);
            if *checked {
                r.draw_rect(x + 3.0, y + 3.0, 12.0, 12.0, COL_ACCENT);
            }
            r.draw_text(x + 26.0, y + 13.0, 1.0, label, COL_TEXT);
        }
    }

    /// Returns true when the selection changed. The open popup is *drawn*
    /// later via [`Ui::draw_popups`] so it stays on top of other widgets.
    #[allow(clippy::too_many_arguments)]
    pub fn dropdown(
        &mut self,
        r: &Renderer,
        id: u32,
        x: f32,
        y: f32,
        w: f32,
        options: &'static [&'static str],
        sel: &mut usize,
    ) -> bool {
        let inside = self.hit(id, x, y, w, FIELD_H);
        if self.take_click(id, inside) {
            self.open_popup = if self.open_popup == id { 0 } else { id };
        }
        let open = self.open_popup == id;
        unsafe {
            r.draw_rect(x, y, w, FIELD_H, COL_FIELD);
            let border = if open || self.hot == id {
                COL_BORDER_FOCUS
            } else {
                COL_BORDER
            };
            r.draw_rect(x, y, w, 1.0, border);
            r.draw_rect(x, y + FIELD_H - 1.0, w, 1.0, border);
            r.draw_rect(x, y, 1.0, FIELD_H, border);
            r.draw_rect(x + w - 1.0, y, 1.0, FIELD_H, border);
            let cur = options.get(*sel).copied().unwrap_or("");
            r.draw_text(x + 7.0, y + FIELD_H / 2.0 + 7.0, 1.0, cur, COL_TEXT);
            let ax = x + w - 16.0;
            r.draw_text(
                ax,
                y + FIELD_H / 2.0 + 7.0,
                1.0,
                if open { "^" } else { "v" },
                COL_TEXT_DIM,
            );
        }
        let mut changed = false;
        if open {
            for (i, _opt) in options.iter().enumerate() {
                let oy = y + FIELD_H + i as f32 * FIELD_H;
                let iid = id.wrapping_mul(1000).wrapping_add(i as u32 + 1);
                let inside = self.hit(iid, x, oy, w, FIELD_H);
                if self.take_click(iid, inside) {
                    if *sel != i {
                        *sel = i;
                        changed = true;
                    }
                    self.open_popup = 0;
                }
            }
            // a click that was not consumed by the header or an item closes the popup
            if self.open_popup == id && self.released != 0 {
                self.open_popup = 0;
            }
            if self.open_popup == id {
                self.popup_rect = (x, y + FIELD_H, w, options.len() as f32 * FIELD_H);
                self.popups.push(PopupDraw {
                    x,
                    y,
                    w,
                    options,
                    sel: *sel,
                });
            } else if self.open_popup == 0 {
                self.popup_rect = (0.0, 0.0, 0.0, 0.0);
            }
        }
        changed
    }

    /// Draw all open dropdown popups on top of everything else.
    pub fn draw_popups(&mut self, r: &Renderer) {
        let popups = std::mem::take(&mut self.popups);
        for p in &popups {
            let n = p.options.len() as f32;
            unsafe {
                r.draw_rect(p.x, p.y + FIELD_H, p.w, n * FIELD_H, COL_PANEL);
                r.draw_rect(p.x, p.y + FIELD_H, p.w, 1.0, COL_BORDER);
                r.draw_rect(p.x, p.y + FIELD_H + n * FIELD_H - 1.0, p.w, 1.0, COL_BORDER);
                r.draw_rect(p.x, p.y + FIELD_H, 1.0, n * FIELD_H, COL_BORDER);
                r.draw_rect(p.x + p.w - 1.0, p.y + FIELD_H, 1.0, n * FIELD_H, COL_BORDER);
                for (i, opt) in p.options.iter().enumerate() {
                    let oy = p.y + FIELD_H + i as f32 * FIELD_H;
                    let hov = self.mouse.0 >= p.x
                        && self.mouse.0 < p.x + p.w
                        && self.mouse.1 >= oy
                        && self.mouse.1 < oy + FIELD_H;
                    if hov {
                        r.draw_rect(p.x, oy, p.w, FIELD_H, COL_BTN_HOT);
                    }
                    if i == p.sel {
                        r.draw_rect(p.x, oy, 3.0, FIELD_H, COL_ACCENT);
                    }
                    r.draw_text(p.x + 10.0, oy + FIELD_H / 2.0 + 7.0, 1.0, opt, COL_TEXT);
                }
            }
        }
        self.popups = popups;
    }
}
