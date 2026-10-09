//! Hooks for OpenGL 3.

use std::ffi::{c_void, CString};
use std::mem;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

use imgui::Context;
use once_cell::sync::OnceCell;
use parking_lot::Mutex;
use tracing::{error, trace};
use windows::core::{Error, Result, HRESULT, PCSTR};
use windows::Win32::Graphics::Gdi::{WindowFromDC, HDC};
use windows::Win32::System::LibraryLoader::{GetModuleHandleA, GetProcAddress};

use crate::mh::MhHook;
use crate::renderer::{OpenGl3RenderEngine, Pipeline};
use crate::{perform_eject, Hooks, ImguiRenderLoop, EJECT_REQUESTED, HOOK_EJECTION_BARRIER};

type OpenGl32wglSwapBuffersType = unsafe extern "system" fn(HDC) -> ();

struct Trampolines {
    opengl32_wgl_swap_buffers: OpenGl32wglSwapBuffersType,
}

static mut TRAMPOLINES: OnceLock<Trampolines> = OnceLock::new();
static mut PIPELINE: OnceCell<Mutex<Pipeline<OpenGl3RenderEngine>>> = OnceCell::new();
static mut RENDER_LOOP: OnceCell<Box<dyn ImguiRenderLoop + Send + Sync>> = OnceCell::new();
static STOPPING: AtomicBool = AtomicBool::new(false);
static OWNED_LIFECYCLE: AtomicBool = AtomicBool::new(false);

unsafe fn init_pipeline(dc: HDC) -> Result<Mutex<Pipeline<OpenGl3RenderEngine>>> {
    let hwnd = WindowFromDC(dc);

    let mut ctx = Context::create();
    let engine = OpenGl3RenderEngine::new(&mut ctx)?;

    let Some(render_loop) = RENDER_LOOP.take() else {
        error!("Render loop not yet initialized");
        return Err(Error::from_hresult(HRESULT(-1)));
    };

    let pipeline = Pipeline::new(hwnd, ctx, engine, render_loop).map_err(|(e, render_loop)| {
        RENDER_LOOP.get_or_init(move || render_loop);
        e
    })?;

    Ok(Mutex::new(pipeline))
}

fn render(dc: HDC) -> Result<()> {
    unsafe {
        let pipeline = PIPELINE.get_or_try_init(|| init_pipeline(dc))?;

        let Some(mut pipeline) = pipeline.try_lock() else {
            error!("Could not lock pipeline");
            return Err(Error::from_hresult(HRESULT(-1)));
        };

        pipeline.prepare_render()?;

        pipeline.render(())?;
    }

    Ok(())
}

unsafe extern "system" fn opengl32_wgl_swap_buffers_impl(dc: HDC) {
    let _hook_ejection_guard = HOOK_EJECTION_BARRIER.acquire_ejection_guard();

    let Trampolines { opengl32_wgl_swap_buffers } =
        TRAMPOLINES.get().expect("OpenGL3 trampolines uninitialized");

    if !STOPPING.load(Ordering::Acquire) {
        if let Err(e) = render(dc) {
            error!("Render error: {e:?}");
        }
    }

    trace!("Call OpenGL3 wglSwapBuffers trampoline");
    opengl32_wgl_swap_buffers(dc);
    if !OWNED_LIFECYCLE.load(Ordering::Acquire) && EJECT_REQUESTED.load(Ordering::SeqCst) {
        perform_eject();
    }
}

// Get the address of wglSwapBuffers in opengl32.dll
unsafe fn get_opengl_wglswapbuffers_addr() -> OpenGl32wglSwapBuffersType {
    // Grab a handle to opengl32.dll
    let opengl32dll = CString::new("opengl32.dll").unwrap();
    let opengl32module = GetModuleHandleA(PCSTR(opengl32dll.as_ptr() as *mut _))
        .expect("failed finding opengl32.dll");

    // Grab the address of wglSwapBuffers
    let wglswapbuffers = CString::new("wglSwapBuffers").unwrap();
    let wglswapbuffers_func =
        GetProcAddress(opengl32module, PCSTR(wglswapbuffers.as_ptr() as *mut _)).unwrap();

    mem::transmute::<unsafe extern "system" fn() -> isize, OpenGl32wglSwapBuffersType>(
        wglswapbuffers_func,
    )
}

/// Hooks for OpenGL 3.
pub struct ImguiOpenGl3Hooks([MhHook; 1]);

impl ImguiOpenGl3Hooks {
    /// Construct a set of [`MhHook`]s that will render UI via the
    /// provided [`ImguiRenderLoop`].
    ///
    /// The following functions are hooked:
    /// - `opengl32::wglSwapBuffers`
    ///
    /// # Safety
    ///
    /// yolo
    pub unsafe fn new<T>(t: T) -> Self
    where
        T: ImguiRenderLoop + Send + Sync + 'static,
    {
        STOPPING.store(false, Ordering::Release);
        OWNED_LIFECYCLE.store(false, Ordering::Release);
        // Grab the addresses
        let hook_opengl_swap_buffers_address = get_opengl_wglswapbuffers_addr();

        // Create detours
        let hook_opengl_wgl_swap_buffers = MhHook::new(
            hook_opengl_swap_buffers_address as *mut _,
            opengl32_wgl_swap_buffers_impl as *mut _,
        )
        .expect("couldn't create opengl32.wglSwapBuffers hook");

        // Initialize the render loop and store detours
        RENDER_LOOP.get_or_init(move || Box::new(t));
        TRAMPOLINES.get_or_init(|| Trampolines {
            opengl32_wgl_swap_buffers: mem::transmute::<*mut c_void, OpenGl32wglSwapBuffersType>(
                hook_opengl_wgl_swap_buffers.trampoline(),
            ),
        });

        Self([hook_opengl_wgl_swap_buffers])
    }
}

impl Hooks for ImguiOpenGl3Hooks {
    fn from_render_loop<T>(t: T) -> Box<Self>
    where
        Self: Sized,
        T: ImguiRenderLoop + Send + Sync + 'static,
    {
        Box::new(unsafe { ImguiOpenGl3Hooks::new(t) })
    }

    fn hooks(&self) -> &[MhHook] {
        &self.0
    }

    fn use_owned_lifecycle(&mut self) {
        OWNED_LIFECYCLE.store(true, Ordering::Release);
    }

    fn begin_shutdown(&mut self) {
        STOPPING.store(true, Ordering::Release);
    }

    fn detach_window_procedures(&mut self) -> Result<()> {
        if let Some(pipeline) = unsafe { PIPELINE.get() } {
            let Some(mut pipeline) = pipeline.try_lock() else {
                return Err(Error::from_hresult(HRESULT(0x80004005u32 as i32)));
            };
            pipeline.detach_window_procedure()?;
        }
        Ok(())
    }

    unsafe fn release_render_resources(&mut self) -> Result<()> {
        if !STOPPING.load(Ordering::Acquire) {
            return Err(Error::from_hresult(HRESULT(0x80004005u32 as i32)));
        }
        if let Some(pipeline) = PIPELINE.take() {
            match pipeline.into_inner().take_resident() {
                Ok(render_loop) => drop(render_loop),
                Err((error, pipeline)) => {
                    let _ = PIPELINE.set(Mutex::new(pipeline));
                    return Err(error);
                },
            }
        }
        RENDER_LOOP.take();
        // Keep TRAMPOLINES and MinHook records: a caller may have cached a
        // detour address before we disabled the hook, but entered it later.
        Ok(())
    }

    unsafe fn unhook(&mut self) {
        TRAMPOLINES.take();
        PIPELINE.take().map(|p| p.into_inner().take());
        RENDER_LOOP.take();
    }
}
