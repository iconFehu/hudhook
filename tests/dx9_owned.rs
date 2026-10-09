//! Explicit graphics smoke test for the resident DirectX 9 lifecycle.
#![cfg(feature = "dx9")]

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant};

use hudhook::hooks::dx9::ImguiDx9Hooks;
use hudhook::{imgui, Hudhook, ImguiRenderLoop, RenderContext};
use tracing_subscriber::prelude::*;
use windows::core::w;
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Direct3D9::{
    Direct3DCreate9, D3DADAPTER_DEFAULT, D3DCLEAR_TARGET, D3DCREATE_SOFTWARE_VERTEXPROCESSING,
    D3DDEVTYPE_HAL, D3DPRESENT_PARAMETERS, D3DSWAPEFFECT_DISCARD, D3D_SDK_VERSION,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetWindowLongPtrW,
    PeekMessageW, RegisterClassW, TranslateMessage, UnregisterClassW, GWLP_WNDPROC, PM_REMOVE,
    WINDOW_EX_STYLE, WNDCLASSW, WS_OVERLAPPEDWINDOW,
};

struct HiddenWindow(HWND, HINSTANCE);

impl HiddenWindow {
    fn new() -> Self {
        unsafe extern "system" fn wnd_proc(
            hwnd: HWND,
            msg: u32,
            wparam: WPARAM,
            lparam: LPARAM,
        ) -> LRESULT {
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        let instance = unsafe { GetModuleHandleW(None).unwrap().into() };
        let class = WNDCLASSW {
            hInstance: instance,
            lpfnWndProc: Some(wnd_proc),
            lpszClassName: w!("HUDHOOK_OWNED_DX9_TEST"),
            ..Default::default()
        };
        assert_ne!(unsafe { RegisterClassW(&class) }, 0);
        let hwnd = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                class.lpszClassName,
                w!("Owned lifecycle test"),
                WS_OVERLAPPEDWINDOW,
                0,
                0,
                320,
                240,
                None,
                None,
                Some(instance),
                None,
            )
        }
        .unwrap();
        Self(hwnd, instance)
    }
}

impl Drop for HiddenWindow {
    fn drop(&mut self) {
        unsafe {
            DestroyWindow(self.0).unwrap();
            UnregisterClassW(w!("HUDHOOK_OWNED_DX9_TEST"), Some(self.1)).unwrap();
        }
    }
}

struct ErrorCounter(Arc<AtomicUsize>);

impl<S: hudhook::tracing::Subscriber> tracing_subscriber::Layer<S> for ErrorCounter {
    fn on_event(
        &self,
        event: &hudhook::tracing::Event<'_>,
        _: tracing_subscriber::layer::Context<'_, S>,
    ) {
        if *event.metadata().level() == hudhook::tracing::Level::ERROR {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }
}

struct CounterLoop {
    initialized: Arc<AtomicUsize>,
    frames: Arc<AtomicUsize>,
    dropped: Arc<AtomicUsize>,
}

impl ImguiRenderLoop for CounterLoop {
    fn initialize<'a>(&'a mut self, _: &mut imgui::Context, _: &'a mut dyn RenderContext) {
        self.initialized.fetch_add(1, Ordering::Relaxed);
    }

    fn render(&mut self, ui: &mut imgui::Ui) {
        ui.text("Owned lifecycle resume smoke test");
        self.frames.fetch_add(1, Ordering::Relaxed);
    }
}

impl Drop for CounterLoop {
    fn drop(&mut self) {
        self.dropped.fetch_add(1, Ordering::Relaxed);
    }
}

struct RenderThread {
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl Drop for RenderThread {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn wait_until(description: &str, condition: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !condition() {
        assert!(Instant::now() < deadline, "Timed out waiting for {description}");
        thread::sleep(Duration::from_millis(5));
    }
}

fn stop_owned(hooks: &mut Hudhook) {
    hooks.disable().unwrap();
    assert!(hooks.wait_for_idle(Duration::from_secs(2)));
    hooks.detach_window_procedures().unwrap();
    assert!(hooks.wait_for_idle(Duration::from_secs(2)));
    hooks.release_owned_renderers().unwrap();
}

#[test]
#[ignore = "requires a Windows DirectX 9 graphics device; run explicitly"]
fn owned_dx9_can_stop_and_resume_three_times() {
    let errors = Arc::new(AtomicUsize::new(0));
    tracing_subscriber::registry()
        .with(ErrorCounter(Arc::clone(&errors)))
        .with(
            tracing_subscriber::fmt::layer()
                .with_filter(tracing_subscriber::filter::LevelFilter::ERROR),
        )
        .init();

    let stop = Arc::new(AtomicBool::new(false));
    let presents = Arc::new(AtomicUsize::new(0));
    let (ready_tx, ready_rx) = mpsc::channel();
    let stop_for_thread = Arc::clone(&stop);
    let presents_for_thread = Arc::clone(&presents);
    let thread = thread::spawn(move || {
        // Its device and message pump both
        // stay on this thread, matching a game's normal render loop.
        let window = HiddenWindow::new();
        let hwnd = window.0;
        let direct3d = unsafe { Direct3DCreate9(D3D_SDK_VERSION) }.unwrap();
        let mut device = None;
        unsafe {
            direct3d.CreateDevice(
                D3DADAPTER_DEFAULT,
                D3DDEVTYPE_HAL,
                hwnd,
                D3DCREATE_SOFTWARE_VERTEXPROCESSING as _,
                &mut D3DPRESENT_PARAMETERS {
                    Windowed: true.into(),
                    SwapEffect: D3DSWAPEFFECT_DISCARD,
                    ..Default::default()
                },
                &mut device,
            )
        }
        .unwrap();
        let device = device.unwrap();
        ready_tx.send(hwnd.0 as usize).unwrap();
        while !stop_for_thread.load(Ordering::Acquire) {
            unsafe {
                device
                    .Clear(0, std::ptr::null(), D3DCLEAR_TARGET as _, 0x0022cc22, 1.0, 0)
                    .unwrap();
                device.Present(std::ptr::null(), std::ptr::null(), hwnd, std::ptr::null()).unwrap();
                presents_for_thread.fetch_add(1, Ordering::Relaxed);
                let mut message = Default::default();
                while PeekMessageW(&mut message, Some(hwnd), 0, 0, PM_REMOVE).as_bool() {
                    let _ = TranslateMessage(&message);
                    DispatchMessageW(&message);
                }
            }
            thread::sleep(Duration::from_millis(5));
        }
    });
    let render_thread = RenderThread { stop, thread: Some(thread) };
    let hwnd = HWND(ready_rx.recv_timeout(Duration::from_secs(5)).unwrap() as _);
    let original_wnd_proc = unsafe { GetWindowLongPtrW(hwnd, GWLP_WNDPROC) };
    let initialized = Arc::new(AtomicUsize::new(0));
    let frames = Arc::new(AtomicUsize::new(0));
    let dropped = Arc::new(AtomicUsize::new(0));
    let mut hooks = Hudhook::builder()
        .with::<ImguiDx9Hooks>(CounterLoop {
            initialized: Arc::clone(&initialized),
            frames: Arc::clone(&frames),
            dropped: Arc::clone(&dropped),
        })
        .build();
    hooks.apply_owned().unwrap();
    wait_until("initial rendering", || frames.load(Ordering::Relaxed) >= 5);

    for cycle in 1..=3 {
        assert_eq!(initialized.load(Ordering::Relaxed), cycle);
        stop_owned(&mut hooks);
        assert_eq!(unsafe { GetWindowLongPtrW(hwnd, GWLP_WNDPROC) }, original_wnd_proc);
        assert_eq!(dropped.load(Ordering::Relaxed), 0);
        let stopped_frames = frames.load(Ordering::Relaxed);
        let stopped_presents = presents.load(Ordering::Relaxed);
        wait_until("unhooked application frames", || {
            presents.load(Ordering::Relaxed) >= stopped_presents + 5
        });
        assert_eq!(frames.load(Ordering::Relaxed), stopped_frames);
        hooks.resume_owned().unwrap();
        wait_until("resumed rendering", || frames.load(Ordering::Relaxed) >= stopped_frames + 5);
    }
    stop_owned(&mut hooks);
    assert_eq!(initialized.load(Ordering::Relaxed), 4);
    assert_eq!(dropped.load(Ordering::Relaxed), 0);
    drop(render_thread);
    assert_eq!(errors.load(Ordering::Relaxed), 0, "Unexpected hudhook error events");
}
