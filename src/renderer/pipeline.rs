use std::collections::HashMap;
use std::mem;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use imgui::Context;
use once_cell::sync::Lazy;
use parking_lot::Mutex;
use tracing::{error, warn};
use windows::core::{Error, Result, HRESULT};
use windows::Win32::Foundation::{
    GetLastError, SetLastError, HWND, LPARAM, LRESULT, WIN32_ERROR, WPARAM,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallWindowProcW, DefWindowProcW, GetWindowLongPtrW, IsWindow, SetWindowLongPtrW, GWLP_WNDPROC,
};

use crate::renderer::input::{imgui_wnd_proc_impl, WndProcType};
use crate::renderer::RenderEngine;
use crate::{util, ImguiRenderLoop, MessageFilter, HOOK_EJECTION_BARRIER};

type RenderLoop = Box<dyn ImguiRenderLoop + Send + Sync>;

// Safety: HWND is an opaque integer handle, safe to send/share across threads.
#[derive(Clone, Copy, Debug)]
#[repr(transparent)]
pub(crate) struct SendableHwnd(HWND);
unsafe impl Send for SendableHwnd {}
unsafe impl Sync for SendableHwnd {}

static PIPELINE_STATES: Lazy<Mutex<HashMap<usize, Arc<PipelineSharedState>>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

#[derive(Debug)]
pub(crate) struct PipelineMessage(
    pub(crate) SendableHwnd,
    pub(crate) u32,
    pub(crate) WPARAM,
    pub(crate) LPARAM,
);

pub(crate) struct PipelineSharedState {
    pub(crate) message_filter: AtomicU32,
    pub(crate) wnd_proc: WndProcType,
    pub(crate) tx: Sender<PipelineMessage>,
}

pub(crate) struct Pipeline<T: RenderEngine> {
    hwnd: HWND,
    ctx: Context,
    engine: T,
    render_loop: RenderLoop,
    rx: Receiver<PipelineMessage>,
    shared_state: Arc<PipelineSharedState>,
    queue_buffer: Vec<PipelineMessage>,
    last_frame: Option<Instant>,
    window_proc_attached: bool,
}

impl<T: RenderEngine> Pipeline<T> {
    pub(crate) fn new(
        hwnd: HWND,
        mut ctx: Context,
        mut engine: T,
        mut render_loop: RenderLoop,
    ) -> std::result::Result<Self, (Error, RenderLoop)> {
        let (width, height) = util::win_size(hwnd);

        ctx.io_mut().display_size = [width as f32, height as f32];

        render_loop.initialize(&mut ctx, &mut engine);

        if let Err(e) = engine.setup_fonts(&mut ctx) {
            return Err((e, render_loop));
        }

        let wnd_proc = unsafe {
            #[cfg(target_arch = "x86")]
            type SwlpRet = i32;
            #[cfg(target_arch = "x86_64")]
            type SwlpRet = isize;

            mem::transmute::<SwlpRet, WndProcType>(SetWindowLongPtrW(
                hwnd,
                GWLP_WNDPROC,
                pipeline_wnd_proc as *const () as _,
            ))
        };

        let (tx, rx) = mpsc::channel();
        let shared_state = Arc::new(PipelineSharedState {
            message_filter: AtomicU32::new(MessageFilter::empty().bits()),
            wnd_proc,
            tx,
        });

        PIPELINE_STATES.lock().insert(hwnd.0 as usize, Arc::clone(&shared_state));

        Ok(Self {
            hwnd,
            ctx,
            engine,
            render_loop,
            rx,
            shared_state: Arc::clone(&shared_state),
            queue_buffer: Vec::new(),
            last_frame: None,
            window_proc_attached: true,
        })
    }

    pub(crate) fn prepare_render(&mut self) -> Result<()> {
        let mut queue_buffer = mem::take(&mut self.queue_buffer);
        queue_buffer.extend(self.rx.try_iter());
        queue_buffer.drain(..).for_each(
            |PipelineMessage(SendableHwnd(hwnd), umsg, wparam, lparam)| {
                imgui_wnd_proc_impl(hwnd, umsg, wparam, lparam, self);
            },
        );
        self.queue_buffer = queue_buffer;

        let message_filter = self.render_loop.message_filter(self.ctx.io());

        self.shared_state.message_filter.store(message_filter.bits(), Ordering::SeqCst);

        let io = self.ctx.io_mut();

        io.nav_active = true;
        io.nav_visible = true;

        self.render_loop.before_render(&mut self.ctx, &mut self.engine);

        Ok(())
    }

    pub(crate) fn render(&mut self, render_target: T::RenderTarget) -> Result<()> {
        let now = Instant::now();
        let delta_time = self.last_frame.map_or(Duration::ZERO, |last| now - last);
        self.last_frame = Some(now);

        self.ctx.io_mut().update_delta_time(delta_time);

        let [w, h] = self.ctx.io().display_size;
        let [fsw, fsh] = self.ctx.io().display_framebuffer_scale;

        if (w * fsw) <= 0.0 || (h * fsh) <= 0.0 {
            warn!(
                "Insufficient display size: {w}x{h}, framebuffer_scale: {fsw}x{fsh}; skipping \
                 frame"
            );
            return Ok(());
        }

        let ui = self.ctx.frame();
        self.render_loop.render(ui);
        let draw_data = self.ctx.render();

        self.engine.update_textures(draw_data)?;
        self.engine.render(draw_data, render_target)?;

        Ok(())
    }

    pub(crate) fn context(&mut self) -> &mut Context {
        &mut self.ctx
    }

    pub(crate) fn render_loop(&mut self) -> &mut RenderLoop {
        &mut self.render_loop
    }

    pub(crate) fn resize(&mut self, width: u32, height: u32) {
        if width > 0 && height > 0 {
            self.ctx.io_mut().display_size = [width as f32, height as f32];
        }
    }

    pub(crate) fn update_display_size_from_swap_chain(&mut self, width: u32, height: u32) {
        self.resize(width, height);
    }

    pub(crate) fn wait_idle(&mut self) -> Result<()> {
        self.engine.wait_idle()
    }

    pub(crate) fn detach_window_procedure(&mut self) -> Result<()> {
        if !self.window_proc_attached {
            return Ok(());
        }
        restore_window_procedure(self.hwnd, self.shared_state.wnd_proc)?;
        self.window_proc_attached = false;
        Ok(())
    }

    pub(crate) fn cleanup(&mut self) {
        if let Err(e) = self.detach_window_procedure() {
            error!("Could not detach renderer window procedure: {e:?}");
            return;
        }
        PIPELINE_STATES.lock().remove(&(self.hwnd.0 as usize));
    }

    pub(crate) fn take(mut self) -> RenderLoop {
        self.cleanup();
        self.render_loop
    }

    pub(crate) fn take_resident(mut self) -> std::result::Result<RenderLoop, (Error, Self)> {
        if let Err(error) = self.detach_window_procedure() {
            return Err((error, self));
        }
        self.shared_state.message_filter.store(MessageFilter::empty().bits(), Ordering::SeqCst);
        // Preserve PIPELINE_STATES so a late window callback can still find
        // the original procedure after renderer resources have been released.
        Ok(self.render_loop)
    }
}

fn restore_window_procedure(hwnd: HWND, original: WndProcType) -> Result<()> {
    unsafe {
        if !IsWindow(Some(hwnd)).as_bool() {
            return Ok(());
        }
        let current = GetWindowLongPtrW(hwnd, GWLP_WNDPROC) as usize;
        if current == original as usize {
            return Ok(());
        }
        if current != pipeline_wnd_proc as *const () as usize {
            // A later subclass may still call through us. Overwriting it
            // would break its chain and would not make DLL unloading safe.
            return Err(Error::from_hresult(HRESULT(0x80004005u32 as i32)));
        }
        SetLastError(WIN32_ERROR(0));
        let previous = SetWindowLongPtrW(hwnd, GWLP_WNDPROC, original as usize as _);
        if previous == 0 && GetLastError().0 != 0 {
            return Err(Error::from_thread());
        }
        if previous as usize != pipeline_wnd_proc as *const () as usize {
            // Another subclass arrived between the read and restore.
            // Put its procedure back and retain our forwarding state.
            SetWindowLongPtrW(hwnd, GWLP_WNDPROC, previous);
            return Err(Error::from_hresult(HRESULT(0x80004005u32 as i32)));
        }
        Ok(())
    }
}

unsafe extern "system" fn pipeline_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let _guard = HOOK_EJECTION_BARRIER.acquire_ejection_guard();
    let shared_state = {
        let Some(shared_state_guard) = PIPELINE_STATES.try_lock() else {
            error!("Could not lock shared state in window procedure");
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        };

        let Some(shared_state) = shared_state_guard.get(&(hwnd.0 as usize)) else {
            error!("Could not get shared state for handle {hwnd:?}");
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        };

        Arc::clone(shared_state)
    };

    if let Err(e) = shared_state.tx.send(PipelineMessage(SendableHwnd(hwnd), msg, wparam, lparam)) {
        error!("Could not send window message through pipeline: {e:?}");
    }

    // CONCURRENCY: as the message interpretation now happens out of band, this
    // expresses the intent as of *before* the current message was received.
    let message_filter =
        MessageFilter::from_bits_retain(shared_state.message_filter.load(Ordering::SeqCst));

    if message_filter.is_blocking(msg) {
        LRESULT(1)
    } else {
        CallWindowProcW(Some(shared_state.wnd_proc), hwnd, msg, wparam, lparam)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::DummyHwnd;

    #[test]
    fn window_procedure_detach_preserves_later_subclasses() {
        unsafe extern "system" fn later_subclass(
            hwnd: HWND,
            msg: u32,
            wparam: WPARAM,
            lparam: LPARAM,
        ) -> LRESULT {
            if msg == 0x8001 {
                return LRESULT(37);
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }

        let window = DummyHwnd::new();
        let hwnd = window.hwnd();
        assert!(unsafe { IsWindow(Some(hwnd)) }.as_bool());
        let original: WndProcType =
            unsafe { mem::transmute(GetWindowLongPtrW(hwnd, GWLP_WNDPROC)) };

        unsafe {
            SetWindowLongPtrW(hwnd, GWLP_WNDPROC, pipeline_wnd_proc as *const () as usize as _)
        };
        assert!(restore_window_procedure(hwnd, original).is_ok());
        assert_eq!(unsafe { GetWindowLongPtrW(hwnd, GWLP_WNDPROC) } as usize, original as usize);
        assert!(restore_window_procedure(hwnd, original).is_ok());

        unsafe { SetWindowLongPtrW(hwnd, GWLP_WNDPROC, later_subclass as *const () as usize as _) };
        assert!(restore_window_procedure(hwnd, original).is_err());
        assert_eq!(
            unsafe { GetWindowLongPtrW(hwnd, GWLP_WNDPROC) } as usize,
            later_subclass as *const () as usize,
        );
        unsafe { SetWindowLongPtrW(hwnd, GWLP_WNDPROC, original as usize as _) };
    }
}
