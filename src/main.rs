#![windows_subsystem = "windows"]

mod bt;
mod names;

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bt::{Cmd, DeviceRow, Event, RadioInfo};
use names::Names;
use native_windows_gui as nwg;
use nwg::{CheckBoxState, Event as E};

const W: i32 = 640;
const H: i32 = 600;
/// Content width (16 px margins).
const CW: i32 = W - 32;
const BG: [u8; 3] = [255, 255, 255];

#[derive(Default)]
struct Ui {
    window: nwg::Window,
    embed: nwg::EmbedResource,
    icon: nwg::Icon,
    font_title: nwg::Font,
    font_body: nwg::Font,
    font_bold: nwg::Font,
    font_button: nwg::Font,
    font_log: nwg::Font,
    title: nwg::Label,
    step1: nwg::Label,
    step2: nwg::Label,
    list: nwg::ListView,
    nick_label: nwg::Label,
    nick_input: nwg::TextInput,
    nick_save: nwg::Button,
    show_all: nwg::CheckBox,
    auto: nwg::CheckBox,
    rescan: nwg::Button,
    forget: nwg::Button,
    connect: nwg::Button,
    status: nwg::Label,
    log: nwg::TextBox,
    notice: nwg::Notice,
    #[allow(deprecated)]
    timer: nwg::Timer,
}

struct State {
    all: HashMap<String, DeviceRow>,
    /// Ids in the order they are shown in the list.
    order: Vec<String>,
    busy: Option<String>,
    radio: RadioInfo,
    /// While set (and in the future), the status line keeps its message instead of the idle hint.
    sticky_until: Option<Instant>,
    scan_error: Option<String>,
    auto_tried: HashMap<String, Instant>,
    log: Vec<String>,
    names: Names,
    /// Device whose nickname is currently loaded into the nickname box.
    nick_for: Option<String>,
}

impl State {
    fn nickname(&self, r: &DeviceRow) -> Option<String> {
        self.names.get(&r.address).map(|n| n.nickname.clone())
    }

    /// "Red Joy-Con  ·  Joy-Con (L)" when nicknamed, otherwise the device's own name.
    fn label(&self, r: &DeviceRow) -> String {
        match self.names.get(&r.address) {
            Some(n) => {
                let model = if !n.model.is_empty() {
                    n.model.clone()
                } else if r.name != n.nickname {
                    r.name.clone()
                } else {
                    String::new()
                };
                if model.trim().is_empty() || model == n.nickname {
                    n.nickname.clone()
                } else {
                    format!("{}  ·  {}", n.nickname, model)
                }
            }
            None if r.name.trim().is_empty() => "(unnamed device)".into(),
            None => r.name.clone(),
        }
    }
}

struct App {
    ui: Ui,
    st: RefCell<State>,
    rx: Receiver<Event>,
    cmd: Sender<Cmd>,
    rebuilding: Cell<bool>,
}

fn px(v: i32) -> i32 {
    (v as f64 * nwg::scale_factor()).round() as i32
}

fn now_hms() -> String {
    let t = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
    format!("{:02}:{:02}:{:02}", t.wHour, t.wMinute, t.wSecond)
}

fn row_status(r: &DeviceRow, busy: bool) -> String {
    if busy {
        return "Working…".into();
    }
    if r.connected {
        "Connected".into()
    } else if r.paired {
        "Saved, not connected – will re-pair".into()
    } else if r.is_switch {
        match r.rssi {
            Some(db) => format!("In pairing mode – ready  ({db} dBm)"),
            None => "In pairing mode – ready".into(),
        }
    } else {
        "Discoverable – ready to pair".into()
    }
}

fn row_rank(r: &DeviceRow) -> (u8, u8) {
    let kind = if r.is_switch { 0 } else { 1 };
    let state = if !r.paired { 0 } else if !r.connected { 1 } else { 2 };
    (kind, state)
}

/// nwg's window class uses a system-colour brush that differs between Windows versions;
/// paint the window plain white so it matches the label/checkbox backgrounds.
fn paint_window_background(window: &nwg::Window) {
    use windows::Win32::Foundation::{COLORREF, HWND};
    use windows::Win32::Graphics::Gdi::{CreateSolidBrush, InvalidateRect};
    use windows::Win32::UI::WindowsAndMessaging::{SetClassLongPtrW, GCLP_HBRBACKGROUND};
    if let Some(h) = window.handle.hwnd() {
        let hwnd = HWND(h as *mut core::ffi::c_void);
        unsafe {
            let brush = CreateSolidBrush(COLORREF(u32::from_le_bytes([BG[0], BG[1], BG[2], 0])));
            SetClassLongPtrW(hwnd, GCLP_HBRBACKGROUND, brush.0 as isize);
            let _ = InvalidateRect(Some(hwnd), None, true);
        }
    }
}

fn has_focus(handle: &nwg::ControlHandle) -> bool {
    use windows::Win32::UI::Input::KeyboardAndMouse::GetFocus;
    match handle.hwnd() {
        Some(h) => unsafe { GetFocus().0 == h as *mut core::ffi::c_void },
        None => false,
    }
}

/// The list view builder has no font option, so set it directly.
fn set_control_font(handle: &nwg::ControlHandle, font: &nwg::Font) {
    use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{SendMessageW, WM_SETFONT};
    if let Some(h) = handle.hwnd() {
        unsafe {
            SendMessageW(
                HWND(h as *mut core::ffi::c_void),
                WM_SETFONT,
                Some(WPARAM(font.handle as usize)),
                Some(LPARAM(1)),
            );
        }
    }
}

fn build_ui(ui: &mut Ui) -> Result<(), nwg::NwgError> {
    nwg::EmbedResource::builder().build(&mut ui.embed)?;
    let _ = nwg::Icon::builder()
        .source_embed(Some(&ui.embed))
        .source_embed_id(1)
        .strict(false)
        .size(Some((32, 32)))
        .build(&mut ui.icon);

    nwg::Font::builder().family("Segoe UI Semibold").size(26).build(&mut ui.font_title)?;
    nwg::Font::builder().family("Segoe UI").size(16).build(&mut ui.font_body)?;
    nwg::Font::builder().family("Segoe UI").size(16).weight(700).build(&mut ui.font_bold)?;
    nwg::Font::builder().family("Segoe UI").size(18).weight(700).build(&mut ui.font_button)?;
    nwg::Font::builder().family("Consolas").size(14).build(&mut ui.font_log)?;
    // Global default font (used by controls that can't take a font, like the list view).
    let mut global = nwg::Font::default();
    nwg::Font::builder().family("Segoe UI").size(16).build(&mut global)?;
    nwg::Font::set_global_default(Some(global));

    nwg::Window::builder()
        .size((W, H))
        .center(true)
        .title("Switch Controller Connector")
        .icon(Some(&ui.icon))
        .flags(nwg::WindowFlags::WINDOW)
        .build(&mut ui.window)?;
    paint_window_background(&ui.window);

    let p = &ui.window;
    nwg::Label::builder()
        .parent(p)
        .background_color(Some(BG))
        .text("Switch Controller Connector")
        .font(Some(&ui.font_title))
        .position((16, 10))
        .size((CW, 36))
        .build(&mut ui.title)?;

    nwg::Label::builder()
        .parent(p)
        .background_color(Some(BG))
        .text("1.  Hold the small SYNC button until the controller's lights run back and forth.\n     Joy-Con: on the rail between SL and SR  ·  Pro Controller: on top, by the USB-C port")
        .font(Some(&ui.font_body))
        .v_align(nwg::VTextAlign::Top)
        .position((16, 52))
        .size((CW, 46))
        .build(&mut ui.step1)?;

    nwg::Label::builder()
        .parent(p)
        .background_color(Some(BG))
        .text("2.  Select it below and click Connect – any old pairing is cleared and redone for you.\n3.  Optional: type a nickname and click Save name so you can tell your controllers apart.")
        .font(Some(&ui.font_body))
        .v_align(nwg::VTextAlign::Top)
        .position((16, 100))
        .size((CW, 42))
        .build(&mut ui.step2)?;

    nwg::ListView::builder()
        .parent(p)
        .position((16, 148))
        .size((CW, 176))
        .list_style(nwg::ListViewStyle::Detailed)
        .flags(
            nwg::ListViewFlags::VISIBLE
                | nwg::ListViewFlags::TAB_STOP
                | nwg::ListViewFlags::SINGLE_SELECTION
                | nwg::ListViewFlags::ALWAYS_SHOW_SELECTION,
        )
        .ex_flags(nwg::ListViewExFlags::FULL_ROW_SELECT)
        .build(&mut ui.list)?;
    ui.list.set_headers_enabled(true);
    set_control_font(&ui.list.handle, &ui.font_body);
    for (i, (name, width)) in [("Controller", 210), ("Status", 255), ("Address", 135)].iter().enumerate() {
        ui.list.insert_column(nwg::InsertListViewColumn {
            index: Some(i as i32),
            fmt: None,
            width: Some(px(*width)),
            text: Some((*name).to_string()),
        });
    }

    nwg::Label::builder()
        .parent(p)
        .background_color(Some(BG))
        .text("Nickname:")
        .font(Some(&ui.font_body))
        .position((16, 336))
        .size((78, 24))
        .build(&mut ui.nick_label)?;

    nwg::TextInput::builder()
        .parent(p)
        .font(Some(&ui.font_body))
        .placeholder_text(Some("e.g. Red Joy-Con or Pro (left desk)"))
        .limit(names::MAX_LEN)
        .position((96, 332))
        .size((CW - 80 - 176, 30))
        .build(&mut ui.nick_input)?;

    nwg::Button::builder()
        .parent(p)
        .text("Save name")
        .font(Some(&ui.font_body))
        .position((16 + CW - 168, 331))
        .size((168, 32))
        .build(&mut ui.nick_save)?;

    nwg::CheckBox::builder()
        .parent(p)
        .background_color(Some(BG))
        .text("Show all Bluetooth devices")
        .font(Some(&ui.font_body))
        .position((16, 372))
        .size((230, 26))
        .build(&mut ui.show_all)?;

    nwg::CheckBox::builder()
        .parent(p)
        .background_color(Some(BG))
        .text("Auto-connect controllers that enter pairing mode")
        .font(Some(&ui.font_body))
        .position((16 + CW - 334, 372))
        .size((334, 26))
        .build(&mut ui.auto)?;

    nwg::Button::builder()
        .parent(p)
        .text("Rescan")
        .font(Some(&ui.font_body))
        .position((16, 406))
        .size((110, 40))
        .build(&mut ui.rescan)?;

    nwg::Button::builder()
        .parent(p)
        .text("Forget pairing")
        .font(Some(&ui.font_body))
        .position((134, 406))
        .size((140, 40))
        .build(&mut ui.forget)?;

    nwg::Button::builder()
        .parent(p)
        .text("Connect")
        .font(Some(&ui.font_button))
        .position((16 + CW - 200, 406))
        .size((200, 40))
        .focus(true)
        .build(&mut ui.connect)?;

    nwg::Label::builder()
        .parent(p)
        .background_color(Some(BG))
        .text("Starting Bluetooth scan…")
        .font(Some(&ui.font_bold))
        .v_align(nwg::VTextAlign::Top)
        .position((16, 456))
        .size((CW, 44))
        .build(&mut ui.status)?;

    nwg::TextBox::builder()
        .parent(p)
        .font(Some(&ui.font_log))
        .flags(nwg::TextBoxFlags::VISIBLE | nwg::TextBoxFlags::VSCROLL | nwg::TextBoxFlags::AUTOVSCROLL)
        .readonly(true)
        .position((16, 504))
        .size((CW, 80))
        .build(&mut ui.log)?;

    nwg::Notice::builder().parent(p).build(&mut ui.notice)?;
    #[allow(deprecated)]
    nwg::Timer::builder().parent(p).interval(2000).stopped(false).build(&mut ui.timer)?;
    Ok(())
}

impl App {
    fn on_event(&self, evt: E, handle: nwg::ControlHandle) {
        let ui = &self.ui;
        match evt {
            E::OnWindowClose if handle == ui.window.handle => {
                let _ = self.cmd.send(Cmd::Quit);
                nwg::stop_thread_dispatch();
            }
            E::OnNotice if handle == ui.notice.handle => self.drain(),
            E::OnTimerTick => self.refresh_controls(),
            E::OnButtonClick => {
                if handle == ui.connect.handle {
                    self.on_connect();
                } else if handle == ui.forget.handle {
                    self.on_forget();
                } else if handle == ui.rescan.handle {
                    self.on_rescan();
                } else if handle == ui.show_all.handle {
                    self.rebuild_list();
                    self.refresh_controls();
                } else if handle == ui.auto.handle {
                    self.try_auto_connect();
                } else if handle == ui.nick_save.handle {
                    self.on_save_name();
                }
            }
            E::OnListViewDoubleClick | E::OnListViewItemActivated if handle == ui.list.handle => {
                if !self.rebuilding.get() {
                    self.on_connect();
                }
            }
            E::OnListViewItemChanged | E::OnListViewClick if handle == ui.list.handle => {
                if !self.rebuilding.get() {
                    self.sync_nick_input();
                    self.refresh_controls();
                }
            }
            // Enter is routed to the window by the dialog manager, so check where the focus is.
            E::OnKeyEnter if has_focus(&ui.nick_input.handle) => self.on_save_name(),
            _ => {}
        }
    }

    fn log(&self, msg: &str) {
        let text = {
            let Ok(mut st) = self.st.try_borrow_mut() else { return };
            st.log.push(format!("[{}] {}", now_hms(), msg));
            let n = st.log.len();
            if n > 300 {
                st.log.drain(0..n - 300);
            }
            st.log.join("\r\n")
        };
        self.ui.log.set_text(&text);
        self.ui.log.scroll_lastline();
    }

    fn set_status(&self, msg: &str, sticky: bool) {
        if let Ok(mut st) = self.st.try_borrow_mut() {
            st.sticky_until = if sticky { Some(Instant::now() + Duration::from_secs(15)) } else { None };
        }
        self.ui.status.set_text(msg);
    }

    fn drain(&self) {
        let mut dirty = false;
        while let Ok(ev) = self.rx.try_recv() {
            match ev {
                Event::Clear => {
                    if let Ok(mut st) = self.st.try_borrow_mut() {
                        st.all.clear();
                    }
                    dirty = true;
                }
                Event::Upsert(mut row) => {
                    if let Ok(mut st) = self.st.try_borrow_mut() {
                        if let Some(nick) = st.nickname(&row) {
                            row.is_switch = true;
                            // While in pairing mode the controller reports its real model name.
                            if row.name != nick && bt::looks_like_switch(&row.name) {
                                st.names.note_model(&row.address, &row.name);
                            }
                        }
                        st.all.insert(row.id.clone(), row);
                    }
                    dirty = true;
                }
                Event::Remove(id) => {
                    if let Ok(mut st) = self.st.try_borrow_mut() {
                        st.all.remove(&id);
                    }
                    dirty = true;
                }
                Event::Status(s) => self.set_status(&s, true),
                Event::Log(s) => self.log(&s),
                Event::Busy(b) => {
                    if let Ok(mut st) = self.st.try_borrow_mut() {
                        st.busy = b;
                    }
                    dirty = true;
                }
                Event::Finished { ok, message } => {
                    let _ = ok;
                    self.set_status(&message, true);
                }
                Event::ScanError(e) => {
                    if let Ok(mut st) = self.st.try_borrow_mut() {
                        st.scan_error = e;
                    }
                }
                Event::Radio(r) => {
                    if let Ok(mut st) = self.st.try_borrow_mut() {
                        st.radio = r;
                    }
                    match r {
                        RadioInfo::Off => self.set_status("Bluetooth is turned off. Turn it on (Settings > Bluetooth & devices), then click Rescan.", true),
                        RadioInfo::Missing => {
                            self.log("Windows didn't report a Bluetooth radio.");
                            let empty = self.st.try_borrow().map(|s| s.all.is_empty()).unwrap_or(true);
                            if empty {
                                self.set_status("No Bluetooth adapter found. Plug one in (or turn Bluetooth on), then click Rescan.", true);
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        if dirty {
            self.rebuild_list();
            self.try_auto_connect();
        }
        self.refresh_controls();
    }

    fn selected_id(&self) -> Option<String> {
        let idx = self.ui.list.selected_item()?;
        let st = self.st.try_borrow().ok()?;
        st.order.get(idx).cloned()
    }

    fn rebuild_list(&self) {
        let selected = self.selected_id();
        let show_all = self.ui.show_all.check_state() == CheckBoxState::Checked;

        let rows: Vec<(DeviceRow, bool, String)> = {
            let Ok(mut st) = self.st.try_borrow_mut() else { return };
            let mut rows: Vec<(DeviceRow, String)> = st
                .all
                .values()
                .filter(|r| r.is_switch || show_all)
                .map(|r| (r.clone(), st.label(r)))
                .collect();
            rows.sort_by(|a, b| row_rank(&a.0).cmp(&row_rank(&b.0)).then_with(|| a.1.cmp(&b.1)));
            st.order = rows.iter().map(|(r, _)| r.id.clone()).collect();
            let busy = st.busy.clone();
            rows.into_iter()
                .map(|(r, label)| {
                    let b = busy.as_deref() == Some(r.id.as_str());
                    (r, b, label)
                })
                .collect()
        };

        self.rebuilding.set(true);
        let list = &self.ui.list;
        list.set_redraw(false);
        list.clear();
        for (i, (r, busy, label)) in rows.iter().enumerate() {
            list.insert_items_row(Some(i as i32), &[label.clone(), row_status(r, *busy), r.address.clone()]);
        }
        // Keep the selection; otherwise pre-select the best controller so one click is enough.
        let target = selected
            .and_then(|id| rows.iter().position(|(r, _, _)| r.id == id))
            .or_else(|| rows.iter().position(|(r, _, _)| r.is_switch));
        if let Some(i) = target {
            list.select_item(i, true);
        }
        list.set_redraw(true);
        self.rebuilding.set(false);
        self.sync_nick_input();
    }

    /// Load the selected controller's nickname into the box (only when the selection changes,
    /// so list refreshes never overwrite what you're typing).
    fn sync_nick_input(&self) {
        let selected = self.selected_id();
        let text = {
            let Ok(mut st) = self.st.try_borrow_mut() else { return };
            if st.nick_for == selected {
                return;
            }
            st.nick_for = selected.clone();
            selected
                .as_ref()
                .and_then(|id| st.all.get(id))
                .and_then(|r| st.nickname(r))
                .unwrap_or_default()
        };
        self.ui.nick_input.set_text(&text);
    }

    fn on_save_name(&self) {
        if self.is_busy() {
            return;
        }
        let Some(id) = self.selected_id() else {
            self.set_status("Select a controller in the list first, then type its nickname.", true);
            return;
        };
        let typed = self.ui.nick_input.text();
        let outcome = {
            let Ok(mut st) = self.st.try_borrow_mut() else { return };
            let Some(row) = st.all.get(&id).cloned() else { return };
            if row.address.is_empty() {
                drop(st);
                self.set_status("Can't name this device: Windows didn't report its Bluetooth address.", true);
                return;
            }
            let existing = st.names.get(&row.address).cloned();
            let model = existing
                .as_ref()
                .map(|n| n.model.clone())
                .filter(|m| !m.is_empty())
                .or_else(|| {
                    let own = existing.as_ref().map(|n| n.nickname != row.name).unwrap_or(true);
                    (own && !row.name.trim().is_empty()).then(|| row.name.clone())
                })
                .unwrap_or_default();
            let res = st.names.set(&row.address, &typed, &model);
            if let (Ok(Some(_)), Some(r)) = (&res, st.all.get_mut(&id)) {
                r.is_switch = true;
            }
            (res, row, model)
        };
        let (res, row, model) = outcome;
        match res {
            Err(e) => self.set_status(&format!("Couldn't save the nickname: {e}"), true),
            Ok(Some(nick)) => {
                self.ui.nick_input.set_text(&nick);
                self.log(&format!("Nickname for {} set to \"{nick}\".", row.address));
                if row.paired {
                    self.start_job(
                        id.clone(),
                        Cmd::Rename {
                            id,
                            address: row.address.clone(),
                            windows_name: nick.clone(),
                            message: format!("Saved! This controller is now \"{nick}\" here and in Windows' Bluetooth list."),
                        },
                    );
                } else {
                    self.set_status(&format!("Saved \"{nick}\". It's also applied in Windows once you connect it."), true);
                    self.rebuild_list();
                }
            }
            Ok(None) => {
                self.log(&format!("Nickname for {} removed.", row.address));
                if row.paired && !model.is_empty() {
                    self.start_job(
                        id.clone(),
                        Cmd::Rename {
                            id,
                            address: row.address.clone(),
                            windows_name: model.clone(),
                            message: format!("Nickname removed – back to \"{model}\"."),
                        },
                    );
                } else {
                    self.set_status("Nickname removed.", true);
                    self.rebuild_list();
                }
            }
        }
    }

    fn refresh_controls(&self) {
        let (busy, selected_paired, sticky, radio, ready, saved, connected, scan_error, has_sel) = {
            let Ok(st) = self.st.try_borrow() else { return };
            let sel = self.ui.list.selected_item().and_then(|i| st.order.get(i)).and_then(|id| st.all.get(id));
            let sw = || st.all.values().filter(|r| r.is_switch);
            let mut connected: Vec<String> = sw()
                .filter(|r| r.connected)
                .map(|r| st.nickname(r).unwrap_or_else(|| r.name.clone()))
                .collect();
            connected.sort();
            (
                st.busy.is_some(),
                sel.map(|r| r.paired).unwrap_or(false),
                st.sticky_until.map(|t| Instant::now() < t).unwrap_or(false),
                st.radio,
                sw().filter(|r| !r.paired).count(),
                sw().filter(|r| r.paired && !r.connected).count(),
                connected,
                st.scan_error.clone(),
                sel.is_some(),
            )
        };
        self.ui.nick_input.set_enabled(!busy && has_sel);
        self.ui.nick_save.set_enabled(!busy && has_sel);
        self.ui.connect.set_enabled(!busy);
        self.ui.rescan.set_enabled(!busy);
        self.ui.forget.set_enabled(!busy && selected_paired);
        self.ui.connect.set_text(if busy { "Working…" } else { "Connect" });

        if !busy && !sticky && radio != RadioInfo::Off {
            let hint = if scan_error.is_some() {
                "Bluetooth scanning isn't working. Make sure Bluetooth is on and an adapter is plugged in, then click Rescan.".to_string()
            } else if ready == 1 {
                "A controller is in pairing mode – click Connect.".to_string()
            } else if ready > 1 {
                format!("{ready} controllers are in pairing mode – select one and click Connect.")
            } else if !connected.is_empty() {
                format!("Connected: {}.  To add another, hold its SYNC button until the lights run.", connected.join(", "))
            } else if saved > 0 {
                "Found a saved controller. Hold its SYNC button until the lights run, then click Connect.".to_string()
            } else {
                "Scanning… put your controller in pairing mode and it will show up here.".to_string()
            };
            self.ui.status.set_text(&hint);
        }
    }

    fn start_job(&self, id: String, cmd: Cmd) {
        if let Ok(mut st) = self.st.try_borrow_mut() {
            st.busy = Some(id);
        }
        let _ = self.cmd.send(cmd);
        self.rebuild_list();
        self.refresh_controls();
    }

    fn connect_cmd(&self, id: &str) -> Cmd {
        let nickname = self
            .st
            .try_borrow()
            .ok()
            .and_then(|st| st.all.get(id).and_then(|r| st.nickname(r)));
        Cmd::Connect { id: id.to_string(), nickname }
    }

    fn is_busy(&self) -> bool {
        self.st.try_borrow().map(|s| s.busy.is_some()).unwrap_or(true)
    }

    fn on_connect(&self) {
        if self.is_busy() {
            return;
        }
        match self.selected_id() {
            Some(id) => {
                self.set_status("Connecting…", true);
                let cmd = self.connect_cmd(&id);
                self.start_job(id, cmd);
            }
            None => self.set_status(
                "No controller selected. Hold the SYNC button until the lights run, wait for it to appear in the list, then click Connect.",
                true,
            ),
        }
    }

    fn on_forget(&self) {
        if self.is_busy() {
            return;
        }
        if let Some(id) = self.selected_id() {
            self.start_job(id.clone(), Cmd::Forget(id));
        }
    }

    fn on_rescan(&self) {
        if self.is_busy() {
            return;
        }
        if let Ok(mut st) = self.st.try_borrow_mut() {
            st.sticky_until = None;
            st.auto_tried.clear();
        }
        let _ = self.cmd.send(Cmd::Rescan);
        self.refresh_controls();
    }

    /// When "Auto-connect" is ticked, pair any Switch controller that shows up in pairing mode.
    fn try_auto_connect(&self) {
        if self.ui.auto.check_state() != CheckBoxState::Checked || self.is_busy() {
            return;
        }
        let candidate = {
            let Ok(mut st) = self.st.try_borrow_mut() else { return };
            let now = Instant::now();
            let id = st
                .all
                .values()
                .filter(|r| r.is_switch && !r.paired && !r.connected)
                .map(|r| r.id.clone())
                .find(|id| st.auto_tried.get(id).map(|t| now.duration_since(*t) > Duration::from_secs(60)).unwrap_or(true));
            if let Some(id) = &id {
                st.auto_tried.insert(id.clone(), now);
            }
            id
        };
        if let Some(id) = candidate {
            self.log("Auto-connect: controller in pairing mode detected.");
            self.set_status("Controller in pairing mode detected – connecting…", true);
            let cmd = self.connect_cmd(&id);
            self.start_job(id, cmd);
        }
    }
}

fn main() {
    if nwg::init().is_err() {
        nwg::fatal_message("Switch Controller Connector", "Failed to initialise the Windows UI.");
    }
    let mut ui = Ui::default();
    if let Err(e) = build_ui(&mut ui) {
        nwg::fatal_message("Switch Controller Connector", &format!("Failed to build the window: {e}"));
    }

    let (etx, erx) = mpsc::channel::<Event>();
    let sender = ui.notice.sender();
    #[cfg(not(feature = "demo"))]
    let cmd = bt::spawn(bt::Tx::new(etx, Arc::new(move || sender.notice())));
    // Demo build (for screenshots): sample rows only, no Bluetooth worker.
    #[cfg(feature = "demo")]
    let cmd = {
        let _ = (etx, sender);
        let (c, r) = mpsc::channel::<Cmd>();
        std::mem::forget(r);
        c
    };

    let app = Rc::new(App {
        ui,
        st: RefCell::new(State {
            all: HashMap::new(),
            order: Vec::new(),
            busy: None,
            radio: RadioInfo::Unknown,
            sticky_until: None,
            scan_error: None,
            auto_tried: HashMap::new(),
            log: Vec::new(),
            names: Names::load(),
            nick_for: None,
        }),
        rx: erx,
        cmd,
        rebuilding: Cell::new(false),
    });

    app.log("Ready. Looking for Nintendo Switch controllers over Bluetooth…");
    #[cfg(feature = "demo")]
    {
        let mk = |n: &str, a: &str, p: bool, c: bool, r: Option<i32>| DeviceRow {
            id: format!("Bluetooth#Bluetooth00:00:00:00:00:00-{a}"),
            name: n.into(), address: a.into(), paired: p, connected: c, present: true, rssi: r, is_switch: bt::looks_like_switch(n),
        };
        if let Ok(mut st) = app.st.try_borrow_mut() {
            for r in [
                mk("Pro Controller", "98:B6:E9:12:34:56", false, false, Some(-52)),
                mk("Joy-Con (L)", "7C:BB:8A:AA:01:02", true, false, None),
                mk("Joy-Con (R)", "7C:BB:8A:AA:01:03", true, true, None),
            ] {
                st.all.insert(r.id.clone(), r);
            }
            let _ = st.names.set("7C:BB:8A:AA:01:02", "Red Joy-Con", "Joy-Con (L)");
            st.scan_error = None;
        }
        app.rebuild_list();
    }
    app.refresh_controls();
    app.ui.window.set_visible(true);

    let a = app.clone();
    let handler = nwg::full_bind_event_handler(&app.ui.window.handle, move |evt, _data, handle| {
        a.on_event(evt, handle);
    });
    nwg::dispatch_thread_events();
    nwg::unbind_event_handler(&handler);
}
