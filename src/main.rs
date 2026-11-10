// SPDX-License-Identifier: MIT
//! Tray icon and popup panel showing battery power, health, capacity and time left.

#![windows_subsystem = "windows"]

use std::cell::{Cell, RefCell};
use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::process::CommandExt;
use std::rc::Rc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use batlens_core::types::{Report, Sample};
use ferrokit_core::event::{Event, EventKind};
use ferrokit_core::object::{ObjectData, ObjectId, QObject, register_owned_qobject};
use ferrokit_core::timer::Timer;
use ferrokit_gui::application::GuiApplication;
use ferrokit_gui::geometry::primitives::{Point, PointF, Rect, RectF};
use ferrokit_gui::image::Icon;
use ferrokit_gui::paint::{Painter, Pixmap};
use ferrokit_gui::text::{Font, FontMetrics, FontWeight};
use ferrokit_gui::tiny_skia::Color;
use ferrokit_platform::WindowFlags;
use ferrokit_widgets::FocusPolicy;
use ferrokit_widgets::action::{ActionGroup, ActionRef};
use ferrokit_widgets::application::Application;
use ferrokit_widgets::menu::{Menu, MenuRef};
use ferrokit_widgets::system_tray::{ActivationReason, MessageIcon, SystemTrayIcon};
use ferrokit_widgets::widget::{EmptyWidget, Widget, WidgetRef};
use ferrokit_widgets::window::Window;
use windows_sys::Win32::Foundation::{ERROR_SUCCESS, HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::System::Power::RegisterPowerSettingNotification;
use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_READ, KEY_SET_VALUE, REG_DWORD, REG_OPTION_NON_VOLATILE, REG_SZ,
    RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
};
use windows_sys::Win32::System::SystemServices::GUID_ACDC_POWER_SOURCE;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CallWindowProcW, GWLP_WNDPROC, PBT_APMPOWERSTATUSCHANGE, PBT_APMRESUMEAUTOMATIC,
    PBT_POWERSETTINGCHANGE, PostMessageW, SetForegroundWindow, SetWindowLongPtrW, WM_APP,
    WM_POWERBROADCAST,
};
const POLL_MS: u64 = 30_000;
const LOW_PERCENT: u32 = 20;
/// Icon edge in pixels. Larger than the 16 px base so Windows scales it down at high DPI.
const SIZE: usize = 32;
const PANEL_W: i32 = 236;

const APP_KEY: &str = "Software\\batlens-gui";
const ICON_MODE_VAL: &str = "IconMode";
const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
const RUN_VAL: &str = "batlens-gui";

const WM_APP_WEAR_READY: u32 = WM_APP + 1;
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

static WEAR_TEXT: OnceLock<String> = OnceLock::new();
static ORIG_WNDPROC: AtomicIsize = AtomicIsize::new(0);

thread_local! {
    static EVENT_TIMERS: RefCell<Option<(Timer, Timer)>> = const { RefCell::new(None) };
}
/// 3x5 digit glyphs, one row per entry, bit 2 is the left column.
const DIGITS: [[u8; 5]; 10] = [
    [7, 5, 5, 5, 7],
    [2, 6, 2, 2, 7],
    [7, 1, 7, 4, 7],
    [7, 1, 7, 1, 7],
    [5, 5, 7, 1, 1],
    [7, 4, 7, 1, 7],
    [7, 4, 7, 5, 7],
    [7, 1, 1, 1, 1],
    [7, 5, 7, 5, 7],
    [7, 5, 7, 1, 7],
];

#[derive(Clone, Copy, PartialEq, Eq)]
enum IconMode {
    Charge = 0,
    Health = 1,
}

fn wide(s: &str) -> Vec<u16> {
    OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

fn load_icon_mode() -> IconMode {
    let subkey = wide(APP_KEY);
    let name = wide(ICON_MODE_VAL);
    let mut hkey: HKEY = std::ptr::null_mut();
    // SAFETY: `subkey` is NUL-terminated UTF-16 and `hkey` is a valid out-pointer.
    let st = unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, subkey.as_ptr(), 0, KEY_READ, &mut hkey) };
    if st != ERROR_SUCCESS {
        return IconMode::Charge;
    }
    let mut val = 0u32;
    let mut ty = 0u32;
    let mut len = std::mem::size_of::<u32>() as u32;
    // SAFETY: `hkey` is an open key, `name` is NUL-terminated, and `val`/`len` match `u32`.
    let st = unsafe {
        RegQueryValueExW(
            hkey,
            name.as_ptr(),
            std::ptr::null_mut(),
            &mut ty,
            (&raw mut val).cast(),
            &mut len,
        )
    };
    // SAFETY: `hkey` was opened above and is not used after this close.
    unsafe { RegCloseKey(hkey) };
    if st == ERROR_SUCCESS && ty == REG_DWORD && val == 1 {
        IconMode::Health
    } else {
        IconMode::Charge
    }
}

fn save_icon_mode(mode: IconMode) {
    let subkey = wide(APP_KEY);
    let name = wide(ICON_MODE_VAL);
    let mut hkey: HKEY = std::ptr::null_mut();
    // SAFETY: `subkey` is NUL-terminated UTF-16 and `hkey` is a valid out-pointer.
    let st = unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            subkey.as_ptr(),
            0,
            std::ptr::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE,
            std::ptr::null(),
            &mut hkey,
            std::ptr::null_mut(),
        )
    };
    if st != ERROR_SUCCESS {
        return;
    }
    let val = mode as u32;
    // SAFETY: `hkey` is open with `KEY_SET_VALUE`, `name` is NUL-terminated, and `val` is 4 bytes.
    unsafe {
        RegSetValueExW(
            hkey,
            name.as_ptr(),
            0,
            REG_DWORD,
            (&raw const val).cast(),
            std::mem::size_of::<u32>() as u32,
        );
        RegCloseKey(hkey);
    }
}

fn is_autostart_enabled() -> bool {
    let subkey = wide(RUN_KEY);
    let name = wide(RUN_VAL);
    let mut hkey: HKEY = std::ptr::null_mut();
    // SAFETY: `subkey` is NUL-terminated UTF-16 and `hkey` is a valid out-pointer.
    let st = unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, subkey.as_ptr(), 0, KEY_READ, &mut hkey) };
    if st != ERROR_SUCCESS {
        return false;
    }
    let mut ty = 0u32;
    let mut len = 0u32;
    // SAFETY: `hkey` is open and a null data pointer queries the value type and byte length.
    let st = unsafe {
        RegQueryValueExW(
            hkey,
            name.as_ptr(),
            std::ptr::null_mut(),
            &mut ty,
            std::ptr::null_mut(),
            &mut len,
        )
    };
    // SAFETY: `hkey` was opened above and is not used after this close.
    unsafe { RegCloseKey(hkey) };
    st == ERROR_SUCCESS && ty == REG_SZ && len > 2
}

fn set_autostart(enabled: bool) {
    let subkey = wide(RUN_KEY);
    let name = wide(RUN_VAL);
    let mut hkey: HKEY = std::ptr::null_mut();
    // SAFETY: `subkey` is NUL-terminated UTF-16 and `hkey` is a valid out-pointer.
    let st = unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            subkey.as_ptr(),
            0,
            KEY_SET_VALUE,
            &mut hkey,
        )
    };
    if st != ERROR_SUCCESS {
        return;
    }
    if enabled {
        if let Ok(exe) = std::env::current_exe() {
            let data = wide(&format!("\"{}\"", exe.display()));
            // SAFETY: `hkey` is open with `KEY_SET_VALUE` and `data` is NUL-terminated UTF-16.
            unsafe {
                RegSetValueExW(
                    hkey,
                    name.as_ptr(),
                    0,
                    REG_SZ,
                    data.as_ptr().cast(),
                    (data.len() * 2) as u32,
                );
            }
        }
    } else {
        // SAFETY: `hkey` is open with `KEY_SET_VALUE` and `name` is NUL-terminated UTF-16.
        unsafe {
            RegDeleteValueW(hkey, name.as_ptr());
        }
    }
    // SAFETY: `hkey` was opened above and is not used after this close.
    unsafe { RegCloseKey(hkey) };
}

fn put(data: &mut [u8], x: usize, y: usize, rgb: [u8; 3]) {
    let i = (y * SIZE + x) * 4;
    data[i..i + 4].copy_from_slice(&[rgb[0], rgb[1], rgb[2], 255]);
}

/// 32x32 icon: a coloured square with two digits (100 shows as 99).
fn make_icon(value: Option<u32>, bg: [u8; 3]) -> Icon {
    let mut pm = Pixmap::new(SIZE as u32, SIZE as u32).expect("icon pixmap");
    let data = pm.data_mut();
    data.fill(0);
    for y in 0..SIZE {
        for x in 0..SIZE {
            put(data, x, y, bg);
        }
    }
    if let Some(p) = value {
        let p = p.min(99) as usize;
        for (n, d) in [p / 10, p % 10].into_iter().enumerate() {
            for (row, bits) in DIGITS[d].iter().enumerate() {
                for col in 0..3 {
                    if bits >> (2 - col) & 1 == 1 {
                        // Each glyph pixel is 4x4; glyphs start at x = 2 and x = 18.
                        for (dx, dy) in (0..4).flat_map(|a| (0..4).map(move |b| (a, b))) {
                            put(data, 2 + n * 16 + col * 4 + dx, 6 + row * 4 + dy, [255; 3]);
                        }
                    }
                }
            }
        }
    }
    Icon::from_pixmap(&pm)
}

fn load_wear_report() -> Option<Report> {
    let out = batlens_core::xml::get_report_cache_path();
    let fresh = std::fs::metadata(&out)
        .and_then(|m| m.modified())
        .is_ok_and(|t| t.elapsed().is_ok_and(|d| d.as_secs() < 3600));
    if fresh
        && let Ok(xml) = batlens_core::xml::read_xml_string(&out)
        && let Ok(rep) = batlens_core::xml::parse_report(&xml)
    {
        return Some(rep);
    }
    let tmp = out.with_extension(format!("tmp.{}.xml", std::process::id()));
    let status = std::process::Command::new(batlens_core::xml::get_powercfg_path())
        .args(["/batteryreport", "/xml", "/output"])
        .arg(&tmp)
        .creation_flags(CREATE_NO_WINDOW)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .ok()?;
    if !status.success() {
        let _ = std::fs::remove_file(&tmp);
        let xml = batlens_core::xml::read_xml_string(&out).ok()?;
        return batlens_core::xml::parse_report(&xml).ok();
    }
    let xml = batlens_core::xml::read_xml_string(&tmp).ok();
    if std::fs::rename(&tmp, &out).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    let xml = match xml {
        Some(x) => x,
        None => batlens_core::xml::read_xml_string(&out).ok()?,
    };
    batlens_core::xml::parse_report(&xml).ok()
}

fn compute_wear_text() -> Option<String> {
    let r = load_wear_report()?;
    batlens_core::xml::update_design_capacity_cache_from_report(&r);
    let pts = batlens_core::stats::clean_history(&r);
    if pts.is_empty() {
        return None;
    }
    let (d, f) = (r.design_capacity?, r.full_charge?);
    if d <= 0 || f <= 0 {
        return None;
    }
    let now = 100.0 * f as f64 / d as f64;
    let (t_base, h_base) = match batlens_core::stats::find_battery_replacement(&pts) {
        Some(idx) => (pts[idx].0, pts[idx].1),
        None => (pts[0].0, pts[0].1),
    };
    let now_secs = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs() as i64;
    let days_span = (now_secs - t_base.timestamp()) as f64 / 86400.0;
    let wear = batlens_core::stats::compute_wear(now, h_base, days_span, 80.0);
    if wear.rate_str == "N/A" {
        None
    } else if wear.is_decay {
        Some(format!("-{}", wear.rate_str.replace(" %/month", "% / mo")))
    } else {
        Some(wear.rate_str.replace(" %/month", "% / mo"))
    }
}

unsafe extern "system" fn power_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_POWERBROADCAST {
        let wp = wparam as u32;
        if wp == PBT_APMPOWERSTATUSCHANGE
            || wp == PBT_POWERSETTINGCHANGE
            || wp == PBT_APMRESUMEAUTOMATIC
        {
            EVENT_TIMERS.with(|cell| {
                if let Some((fast, settle)) = cell.borrow().as_ref() {
                    fast.start_with_interval(0);
                    settle.start_with_interval(800);
                }
            });
        }
    } else if msg == WM_APP_WEAR_READY {
        EVENT_TIMERS.with(|cell| {
            if let Some((fast, _)) = cell.borrow().as_ref() {
                fast.start_with_interval(0);
            }
        });
        return 0;
    }
    let orig = ORIG_WNDPROC.load(Ordering::Relaxed);
    if orig != 0 {
        // SAFETY: `orig` is the previous window procedure returned by `SetWindowLongPtrW`.
        let proc: unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT =
            unsafe { std::mem::transmute(orig) };
        // SAFETY: Forwarding message parameters to the original window procedure.
        unsafe { CallWindowProcW(Some(proc), hwnd, msg, wparam, lparam) }
    } else {
        0
    }
}

fn fmt_eta(h: f64) -> String {
    if h <= 36.0 {
        let m = (h * 60.0).round() as u64;
        format!("{}h {:02}m", m / 60, m % 60)
    } else {
        "> 36 h".to_string()
    }
}

#[derive(Clone, Default)]
struct DisplayData {
    percent: Option<u32>,
    health_pct: Option<u32>,
    ac: bool,
    pct_text: String,
    state_text: &'static str,
    power: String,
    health: String,
    wear: Option<String>,
    time_left: String,
    capacity: Option<String>,
}

impl DisplayData {
    fn from_sample(s: &Sample) -> Self {
        let pct_text = s.percent.map_or("N/A".to_string(), |p| format!("{p}%"));
        let state_text = if s.ac {
            if s.rate_mw > 0 { "Charging" } else { "On AC" }
        } else {
            "On battery"
        };
        let power = format!("{:+.1} W", s.rate_mw as f64 / 1000.0);
        let health_pct = (s.design_mwh > 0 && s.full_mwh > 0)
            .then(|| (s.full_mwh * 100 / s.design_mwh).max(0) as u32);
        let health = health_pct.map_or("N/A".to_string(), |h| format!("{h}%"));
        let time_left = if s.dis_w >= 0.1 && s.percent.is_some() {
            fmt_eta(s.remaining_mwh as f64 / 1000.0 / s.dis_w)
        } else if s.ac
            && s.rate_mw >= 100
            && s.full_mwh > s.remaining_mwh
            && let Some(p) = s.percent
        {
            let target_80 = s.full_mwh * 80 / 100;
            let (target_mwh, tag) = if p < 80 && s.remaining_mwh < target_80 {
                (target_80, "80%")
            } else {
                (s.full_mwh, "100%")
            };
            let h = (target_mwh - s.remaining_mwh) as f64 / s.rate_mw as f64;
            format!("{} ({tag})", fmt_eta(h))
        } else {
            "N/A".to_string()
        };
        let capacity = if s.full_mwh > 0 && s.design_mwh > 0 {
            Some(format!(
                "{:.1} / {:.1} Wh",
                s.full_mwh as f64 / 1000.0,
                s.design_mwh as f64 / 1000.0
            ))
        } else if s.full_mwh > 0 {
            Some(format!("{:.1} Wh", s.full_mwh as f64 / 1000.0))
        } else {
            None
        };
        Self {
            percent: s.percent,
            health_pct,
            ac: s.ac,
            pct_text,
            state_text,
            power,
            health,
            wear: WEAR_TEXT.get().cloned(),
            time_left,
            capacity,
        }
    }

    fn tooltip(&self) -> String {
        match &self.wear {
            Some(w) => format!(
                "{} ({})\nPower: {}\nHealth: {} ({})\nTime left: {}",
                self.pct_text, self.state_text, self.power, self.health, w, self.time_left
            ),
            None => format!(
                "{} ({})\nPower: {}\nHealth: {}\nTime left: {}",
                self.pct_text, self.state_text, self.power, self.health, self.time_left
            ),
        }
    }

    fn panel_height(&self) -> i32 {
        let rows = 3 + usize::from(self.wear.is_some()) + usize::from(self.capacity.is_some());
        86 + rows as i32 * 26
    }
}

fn paint_panel(painter: &mut Painter, data: &DisplayData) {
    let w = PANEL_W as f32;
    let h = data.panel_height() as f32;
    let border = Color::from_rgba8(62, 62, 66, 255);
    let bg = Color::from_rgba8(32, 32, 32, 255);
    painter.fill_rect(RectF::new(0.0, 0.0, w, h), border);
    painter.fill_rect(RectF::new(1.0, 1.0, w - 2.0, h - 2.0), bg);

    let pad = 16.0;
    let mut big_font = Font::new("Segoe UI", 32.0);
    big_font.set_weight(FontWeight::Bold);
    let big_metrics = FontMetrics::from_font(&big_font);

    let mut state_font = Font::new("Segoe UI", 13.0);
    state_font.set_weight(FontWeight::SemiBold);

    let label_font = Font::new("Segoe UI", 13.0);
    let mut val_font = Font::new("Segoe UI", 13.0);
    val_font.set_weight(FontWeight::SemiBold);
    let val_metrics = FontMetrics::from_font(&val_font);

    let white = Color::from_rgba8(244, 244, 246, 255);
    let dim = Color::from_rgba8(154, 154, 154, 255);
    let val_color = Color::from_rgba8(240, 240, 240, 255);
    let low = !data.ac && data.percent.is_some_and(|p| p <= LOW_PERCENT);
    let accent = if data.ac {
        Color::from_rgba8(74, 222, 128, 255)
    } else if low {
        Color::from_rgba8(251, 146, 60, 255)
    } else {
        Color::from_rgba8(180, 188, 198, 255)
    };

    let header_y = 46.0;
    painter.draw_text_colored(PointF::new(pad, header_y), &data.pct_text, &big_font, white);
    let pct_w = big_metrics.horizontal_advance_exact(&data.pct_text, &big_font);
    painter.draw_text_colored(
        PointF::new((pad + pct_w + 10.0).round(), header_y),
        data.state_text,
        &state_font,
        accent,
    );

    let bar_w = w - pad * 2.0;
    let bar_y = 58.0;
    let bar_h = 6.0;
    painter.fill_rect(
        RectF::new(pad, bar_y, bar_w, bar_h),
        Color::from_rgba8(56, 56, 60, 255),
    );
    if let Some(p) = data.percent {
        let fill_w = (bar_w * (p.min(100) as f32 / 100.0)).round();
        if fill_w > 0.0 {
            let bar_color = if data.ac {
                Color::from_rgba8(52, 211, 153, 255)
            } else if low {
                Color::from_rgba8(239, 68, 68, 255)
            } else {
                Color::from_rgba8(56, 189, 248, 255)
            };
            painter.fill_rect(RectF::new(pad, bar_y, fill_w, bar_h), bar_color);
        }
    }

    let mut rows: Vec<(&str, &str)> = vec![("Power", &data.power), ("Health", &data.health)];
    if let Some(wear) = &data.wear {
        rows.push(("Wear", wear));
    }
    rows.push(("Time left", &data.time_left));
    if let Some(cap) = &data.capacity {
        rows.push(("Capacity", cap));
    }
    for (i, (label, value)) in rows.into_iter().enumerate() {
        let y = 92.0 + i as f32 * 26.0;
        painter.draw_text_colored(PointF::new(pad, y), label, &label_font, dim);
        let vw = val_metrics.horizontal_advance_exact(value, &val_font);
        let vx = (w - pad - vw).round().max(pad + 72.0);
        painter.draw_text_colored(PointF::new(vx, y), value, &val_font, val_color);
    }
}

struct EscFilter {
    data: ObjectData,
    hide_timer: Timer,
}

impl QObject for EscFilter {
    fn object_data(&self) -> &ObjectData {
        &self.data
    }

    fn object_data_mut(&mut self) -> ferrokit_core::object::ObjectDataMut<'_> {
        ferrokit_core::object::ObjectDataMut::new(&mut self.data)
    }

    fn event(&mut self, _event: &mut Event) -> bool {
        false
    }

    fn event_filter(&mut self, _watched: ObjectId, event: &mut Event) -> bool {
        if let EventKind::KeyPress { key, .. } = event.kind
            && (key == 0x1B || key == 0x0100_0000)
        {
            self.hide_timer.start_with_interval(0);
            return true;
        }
        false
    }
}

struct Tray {
    icon: SystemTrayIcon,
    panel: Rc<RefCell<Window>>,
    data: Rc<RefCell<DisplayData>>,
    last_deactivate: Rc<Cell<Option<Instant>>>,
    icon_mode: IconMode,
    warned: bool,
}

impl Tray {
    fn update_icon(&mut self, d: &DisplayData) {
        let (val, bg) = match self.icon_mode {
            IconMode::Charge => {
                let bg = match d.percent {
                    None => [90, 90, 90],
                    Some(_) if d.ac => [30, 140, 60],
                    Some(p) if p <= LOW_PERCENT => [190, 40, 40],
                    Some(_) => [60, 60, 60],
                };
                (d.percent, bg)
            }
            IconMode::Health => {
                let bg = if d.health_pct.is_some() {
                    [55, 85, 115]
                } else {
                    [90, 90, 90]
                };
                (d.health_pct, bg)
            }
        };
        let _ = self.icon.set_icon(&make_icon(val, bg));
    }

    fn refresh(&mut self) {
        let Ok(s) = batlens_core::sys::sample() else {
            return;
        };
        let d = DisplayData::from_sample(&s);
        self.update_icon(&d);
        let _ = self.icon.set_tooltip(d.tooltip());

        let low = !d.ac && d.percent.is_some_and(|p| p <= LOW_PERCENT);
        if low && !self.warned {
            let _ = self.icon.show_message(
                "Battery low",
                &format!("{} ({})", d.pct_text, d.state_text),
                MessageIcon::Warning,
                5000,
            );
        }
        self.warned = low;

        let h = d.panel_height();
        *self.data.borrow_mut() = d;
        if let Ok(mut win) = self.panel.try_borrow_mut()
            && win.is_visible()
        {
            let rect = self.panel_rect(h);
            win.set_geometry(rect);
            win.render_and_present();
        }
    }

    fn set_icon_mode(&mut self, mode: IconMode) {
        if self.icon_mode == mode {
            return;
        }
        self.icon_mode = mode;
        save_icon_mode(mode);
        let d = self.data.borrow().clone();
        self.update_icon(&d);
    }

    fn panel_rect(&self, panel_h: i32) -> Rect {
        let icon_rect = self.icon.geometry();
        let platform = ferrokit_platform::platform();
        let center = Point::new(
            icon_rect.x + icon_rect.width / 2,
            icon_rect.y + icon_rect.height / 2,
        );
        let work = platform
            .screens()
            .into_iter()
            .find(|s| s.geometry().contains(center))
            .unwrap_or_else(|| platform.primary_screen())
            .available_geometry();
        const MARGIN: i32 = 8;
        const GAP: i32 = 6;
        let (mut x, mut y) = if icon_rect.width > 0 && icon_rect.height > 0 {
            let x = icon_rect.x + (icon_rect.width - PANEL_W) / 2;
            let y = if icon_rect.y - panel_h - GAP >= work.y {
                icon_rect.y - panel_h - GAP
            } else {
                icon_rect.bottom() + GAP
            };
            (x, y)
        } else {
            (
                work.right() - PANEL_W - MARGIN,
                work.bottom() - panel_h - MARGIN,
            )
        };
        x = x.clamp(
            work.x + MARGIN,
            (work.right() - PANEL_W - MARGIN).max(work.x + MARGIN),
        );
        y = y.clamp(
            work.y + MARGIN,
            (work.bottom() - panel_h - MARGIN).max(work.y + MARGIN),
        );
        Rect::new(x, y, PANEL_W, panel_h)
    }

    fn toggle_panel(&mut self) {
        let visible = self.panel.borrow().is_visible();
        if visible {
            self.last_deactivate.set(None);
            self.panel.borrow_mut().hide();
            return;
        }
        if self
            .last_deactivate
            .take()
            .is_some_and(|t| t.elapsed() < Duration::from_millis(250))
        {
            return;
        }
        self.refresh();
        let h = self.data.borrow().panel_height();
        let rect = self.panel_rect(h);
        let mut win = self.panel.borrow_mut();
        win.set_geometry(rect);
        win.show();
        let hwnd = win.native_handle() as HWND;
        // SAFETY: `hwnd` is the live window handle owned by this thread.
        unsafe {
            SetForegroundWindow(hwnd);
        }
        win.activate_window();
    }
}

fn main() {
    ferrokit_platform::set_dpi_awareness();
    let mut app = Application::new(vec!["batlens-gui".to_string()]);
    GuiApplication::set_quit_on_last_window_closed(false);
    if !SystemTrayIcon::is_system_tray_available() {
        return;
    }

    let data = Rc::new(RefCell::new(DisplayData::default()));
    let last_deactivate = Rc::new(Cell::new(None));

    let init_rect = Rect::new(0, 0, PANEL_W, 190);
    let Ok(mut window) = Window::new(
        "batlens",
        init_rect,
        WindowFlags::FRAMELESS
            | WindowFlags::TOOL
            | WindowFlags::STAYS_ON_TOP
            | WindowFlags::LAYERED,
    ) else {
        return;
    };

    let mut root_widget = EmptyWidget::with_geometry(init_rect);
    root_widget
        .widget_base()
        .set_focus_policy(FocusPolicy::StrongFocus);
    let paint_data = Rc::clone(&data);
    root_widget.set_paint_handler(move |painter| {
        paint_panel(painter, &paint_data.borrow());
    });
    let root: WidgetRef = Rc::new(RefCell::new(Box::new(root_widget)));
    window.set_root_widget(Rc::clone(&root));

    let panel = Rc::new(RefCell::new(window));
    let deact_panel = Rc::clone(&panel);
    let deact_time = Rc::clone(&last_deactivate);
    panel
        .borrow_mut()
        .set_window_event_handler(move |ev: &mut Event| {
            if matches!(ev.kind, EventKind::WindowDeactivate) {
                deact_time.set(Some(Instant::now()));
                if let Ok(mut w) = deact_panel.try_borrow_mut()
                    && w.is_visible()
                {
                    w.hide();
                }
            }
        });

    let hide_timer = Timer::new();
    hide_timer.set_single_shot(true);
    let esc_panel = Rc::clone(&panel);
    let esc_time = Rc::clone(&last_deactivate);
    hide_timer.timeout.connect_local(move |_| {
        esc_time.set(None);
        if let Ok(mut w) = esc_panel.try_borrow_mut()
            && w.is_visible()
        {
            w.hide();
        }
    });
    let filter = EscFilter {
        data: ObjectData::with_auto_id(),
        hide_timer,
    };
    // SAFETY: `_esc_owner` lives on the main thread for the entire duration of `main`.
    let _esc_owner = unsafe { register_owned_qobject(filter) };
    root.borrow_mut()
        .object_data_mut()
        .install_event_filter(ferrokit_core::object::OwnedObject::object_id(&_esc_owner));

    let initial_mode = load_icon_mode();
    let menu: MenuRef = Menu::new_ref("batlens");
    let icon_menu = menu.borrow_mut().add_menu("Icon shows");
    let charge_act: ActionRef = icon_menu.borrow_mut().add_new_action("Charge");
    let health_act: ActionRef = icon_menu.borrow_mut().add_new_action("Health");
    charge_act.borrow_mut().set_checkable(true);
    health_act.borrow_mut().set_checkable(true);
    let mut mode_group = ActionGroup::new();
    mode_group.add_action(&charge_act);
    mode_group.add_action(&health_act);
    charge_act
        .borrow_mut()
        .set_checked(initial_mode == IconMode::Charge);
    health_act
        .borrow_mut()
        .set_checked(initial_mode == IconMode::Health);
    let autostart_act = menu.borrow_mut().add_new_action("Start with Windows");
    autostart_act.borrow_mut().set_checkable(true);
    autostart_act
        .borrow_mut()
        .set_checked(is_autostart_enabled());

    menu.borrow_mut().add_separator();
    let quit = menu.borrow_mut().add_new_action("Quit");
    quit.borrow().triggered.connect(|_| Application::quit());

    let Ok(mut icon) = SystemTrayIcon::from_icon(&make_icon(None, [90, 90, 90])) else {
        return;
    };
    icon.set_context_menu(Rc::clone(&menu));
    let _ = icon.show();
    let tray = Rc::new(RefCell::new(Tray {
        icon,
        panel,
        data,
        last_deactivate,
        icon_mode: initial_mode,
        warned: false,
    }));
    tray.borrow_mut().refresh();

    let t_act = Rc::clone(&tray);
    tray.borrow()
        .icon
        .activated
        .connect_local(move |reason: &ActivationReason| {
            if matches!(reason, ActivationReason::Trigger) {
                t_act.borrow_mut().toggle_panel();
            }
        });

    let t_charge = Rc::clone(&tray);
    charge_act.borrow().triggered.connect_local(move |_| {
        t_charge.borrow_mut().set_icon_mode(IconMode::Charge);
    });
    let t_health = Rc::clone(&tray);
    health_act.borrow().triggered.connect_local(move |_| {
        t_health.borrow_mut().set_icon_mode(IconMode::Health);
    });
    autostart_act
        .borrow()
        .triggered
        .connect_local(|checked: &bool| {
            set_autostart(*checked);
        });
    let auto_sync = Rc::clone(&autostart_act);
    let charge_sync = Rc::clone(&charge_act);
    let health_sync = Rc::clone(&health_act);
    let t_sync = Rc::clone(&tray);
    menu.borrow().about_to_show.connect_local(move |_| {
        auto_sync.borrow_mut().set_checked(is_autostart_enabled());
        let mode = t_sync.borrow().icon_mode;
        charge_sync
            .borrow_mut()
            .set_checked(mode == IconMode::Charge);
        health_sync
            .borrow_mut()
            .set_checked(mode == IconMode::Health);
    });

    let fast_timer = Timer::new();
    fast_timer.set_single_shot(true);
    let t_fast = Rc::clone(&tray);
    fast_timer
        .timeout
        .connect_local(move |_| t_fast.borrow_mut().refresh());

    let settle_timer = Timer::new();
    settle_timer.set_single_shot(true);
    let t_settle = Rc::clone(&tray);
    settle_timer
        .timeout
        .connect_local(move |_| t_settle.borrow_mut().refresh());

    EVENT_TIMERS.with(|cell| {
        *cell.borrow_mut() = Some((fast_timer, settle_timer));
    });

    let panel_hwnd = tray.borrow().panel.borrow().native_handle() as HWND;
    // SAFETY: `panel_hwnd` is a valid main-thread window handle; subclassing intercepts
    // `WM_POWERBROADCAST` and `WM_APP_WEAR_READY` and forwards everything else.
    let _power_notify = unsafe {
        let prev = SetWindowLongPtrW(
            panel_hwnd,
            GWLP_WNDPROC,
            power_wnd_proc as *const () as usize as isize,
        );
        ORIG_WNDPROC.store(prev, Ordering::Relaxed);
        RegisterPowerSettingNotification(panel_hwnd, &GUID_ACDC_POWER_SOURCE, 0)
    };

    let wear_hwnd = panel_hwnd as isize;
    std::thread::spawn(move || {
        if let Some(w) = compute_wear_text() {
            let _ = WEAR_TEXT.set(w);
            // SAFETY: `PostMessageW` is thread-safe and posts a notification message to `panel_hwnd`.
            unsafe {
                PostMessageW(wear_hwnd as HWND, WM_APP_WEAR_READY, 0, 0);
            }
        }
    });

    let timer = Timer::new();
    timer.set_interval(POLL_MS);
    let t_poll = Rc::clone(&tray);
    timer
        .timeout
        .connect_local(move |_| t_poll.borrow_mut().refresh());
    timer.start();

    let exit_code = app.exec();
    drop(mode_group);
    std::process::exit(exit_code);
}
