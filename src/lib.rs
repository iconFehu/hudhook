//! # hudhook
//!
//! This library implements a mechanism for hooking into the
//! render loop of applications and drawing things on screen via
//! [`dear imgui`](https://docs.rs/imgui/0.11.0/imgui/).
//!
//! Currently, DirectX9, DirectX 11, DirectX 12 and OpenGL 3 are supported.
//!
//! For complete, fully fledged examples of usage, check out the following
//! projects:
//!
//! - [`darksoulsiii-practice-tool`](https://github.com/veeenu/darksoulsiii-practice-tool)
//! - [`eldenring-practice-tool`](https://github.com/veeenu/eldenring-practice-tool)
//!
//! It is a good idea to refer to these projects for any doubts about the API
//! which aren't clarified by this documentation, as this project is directly
//! derived from them.
//!
//! Refer to [this post](https://veeenu.github.io/blog/sekiro-practice-tool-architecture/) for
//! in-depth information about the architecture of the library.
//!
//! A [tutorial book](https://veeenu.github.io/hudhook/) is also available, with end-to-end
//! examples.
//!
//! [`darksoulsiii-practice-tool`]: https://github.com/veeenu/darksoulsiii-practice-tool
//! [`eldenring-practice-tool`]: https://github.com/veeenu/eldenring-practice-tool
//!
//! ## Fair warning
//!
//! [`hudhook`](crate) provides essential, crash-safe features for memory
//! manipulation and UI rendering. It does, alas, contain a hefty amount of FFI
//! and `unsafe` code which still has to be thoroughly tested, validated and
//! audited for soundness. It should be OK for small projects such as videogame
//! mods, but it may crash your application at this stage.
//!
//! ## Examples
//!
//! ### Hooking the render loop and drawing things with `imgui`
//!
//! Compile your crate with both a `cdylib` and an executable target. The
//! executable will be very minimal and used to inject the DLL into the
//! target process.
//!
//! #### Building the render loop
//!
//! Implement the render loop trait for your hook target.
//!
//! ##### Example
//!
//! Implement the [`ImguiRenderLoop`] trait:
//!
//! ```no_run
//! // lib.rs
//! use hudhook::*;
//!
//! pub struct MyRenderLoop;
//!
//! impl ImguiRenderLoop for MyRenderLoop {
//!     fn render(&mut self, ui: &mut imgui::Ui) {
//!         ui.window("My first render loop")
//!             .position([0., 0.], imgui::Condition::FirstUseEver)
//!             .size([320., 200.], imgui::Condition::FirstUseEver)
//!             .build(|| {
//!                 ui.text("Hello, hello!");
//!             });
//!     }
//! }
//!
//! {
//!     // Use this if hooking into a DirectX 9 application.
//!     use hudhook::hooks::dx9::ImguiDx9Hooks;
//!     hudhook!(ImguiDx9Hooks, MyRenderLoop);
//! }
//!
//! {
//!     // Use this if hooking into a DirectX 11 application.
//!     use hudhook::hooks::dx11::ImguiDx11Hooks;
//!     hudhook!(ImguiDx11Hooks, MyRenderLoop);
//! }
//!
//! {
//!     // Use this if hooking into a DirectX 12 application.
//!     use hudhook::hooks::dx12::ImguiDx12Hooks;
//!     hudhook!(ImguiDx12Hooks, MyRenderLoop);
//! }
//!
//! {
//!     // Use this if hooking into a OpenGL 3 application.
//!     use hudhook::hooks::opengl3::ImguiOpenGl3Hooks;
//!     hudhook!(ImguiOpenGl3Hooks, MyRenderLoop);
//! }
//! ```
//!
//! #### Injecting the DLL
//!
//! You can use the facilities in [`inject`] in your binaries to inject
//! the DLL in your target process.
//!
//! ```no_run
//! // main.rs
//! use hudhook::inject::Process;
//!
//! fn main() {
//!     let mut cur_exe = std::env::current_exe().unwrap();
//!     cur_exe.push("..");
//!     cur_exe.push("libmyhook.dll");
//!
//!     let cur_dll = cur_exe.canonicalize().unwrap();
//!
//!     Process::by_name("MyTargetApplication.exe").unwrap().inject(cur_dll).unwrap();
//! }
//! ```
#![allow(clippy::needless_doctest_main)]
#![allow(static_mut_refs)]
#![deny(missing_docs)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

pub use imgui;
use imgui::{Context, Io, TextureId, Ui};
use once_cell::sync::OnceCell;
pub use tracing;
use tracing::{error, trace, warn};
pub use windows;
use windows::core::Error;
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, WPARAM};
use windows::Win32::System::Console::{
    AllocConsole, FreeConsole, GetConsoleMode, GetStdHandle, SetConsoleMode, CONSOLE_MODE,
    ENABLE_VIRTUAL_TERMINAL_PROCESSING, STD_OUTPUT_HANDLE,
};
use windows::Win32::System::LibraryLoader::FreeLibraryAndExitThread;

use crate::mh::{MH_ApplyQueued, MH_Initialize, MH_Uninitialize, MhHook, MH_STATUS};
use crate::util::HookEjectionBarrier;

pub mod hooks;
#[cfg(feature = "inject")]
pub mod inject;
pub mod mh;
pub(crate) mod renderer;

pub use renderer::msg_filter::MessageFilter;

pub mod util;

// Global state objects.
static mut MODULE: OnceCell<HINSTANCE> = OnceCell::new();
static mut HUDHOOK: OnceCell<Hudhook> = OnceCell::new();
static CONSOLE_ALLOCATED: AtomicBool = AtomicBool::new(false);
static EJECT_REQUESTED: AtomicBool = AtomicBool::new(false);
static HOOK_EJECTION_BARRIER: HookEjectionBarrier = HookEjectionBarrier::new();
#[cfg(test)]
pub(crate) static LIFECYCLE_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Texture Loader for ImguiRenderLoop callbacks to load and replace textures
pub trait RenderContext {
    /// Load texture and return TextureId to use. Invoke it in your
    /// [`crate::ImguiRenderLoop::initialize`] method for setting up textures.
    fn load_texture(&mut self, data: &[u8], width: u32, height: u32) -> Result<TextureId, Error>;

    /// Upload an image to an existing texture, replacing its content. Invoke it
    /// in your [`crate::ImguiRenderLoop::before_render`] method for
    /// updating textures.
    fn replace_texture(
        &mut self,
        texture_id: TextureId,
        data: &[u8],
        width: u32,
        height: u32,
    ) -> Result<(), Error>;
}

/// Represents a control flow decision before the `wnd_proc` is executed.
///
/// See [`crate::ImguiRenderLoop::before_wnd_proc`].
#[derive(PartialEq, Eq)]
pub enum BeforeWndProc {
    /// Execute the `wnd_proc` code, and then run
    /// [`crate::ImguiRenderLoop::after_wnd_proc`].
    Continue,
    /// Skip the `wnd_proc` code, run
    /// [`crate::ImguiRenderLoop::after_wnd_proc`].
    Break,
}

/// Allocate a Windows console.
pub fn alloc_console() -> Result<(), Error> {
    if !CONSOLE_ALLOCATED.swap(true, Ordering::SeqCst) {
        unsafe { AllocConsole()? };
    }

    Ok(())
}

/// Enable console colors if the console is allocated.
pub fn enable_console_colors() {
    if CONSOLE_ALLOCATED.load(Ordering::SeqCst) {
        unsafe {
            // Get the stdout handle
            let stdout_handle = GetStdHandle(STD_OUTPUT_HANDLE).unwrap();

            // Call GetConsoleMode to get the current mode of the console
            let mut current_console_mode = CONSOLE_MODE(0);
            GetConsoleMode(stdout_handle, &mut current_console_mode).unwrap();

            // Set the new mode to include ENABLE_VIRTUAL_TERMINAL_PROCESSING
            // for ANSI escape sequences
            current_console_mode.0 |= ENABLE_VIRTUAL_TERMINAL_PROCESSING.0;

            // Call SetConsoleMode to set the new mode
            SetConsoleMode(stdout_handle, current_console_mode).unwrap();
        }
    }
}

/// Free the previously allocated Windows console.
pub fn free_console() -> Result<(), Error> {
    if CONSOLE_ALLOCATED.swap(false, Ordering::SeqCst) {
        unsafe { FreeConsole()? };
    }

    Ok(())
}

/// Disable hooks and eject the DLL.
///
/// ## Ejecting a DLL
///
/// To eject your DLL, invoke the [`eject`] method from anywhere in your
/// render loop. This will disable the hooks, free the console (if it has
/// been created before) and invoke
/// [`windows::Win32::System::LibraryLoader::FreeLibraryAndExitThread`].
///
/// Befor calling [`eject`], make sure to perform any manual cleanup (e.g.
/// dropping/resetting the contents of static mutable variables).
pub fn eject() {
    trace!("Requesting eject");
    EJECT_REQUESTED.store(true, Ordering::SeqCst);
}

/// Perform the ejection that was previously requested
unsafe fn perform_eject() {
    trace!("Performing eject");
    if let Err(e) = free_console() {
        error!("{e:?}");
    }

    if let Some(mut hudhook) = HUDHOOK.take() {
        if let Err(e) = hudhook.unapply() {
            error!("Couldn't unapply hooks: {e:?}");
        }
    }

    thread::spawn(|| unsafe {
        // Wait for all hook ejection guards to complete. As we have
        // already called `hudhook.unapply()` above any future invocations
        // of the hooked functions will call the original code, so we just
        // have to wait for the previous hook invocations to complete before
        // we continue to free the library.
        HOOK_EJECTION_BARRIER.wait_for_all_guards();

        if let Some(module) = MODULE.take() {
            FreeLibraryAndExitThread(module.into(), 0);
        }
        trace!("Finished ejecting!");
    });
}

/// Implement your `imgui` rendering logic via this trait.
pub trait ImguiRenderLoop {
    /// Called once at the first occurrence of the hook. Implement this to
    /// initialize your data.
    /// `ctx` is the imgui context, and `render_context` is meant to access
    /// hudhook renderers' extensions such as texture management.
    fn initialize<'a>(
        &'a mut self,
        _ctx: &mut Context,
        _render_context: &'a mut dyn RenderContext,
    ) {
    }

    /// Called before rendering each frame. Use the provided `ctx` object to
    /// modify imgui settings before rendering the UI.
    /// `ctx` is the imgui context, and `render_context` is meant to access
    /// hudhook renderers' extensions such as texture management.
    fn before_render<'a>(
        &'a mut self,
        _ctx: &mut Context,
        _render_context: &'a mut dyn RenderContext,
    ) {
    }

    /// Called every frame. Use the provided `ui` object to build your UI.
    fn render(&mut self, ui: &mut Ui);

    /// Called before the window procedure.
    fn before_wnd_proc(
        &self,
        _hwnd: HWND,
        _umsg: u32,
        _wparam: WPARAM,
        _lparam: LPARAM,
    ) -> BeforeWndProc {
        BeforeWndProc::Continue
    }

    /// Called after the window procedure.
    fn after_wnd_proc(&self, _hwnd: HWND, _umsg: u32, _wparam: WPARAM, _lparam: LPARAM) {}

    /// Returns the types of window message that
    /// you do not want to propagate to the main window
    fn message_filter(&self, _io: &Io) -> MessageFilter {
        MessageFilter::empty()
    }
}

/// Generic trait for platform-specific hooks.
///
/// Implement this if you are building a custom hook for a non-supported
/// renderer.
///
/// Check out first party implementations for guidance on how to implement the
/// methods:
/// - [`ImguiDx9Hooks`](crate::hooks::dx9::ImguiDx9Hooks)
/// - [`ImguiDx11Hooks`](crate::hooks::dx11::ImguiDx11Hooks)
/// - [`ImguiDx12Hooks`](crate::hooks::dx12::ImguiDx12Hooks)
/// - [`ImguiOpenGl3Hooks`](crate::hooks::opengl3::ImguiOpenGl3Hooks)
pub trait Hooks {
    /// Construct a boxed instance of the implementor, storing the provided
    /// render loop where appropriate.
    fn from_render_loop<T>(t: T) -> Box<Self>
    where
        Self: Sized,
        T: ImguiRenderLoop + Send + Sync + 'static;

    /// Return the list of hooks to be enabled, in order.
    fn hooks(&self) -> &[MhHook];

    /// Stop creating renderer resources before disabling this hook set.
    fn begin_shutdown(&mut self) {}

    /// Route teardown through the owner instead of the global eject request.
    fn use_owned_lifecycle(&mut self) {}

    /// Restore window procedures while retaining their forwarding state.
    /// Call this after disabling hooks and waiting for render callbacks.
    fn detach_window_procedures(&mut self) -> Result<(), Error> {
        Err(Error::from_hresult(windows::core::HRESULT(0x80004001u32 as i32)))
    }

    /// Release renderer resources while retaining callback forwarding code,
    /// original functions, and disabled MinHook records for a resident DLL.
    ///
    /// # Safety
    /// Rendering must have stopped, window procedures must have been detached,
    /// and callbacks using renderer resources must have returned.
    unsafe fn release_render_resources(&mut self) -> Result<(), Error> {
        Err(Error::from_hresult(windows::core::HRESULT(0x80004001u32 as i32)))
    }

    /// Validate that stopped renderer resources can be recreated using the
    /// retained render loop and hooks. The default rejects resuming.
    fn validate_owned_resume(&self) -> Result<(), Error> {
        Err(Error::from_hresult(windows::core::HRESULT(0x80004001u32 as i32)))
    }

    /// Allow renderer creation after all retained hooks have been enabled.
    /// This is called only after [`Self::validate_owned_resume`] succeeds.
    fn resume_owned_rendering(&mut self) {}

    /// Cleanup global data and disable the hooks.
    ///
    /// # Safety
    ///
    /// Is most definitely UB.
    unsafe fn unhook(&mut self);
}

/// Holds all the activated hooks and manages their lifetime.
pub struct Hudhook {
    hooks: Vec<Box<dyn Hooks>>,
    active: bool,
    window_procedures_detached: bool,
    owned_lifecycle: bool,
    shutdown_started: bool,
    renderers_released: bool,
}
unsafe impl Send for Hudhook {}
unsafe impl Sync for Hudhook {}

impl Hudhook {
    /// Create a builder object.
    pub fn builder() -> HudhookBuilder {
        HudhookBuilder(Hudhook::new())
    }

    fn new() -> Self {
        // Initialize minhook.
        match unsafe { MH_Initialize() } {
            MH_STATUS::MH_OK => {},
            MH_STATUS::MH_ERROR_ALREADY_INITIALIZED => {
                warn!("Minhook already initialized");
            },
            status @ MH_STATUS::MH_ERROR_MEMORY_ALLOC => panic!("MH_Initialize: {status:?}"),
            _ => unreachable!(),
        }

        Hudhook {
            hooks: Vec::new(),
            active: false,
            window_procedures_detached: false,
            owned_lifecycle: false,
            shutdown_started: false,
            renderers_released: false,
        }
    }

    /// Return an iterator of all the activated raw hooks.
    fn hooks(&self) -> impl IntoIterator<Item = &MhHook> {
        self.hooks.iter().flat_map(|h| h.hooks())
    }

    /// Apply the hooks.
    pub fn apply(mut self) -> Result<(), MH_STATUS> {
        self.enable()?;

        unsafe { HUDHOOK.set(self).ok() };

        Ok(())
    }

    /// Enable hooks while retaining ownership in the caller. This does not
    /// register an eject handler or transfer ownership of the containing DLL.
    /// After shutdown, use [`Self::resume_owned`] instead of applying again.
    pub fn apply_owned(&mut self) -> Result<(), MH_STATUS> {
        if self.shutdown_started {
            return Err(MH_STATUS::MH_ERROR_DISABLED);
        }
        for hook in &mut self.hooks {
            hook.use_owned_lifecycle();
        }
        self.owned_lifecycle = true;
        self.enable()
    }

    fn enable(&mut self) -> Result<(), MH_STATUS> {
        if self.active {
            return Ok(());
        }
        // Queue enabling all the hooks.
        for hook in self.hooks() {
            unsafe { hook.queue_enable()? };
        }

        // Apply the queue of enable actions.
        unsafe { MH_ApplyQueued().ok_context("MH_ApplyQueued")? };

        self.active = true;
        self.window_procedures_detached = false;

        Ok(())
    }

    /// Disable hook entry points without releasing trampolines or renderers.
    /// The caller must also detach window procedures and drain callbacks
    /// before calling [`Self::cleanup_owned`].
    pub fn disable(&mut self) -> Result<(), MH_STATUS> {
        self.shutdown_started = true;
        for hook in &mut self.hooks {
            hook.begin_shutdown();
        }
        for hook in self.hooks() {
            unsafe { hook.queue_disable()? };
        }
        unsafe { MH_ApplyQueued().ok_context("MH_ApplyQueued")? };
        self.active = false;
        Ok(())
    }

    /// Wait for guarded render/window callbacks to return. Use once after
    /// [`Self::disable`], then again after detaching window procedures.
    /// This coordinates resource cleanup; it does not prove the containing
    /// DLL can be unmapped, since callers may have cached detour addresses.
    pub fn wait_for_idle(&self, timeout: Duration) -> bool {
        HOOK_EJECTION_BARRIER.wait_for_all_guards_timeout(timeout)
    }

    /// Restore window procedures without freeing state used by callbacks.
    /// A later subclass in the window procedure chain causes an error; keep
    /// this instance and its containing DLL loaded in that case. Currently
    /// supported by DirectX 9; other hook sets must implement detachment.
    pub fn detach_window_procedures(&mut self) -> Result<(), Error> {
        for hook in &mut self.hooks {
            hook.detach_window_procedures()?;
        }
        self.window_procedures_detached = true;
        Ok(())
    }

    /// Release stopped renderer resources while keeping callback forwarding
    /// state and disabled hooks resident. Use after disabling hooks, draining
    /// callbacks, detaching window procedures, and draining callbacks again.
    /// The containing DLL must remain loaded after this logical shutdown.
    pub fn release_owned_renderers(&mut self) -> Result<(), Error> {
        if !self.owned_lifecycle
            || !self.shutdown_started
            || self.active
            || !self.window_procedures_detached
            || !self.wait_for_idle(Duration::ZERO)
        {
            return Err(Error::from_hresult(windows::core::HRESULT(0x80004005u32 as i32)));
        }
        if self.renderers_released {
            return Ok(());
        }
        for hook in &mut self.hooks {
            unsafe { hook.release_render_resources()? };
        }
        self.renderers_released = true;
        Ok(())
    }

    /// Resume a resident hook set after [`Self::release_owned_renderers`]
    /// succeeds. Reuses its existing MinHook records and trampolines; do not
    /// construct a second hook set for the same rendering backend.
    ///
    /// DirectX 9 retains the render loop and creates a new ImGui context and
    /// renderer on the next render callback, calling `initialize` again.
    /// Other hook sets must implement resume validation and rendering.
    /// Calls during an incomplete shutdown are rejected. On enable failure,
    /// rendering stays stopped and disabling the retained hooks is attempted.
    pub fn resume_owned(&mut self) -> Result<(), Error> {
        if !self.owned_lifecycle
            || !self.shutdown_started
            || !self.renderers_released
            || self.active
            || !self.window_procedures_detached
        {
            return Err(Error::from_hresult(windows::core::HRESULT(0x80004005u32 as i32)));
        }
        for hook in &self.hooks {
            hook.validate_owned_resume()?;
        }
        if let Err(status) = self.enable() {
            // MinHook can fail after enabling part of its queue. Treat this
            // conservatively as active until rollback has succeeded.
            self.active = true;
            if let Err(rollback_status) = self.disable() {
                error!("Could not roll back resumed hooks: {rollback_status:?}");
            }
            return Err(Error::new(
                windows::core::HRESULT(0x80004005u32 as i32),
                format!("Could not resume hooks: {status:?}"),
            ));
        }
        self.renderers_released = false;
        self.shutdown_started = false;
        for hook in &mut self.hooks {
            hook.resume_owned_rendering();
        }
        Ok(())
    }

    /// Remove this instance's MinHook records and release renderer resources.
    /// Other users of the process-wide MinHook library remain initialized.
    ///
    /// # Safety
    /// Hook entry points and window procedures must have been detached and
    /// all callbacks drained. This must not run from a hook callback.
    /// Cached detour/trampoline addresses and callbacks outside the guards
    /// must also be ruled out by the caller; `wait_for_idle` alone is not
    /// sufficient. Prefer [`Self::release_owned_renderers`] for resident DLLs.
    pub unsafe fn cleanup_owned(&mut self) -> Result<(), MH_STATUS> {
        if self.active {
            return Err(MH_STATUS::MH_ERROR_ENABLED);
        }
        for hook in self.hooks() {
            hook.remove()?;
        }
        for hook in &mut self.hooks {
            hook.unhook();
        }
        self.hooks.clear();
        self.renderers_released = false;
        Ok(())
    }

    /// Disable and cleanup the hooks.
    pub fn unapply(&mut self) -> Result<(), MH_STATUS> {
        trace!("Unapply hook");
        // Queue disabling all the hooks.
        for hook in self.hooks() {
            unsafe { hook.queue_disable()? };
        }

        // Apply the queue of disable actions.
        unsafe { MH_ApplyQueued().ok_context("MH_ApplyQueued")? };

        // Uninitialize minhook.
        unsafe { MH_Uninitialize().ok_context("MH_Uninitialize")? };

        // Invoke cleanup for all hooks.
        for hook in &mut self.hooks {
            unsafe { hook.unhook() };
        }
        trace!("Finished removing hook");

        Ok(())
    }
}

/// Builder object for [`Hudhook`].
///
/// Example usage:
/// ```no_run
/// use hudhook::hooks::dx12::ImguiDx12Hooks;
/// use hudhook::windows::Win32::Foundation::HINSTANCE;
/// use hudhook::windows::Win32::System::SystemServices::DLL_PROCESS_ATTACH;
/// use hudhook::*;
///
/// pub struct MyRenderLoop;
///
/// impl ImguiRenderLoop for MyRenderLoop {
///     fn render(&mut self, frame: &mut imgui::Ui) {
///         // ...
///     }
/// }
///
/// #[no_mangle]
/// pub unsafe extern "system" fn DllMain(
///     hmodule: HINSTANCE,
///     reason: u32,
///     _: *mut std::ffi::c_void,
/// ) {
///     if reason == DLL_PROCESS_ATTACH {
///         let hmodule_raw = hmodule.0 as usize;
///         std::thread::spawn(move || {
///             let hmodule = HINSTANCE(hmodule_raw as _);
///             let hooks = Hudhook::builder()
///                 .with::<ImguiDx12Hooks>(MyRenderLoop)
///                 .with_hmodule(hmodule)
///                 .build();
///             hooks.apply();
///         });
///     }
/// }
pub struct HudhookBuilder(Hudhook);

impl HudhookBuilder {
    /// Add a hook object.
    pub fn with<T: Hooks + 'static>(
        mut self,
        render_loop: impl ImguiRenderLoop + Send + Sync + 'static,
    ) -> Self {
        self.0.hooks.push(T::from_render_loop(render_loop));
        self
    }

    /// Save the DLL instance (for the [`eject`] method).
    pub fn with_hmodule(self, module: HINSTANCE) -> Self {
        unsafe { MODULE.set(module).unwrap() };
        self
    }

    /// Build the [`Hudhook`] object.
    pub fn build(self) -> Hudhook {
        self.0
    }
}

/// Entry point generator for the library.
///
/// After implementing your [render loop](crate::hooks) of choice, invoke
/// the macro to generate the `DllMain` function that will serve as entry point
/// for your hook.
///
/// Example usage:
/// ```no_run
/// use hudhook::hooks::dx12::ImguiDx12Hooks;
/// use hudhook::*;
///
/// pub struct MyRenderLoop;
///
/// impl ImguiRenderLoop for MyRenderLoop {
///     fn render(&mut self, frame: &mut imgui::Ui) {
///         // ...
///     }
/// }
///
/// hudhook::hudhook!(ImguiDx12Hooks, MyRenderLoop);
/// ```
#[macro_export]
macro_rules! hudhook {
    ($t:ty, $hooks:expr) => {
        /// Entry point created by the `hudhook` library.
        #[no_mangle]
        pub unsafe extern "system" fn DllMain(
            hmodule: ::hudhook::windows::Win32::Foundation::HINSTANCE,
            reason: u32,
            _: *mut ::std::ffi::c_void,
        ) {
            use ::hudhook::*;

            if reason == ::hudhook::windows::Win32::System::SystemServices::DLL_PROCESS_ATTACH {
                ::hudhook::tracing::trace!("DllMain()");
                let hmodule_raw = hmodule.0 as usize;
                ::std::thread::spawn(move || {
                    let hmodule =
                        ::hudhook::windows::Win32::Foundation::HINSTANCE(hmodule_raw as _);
                    if let Err(e) = ::hudhook::Hudhook::builder()
                        .with::<$t>({ $hooks })
                        .with_hmodule(hmodule)
                        .build()
                        .apply()
                    {
                        ::hudhook::tracing::error!("Couldn't apply hooks: {e:?}");
                        ::hudhook::eject();
                    }
                });
            }
        }
    };
}

#[cfg(test)]
mod owned_lifecycle_tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use super::*;

    struct ResidentHooks(Arc<AtomicUsize>);

    impl Hooks for ResidentHooks {
        fn from_render_loop<T>(_: T) -> Box<Self>
        where
            Self: Sized,
            T: ImguiRenderLoop + Send + Sync + 'static,
        {
            unreachable!()
        }

        fn hooks(&self) -> &[MhHook] {
            &[]
        }

        fn detach_window_procedures(&mut self) -> Result<(), Error> {
            Ok(())
        }

        unsafe fn release_render_resources(&mut self) -> Result<(), Error> {
            self.0.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }

        unsafe fn unhook(&mut self) {
            panic!("Resident shutdown must preserve callback forwarding state");
        }
    }

    #[test]
    fn resident_release_requires_detach_and_idle_callbacks() {
        let _test_lock = LIFECYCLE_TEST_LOCK.lock().unwrap();
        let released = Arc::new(AtomicUsize::new(0));
        let mut hook = Hudhook {
            hooks: vec![Box::new(ResidentHooks(Arc::clone(&released)))],
            active: false,
            window_procedures_detached: false,
            owned_lifecycle: true,
            shutdown_started: true,
            renderers_released: false,
        };
        assert!(hook.release_owned_renderers().is_err());
        hook.detach_window_procedures().unwrap();
        let callback = HOOK_EJECTION_BARRIER.acquire_ejection_guard();
        assert!(hook.release_owned_renderers().is_err());
        assert_eq!(released.load(Ordering::Relaxed), 0);
        drop(callback);
        hook.release_owned_renderers().unwrap();
        assert_eq!(released.load(Ordering::Relaxed), 1);
        assert_eq!(hook.hooks.len(), 1);
        assert!(hook.resume_owned().is_err());
        assert!(!hook.active);
    }

    struct ResumableHooks {
        released: Arc<AtomicUsize>,
        resumed: Arc<AtomicUsize>,
        stopping: bool,
    }

    impl Hooks for ResumableHooks {
        fn from_render_loop<T>(_: T) -> Box<Self>
        where
            Self: Sized,
            T: ImguiRenderLoop + Send + Sync + 'static,
        {
            unreachable!()
        }

        fn hooks(&self) -> &[MhHook] {
            &[]
        }

        fn begin_shutdown(&mut self) {
            self.stopping = true;
        }

        fn detach_window_procedures(&mut self) -> Result<(), Error> {
            assert!(self.stopping);
            Ok(())
        }

        unsafe fn release_render_resources(&mut self) -> Result<(), Error> {
            assert!(self.stopping);
            self.released.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }

        fn validate_owned_resume(&self) -> Result<(), Error> {
            assert!(self.stopping);
            assert!(self.released.load(Ordering::Relaxed) > self.resumed.load(Ordering::Relaxed));
            Ok(())
        }

        fn resume_owned_rendering(&mut self) {
            assert!(self.stopping);
            self.stopping = false;
            self.resumed.fetch_add(1, Ordering::Relaxed);
        }

        unsafe fn unhook(&mut self) {
            panic!("Resume must reuse the retained hook set");
        }
    }

    #[test]
    fn resident_resume_requires_complete_shutdown_and_can_repeat() {
        let _test_lock = LIFECYCLE_TEST_LOCK.lock().unwrap();
        let released = Arc::new(AtomicUsize::new(0));
        let resumed = Arc::new(AtomicUsize::new(0));
        let mut hook = Hudhook::new();
        hook.hooks.push(Box::new(ResumableHooks {
            released: Arc::clone(&released),
            resumed: Arc::clone(&resumed),
            stopping: false,
        }));
        assert!(hook.resume_owned().is_err());
        hook.apply_owned().unwrap();
        assert!(hook.resume_owned().is_err());
        for cycle in 1..=3 {
            hook.disable().unwrap();
            assert!(hook.resume_owned().is_err());
            hook.detach_window_procedures().unwrap();
            assert!(hook.resume_owned().is_err());
            hook.release_owned_renderers().unwrap();
            hook.release_owned_renderers().unwrap();
            assert_eq!(released.load(Ordering::Relaxed), cycle);
            assert_eq!(hook.apply_owned(), Err(MH_STATUS::MH_ERROR_DISABLED));
            hook.resume_owned().unwrap();
            assert!(hook.active);
            assert_eq!(resumed.load(Ordering::Relaxed), cycle);
            assert_eq!(hook.hooks.len(), 1);
            assert!(hook.resume_owned().is_err());
        }
    }
}
