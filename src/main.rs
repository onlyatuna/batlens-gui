// SPDX-License-Identifier: MIT
//! Tray icon and popup panel showing battery power, health, wear rate, capacity and time left.

#![windows_subsystem = "windows"]

use std::cell::RefCell;
use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::process::CommandExt;
use std::ptr;
use std::sync::mpsc::{self, SyncSender};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use batlens_core::types::{Report, Sample};
use windows_sys::Win32::Foundation::{
    ERROR_SUCCESS, HWND, LPARAM, LRESULT, POINT, RECT, SIZE as WIN_SIZE, WPARAM,
};
use windows_sys::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BeginPaint, BitBlt, CLEARTYPE_QUALITY, CreateBitmap,
    CreateCompatibleBitmap, CreateCompatibleDC, CreateDIBSection, CreateFontW, CreateSolidBrush,
    DIB_RGB_COLORS, DT_LEFT, DT_NOCLIP, DT_RIGHT, DT_SINGLELINE, DeleteDC, DeleteObject, DrawTextW,
    EndPaint, FW_BOLD, FW_NORMAL, FW_SEMIBOLD, FillRect, GetDC, GetMonitorInfoW,
    GetTextExtentPoint32W, HDC, HFONT, InvalidateRect, MONITOR_DEFAULTTONEAREST, MONITORINFO,
    MonitorFromPoint, PAINTSTRUCT, ReleaseDC, SRCCOPY, SelectObject, SetBkMode, SetTextColor,
    TRANSPARENT,
};
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress, LoadLibraryW};
use windows_sys::Win32::System::Power::RegisterPowerSettingNotification;
use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_READ, KEY_SET_VALUE, REG_DWORD, REG_OPTION_NON_VOLATILE, REG_SZ,
    RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
};
use windows_sys::Win32::System::SystemServices::GUID_ACDC_POWER_SOURCE;
use windows_sys::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForWindow, SetProcessDpiAwarenessContext,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::VK_ESCAPE;
use windows_sys::Win32::UI::Shell::{
    NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIIF_WARNING, NIM_ADD, NIM_DELETE,
    NIM_MODIFY, NIM_SETVERSION, NIN_SELECT, NOTIFYICON_VERSION_4, NOTIFYICONDATAW,
    NOTIFYICONIDENTIFIER, Shell_NotifyIconGetRect, Shell_NotifyIconW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CS_HREDRAW, CS_VREDRAW, CheckMenuRadioItem, CreateIconIndirect, CreatePopupMenu,
    CreateWindowExW, DefWindowProcW, DestroyIcon, DestroyMenu, DispatchMessageW, GetClientRect,
    GetCursorPos, GetMessageW, HICON, ICONINFO, IDC_ARROW, IsWindowVisible, KillTimer, LoadCursorW,
    MF_BYCOMMAND, MF_CHECKED, MF_POPUP, MF_SEPARATOR, MF_STRING, MF_UNCHECKED, MSG,
    PBT_APMPOWERSTATUSCHANGE, PBT_APMRESUMEAUTOMATIC, PBT_POWERSETTINGCHANGE, PM_NOREMOVE,
    PeekMessageW, PostMessageW, PostQuitMessage, RegisterClassExW, RegisterWindowMessageW, SW_HIDE,
    SW_SHOW, SWP_NOACTIVATE, SWP_NOZORDER, SetCursor, SetForegroundWindow, SetTimer, SetWindowPos,
    ShowWindow, TPM_BOTTOMALIGN, TPM_LEFTALIGN, TPM_NONOTIFY, TPM_RETURNCMD, TPM_RIGHTBUTTON,
    TrackPopupMenuEx, TranslateMessage, WA_INACTIVE, WM_ACTIVATE, WM_APP, WM_CLOSE, WM_CONTEXTMENU,
    WM_DESTROY, WM_DPICHANGED, WM_ERASEBKGND, WM_KEYDOWN, WM_NULL, WM_PAINT, WM_POWERBROADCAST,
    WM_SETCURSOR, WM_TIMER, WNDCLASSEXW, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};

const POLL_MS: u32 = 30_000;
const SETTLE_MS: u32 = 800;
const LOW_PERCENT: u32 = 20;
const ICON_PX: usize = 32;
const PANEL_W: i32 = 236;
const TRAY_UID: u32 = 1;

const TIMER_SETTLE: usize = 2;

const WM_TRAY_CALLBACK: u32 = WM_APP + 1;
const WM_APP_DATA_READY: u32 = WM_APP + 2;
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const NIN_KEYSELECT: u32 = NIN_SELECT | 1;

const CMD_ICON_CHARGE: u32 = 1001;
const CMD_ICON_HEALTH: u32 = 1002;
const CMD_AUTOSTART: u32 = 1003;
const CMD_QUIT: u32 = 1004;

const APP_KEY: &str = "Software\\batlens-gui";
const ICON_MODE_VAL: &str = "IconMode";
const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
const RUN_VAL: &str = "batlens-gui";

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

static WEAR_TEXT: OnceLock<String> = OnceLock::new();
static LATEST_DATA: Mutex<Option<DisplayData>> = Mutex::new(None);
static POLL_WAKE: OnceLock<SyncSender<()>> = OnceLock::new();

fn wake_worker() {
    if let Some(tx) = POLL_WAKE.get() {
        let _ = tx.try_send(());
    }
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum IconMode {
    Charge = 0,
    Health = 1,
}

const fn rgb(r: u8, g: u8, b: u8) -> u32 {
    (r as u32) | ((g as u32) << 8) | ((b as u32) << 16)
}

fn wide(s: &str) -> Vec<u16> {
    OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

fn copy_wide_trunc(dst: &mut [u16], s: &str) {
    dst.fill(0);
    let max = dst.len().saturating_sub(1);
    for (i, u) in OsStr::new(s).encode_wide().take(max).enumerate() {
        dst[i] = u;
    }
}

fn enable_dark_menus() {
    let lib_name = wide("uxtheme.dll");
    // SAFETY: Loading system `uxtheme.dll` and resolving undocumented dark-mode ordinals 135/136.
    unsafe {
        let hmod = LoadLibraryW(lib_name.as_ptr());
        if hmod.is_null() {
            return;
        }
        if let Some(p) = GetProcAddress(hmod, 135usize as *const u8) {
            let set_mode: unsafe extern "system" fn(i32) -> i32 = std::mem::transmute(p);
            set_mode(2); // ForceDark
        }
        if let Some(p) = GetProcAddress(hmod, 136usize as *const u8) {
            let flush: unsafe extern "system" fn() = std::mem::transmute(p);
            flush();
        }
    }
}

fn load_icon_mode() -> IconMode {
    let subkey = wide(APP_KEY);
    let name = wide(ICON_MODE_VAL);
    let mut hkey: HKEY = ptr::null_mut();
    // SAFETY: `subkey` is NUL-terminated UTF-16 and `hkey` is a valid out-pointer.
    let st = unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, subkey.as_ptr(), 0, KEY_READ, &mut hkey) };
    if st != ERROR_SUCCESS {
        return IconMode::Charge;
    }
    let mut val = 0u32;
    let mut ty = 0u32;
    let mut len = std::mem::size_of::<u32>() as u32;
    // SAFETY: `hkey` is open, `name` is NUL-terminated, and `val`/`len` match `u32`.
    let st = unsafe {
        RegQueryValueExW(
            hkey,
            name.as_ptr(),
            ptr::null_mut(),
            &mut ty,
            (&raw mut val).cast(),
            &mut len,
        )
    };
    // SAFETY: `hkey` was opened above and is not used after closing.
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
    let mut hkey: HKEY = ptr::null_mut();
    // SAFETY: `subkey` is NUL-terminated UTF-16 and `hkey` is a valid out-pointer.
    let st = unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            subkey.as_ptr(),
            0,
            ptr::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE,
            ptr::null(),
            &mut hkey,
            ptr::null_mut(),
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
    let mut hkey: HKEY = ptr::null_mut();
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
            ptr::null_mut(),
            &mut ty,
            ptr::null_mut(),
            &mut len,
        )
    };
    // SAFETY: `hkey` was opened above and is not used after closing.
    unsafe { RegCloseKey(hkey) };
    st == ERROR_SUCCESS && ty == REG_SZ && len > 2
}

fn set_autostart(enabled: bool) {
    let subkey = wide(RUN_KEY);
    let name = wide(RUN_VAL);
    let mut hkey: HKEY = ptr::null_mut();
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
    // SAFETY: `hkey` was opened above and is not used after closing.
    unsafe { RegCloseKey(hkey) };
}

fn put_bgra(data: &mut [u8], x: usize, y: usize, rgb_val: [u8; 3]) {
    let i = (y * ICON_PX + x) * 4;
    data[i..i + 4].copy_from_slice(&[rgb_val[2], rgb_val[1], rgb_val[0], 255]);
}

/// Creates a 32x32 `HICON` with a coloured square and two 3x5 digits (100 shows as 99).
fn make_hicon(value: Option<u32>, bg: [u8; 3]) -> HICON {
    let mut bgra = [0u8; ICON_PX * ICON_PX * 4];
    for y in 0..ICON_PX {
        for x in 0..ICON_PX {
            put_bgra(&mut bgra, x, y, bg);
        }
    }
    if let Some(p) = value {
        let p = p.min(99) as usize;
        for (n, d) in [p / 10, p % 10].into_iter().enumerate() {
            for (row, bits) in DIGITS[d].iter().enumerate() {
                for col in 0..3 {
                    if bits >> (2 - col) & 1 == 1 {
                        for (dx, dy) in (0..4).flat_map(|a| (0..4).map(move |b| (a, b))) {
                            put_bgra(
                                &mut bgra,
                                2 + n * 16 + col * 4 + dx,
                                6 + row * 4 + dy,
                                [255; 3],
                            );
                        }
                    }
                }
            }
        }
    }

    let bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: ICON_PX as i32,
            biHeight: -(ICON_PX as i32), // top-down
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut bits: *mut core::ffi::c_void = ptr::null_mut();
    // SAFETY: Creating a 32x32 32-bit DIB section, copying `bgra` into `bits`, creating a 1-bit mask
    // bitmap, calling `CreateIconIndirect`, and deleting both temporary bitmaps before returning.
    unsafe {
        let hbm_color = CreateDIBSection(
            ptr::null_mut(),
            &bmi,
            DIB_RGB_COLORS,
            &mut bits,
            ptr::null_mut(),
            0,
        );
        if hbm_color.is_null() || bits.is_null() {
            if !hbm_color.is_null() {
                DeleteObject(hbm_color);
            }
            return ptr::null_mut();
        }
        ptr::copy_nonoverlapping(bgra.as_ptr(), bits.cast::<u8>(), bgra.len());
        let mask_bytes = [0u8; (ICON_PX / 8) * ICON_PX];
        let hbm_mask = CreateBitmap(
            ICON_PX as i32,
            ICON_PX as i32,
            1,
            1,
            mask_bytes.as_ptr().cast(),
        );
        let ii = ICONINFO {
            fIcon: 1,
            xHotspot: 0,
            yHotspot: 0,
            hbmMask: hbm_mask,
            hbmColor: hbm_color,
        };
        let hicon = CreateIconIndirect(&ii);
        if !hbm_mask.is_null() {
            DeleteObject(hbm_mask);
        }
        DeleteObject(hbm_color);
        hicon
    }
}

fn wear_from_report(r: &Report) -> Option<String> {
    batlens_core::xml::update_design_capacity_cache_from_report(r);
    let pts = batlens_core::stats::clean_history(r);
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

fn try_cached_wear_text() -> Option<String> {
    let out = batlens_core::xml::get_report_cache_path();
    let fresh = std::fs::metadata(&out)
        .and_then(|m| m.modified())
        .is_ok_and(|t| t.elapsed().is_ok_and(|d| d.as_secs() < 3600));
    if !fresh {
        return None;
    }
    let xml = batlens_core::xml::read_xml_string(&out).ok()?;
    let rep = batlens_core::xml::parse_report(&xml).ok()?;
    wear_from_report(&rep)
}

fn generate_wear_text() -> Option<String> {
    let out = batlens_core::xml::get_report_cache_path();
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
        let rep = batlens_core::xml::parse_report(&xml).ok()?;
        return wear_from_report(&rep);
    }
    let xml = batlens_core::xml::read_xml_string(&tmp).ok();
    if std::fs::rename(&tmp, &out).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    let xml = match xml {
        Some(x) => x,
        None => batlens_core::xml::read_xml_string(&out).ok()?,
    };
    let rep = batlens_core::xml::parse_report(&xml).ok()?;
    wear_from_report(&rep)
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

fn fill_solid(hdc: HDC, r: RECT, color: u32) {
    // SAFETY: `hdc` is a valid device context; brush is created, used for `FillRect`, and deleted.
    unsafe {
        let brush = CreateSolidBrush(color);
        FillRect(hdc, &r, brush);
        DeleteObject(brush);
    }
}

fn make_font(px_height: i32, weight: i32) -> HFONT {
    let face = wide("Segoe UI");
    // SAFETY: `face` is NUL-terminated UTF-16; returns a GDI font handle owned by caller.
    unsafe {
        CreateFontW(
            -px_height,
            0,
            0,
            0,
            weight,
            0,
            0,
            0,
            1, // DEFAULT_CHARSET
            0,
            0,
            CLEARTYPE_QUALITY as u32,
            0,
            face.as_ptr(),
        )
    }
}

fn draw_text(hdc: HDC, text: &str, mut rect: RECT, flags: u32, color: u32) {
    let w: Vec<u16> = OsStr::new(text).encode_wide().collect();
    // SAFETY: `hdc` is valid and `w` is valid for `w.len()` UTF-16 code units.
    unsafe {
        SetTextColor(hdc, color);
        DrawTextW(hdc, w.as_ptr(), w.len() as i32, &mut rect, flags);
    }
}

fn text_size(hdc: HDC, text: &str) -> WIN_SIZE {
    let w: Vec<u16> = OsStr::new(text).encode_wide().collect();
    let mut sz = WIN_SIZE { cx: 0, cy: 0 };
    // SAFETY: `hdc` is valid and `w` holds `w.len()` UTF-16 code units.
    unsafe {
        GetTextExtentPoint32W(hdc, w.as_ptr(), w.len() as i32, &mut sz);
    }
    sz
}

fn paint_panel_gdi(hdc: HDC, width: i32, height: i32, scale: f32, data: &DisplayData) {
    let s = |v: f32| (v * scale).round() as i32;
    let b = s(1.0).max(1);
    fill_solid(
        hdc,
        RECT {
            left: 0,
            top: 0,
            right: width,
            bottom: height,
        },
        rgb(62, 62, 66),
    );
    fill_solid(
        hdc,
        RECT {
            left: b,
            top: b,
            right: width - b,
            bottom: height - b,
        },
        rgb(32, 32, 32),
    );

    let pad = s(16.0);
    let big_font = make_font(s(32.0), FW_BOLD as i32);
    let semi_font = make_font(s(13.0), FW_SEMIBOLD as i32);
    let reg_font = make_font(s(13.0), FW_NORMAL as i32);

    let white = rgb(244, 244, 246);
    let dim = rgb(154, 154, 154);
    let val_color = rgb(240, 240, 240);
    let low = !data.ac && data.percent.is_some_and(|p| p <= LOW_PERCENT);
    let accent = if data.ac {
        rgb(74, 222, 128)
    } else if low {
        rgb(251, 146, 60)
    } else {
        rgb(180, 188, 198)
    };

    // SAFETY: Selecting created fonts into `hdc`, drawing text, and restoring/deleting all GDI fonts.
    unsafe {
        SetBkMode(hdc, TRANSPARENT as i32);

        let old = SelectObject(hdc, big_font);
        let pct_sz = text_size(hdc, &data.pct_text);
        let top_y = s(10.0);
        draw_text(
            hdc,
            &data.pct_text,
            RECT {
                left: pad,
                top: top_y,
                right: width - pad,
                bottom: top_y + pct_sz.cy,
            },
            DT_LEFT | DT_SINGLELINE | DT_NOCLIP,
            white,
        );

        SelectObject(hdc, semi_font);
        let state_sz = text_size(hdc, data.state_text);
        let state_x = pad + pct_sz.cx + s(10.0);
        let state_y = top_y + pct_sz.cy - state_sz.cy - s(5.0);
        draw_text(
            hdc,
            data.state_text,
            RECT {
                left: state_x,
                top: state_y,
                right: width - pad,
                bottom: state_y + state_sz.cy,
            },
            DT_LEFT | DT_SINGLELINE | DT_NOCLIP,
            accent,
        );

        let bar_y = s(58.0);
        let bar_h = s(6.0).max(2);
        let bar_w = width - pad * 2;
        fill_solid(
            hdc,
            RECT {
                left: pad,
                top: bar_y,
                right: pad + bar_w,
                bottom: bar_y + bar_h,
            },
            rgb(56, 56, 60),
        );
        if let Some(p) = data.percent {
            let fill_w = ((bar_w as f32) * (p.min(100) as f32 / 100.0)).round() as i32;
            if fill_w > 0 {
                let bar_color = if data.ac {
                    rgb(52, 211, 153)
                } else if low {
                    rgb(239, 68, 68)
                } else {
                    rgb(56, 189, 248)
                };
                fill_solid(
                    hdc,
                    RECT {
                        left: pad,
                        top: bar_y,
                        right: pad + fill_w,
                        bottom: bar_y + bar_h,
                    },
                    bar_color,
                );
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
            let y = s(76.0 + i as f32 * 26.0);
            let row_rect = RECT {
                left: pad,
                top: y,
                right: width - pad,
                bottom: y + s(22.0),
            };
            SelectObject(hdc, reg_font);
            draw_text(
                hdc,
                label,
                row_rect,
                DT_LEFT | DT_SINGLELINE | DT_NOCLIP,
                dim,
            );
            SelectObject(hdc, semi_font);
            draw_text(
                hdc,
                value,
                row_rect,
                DT_RIGHT | DT_SINGLELINE | DT_NOCLIP,
                val_color,
            );
        }

        SelectObject(hdc, old);
        DeleteObject(big_font);
        DeleteObject(semi_font);
        DeleteObject(reg_font);
    }
}

struct AppState {
    hwnd: HWND,
    hicon: HICON,
    last_icon_spec: Option<(Option<u32>, [u8; 3])>,
    last_tip: String,
    data: DisplayData,
    icon_mode: IconMode,
    warned: bool,
    last_deactivate: Option<Instant>,
    taskbar_created_msg: u32,
}

thread_local! {
    static STATE: RefCell<Option<AppState>> = const { RefCell::new(None) };
}

impl AppState {
    fn current_icon_spec(&self) -> (Option<u32>, [u8; 3]) {
        match self.icon_mode {
            IconMode::Charge => {
                let bg = match self.data.percent {
                    None => [90, 90, 90],
                    Some(_) if self.data.ac => [30, 140, 60],
                    Some(p) if p <= LOW_PERCENT => [190, 40, 40],
                    Some(_) => [60, 60, 60],
                };
                (self.data.percent, bg)
            }
            IconMode::Health => {
                let bg = if self.data.health_pct.is_some() {
                    [55, 85, 115]
                } else {
                    [90, 90, 90]
                };
                (self.data.health_pct, bg)
            }
        }
    }

    fn base_nid(&self) -> NOTIFYICONDATAW {
        let mut nid = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: self.hwnd,
            uID: TRAY_UID,
            uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_SHOWTIP,
            uCallbackMessage: WM_TRAY_CALLBACK,
            hIcon: self.hicon,
            ..Default::default()
        };
        copy_wide_trunc(&mut nid.szTip, &self.data.tooltip());
        nid
    }

    fn add_tray_icon(&mut self) {
        let spec = self.current_icon_spec();
        let new_icon = make_hicon(spec.0, spec.1);
        if !new_icon.is_null() {
            if !self.hicon.is_null() {
                // SAFETY: `self.hicon` was created by `CreateIconIndirect` and is replaced here.
                unsafe { DestroyIcon(self.hicon) };
            }
            self.hicon = new_icon;
            self.last_icon_spec = Some(spec);
        }
        self.last_tip = self.data.tooltip();
        let mut nid = self.base_nid();
        // SAFETY: `nid` is initialized with `cbSize` and valid handles for `NIM_ADD`/`NIM_SETVERSION`.
        unsafe {
            Shell_NotifyIconW(NIM_ADD, &nid);
            nid.Anonymous.uVersion = NOTIFYICON_VERSION_4;
            Shell_NotifyIconW(NIM_SETVERSION, &nid);
        }
    }

    fn update_tray(&mut self, balloon: Option<(&str, &str)>) {
        let spec = self.current_icon_spec();
        let tip = self.data.tooltip();
        let icon_changed = self.last_icon_spec != Some(spec);
        let tip_changed = self.last_tip != tip;
        if !icon_changed && !tip_changed && balloon.is_none() {
            return;
        }
        if icon_changed {
            let new_icon = make_hicon(spec.0, spec.1);
            if !new_icon.is_null() {
                if !self.hicon.is_null() {
                    // SAFETY: Destroying the previous icon after creating the replacement.
                    unsafe { DestroyIcon(self.hicon) };
                }
                self.hicon = new_icon;
                self.last_icon_spec = Some(spec);
            }
        }
        self.last_tip = tip;
        let mut nid = self.base_nid();
        if !icon_changed {
            nid.uFlags &= !NIF_ICON;
        }
        if let Some((title, body)) = balloon {
            nid.uFlags |= NIF_INFO;
            nid.dwInfoFlags = NIIF_WARNING;
            copy_wide_trunc(&mut nid.szInfoTitle, title);
            copy_wide_trunc(&mut nid.szInfo, body);
        }
        // SAFETY: `nid` is a valid `NOTIFYICONDATAW` for `NIM_MODIFY`.
        unsafe {
            Shell_NotifyIconW(NIM_MODIFY, &nid);
        }
    }

    fn remove_tray(&mut self) {
        let nid = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: self.hwnd,
            uID: TRAY_UID,
            ..Default::default()
        };
        // SAFETY: Removing our tray icon and freeing `self.hicon` on shutdown.
        unsafe {
            Shell_NotifyIconW(NIM_DELETE, &nid);
            if !self.hicon.is_null() {
                DestroyIcon(self.hicon);
                self.hicon = ptr::null_mut();
            }
        }
    }

    fn apply_latest_data(&mut self) {
        let Some(d) = LATEST_DATA.lock().unwrap_or_else(|e| e.into_inner()).take() else {
            return;
        };
        let low = !d.ac && d.percent.is_some_and(|p| p <= LOW_PERCENT);
        let msg = (low && !self.warned).then(|| format!("{} ({})", d.pct_text, d.state_text));
        self.warned = low;
        self.data = d;
        self.update_tray(msg.as_deref().map(|b| ("Battery low", b)));
    }

    fn set_icon_mode(&mut self, mode: IconMode) {
        if self.icon_mode == mode {
            return;
        }
        self.icon_mode = mode;
        save_icon_mode(mode);
        self.update_tray(None);
    }

    fn compute_panel_rect(&self) -> RECT {
        let ident = NOTIFYICONIDENTIFIER {
            cbSize: std::mem::size_of::<NOTIFYICONIDENTIFIER>() as u32,
            hWnd: self.hwnd,
            uID: TRAY_UID,
            ..Default::default()
        };
        let mut icon_rc = RECT::default();
        // SAFETY: Querying the tray icon screen rectangle from the shell.
        let has_icon_rc = unsafe { Shell_NotifyIconGetRect(&ident, &mut icon_rc) } == 0
            && icon_rc.right > icon_rc.left
            && icon_rc.bottom > icon_rc.top;

        let anchor = if has_icon_rc {
            POINT {
                x: (icon_rc.left + icon_rc.right) / 2,
                y: (icon_rc.top + icon_rc.bottom) / 2,
            }
        } else {
            let mut pt = POINT::default();
            // SAFETY: `pt` is a valid out-pointer.
            unsafe { GetCursorPos(&mut pt) };
            pt
        };

        let mut mi = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        // SAFETY: Querying monitor work area containing `anchor`.
        unsafe {
            let hmon = MonitorFromPoint(anchor, MONITOR_DEFAULTTONEAREST);
            GetMonitorInfoW(hmon, &mut mi);
        }
        let work = mi.rcWork;

        // SAFETY: `self.hwnd` is valid; falls back to 96 DPI if 0.
        let dpi = unsafe { GetDpiForWindow(self.hwnd) };
        let scale = if dpi > 0 { dpi as f32 / 96.0 } else { 1.0 };
        let w = (PANEL_W as f32 * scale).round() as i32;
        let h = (self.data.panel_height() as f32 * scale).round() as i32;
        let margin = (8.0 * scale).round() as i32;
        let gap = (6.0 * scale).round() as i32;

        let (mut x, mut y) = if has_icon_rc {
            let x = icon_rc.left + ((icon_rc.right - icon_rc.left) - w) / 2;
            let y = if icon_rc.top - h - gap >= work.top {
                icon_rc.top - h - gap
            } else {
                icon_rc.bottom + gap
            };
            (x, y)
        } else {
            (work.right - w - margin, work.bottom - h - margin)
        };

        x = x.clamp(
            work.left + margin,
            (work.right - w - margin).max(work.left + margin),
        );
        y = y.clamp(
            work.top + margin,
            (work.bottom - h - margin).max(work.top + margin),
        );
        RECT {
            left: x,
            top: y,
            right: x + w,
            bottom: y + h,
        }
    }
}

fn sync_visible_panel(hwnd: HWND) {
    // SAFETY: `hwnd` is our main window handle; only query tray rect when panel is visible.
    if unsafe { IsWindowVisible(hwnd) } != 0 {
        let rect = STATE.with(|cell| {
            cell.try_borrow()
                .ok()
                .and_then(|opt| opt.as_ref().map(AppState::compute_panel_rect))
        });
        if let Some(r) = rect {
            // SAFETY: Resizing/repositioning and invalidating our popup window outside `STATE` borrow.
            unsafe {
                SetWindowPos(
                    hwnd,
                    ptr::null_mut(),
                    r.left,
                    r.top,
                    r.right - r.left,
                    r.bottom - r.top,
                    SWP_NOZORDER | SWP_NOACTIVATE,
                );
                InvalidateRect(hwnd, ptr::null(), 0);
            }
        }
    }
}

fn do_apply_data(hwnd: HWND) {
    STATE.with(|cell| {
        if let Ok(mut opt) = cell.try_borrow_mut()
            && let Some(st) = opt.as_mut()
        {
            st.apply_latest_data();
        }
    });
    sync_visible_panel(hwnd);
}

fn do_toggle_panel(hwnd: HWND) {
    // SAFETY: Checking visibility of `hwnd`.
    if unsafe { IsWindowVisible(hwnd) } != 0 {
        STATE.with(|cell| {
            if let Ok(mut opt) = cell.try_borrow_mut()
                && let Some(st) = opt.as_mut()
            {
                st.last_deactivate = None;
            }
        });
        // SAFETY: Hiding `hwnd` after dropping `STATE` borrow.
        unsafe {
            ShowWindow(hwnd, SW_HIDE);
        }
        return;
    }
    let recently_deactivated = STATE.with(|cell| {
        cell.try_borrow_mut().ok().is_some_and(|mut opt| {
            opt.as_mut().is_some_and(|st| {
                st.last_deactivate
                    .take()
                    .is_some_and(|t| t.elapsed() < Duration::from_millis(250))
            })
        })
    });
    if recently_deactivated {
        return;
    }
    let rect = STATE.with(|cell| {
        cell.try_borrow()
            .ok()
            .and_then(|opt| opt.as_ref().map(AppState::compute_panel_rect))
    });
    let Some(r) = rect else {
        return;
    };
    // SAFETY: Positioning, showing, and activating `hwnd` immediately using cached `DisplayData`,
    // then scheduling an async refresh after the window is already visible.
    unsafe {
        SetWindowPos(
            hwnd,
            ptr::null_mut(),
            r.left,
            r.top,
            r.right - r.left,
            r.bottom - r.top,
            SWP_NOZORDER | SWP_NOACTIVATE,
        );
        ShowWindow(hwnd, SW_SHOW);
        SetForegroundWindow(hwnd);
        InvalidateRect(hwnd, ptr::null(), 0);
    }
    wake_worker();
}

fn do_context_menu(hwnd: HWND) {
    let icon_mode = STATE.with(|cell| {
        cell.try_borrow()
            .ok()
            .and_then(|opt| opt.as_ref().map(|st| st.icon_mode))
            .unwrap_or(IconMode::Charge)
    });
    let mut pt = POINT::default();
    // SAFETY: Creating a Win32 popup menu, running `TrackPopupMenuEx` with `STATE` unborrowed,
    // and destroying the menu afterward.
    let cmd = unsafe {
        GetCursorPos(&mut pt);
        let hmenu = CreatePopupMenu();
        if hmenu.is_null() {
            return;
        }
        let hsub = CreatePopupMenu();
        let charge_txt = wide("Charge");
        let health_txt = wide("Health");
        AppendMenuW(
            hsub,
            MF_STRING,
            CMD_ICON_CHARGE as usize,
            charge_txt.as_ptr(),
        );
        AppendMenuW(
            hsub,
            MF_STRING,
            CMD_ICON_HEALTH as usize,
            health_txt.as_ptr(),
        );
        let checked_id = match icon_mode {
            IconMode::Charge => CMD_ICON_CHARGE,
            IconMode::Health => CMD_ICON_HEALTH,
        };
        CheckMenuRadioItem(
            hsub,
            CMD_ICON_CHARGE,
            CMD_ICON_HEALTH,
            checked_id,
            MF_BYCOMMAND,
        );

        let sub_txt = wide("Icon shows");
        AppendMenuW(hmenu, MF_POPUP, hsub as usize, sub_txt.as_ptr());

        let auto_flags = MF_STRING
            | if is_autostart_enabled() {
                MF_CHECKED
            } else {
                MF_UNCHECKED
            };
        let auto_txt = wide("Start with Windows");
        AppendMenuW(hmenu, auto_flags, CMD_AUTOSTART as usize, auto_txt.as_ptr());

        AppendMenuW(hmenu, MF_SEPARATOR, 0, ptr::null());
        let quit_txt = wide("Quit");
        AppendMenuW(hmenu, MF_STRING, CMD_QUIT as usize, quit_txt.as_ptr());

        SetForegroundWindow(hwnd);
        let selected = TrackPopupMenuEx(
            hmenu,
            TPM_RIGHTBUTTON | TPM_BOTTOMALIGN | TPM_LEFTALIGN | TPM_RETURNCMD | TPM_NONOTIFY,
            pt.x,
            pt.y,
            hwnd,
            ptr::null(),
        ) as u32;
        PostMessageW(hwnd, WM_NULL, 0, 0);
        DestroyMenu(hmenu);
        selected
    };

    match cmd {
        CMD_ICON_CHARGE => STATE.with(|cell| {
            if let Ok(mut opt) = cell.try_borrow_mut()
                && let Some(st) = opt.as_mut()
            {
                st.set_icon_mode(IconMode::Charge);
            }
        }),
        CMD_ICON_HEALTH => STATE.with(|cell| {
            if let Ok(mut opt) = cell.try_borrow_mut()
                && let Some(st) = opt.as_mut()
            {
                st.set_icon_mode(IconMode::Health);
            }
        }),
        CMD_AUTOSTART => set_autostart(!is_autostart_enabled()),
        CMD_QUIT => {
            // SAFETY: Posting `WM_CLOSE` to `hwnd`.
            unsafe {
                PostMessageW(hwnd, WM_CLOSE, 0, 0);
            }
        }
        _ => {}
    }
}

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_SETCURSOR => {
            // SAFETY: Setting the shared system arrow cursor.
            unsafe {
                SetCursor(LoadCursorW(ptr::null_mut(), IDC_ARROW));
            }
            1
        }
        WM_ERASEBKGND => 1,
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            // SAFETY: Standard `BeginPaint` / double-buffered GDI `BitBlt` / `EndPaint` sequence.
            unsafe {
                let hdc = BeginPaint(hwnd, &mut ps);
                let mut rc = RECT::default();
                GetClientRect(hwnd, &mut rc);
                let w = (rc.right - rc.left).max(1);
                let h = (rc.bottom - rc.top).max(1);
                let dpi = GetDpiForWindow(hwnd);
                let scale = if dpi > 0 { dpi as f32 / 96.0 } else { 1.0 };

                let mem_dc = CreateCompatibleDC(hdc);
                let mem_bm = CreateCompatibleBitmap(hdc, w, h);
                let old_bm = SelectObject(mem_dc, mem_bm);
                let snapshot = STATE.with(|cell| {
                    cell.try_borrow()
                        .ok()
                        .and_then(|opt| opt.as_ref().map(|st| st.data.clone()))
                });
                if let Some(d) = snapshot {
                    paint_panel_gdi(mem_dc, w, h, scale, &d);
                }
                BitBlt(hdc, 0, 0, w, h, mem_dc, 0, 0, SRCCOPY);
                SelectObject(mem_dc, old_bm);
                DeleteObject(mem_bm);
                DeleteDC(mem_dc);
                EndPaint(hwnd, &ps);
            }
            0
        }
        WM_ACTIVATE => {
            // SAFETY: Hiding `hwnd` on focus loss after recording timestamp.
            if (wparam & 0xFFFF) as u32 == WA_INACTIVE && unsafe { IsWindowVisible(hwnd) } != 0 {
                STATE.with(|cell| {
                    if let Ok(mut opt) = cell.try_borrow_mut()
                        && let Some(st) = opt.as_mut()
                    {
                        st.last_deactivate = Some(Instant::now());
                    }
                });
                unsafe {
                    ShowWindow(hwnd, SW_HIDE);
                }
            }
            0
        }
        WM_KEYDOWN => {
            if wparam as u16 == VK_ESCAPE {
                STATE.with(|cell| {
                    if let Ok(mut opt) = cell.try_borrow_mut()
                        && let Some(st) = opt.as_mut()
                    {
                        st.last_deactivate = None;
                    }
                });
                // SAFETY: Hiding `hwnd` on Escape key.
                unsafe {
                    ShowWindow(hwnd, SW_HIDE);
                }
            }
            0
        }
        WM_DPICHANGED => {
            sync_visible_panel(hwnd);
            0
        }
        WM_TRAY_CALLBACK => {
            let ev = (lparam & 0xFFFF) as u32;
            match ev {
                NIN_SELECT | NIN_KEYSELECT => do_toggle_panel(hwnd),
                WM_CONTEXTMENU => do_context_menu(hwnd),
                _ => {}
            }
            0
        }
        WM_TIMER => {
            if wparam == TIMER_SETTLE {
                // SAFETY: Stopping the one-shot settle timer.
                unsafe {
                    KillTimer(hwnd, TIMER_SETTLE);
                }
            }
            wake_worker();
            0
        }
        WM_POWERBROADCAST => {
            let wp = wparam as u32;
            if wp == PBT_APMPOWERSTATUSCHANGE
                || wp == PBT_POWERSETTINGCHANGE
                || wp == PBT_APMRESUMEAUTOMATIC
            {
                wake_worker();
                // SAFETY: Scheduling a follow-up refresh 800 ms later once EC wattage settles.
                unsafe {
                    SetTimer(hwnd, TIMER_SETTLE, SETTLE_MS, None);
                }
            }
            1
        }
        WM_APP_DATA_READY => {
            do_apply_data(hwnd);
            0
        }
        WM_CLOSE => {
            STATE.with(|cell| {
                if let Ok(mut opt) = cell.try_borrow_mut()
                    && let Some(st) = opt.as_mut()
                {
                    st.remove_tray();
                }
            });
            // SAFETY: Default handling destroys the window.
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }
        WM_DESTROY => {
            // SAFETY: Posting `WM_QUIT` to end the message loop.
            unsafe {
                PostQuitMessage(0);
            }
            0
        }
        _ => {
            let redraw_tray = STATE.with(|cell| {
                cell.try_borrow().ok().is_some_and(|opt| {
                    opt.as_ref().is_some_and(|st| {
                        st.taskbar_created_msg != 0 && msg == st.taskbar_created_msg
                    })
                })
            });
            if redraw_tray {
                STATE.with(|cell| {
                    if let Ok(mut opt) = cell.try_borrow_mut()
                        && let Some(st) = opt.as_mut()
                    {
                        st.add_tray_icon();
                    }
                });
                return 0;
            }
            // SAFETY: Forwarding unhandled messages to `DefWindowProcW`.
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }
    }
}

fn main() {
    // SAFETY: Enabling Per-Monitor V2 DPI awareness before creating any windows.
    unsafe {
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    enable_dark_menus();

    let class_name = wide("BatlensTrayPanelWindow");
    let title = wide("batlens");
    // SAFETY: Registering the window class and creating the hidden popup/tray owner window.
    let hwnd = unsafe {
        let hinstance = GetModuleHandleW(ptr::null());
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wnd_proc),
            hInstance: hinstance,
            hCursor: LoadCursorW(ptr::null_mut(), IDC_ARROW),
            lpszClassName: class_name.as_ptr(),
            ..Default::default()
        };
        RegisterClassExW(&wc);
        CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
            class_name.as_ptr(),
            title.as_ptr(),
            WS_POPUP,
            -32000,
            -32000,
            1,
            1,
            ptr::null_mut(),
            ptr::null_mut(),
            hinstance,
            ptr::null(),
        )
    };
    if hwnd.is_null() {
        return;
    }

    let taskbar_msg_name = wide("TaskbarCreated");
    // SAFETY: Registering `TaskbarCreated` message to re-add tray icon if Explorer restarts.
    let taskbar_created_msg = unsafe { RegisterWindowMessageW(taskbar_msg_name.as_ptr()) };

    let cached_wear_hit = if let Some(w) = try_cached_wear_text() {
        let _ = WEAR_TEXT.set(w);
        true
    } else {
        false
    };

    let initial_data = batlens_core::sys::sample()
        .map(|s| DisplayData::from_sample(&s))
        .unwrap_or_default();

    // SAFETY: Pre-warming IME/focus state, GDI `Segoe UI` fonts, and popup menu tables before
    // registering the tray icon so the first left/right click has zero cold-start latency.
    unsafe {
        ShowWindow(hwnd, SW_SHOW);
        SetForegroundWindow(hwnd);
        ShowWindow(hwnd, SW_HIDE);
        let hdc = GetDC(hwnd);
        if !hdc.is_null() {
            let mem_dc = CreateCompatibleDC(hdc);
            let mem_bm = CreateCompatibleBitmap(hdc, 4, 4);
            let old_bm = SelectObject(mem_dc, mem_bm);
            paint_panel_gdi(mem_dc, 4, 4, 1.0, &initial_data);
            SelectObject(mem_dc, old_bm);
            DeleteObject(mem_bm);
            DeleteDC(mem_dc);
            ReleaseDC(hwnd, hdc);
        }
        let hm = CreatePopupMenu();
        if !hm.is_null() {
            DestroyMenu(hm);
        }
        let mut msg: MSG = std::mem::zeroed();
        PeekMessageW(&mut msg, ptr::null_mut(), 0, 0, PM_NOREMOVE);
    }

    let mut state = AppState {
        hwnd,
        hicon: ptr::null_mut(),
        last_icon_spec: None,
        last_tip: String::new(),
        data: initial_data,
        icon_mode: load_icon_mode(),
        warned: false,
        last_deactivate: None,
        taskbar_created_msg,
    };
    state.add_tray_icon();
    STATE.with(|cell| {
        *cell.borrow_mut() = Some(state);
    });

    // SAFETY: Registering AC/DC power notification on `hwnd`.
    let _power_notify =
        unsafe { RegisterPowerSettingNotification(hwnd, &GUID_ACDC_POWER_SOURCE, 0) };

    let (tx, rx) = mpsc::sync_channel::<()>(1);
    let _ = POLL_WAKE.set(tx);
    let worker_hwnd = hwnd as isize;
    std::thread::spawn(move || {
        let push_sample = || {
            if let Ok(s) = batlens_core::sys::sample() {
                let d = DisplayData::from_sample(&s);
                *LATEST_DATA.lock().unwrap_or_else(|e| e.into_inner()) = Some(d);
                // SAFETY: `PostMessageW` is thread-safe and posts `WM_APP_DATA_READY` to `hwnd`.
                unsafe {
                    PostMessageW(worker_hwnd as HWND, WM_APP_DATA_READY, 0, 0);
                }
            }
        };
        if !cached_wear_hit && let Some(w) = generate_wear_text() {
            let _ = WEAR_TEXT.set(w);
            push_sample();
        }
        loop {
            let _ = rx.recv_timeout(Duration::from_millis(u64::from(POLL_MS)));
            push_sample();
        }
    });
    // SAFETY: Standard Win32 message loop for the main GUI thread.
    unsafe {
        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}
