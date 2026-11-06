//! System tray icon. Implemented on Windows; on other platforms every function is a no-op
//! and `spawn` returns false (the window then simply behaves like a normal window).
//!
//! The tray lives on its own thread with a hidden window. Left click (or "Open MKShare")
//! brings the main window back; "Quit" closes the app. The tooltip shows the connection status.

#[cfg(windows)]
mod imp {
    use std::{
        sync::{
            atomic::{AtomicBool, AtomicIsize, AtomicU32, Ordering},
            Mutex,
        },
        thread,
        time::Duration,
    };
    use windows_sys::Win32::{
        Foundation::{HWND, LPARAM, LRESULT, WPARAM},
        System::LibraryLoader::GetModuleHandleW,
        UI::{
            Shell::{Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW},
            WindowsAndMessaging::*,
        },
    };

    const WM_TRAY: u32 = WM_APP + 1;
    const ID_OPEN: usize = 1;
    const ID_QUIT: usize = 2;

    static TRAY_HWND: AtomicIsize = AtomicIsize::new(0);
    static QUITTING: AtomicBool = AtomicBool::new(false);
    static TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);
    static CTX: Mutex<Option<eframe::egui::Context>> = Mutex::new(None);
    static TIP: Mutex<String> = Mutex::new(String::new());

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(Some(0)).collect()
    }

    fn icon_data(hwnd: HWND) -> NOTIFYICONDATAW {
        let mut nid: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
        nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = hwnd;
        nid.uID = 1;
        nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
        nid.uCallbackMessage = WM_TRAY;
        // The icon is embedded in the exe by build.rs (resource id 1).
        nid.hIcon = unsafe { LoadIconW(GetModuleHandleW(std::ptr::null()), 1 as *const u16) };
        let tip = TIP.lock().map(|t| t.clone()).unwrap_or_default();
        let tip: Vec<u16> = tip.encode_utf16().take(127).collect();
        nid.szTip[..tip.len()].copy_from_slice(&tip);
        nid
    }

    fn add_icon(hwnd: HWND) {
        let nid = icon_data(hwnd);
        unsafe { Shell_NotifyIconW(NIM_ADD, &nid) };
    }

    /// Brings the main window back (it is found by its title).
    pub fn show_window() {
        unsafe {
            let h = FindWindowW(std::ptr::null(), wide("MKShare").as_ptr());
            if !h.is_null() {
                ShowWindow(h, SW_SHOW);
                ShowWindow(h, SW_RESTORE);
                SetForegroundWindow(h);
            }
        }
        // also tell the UI library, so it knows the window is visible again and keeps drawing it
        if let Some(ctx) = CTX.lock().unwrap().as_ref() {
            ctx.send_viewport_cmd(eframe::egui::ViewportCommand::Visible(true));
            ctx.send_viewport_cmd(eframe::egui::ViewportCommand::Minimized(false));
            ctx.send_viewport_cmd(eframe::egui::ViewportCommand::Focus);
            ctx.request_repaint();
        }
    }

    fn quit() {
        QUITTING.store(true, Ordering::SeqCst);
        unsafe {
            let h = FindWindowW(std::ptr::null(), wide("MKShare").as_ptr());
            if !h.is_null() {
                PostMessageW(h, WM_CLOSE, 0, 0);
            }
        }
        // A window that is hidden may not process the close request: make sure the app ends anyway
        // (the sharing process ends by itself when this one is gone).
        thread::spawn(|| {
            thread::sleep(Duration::from_millis(2500));
            remove();
            std::process::exit(0);
        });
    }

    unsafe fn context_menu(hwnd: HWND) {
        let menu = CreatePopupMenu();
        AppendMenuW(menu, MF_STRING, ID_OPEN, wide("Open MKShare").as_ptr());
        AppendMenuW(menu, MF_SEPARATOR, 0, std::ptr::null());
        AppendMenuW(menu, MF_STRING, ID_QUIT, wide("Quit").as_ptr());
        let mut pt = std::mem::zeroed();
        GetCursorPos(&mut pt);
        SetForegroundWindow(hwnd); // required so the menu closes when clicking elsewhere
        let cmd = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_BOTTOMALIGN, pt.x, pt.y, 0, hwnd, std::ptr::null());
        PostMessageW(hwnd, WM_NULL, 0, 0);
        DestroyMenu(menu);
        match cmd as usize {
            ID_OPEN => show_window(),
            ID_QUIT => quit(),
            _ => {}
        }
    }

    unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        if msg == WM_TRAY {
            match (lparam as u32) & 0xFFFF {
                WM_LBUTTONUP | WM_LBUTTONDBLCLK => show_window(),
                WM_RBUTTONUP | WM_CONTEXTMENU => context_menu(hwnd),
                _ => {}
            }
            return 0;
        }
        let tb = TASKBAR_CREATED.load(Ordering::SeqCst);
        if tb != 0 && msg == tb {
            add_icon(hwnd); // Explorer restarted: put the icon back
            return 0;
        }
        DefWindowProcW(hwnd, msg, wparam, lparam)
    }

    /// Starts the tray icon. Returns true once it is on screen.
    pub fn spawn(ctx: eframe::egui::Context) -> bool {
        *CTX.lock().unwrap() = Some(ctx);
        *TIP.lock().unwrap() = "MKShare".into();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<bool>();
        thread::spawn(move || unsafe {
            let hinst = GetModuleHandleW(std::ptr::null());
            let class = wide("MKShareTray");
            let mut wc: WNDCLASSW = std::mem::zeroed();
            wc.lpfnWndProc = Some(wndproc);
            wc.hInstance = hinst;
            wc.lpszClassName = class.as_ptr();
            RegisterClassW(&wc);
            TASKBAR_CREATED.store(RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()), Ordering::SeqCst);
            // a normal (never shown) window, so that it also receives Explorer's broadcast messages
            let hwnd = CreateWindowExW(
                0,
                class.as_ptr(),
                wide("MKShare tray").as_ptr(),
                WS_OVERLAPPED,
                0,
                0,
                0,
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                hinst,
                std::ptr::null(),
            );
            if hwnd.is_null() {
                let _ = ready_tx.send(false);
                return;
            }
            TRAY_HWND.store(hwnd as isize, Ordering::SeqCst);
            add_icon(hwnd);
            let _ = ready_tx.send(true);
            let mut msg: MSG = std::mem::zeroed();
            while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        });
        ready_rx.recv().unwrap_or(false)
    }

    /// Updates the text shown when hovering over the tray icon.
    pub fn set_tooltip(text: &str) {
        *TIP.lock().unwrap() = text.to_string();
        let h = TRAY_HWND.load(Ordering::SeqCst);
        if h != 0 {
            let nid = icon_data(h as HWND);
            unsafe { Shell_NotifyIconW(NIM_MODIFY, &nid) };
        }
    }

    /// Takes the icon off the tray (call before the app exits).
    pub fn remove() {
        let h = TRAY_HWND.load(Ordering::SeqCst);
        if h != 0 {
            let nid = icon_data(h as HWND);
            unsafe { Shell_NotifyIconW(NIM_DELETE, &nid) };
        }
    }

    /// True once "Quit" was chosen in the tray menu: the window must really close then.
    pub fn is_quitting() -> bool {
        QUITTING.load(Ordering::SeqCst)
    }

    pub const AVAILABLE: bool = true;
}

#[cfg(not(windows))]
mod imp {
    pub fn spawn(_ctx: eframe::egui::Context) -> bool {
        false
    }
    pub fn set_tooltip(_text: &str) {}
    pub fn remove() {}
    pub fn is_quitting() -> bool {
        false
    }
    pub const AVAILABLE: bool = false;
}

pub use imp::*;
