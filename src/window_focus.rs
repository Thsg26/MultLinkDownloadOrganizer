//! Integração de foreground/owner no Windows, sem topmost permanente.
#[cfg(windows)]
mod win {
    #[link(name = "user32")]
    unsafe extern "system" {
        fn EnumWindows(
            callback: Option<unsafe extern "system" fn(isize, isize) -> i32>,
            parameter: isize,
        ) -> i32;
        fn GetWindowThreadProcessId(window: isize, process: *mut u32) -> u32;
        fn GetWindowTextLengthW(window: isize) -> i32;
        fn IsWindowVisible(window: isize) -> i32;
        fn IsWindow(window: isize) -> i32;
        fn SetWindowLongPtrW(window: isize, index: i32, value: isize) -> isize;
        fn AllowSetForegroundWindow(process: u32) -> i32;
        fn ShowWindow(window: isize, command: i32) -> i32;
        fn BringWindowToTop(window: isize) -> i32;
        fn SetForegroundWindow(window: isize) -> i32;
    }

    pub fn application_window() -> Option<isize> {
        struct Search {
            pid: u32,
            window: isize,
        }
        unsafe extern "system" fn visit(window: isize, parameter: isize) -> i32 {
            // EnumWindows é síncrono; Search permanece válido durante a enumeração.
            let search = unsafe { &mut *(parameter as *mut Search) };
            let mut pid = 0;
            unsafe {
                GetWindowThreadProcessId(window, &mut pid);
            }
            if pid == search.pid
                && unsafe { IsWindowVisible(window) != 0 && GetWindowTextLengthW(window) > 0 }
            {
                search.window = window;
                return 0;
            }
            1
        }
        let mut search = Search {
            pid: std::process::id(),
            window: 0,
        };
        unsafe {
            EnumWindows(Some(visit), &mut search as *mut Search as isize);
        }
        (search.window != 0).then_some(search.window)
    }

    pub fn grant(process: u32) {
        unsafe {
            AllowSetForegroundWindow(process);
        }
    }

    pub fn set_owner(window: isize, owner: isize) {
        if owner != 0 && unsafe { IsWindow(owner) != 0 } {
            // GWLP_HWNDPARENT: relação de owner, não uma janela filha embutida.
            unsafe {
                SetWindowLongPtrW(window, -8, owner);
            }
        }
    }

    pub fn bring_forward(window: isize) {
        unsafe {
            ShowWindow(window, 9); // SW_RESTORE
            BringWindowToTop(window);
            SetForegroundWindow(window);
        }
    }
}

#[cfg(windows)]
pub use win::*;

#[cfg(not(windows))]
pub fn application_window() -> Option<isize> {
    None
}
#[cfg(not(windows))]
pub fn grant(_: u32) {}
