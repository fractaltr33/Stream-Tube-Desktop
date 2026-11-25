//! Native child HWND host for zero-copy `libmpv` Direct3D11 video output on Windows.
//!
//! When GPUI's `DirectComposition` swapchain is created with `topmost = true`,
//! it composites above child HWNDs. Painting a transparent "hole" quad in GPUI
//! lets this child window's hardware-decoded `d3d11` flip-model swapchain show
//! through underneath GPUI controls, rounded corners, and overlays, eliminating
//! CPU pixel readback and per-frame full-window GPUI redraws.

#[cfg(windows)]
mod imp {
    use parking_lot::Mutex;
    use std::ffi::c_void;
    use std::ptr::null_mut;
    use std::sync::Once;

    const WS_CHILD: u32 = 0x4000_0000;
    const WS_CLIPSIBLINGS: u32 = 0x0400_0000;
    const WS_DISABLED: u32 = 0x0800_0000;

    const SWP_NOSIZE: u32 = 0x0001;
    const SWP_NOMOVE: u32 = 0x0002;
    const SWP_NOZORDER: u32 = 0x0004;
    const SWP_NOACTIVATE: u32 = 0x0010;
    const SWP_FRAMECHANGED: u32 = 0x0020;
    const SWP_SHOWWINDOW: u32 = 0x0040;
    const SWP_HIDEWINDOW: u32 = 0x0080;
    const SWP_NOOWNERZORDER: u32 = 0x0200;

    const WM_MOUSEACTIVATE: u32 = 0x0021;
    const WM_NCHITTEST: u32 = 0x0084;
    const WM_APP_DESTROY: u32 = 0x8001;
    const HTTRANSPARENT: isize = -1;
    const MA_NOACTIVATE: isize = 3;
    const BLACK_BRUSH: i32 = 4;

    #[repr(C)]
    struct WndClassExW {
        cb_size: u32,
        style: u32,
        lpfn_wnd_proc: Option<unsafe extern "system" fn(isize, u32, usize, isize) -> isize>,
        cb_cls_extra: i32,
        cb_wnd_extra: i32,
        h_instance: isize,
        h_icon: isize,
        h_cursor: isize,
        hbr_background: isize,
        lpsz_menu_name: *const u16,
        lpsz_class_name: *const u16,
        h_icon_sm: isize,
    }

    #[link(name = "user32")]
    unsafe extern "system" {
        fn RegisterClassExW(lpwcx: *const WndClassExW) -> u16;
        fn CreateWindowExW(
            dw_ex_style: u32,
            lp_class_name: *const u16,
            lp_window_name: *const u16,
            dw_style: u32,
            x: i32,
            y: i32,
            n_width: i32,
            n_height: i32,
            h_wnd_parent: isize,
            h_menu: isize,
            h_instance: isize,
            lp_param: *mut c_void,
        ) -> isize;
        fn DestroyWindow(hwnd: isize) -> i32;
        fn DefWindowProcW(hwnd: isize, msg: u32, wparam: usize, lparam: isize) -> isize;
        fn SetWindowPos(
            hwnd: isize,
            hwnd_insert_after: isize,
            x: i32,
            y: i32,
            cx: i32,
            cy: i32,
            flags: u32,
        ) -> i32;
        fn SetParent(hwnd_child: isize, hwnd_new_parent: isize) -> isize;
        fn PostMessageW(hwnd: isize, msg: u32, wparam: usize, lparam: isize) -> i32;
        fn GetWindowThreadProcessId(hwnd: isize, process_id: *mut u32) -> u32;
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetModuleHandleW(lp_module_name: *const u16) -> isize;
        fn GetCurrentThreadId() -> u32;
    }

    #[link(name = "gdi32")]
    unsafe extern "system" {
        fn GetStockObject(i: i32) -> isize;
    }

    unsafe extern "system" fn wnd_proc(hwnd: isize, msg: u32, wparam: usize, lparam: isize) -> isize {
        match msg {
            WM_NCHITTEST => HTTRANSPARENT,
            WM_MOUSEACTIVATE => MA_NOACTIVATE,
            WM_APP_DESTROY => {
                unsafe { DestroyWindow(hwnd) };
                0
            }
            _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
        }
    }

    static REGISTER_CLASS: Once = Once::new();
    const CLASS_NAME: &[u16] = &[
        b'S' as u16, b't' as u16, b'r' as u16, b'e' as u16, b'a' as u16, b'm' as u16,
        b'T' as u16, b'u' as u16, b'b' as u16, b'e' as u16, b'V' as u16, b'i' as u16,
        b'd' as u16, b'e' as u16, b'o' as u16, b'H' as u16, b'o' as u16, b's' as u16,
        b't' as u16, 0,
    ];

    fn ensure_class_registered() -> isize {
        let h_instance = unsafe { GetModuleHandleW(null_mut()) };
        REGISTER_CLASS.call_once(|| {
            let wc = WndClassExW {
                cb_size: std::mem::size_of::<WndClassExW>() as u32,
                style: 0,
                lpfn_wnd_proc: Some(wnd_proc),
                cb_cls_extra: 0,
                cb_wnd_extra: 0,
                h_instance,
                h_icon: 0,
                h_cursor: 0,
                hbr_background: unsafe { GetStockObject(BLACK_BRUSH) },
                lpsz_menu_name: null_mut(),
                lpsz_class_name: CLASS_NAME.as_ptr(),
                h_icon_sm: 0,
            };
            unsafe { RegisterClassExW(&wc) };
        });
        h_instance
    }

    pub fn is_available() -> bool {
        !std::env::var("GPUI_DISABLE_DIRECT_COMPOSITION")
            .is_ok_and(|value| value == "true" || value == "1")
    }

    pub fn hwnd_from_window(window: &gpui_kit::Window) -> Option<isize> {
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        let handle = HasWindowHandle::window_handle(window).ok()?;
        match handle.as_raw() {
            RawWindowHandle::Win32(w) => Some(w.hwnd.get()),
            _ => None,
        }
    }

    struct HostState {
        parent: isize,
        visible: bool,
        rect: (i32, i32, i32, i32),
    }

    pub struct VideoHost {
        hwnd: isize,
        state: Mutex<HostState>,
    }

    impl VideoHost {
        pub fn new(parent: isize) -> anyhow::Result<Self> {
            anyhow::ensure!(parent != 0, "invalid parent HWND");
            let h_instance = ensure_class_registered();
            let hwnd = unsafe {
                CreateWindowExW(
                    0,
                    CLASS_NAME.as_ptr(),
                    CLASS_NAME.as_ptr(),
                    WS_CHILD | WS_CLIPSIBLINGS | WS_DISABLED,
                    0,
                    0,
                    16,
                    16,
                    parent,
                    0,
                    h_instance,
                    null_mut(),
                )
            };
            anyhow::ensure!(hwnd != 0, "CreateWindowExW failed for VideoHost");
            Ok(Self {
                hwnd,
                state: Mutex::new(HostState {
                    parent,
                    visible: false,
                    rect: (0, 0, 16, 16),
                }),
            })
        }

        pub fn hwnd(&self) -> isize {
            self.hwnd
        }

        pub fn sync(&self, parent: isize, x: i32, y: i32, w: i32, h: i32, visible: bool) {
            if parent == 0 {
                self.hide();
                return;
            }
            let mut s = self.state.lock();
            let rect = (x, y, w.max(1), h.max(1));
            let mut flags = SWP_NOACTIVATE | SWP_NOZORDER | SWP_NOOWNERZORDER;
            let parent_changed = s.parent != parent;
            if parent_changed {
                unsafe { SetParent(self.hwnd, parent) };
                s.parent = parent;
                flags |= SWP_FRAMECHANGED;
            }
            if !parent_changed && s.visible == visible && s.rect == rect {
                return;
            }
            if visible {
                flags |= SWP_SHOWWINDOW;
            } else {
                flags |= SWP_HIDEWINDOW;
            }
            unsafe {
                SetWindowPos(self.hwnd, 0, rect.0, rect.1, rect.2, rect.3, flags);
            }
            s.rect = rect;
            s.visible = visible;
        }

        pub fn hide(&self) {
            let mut s = self.state.lock();
            if !s.visible {
                return;
            }
            unsafe {
                SetWindowPos(
                    self.hwnd,
                    0,
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOOWNERZORDER | SWP_HIDEWINDOW,
                );
            }
            s.visible = false;
        }

        /// Moves the child host back to `main_parent` before a temporary host (like PiP) closes.
        pub fn detach_to_main(&self, main_parent: isize) {
            let mut s = self.state.lock();
            if main_parent != 0 && s.parent != main_parent {
                unsafe {
                    SetParent(self.hwnd, main_parent);
                    SetWindowPos(
                        self.hwnd,
                        0,
                        0,
                        0,
                        0,
                        0,
                        SWP_NOMOVE
                            | SWP_NOSIZE
                            | SWP_NOZORDER
                            | SWP_NOACTIVATE
                            | SWP_NOOWNERZORDER
                            | SWP_FRAMECHANGED
                            | SWP_HIDEWINDOW,
                    );
                }
                s.parent = main_parent;
                s.visible = false;
            } else if s.visible {
                unsafe {
                    SetWindowPos(
                        self.hwnd,
                        0,
                        0,
                        0,
                        0,
                        0,
                        SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOOWNERZORDER | SWP_HIDEWINDOW,
                    );
                }
                s.visible = false;
            }
        }
    }

    impl Drop for VideoHost {
        fn drop(&mut self) {
            if self.hwnd == 0 {
                return;
            }
            unsafe {
                let owner_tid = GetWindowThreadProcessId(self.hwnd, null_mut());
                if owner_tid == GetCurrentThreadId() {
                    DestroyWindow(self.hwnd);
                } else {
                    PostMessageW(self.hwnd, WM_APP_DESTROY, 0, 0);
                }
            }
        }
    }
}

#[cfg(not(windows))]
mod imp {
    pub fn is_available() -> bool {
        false
    }

    pub fn hwnd_from_window(_window: &gpui_kit::Window) -> Option<isize> {
        None
    }

    pub struct VideoHost;

    impl VideoHost {
        pub fn new(_parent: isize) -> anyhow::Result<Self> {
            anyhow::bail!("native VideoHost is only supported on Windows")
        }

        pub fn hwnd(&self) -> isize {
            0
        }

        pub fn sync(&self, _parent: isize, _x: i32, _y: i32, _w: i32, _h: i32, _visible: bool) {}

        pub fn hide(&self) {}

        pub fn detach_to_main(&self, _main_parent: isize) {}
    }
}

pub use imp::{VideoHost, hwnd_from_window, is_available};

/// Matches `VIDEO_HOLE_SENTINEL` in `vendor/gpui-pre-windows/src/directx_renderer.rs`.
pub const VIDEO_HOLE_COLOR: gpui_kit::Hsla = gpui_kit::Hsla {
    h: 0.37123,
    s: 0.77123,
    l: 0.01123,
    a: 1.0,
};
