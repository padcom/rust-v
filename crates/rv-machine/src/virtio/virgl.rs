//! virtio-gpu with 3D acceleration (VIRTIO_GPU_F_VIRGL): the guest's Mesa virgl driver sends
//! Gallium command streams that virglrenderer replays on the host's OpenGL.
//!
//! virglrenderer is loaded at run time (it is optional, and absent on most hosts), and all
//! calls into it happen on one thread, which owns its GL context: the device forwards queue
//! notifications there. Fenced commands are answered when virglrenderer signals the fence.
//! The scanout is read back from the host GPU on each flush and shown like the 2D device's.

use std::collections::HashMap;
use std::ffi::{c_char, c_int, c_void};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rv_core::GuestMemory;

use super::gpu::{
    CONTROLQ, CURSORQ, FLAG_FENCE, Format, GpuHandle, HEADER, Rect, Shared, cmd, display_info, rect_at,
    trace, u32_at, u64_at,
};
use super::queue::Chain;
use super::{Activation, VirtioDevice, device_type, read_config_bytes};
use crate::display::{CURSOR_SIZE, Display};

mod cmd3d {
    pub const GET_CAPSET_INFO: u32 = 0x0108;
    pub const GET_CAPSET: u32 = 0x0109;
    pub const CTX_CREATE: u32 = 0x0200;
    pub const CTX_DESTROY: u32 = 0x0201;
    pub const CTX_ATTACH_RESOURCE: u32 = 0x0202;
    pub const CTX_DETACH_RESOURCE: u32 = 0x0203;
    pub const RESOURCE_CREATE_3D: u32 = 0x0204;
    pub const TRANSFER_TO_HOST_3D: u32 = 0x0205;
    pub const TRANSFER_FROM_HOST_3D: u32 = 0x0206;
    pub const SUBMIT_3D: u32 = 0x0207;

    pub const OK_CAPSET_INFO: u32 = 0x1102;
    pub const OK_CAPSET: u32 = 0x1103;
}

const FEATURE_VIRGL: u64 = 1 << 0;
/// Largest request read from the guest (command streams with inline data included).
const MAX_REQUEST: u64 = 16 << 20;
/// How often signalled fences are looked for while some are outstanding.
const FENCE_POLL: Duration = Duration::from_millis(1);

// virglrenderer.h
const USE_EGL: c_int = 1;
const USE_SURFACELESS: c_int = 1 << 3;
const PIPE_TEXTURE_2D: u32 = 2;
const BIND_RENDER_TARGET: u32 = 1 << 1;
const RESOURCE_Y_0_TOP: u32 = 1;

#[repr(C)]
struct Iovec {
    base: *mut c_void,
    len: usize,
}

#[repr(C)]
#[derive(Default)]
struct VirglBox {
    x: u32,
    y: u32,
    z: u32,
    w: u32,
    h: u32,
    d: u32,
}

#[repr(C)]
#[derive(Default)]
struct CreateArgs {
    handle: u32,
    target: u32,
    format: u32,
    bind: u32,
    width: u32,
    height: u32,
    depth: u32,
    array_size: u32,
    last_level: u32,
    nr_samples: u32,
    flags: u32,
}

#[repr(C)]
#[derive(Default)]
struct ResourceInfo {
    handle: u32,
    virgl_format: u32,
    width: u32,
    height: u32,
    depth: u32,
    flags: u32,
    tex_id: u32,
    stride: u32,
    drm_fourcc: c_int,
    fd: c_int,
}

/// `struct virgl_renderer_gl_ctx_param`.
#[repr(C)]
struct GlContextParams {
    version: c_int,
    shared: bool,
    major: c_int,
    minor: c_int,
    compat: c_int,
}

/// `struct virgl_renderer_callbacks`, version 1. With EGL (Linux) virglrenderer makes its own
/// GL contexts and only fences are reported to us; otherwise (macOS) we make the contexts.
#[repr(C)]
struct Callbacks {
    version: c_int,
    write_fence: Option<unsafe extern "C" fn(*mut c_void, u32)>,
    create_gl_context: Option<unsafe extern "C" fn(*mut c_void, c_int, *mut GlContextParams) -> *mut c_void>,
    destroy_gl_context: Option<unsafe extern "C" fn(*mut c_void, *mut c_void)>,
    make_current: Option<unsafe extern "C" fn(*mut c_void, c_int, *mut c_void) -> c_int>,
}

/// What the callbacks get as their cookie.
#[derive(Default)]
struct Cookie {
    /// The last fence virglrenderer signalled.
    signalled: AtomicU32,
}

unsafe extern "C" fn write_fence(cookie: *mut c_void, fence: u32) {
    // SAFETY: the cookie is the renderer's `Cookie`, which outlives virglrenderer's use.
    let cookie = unsafe { &*(cookie as *const Cookie) };
    cookie.signalled.store(fence, Ordering::Release);
}

/// GL contexts from Apple's OpenGL (4.1 core profile, on Metal). virglrenderer renders into
/// framebuffer objects, so the contexts need no drawable.
#[cfg(target_os = "macos")]
mod cgl {
    use super::GlContextParams;
    use std::ffi::{c_char, c_int, c_void};

    const PFA_ACCELERATED: c_int = 73;
    const PFA_ALLOW_OFFLINE_RENDERERS: c_int = 96;
    const PFA_OPENGL_PROFILE: c_int = 99;
    const PROFILE_GL4_CORE: c_int = 0x4100;

    #[link(name = "OpenGL", kind = "framework")]
    unsafe extern "C" {
        fn CGLChoosePixelFormat(attribs: *const c_int, pix: *mut *mut c_void, npix: *mut c_int) -> c_int;
        fn CGLDestroyPixelFormat(pix: *mut c_void) -> c_int;
        fn CGLCreateContext(pix: *mut c_void, share: *mut c_void, ctx: *mut *mut c_void) -> c_int;
        fn CGLDestroyContext(ctx: *mut c_void) -> c_int;
        fn CGLSetCurrentContext(ctx: *mut c_void) -> c_int;
        fn CGLGetCurrentContext() -> *mut c_void;
    }

    pub unsafe extern "C" fn create(
        _cookie: *mut c_void,
        _scanout: c_int,
        params: *mut GlContextParams,
    ) -> *mut c_void {
        // SAFETY: virglrenderer passes valid parameters and our cookie.
        let params = unsafe { &*params };
        // Core profile 4.1 is the newest there is.
        if params.major > 4 || (params.major == 4 && params.minor > 1) {
            return std::ptr::null_mut();
        }
        let attribs = [PFA_OPENGL_PROFILE, PROFILE_GL4_CORE, PFA_ACCELERATED, PFA_ALLOW_OFFLINE_RENDERERS, 0];
        let (mut pix, mut n, mut ctx) = (std::ptr::null_mut(), 0, std::ptr::null_mut());
        // Shared contexts share objects with the current one (as in QEMU).
        let share = if params.shared { unsafe { CGLGetCurrentContext() } } else { std::ptr::null_mut() };
        if super::trace() {
            eprintln!(
                "virgl: GL context {}.{} shared={} compat={}\r",
                params.major, params.minor, params.shared, params.compat
            );
        }
        // SAFETY: plain CGL calls with valid out-parameters.
        unsafe {
            if CGLChoosePixelFormat(attribs.as_ptr(), &mut pix, &mut n) != 0 || pix.is_null() {
                return std::ptr::null_mut();
            }
            let err = CGLCreateContext(pix, share, &mut ctx);
            CGLDestroyPixelFormat(pix);
            if err != 0 {
                eprintln!("virgl: CGLCreateContext failed ({err})\r");
                return std::ptr::null_mut();
            }
        }
        ctx
    }

    pub unsafe extern "C" fn destroy(_cookie: *mut c_void, ctx: *mut c_void) {
        // SAFETY: `ctx` came from `create`.
        unsafe { CGLDestroyContext(ctx) };
    }

    pub unsafe extern "C" fn make_current(_cookie: *mut c_void, _scanout: c_int, ctx: *mut c_void) -> c_int {
        // SAFETY: `ctx` came from `create` (or is null).
        if unsafe { CGLSetCurrentContext(ctx) } == 0 { 0 } else { -1 }
    }

    type ShaderSource = unsafe extern "C" fn(u32, c_int, *const *const c_char, *const c_int);
    static REAL_SHADER_SOURCE: std::sync::OnceLock<ShaderSource> = std::sync::OnceLock::new();

    /// virglrenderer writes GLSL for drivers that accept old `#version`s in a core profile
    /// and list core features as extensions; Apple's does neither. libepoxy, through which
    /// virglrenderer calls OpenGL, keeps each function in an exported pointer: this points
    /// glShaderSource at [`shader_source`], which passes the source through [`super::fix_glsl`].
    pub fn install_shader_shim() -> Result<(), String> {
        let gl = "/System/Library/Frameworks/OpenGL.framework/Versions/Current/OpenGL";
        // SAFETY: loading system and Homebrew libraries without initialisation requirements;
        // the symbols have the types declared (the epoxy one is a pointer variable).
        unsafe {
            let gl = libloading::Library::new(gl).map_err(|e| e.to_string())?;
            let real: ShaderSource = *gl.get(b"glShaderSource\0").map_err(|e| e.to_string())?;
            let _ = REAL_SHADER_SOURCE.set(real);
            std::mem::forget(gl);
            let mut errors = Vec::new();
            for name in [
                "/opt/homebrew/opt/libepoxy/lib/libepoxy.0.dylib",
                "/usr/local/opt/libepoxy/lib/libepoxy.0.dylib",
                "libepoxy.0.dylib",
            ] {
                match libloading::Library::new(name) {
                    Ok(epoxy) => {
                        let slot: libloading::Symbol<*mut ShaderSource> =
                            epoxy.get(b"epoxy_glShaderSource\0").map_err(|e| e.to_string())?;
                        **slot = shader_source;
                        std::mem::forget(epoxy);
                        return Ok(());
                    }
                    Err(e) => errors.push(e.to_string()),
                }
            }
            Err(format!("libepoxy was not found ({})", errors.join("; ")))
        }
    }

    unsafe extern "C" fn shader_source(
        shader: u32,
        count: c_int,
        strings: *const *const c_char,
        lengths: *const c_int,
    ) {
        let mut source = Vec::new();
        for i in 0..count.max(0) as usize {
            // SAFETY: OpenGL's contract: `count` strings, NUL-terminated unless a length is given.
            unsafe {
                let s = *strings.add(i);
                let len = if lengths.is_null() || *lengths.add(i) < 0 {
                    std::ffi::CStr::from_ptr(s).to_bytes().len()
                } else {
                    *lengths.add(i) as usize
                };
                source.extend_from_slice(std::slice::from_raw_parts(s.cast::<u8>(), len));
            }
        }
        let fixed = super::fix_glsl(&String::from_utf8_lossy(&source));
        if let Some(dir) = std::env::var_os("RV_GPU_SHADERS") {
            // A copy of every shader as compiled, for debugging rendering differences.
            static COUNT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            let n = COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let _ = std::fs::write(std::path::Path::new(&dir).join(format!("{n:05}-{shader}.glsl")), &fixed);
        }
        let fixed = std::ffi::CString::new(fixed).unwrap_or_default();
        let real = REAL_SHADER_SOURCE.get().expect("shim installed without the real function");
        // SAFETY: one NUL-terminated string.
        unsafe { real(shader, 1, &fixed.as_ptr(), std::ptr::null()) };
    }
}

/// Extensions whose functionality is core in GLSL 4.10, which Apple's OpenGL does not list.
const CORE_IN_410: &[&str] = &[
    "GL_ARB_fragment_coord_conventions",
    "GL_ARB_explicit_attrib_location",
    "GL_ARB_uniform_buffer_object",
    "GL_ARB_separate_shader_objects",
    "GL_ARB_shader_bit_encoding",
    "GL_ARB_gpu_shader5",
    "GL_ARB_gpu_shader_fp64",
    "GL_ARB_sample_shading",
    "GL_ARB_texture_cube_map_array",
    "GL_ARB_texture_gather",
    "GL_ARB_texture_query_lod",
    "GL_ARB_texture_multisample",
    "GL_ARB_texture_rectangle",
    "GL_ARB_draw_instanced",
    "GL_ARB_tessellation_shader",
    "GL_ARB_viewport_array",
    "GL_ARB_shading_language_420pack_lite",
];

/// Makes desktop GLSL from virglrenderer acceptable to a 4.1 core profile: `#version 410`, and
/// no `#extension` lines for what 4.10 has built in. GLSL ES is left alone.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn fix_glsl(source: &str) -> String {
    let mut out = String::with_capacity(source.len() + 16);
    for line in source.split_inclusive('\n') {
        let t = line.trim_start();
        if let Some(v) = t.strip_prefix("#version") {
            if !v.trim_end().ends_with("es") && v.trim().parse::<u32>().is_ok_and(|v| v < 410) {
                out.push_str("#version 410\n");
                continue;
            }
        } else if let Some(e) = t.strip_prefix("#extension") {
            let name = e.split(':').next().unwrap_or("").trim();
            if CORE_IN_410.contains(&name) {
                out.push('\n'); // keeps the line numbers in compiler messages
                continue;
            }
        }
        out.push_str(line);
    }
    out
}

unsafe extern "C" {
    fn free(p: *mut c_void);
}

/// The virglrenderer functions used.
struct Api {
    _lib: libloading::Library,
    init: unsafe extern "C" fn(*mut c_void, c_int, *mut Callbacks) -> c_int,
    poll: unsafe extern "C" fn(),
    reset: unsafe extern "C" fn(),
    get_cap_set: unsafe extern "C" fn(u32, *mut u32, *mut u32),
    fill_caps: unsafe extern "C" fn(u32, u32, *mut c_void),
    context_create: unsafe extern "C" fn(u32, u32, *const c_char) -> c_int,
    context_destroy: unsafe extern "C" fn(u32),
    ctx_attach_resource: unsafe extern "C" fn(c_int, c_int),
    ctx_detach_resource: unsafe extern "C" fn(c_int, c_int),
    resource_create: unsafe extern "C" fn(*mut CreateArgs, *mut Iovec, u32) -> c_int,
    resource_unref: unsafe extern "C" fn(u32),
    resource_attach_iov: unsafe extern "C" fn(c_int, *mut Iovec, c_int) -> c_int,
    resource_detach_iov: unsafe extern "C" fn(c_int, *mut *mut Iovec, *mut c_int),
    resource_get_info: unsafe extern "C" fn(c_int, *mut ResourceInfo) -> c_int,
    transfer_read_iov:
        unsafe extern "C" fn(u32, u32, u32, u32, u32, *mut VirglBox, u64, *mut Iovec, c_int) -> c_int,
    transfer_write_iov:
        unsafe extern "C" fn(u32, u32, c_int, u32, u32, *mut VirglBox, u64, *mut Iovec, u32) -> c_int,
    submit_cmd: unsafe extern "C" fn(*mut c_void, c_int, c_int) -> c_int,
    create_fence: unsafe extern "C" fn(c_int, u32) -> c_int,
    get_cursor_data: unsafe extern "C" fn(u32, *mut u32, *mut u32) -> *mut c_void,
}

/// Where virglrenderer is looked for: `RUST_V_VIRGL` if set, then the usual names.
fn library_candidates() -> Vec<String> {
    let mut names: Vec<String> = std::env::var("RUST_V_VIRGL").into_iter().collect();
    if cfg!(target_os = "macos") {
        for dir in ["/opt/homebrew/lib", "/usr/local/lib", "/opt/local/lib"] {
            names.push(format!("{dir}/libvirglrenderer.1.dylib"));
        }
        names.push("libvirglrenderer.1.dylib".into());
    } else if cfg!(windows) {
        names.push("virglrenderer.dll".into());
    } else {
        names.push("libvirglrenderer.so.1".into());
    }
    names
}

impl Api {
    fn load() -> Result<Self, String> {
        let mut errors = Vec::new();
        for name in library_candidates() {
            // SAFETY: virglrenderer's initialisers have no requirements.
            match unsafe { libloading::Library::new(&name) } {
                Ok(lib) => return Self::bind(lib).map_err(|e| format!("{name}: {e}")),
                Err(e) => errors.push(e.to_string()),
            }
        }
        Err(format!("virglrenderer was not found ({})", errors.join("; ")))
    }

    fn bind(lib: libloading::Library) -> Result<Self, libloading::Error> {
        macro_rules! sym {
            ($name:literal) => {
                // SAFETY: the types above are the C declarations in virglrenderer.h.
                *unsafe { lib.get(concat!($name, "\0").as_bytes())? }
            };
        }
        Ok(Api {
            init: sym!("virgl_renderer_init"),
            poll: sym!("virgl_renderer_poll"),
            reset: sym!("virgl_renderer_reset"),
            get_cap_set: sym!("virgl_renderer_get_cap_set"),
            fill_caps: sym!("virgl_renderer_fill_caps"),
            context_create: sym!("virgl_renderer_context_create"),
            context_destroy: sym!("virgl_renderer_context_destroy"),
            ctx_attach_resource: sym!("virgl_renderer_ctx_attach_resource"),
            ctx_detach_resource: sym!("virgl_renderer_ctx_detach_resource"),
            resource_create: sym!("virgl_renderer_resource_create"),
            resource_unref: sym!("virgl_renderer_resource_unref"),
            resource_attach_iov: sym!("virgl_renderer_resource_attach_iov"),
            resource_detach_iov: sym!("virgl_renderer_resource_detach_iov"),
            resource_get_info: sym!("virgl_renderer_resource_get_info"),
            transfer_read_iov: sym!("virgl_renderer_transfer_read_iov"),
            transfer_write_iov: sym!("virgl_renderer_transfer_write_iov"),
            submit_cmd: sym!("virgl_renderer_submit_cmd"),
            create_fence: sym!("virgl_renderer_create_fence"),
            get_cursor_data: sym!("virgl_renderer_get_cursor_data"),
            _lib: lib,
        })
    }
}

enum Msg {
    Activate(Activation),
    Notify(usize),
    Reset,
}

/// The device, as seen from the transport: it hands everything to the renderer thread.
pub struct VirglGpu {
    tx: Sender<Msg>,
    shared: Arc<Mutex<Shared>>,
    capsets: u32,
}

impl VirglGpu {
    /// Starts the renderer thread; fails if virglrenderer cannot be loaded or initialised.
    pub fn new(display: Arc<Display>) -> Result<(Self, GpuHandle), String> {
        let shared = Arc::new(Mutex::new(Shared { events_read: 0, active: None }));
        let handle = GpuHandle { display: display.clone(), shared: shared.clone() };
        let (tx, rx) = channel();
        let (ready_tx, ready_rx) = channel();
        std::thread::Builder::new()
            .name("virgl".into())
            .spawn(move || match Renderer::start(display) {
                Ok(mut renderer) => {
                    let _ = ready_tx.send(Ok(renderer.capsets.len() as u32));
                    renderer.run(rx);
                }
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                }
            })
            .map_err(|e| e.to_string())?;
        let capsets = ready_rx.recv().map_err(|_| "the virgl thread failed".to_string())??;
        Ok((VirglGpu { tx, shared, capsets }, handle))
    }
}

impl VirtioDevice for VirglGpu {
    fn device_type(&self) -> u16 {
        device_type::GPU
    }

    fn features(&self) -> u64 {
        FEATURE_VIRGL
    }

    fn queue_sizes(&self) -> Vec<u16> {
        vec![256, 16]
    }

    fn read_config(&self, offset: usize, data: &mut [u8]) {
        let mut config = [0u8; 16];
        config[0..4].copy_from_slice(&self.shared.lock().unwrap().events_read.to_le_bytes());
        config[8..12].copy_from_slice(&1u32.to_le_bytes()); // num_scanouts
        config[12..16].copy_from_slice(&self.capsets.to_le_bytes());
        read_config_bytes(&config, offset, data);
    }

    fn write_config(&mut self, offset: usize, data: &[u8]) {
        if offset == 4 && data.len() == 4 {
            self.shared.lock().unwrap().events_read &= !u32::from_le_bytes(data.try_into().unwrap());
        }
    }

    fn activate(&mut self, activation: Activation) {
        self.shared.lock().unwrap().active = Some(activation.clone());
        let _ = self.tx.send(Msg::Activate(activation));
    }

    fn notify(&mut self, queue: usize) {
        let _ = self.tx.send(Msg::Notify(queue));
    }

    fn reset(&mut self) {
        let mut s = self.shared.lock().unwrap();
        s.active = None;
        s.events_read = 0;
        let _ = self.tx.send(Msg::Reset);
    }

    fn pci_class(&self) -> u32 {
        0x03_80_00
    }
}

struct Resource {
    /// Guest memory backing the resource, as host pointers (virglrenderer keeps the pointer).
    iovs: Vec<Iovec>,
    /// The same, as guest (address, length) pieces.
    backing: Vec<(u64, u32)>,
}

/// A fenced command whose response waits for the fence.
struct Pending {
    fence: u32,
    head: u16,
    written: u32,
}

/// State owned by the renderer thread.
struct Renderer {
    api: Api,
    display: Arc<Display>,
    /// Size of the frame last shown.
    shown: (u32, u32),
    /// Boxed: its address is the callback cookie.
    cookie: Box<Cookie>,
    /// (id, max version, max size) of each capability set.
    capsets: Vec<(u32, u32, u32)>,
    active: Option<Activation>,
    resources: HashMap<u32, Resource>,
    /// Resource on scanout 0, the shown part of it, and its pixel format.
    scanout: Option<(u32, Rect, Format)>,
    pending: Vec<Pending>,
    /// Request buffer (u64s, so command streams are suitably aligned for virglrenderer).
    request: Vec<u64>,
    readback: Vec<u8>,
    /// SUBMIT_3D commands seen (for RV_GPU_TRACE).
    submits: u64,
    /// Scanout resource whose read-back failed (reported once).
    unreadable: Option<u32>,
}

impl Renderer {
    fn start(display: Arc<Display>) -> Result<Self, String> {
        let api = Api::load()?;
        #[cfg(target_os = "macos")]
        cgl::install_shader_shim().map_err(|e| format!("OpenGL shader shim: {e}"))?;
        let cookie = Box::new(Cookie::default());
        let callbacks = Box::leak(Box::new(Callbacks {
            version: 1,
            write_fence: Some(write_fence),
            create_gl_context: None,
            destroy_gl_context: None,
            make_current: None,
        }));
        #[cfg(target_os = "macos")]
        {
            callbacks.create_gl_context = Some(cgl::create);
            callbacks.destroy_gl_context = Some(cgl::destroy);
            callbacks.make_current = Some(cgl::make_current);
        }
        // With EGL: surfaceless first (works without a display server or GPU device), then
        // the default platform. Without (macOS): our contexts.
        let attempts: &[c_int] =
            if cfg!(target_os = "macos") { &[0] } else { &[USE_EGL | USE_SURFACELESS, USE_EGL] };
        let cookie_ptr = &*cookie as *const Cookie as *mut c_void;
        let ok = attempts
            .iter()
            // SAFETY: the cookie and callbacks live as long as the process uses virglrenderer.
            .any(|&flags| unsafe { (api.init)(cookie_ptr, flags, callbacks) } == 0);
        if !ok {
            return Err("virglrenderer could not set up OpenGL".into());
        }
        let mut capsets = Vec::new();
        for id in [1, 2] {
            let (mut version, mut size) = (0, 0);
            // SAFETY: plain out-parameters.
            unsafe { (api.get_cap_set)(id, &mut version, &mut size) };
            if version > 0 {
                capsets.push((id, version, size));
            }
        }
        if capsets.is_empty() {
            return Err("virglrenderer offers no capability sets".into());
        }
        Ok(Renderer {
            api,
            display,
            shown: (0, 0),
            cookie,
            capsets,
            active: None,
            resources: HashMap::new(),
            scanout: None,
            pending: Vec::new(),
            request: Vec::new(),
            readback: Vec::new(),
            submits: 0,
            unreadable: None,
        })
    }

    fn run(&mut self, rx: Receiver<Msg>) {
        loop {
            let msg = if self.pending.is_empty() {
                match rx.recv() {
                    Ok(m) => Some(m),
                    Err(_) => return,
                }
            } else {
                match rx.recv_timeout(FENCE_POLL) {
                    Ok(m) => Some(m),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            };
            match msg {
                Some(Msg::Activate(a)) => self.active = Some(a),
                Some(Msg::Notify(queue)) => self.process(queue),
                Some(Msg::Reset) => self.reset(),
                None => {}
            }
            if !self.pending.is_empty() {
                // SAFETY: called on the renderer thread.
                unsafe { (self.api.poll)() };
                self.complete_fences();
            }
        }
    }

    fn reset(&mut self) {
        self.active = None;
        self.pending.clear();
        self.scanout = None;
        // SAFETY: destroys every context and resource; their iovecs are dropped after.
        unsafe { (self.api.reset)() };
        self.resources.clear();
    }

    /// Answers the fenced commands whose fences have signalled.
    fn complete_fences(&mut self) {
        let Some(a) = &self.active else { return };
        let signalled = self.cookie.signalled.load(Ordering::Acquire);
        let mut q = a.queues[CONTROLQ].lock().unwrap();
        let mut any = false;
        self.pending.retain(|p| {
            if signalled.wrapping_sub(p.fence) as i32 >= 0 {
                let _ = q.push(&a.mem, p.head, p.written);
                any = true;
                false
            } else {
                true
            }
        });
        if any && q.needs_interrupt(&a.mem).unwrap_or(true) {
            a.irq.queue();
        }
    }

    fn process(&mut self, queue: usize) {
        let Some(a) = self.active.clone() else { return };
        let mut answered = false;
        loop {
            let chain = match a.queues[queue].lock().unwrap().pop(&a.mem) {
                Ok(Some(chain)) => chain,
                Ok(None) => break,
                Err(e) => {
                    eprintln!("virtio-gpu: queue {queue}: {e}\r");
                    break;
                }
            };
            let len = self.read_request(&a.mem, &chain);
            let written = match queue {
                CONTROLQ => match self.control(&a, &chain, len) {
                    Some(written) => written,
                    None => continue, // waits for its fence
                },
                CURSORQ => {
                    self.cursor(len);
                    0
                }
                _ => 0,
            };
            let _ = a.queues[queue].lock().unwrap().push(&a.mem, chain.head, written);
            answered = true;
        }
        if answered && a.queues[queue].lock().unwrap().needs_interrupt(&a.mem).unwrap_or(true) {
            a.irq.queue();
        }
    }

    /// Reads the chain's request into `self.request`; returns its length in bytes.
    fn read_request(&mut self, mem: &GuestMemory, chain: &Chain) -> usize {
        let len = chain.readable_len().min(MAX_REQUEST) as usize;
        self.request.clear();
        self.request.resize(len.div_ceil(8), 0);
        chain.reader(mem).read(&mut request_bytes(&mut self.request)[..len]).unwrap_or(0)
    }

    /// Handles a control command; returns the bytes written into the chain, or `None` if the
    /// response is held back until its fence signals.
    fn control(&mut self, a: &Activation, chain: &Chain, len: usize) -> Option<u32> {
        let mut req = std::mem::take(&mut self.request);
        let bytes = &mut request_bytes(&mut req)[..len];
        let (kind, payload) =
            if len < HEADER { (cmd::ERR_UNSPEC, vec![]) } else { self.command(&a.mem, bytes) };
        if trace() {
            let t = u32_at(bytes, 0);
            if t == cmd3d::SUBMIT_3D {
                self.submits += 1;
                if self.submits.is_power_of_two() {
                    eprintln!("virtio-gpu: {} 3D command streams\r", self.submits);
                }
            }
            if !matches!(t, cmd::TRANSFER_TO_HOST_2D | cmd::RESOURCE_FLUSH | cmd3d::SUBMIT_3D)
                || kind >= cmd::ERR_UNSPEC
            {
                eprintln!("virtio-gpu: {t:#06x} -> {kind:#06x}\r");
            }
        }
        let mut resp = vec![0u8; HEADER];
        resp[0..4].copy_from_slice(&kind.to_le_bytes());
        let fenced = len >= HEADER && u32_at(bytes, 4) & FLAG_FENCE != 0;
        let fence = u64_at(bytes, 8);
        if fenced {
            resp[4..8].copy_from_slice(&FLAG_FENCE.to_le_bytes());
            resp[8..20].copy_from_slice(&bytes[8..20]);
        }
        let ctx_id = u32_at(bytes, 16);
        let cmd_type = u32_at(bytes, 0);
        self.request = req;
        resp.extend_from_slice(&payload);
        let written = chain.writer(&a.mem).write(&resp).unwrap_or(0) as u32;
        if fenced && kind < cmd::ERR_UNSPEC {
            // SAFETY: called on the renderer thread.
            let r = unsafe { (self.api.create_fence)(fence as u32 as c_int, ctx_id) };
            if trace() {
                eprintln!(
                    "virtio-gpu: fence {fence} for {cmd_type:#06x} -> {r}, signalled {}\r",
                    self.cookie.signalled.load(Ordering::Acquire)
                );
            }
            self.pending.push(Pending { fence: fence as u32, head: chain.head, written });
            return None;
        }
        Some(written)
    }

    fn command(&mut self, mem: &GuestMemory, req: &mut [u8]) -> (u32, Vec<u8>) {
        let kind = u32_at(req, 0);
        let ctx_id = u32_at(req, 16);
        let (_, body) = req.split_at_mut(HEADER);
        let ok = |r: c_int| if r == 0 { (cmd::OK_NODATA, vec![]) } else { (cmd::ERR_UNSPEC, vec![]) };
        let api = &self.api;
        // SAFETY (for the calls below): virglrenderer validates handles and sizes itself;
        // pointers passed are to live buffers of the stated lengths.
        match kind {
            cmd::GET_DISPLAY_INFO => (cmd::OK_DISPLAY_INFO, display_info(&self.display)),
            cmd3d::GET_CAPSET_INFO => match self.capsets.get(u32_at(body, 0) as usize) {
                Some(&(id, version, size)) => {
                    let mut info = vec![0u8; 16];
                    info[0..4].copy_from_slice(&id.to_le_bytes());
                    info[4..8].copy_from_slice(&version.to_le_bytes());
                    info[8..12].copy_from_slice(&size.to_le_bytes());
                    (cmd3d::OK_CAPSET_INFO, info)
                }
                None => (cmd::ERR_INVALID_PARAMETER, vec![]),
            },
            cmd3d::GET_CAPSET => {
                let (id, version) = (u32_at(body, 0), u32_at(body, 4));
                match self.capsets.iter().find(|c| c.0 == id) {
                    Some(&(_, max, size)) if version <= max => {
                        let mut caps = vec![0u8; size as usize];
                        unsafe { (api.fill_caps)(id, version, caps.as_mut_ptr().cast()) };
                        (cmd3d::OK_CAPSET, caps)
                    }
                    _ => (cmd::ERR_INVALID_PARAMETER, vec![]),
                }
            }
            cmd::RESOURCE_CREATE_2D => {
                let mut args = CreateArgs {
                    handle: u32_at(body, 0),
                    target: PIPE_TEXTURE_2D,
                    format: u32_at(body, 4),
                    bind: BIND_RENDER_TARGET,
                    width: u32_at(body, 8),
                    height: u32_at(body, 12),
                    depth: 1,
                    array_size: 1,
                    flags: RESOURCE_Y_0_TOP,
                    ..Default::default()
                };
                self.create_resource(&mut args)
            }
            cmd3d::RESOURCE_CREATE_3D => {
                let f = |i: usize| u32_at(body, 4 * i);
                let mut args = CreateArgs {
                    handle: f(0),
                    target: f(1),
                    format: f(2),
                    bind: f(3),
                    width: f(4),
                    height: f(5),
                    depth: f(6),
                    array_size: f(7),
                    last_level: f(8),
                    nr_samples: f(9),
                    flags: f(10),
                };
                self.create_resource(&mut args)
            }
            cmd::RESOURCE_UNREF => {
                let id = u32_at(body, 0);
                if self.resources.remove(&id).is_none() {
                    return (cmd::ERR_INVALID_RESOURCE_ID, vec![]);
                }
                // Detach first so virglrenderer drops its pointer to the (now freed) iovecs
                // only after it has stopped using them.
                unsafe {
                    (api.resource_detach_iov)(id as c_int, std::ptr::null_mut(), std::ptr::null_mut());
                    (api.resource_unref)(id);
                }
                if self.scanout.is_some_and(|(s, _, _)| s == id) {
                    self.scanout = None;
                }
                (cmd::OK_NODATA, vec![])
            }
            cmd::RESOURCE_ATTACH_BACKING => {
                let (id, count) = (u32_at(body, 0), u32_at(body, 4) as usize);
                if body.len() < 8 + 16 * count {
                    return (cmd::ERR_INVALID_PARAMETER, vec![]);
                }
                let Some(res) = self.resources.get_mut(&id) else {
                    return (cmd::ERR_INVALID_RESOURCE_ID, vec![]);
                };
                let backing: Vec<(u64, u32)> =
                    (0..count).map(|i| (u64_at(body, 8 + 16 * i), u32_at(body, 16 + 16 * i))).collect();
                let mut iovs = Vec::with_capacity(count);
                for &(addr, len) in &backing {
                    match mem.host_ptr(addr, len as u64) {
                        Ok(p) => iovs.push(Iovec { base: p.cast(), len: len as usize }),
                        Err(_) => return (cmd::ERR_INVALID_PARAMETER, vec![]),
                    }
                }
                let r = unsafe { (api.resource_attach_iov)(id as c_int, iovs.as_mut_ptr(), count as c_int) };
                if r == 0 {
                    res.iovs = iovs;
                    res.backing = backing;
                }
                ok(r)
            }
            cmd::RESOURCE_DETACH_BACKING => {
                let id = u32_at(body, 0);
                let Some(res) = self.resources.get_mut(&id) else {
                    return (cmd::ERR_INVALID_RESOURCE_ID, vec![]);
                };
                unsafe { (api.resource_detach_iov)(id as c_int, std::ptr::null_mut(), std::ptr::null_mut()) };
                res.iovs.clear();
                res.backing.clear();
                (cmd::OK_NODATA, vec![])
            }
            cmd3d::CTX_CREATE => {
                let nlen = u32_at(body, 0).min(64);
                let name = body.get(8..8 + nlen as usize).unwrap_or(&[]);
                ok(unsafe { (api.context_create)(ctx_id, name.len() as u32, name.as_ptr().cast()) })
            }
            cmd3d::CTX_DESTROY => {
                unsafe { (api.context_destroy)(ctx_id) };
                (cmd::OK_NODATA, vec![])
            }
            cmd3d::CTX_ATTACH_RESOURCE => {
                unsafe { (api.ctx_attach_resource)(ctx_id as c_int, u32_at(body, 0) as c_int) };
                (cmd::OK_NODATA, vec![])
            }
            cmd3d::CTX_DETACH_RESOURCE => {
                unsafe { (api.ctx_detach_resource)(ctx_id as c_int, u32_at(body, 0) as c_int) };
                (cmd::OK_NODATA, vec![])
            }
            cmd3d::SUBMIT_3D => {
                let size = u32_at(body, 0) as usize;
                let Some(stream) = body.get_mut(8..8 + size) else {
                    return (cmd::ERR_INVALID_PARAMETER, vec![]);
                };
                // `stream` starts 32 bytes into the u64-aligned request.
                ok(unsafe {
                    (api.submit_cmd)(stream.as_mut_ptr().cast(), ctx_id as c_int, (size / 4) as c_int)
                })
            }
            cmd::TRANSFER_TO_HOST_2D => {
                let (r, offset, id) = (rect_at(body, 0), u64_at(body, 16), u32_at(body, 24));
                let mut b = VirglBox { x: r.x, y: r.y, z: 0, w: r.width, h: r.height, d: 1 };
                ok(unsafe {
                    (api.transfer_write_iov)(id, 0, 0, 0, 0, &mut b, offset, std::ptr::null_mut(), 0)
                })
            }
            cmd3d::TRANSFER_TO_HOST_3D | cmd3d::TRANSFER_FROM_HOST_3D => {
                let f = |i: usize| u32_at(body, 4 * i);
                let mut b = VirglBox { x: f(0), y: f(1), z: f(2), w: f(3), h: f(4), d: f(5) };
                let offset = u64_at(body, 24);
                let (id, level, stride, layer_stride) = (f(8), f(9), f(10), f(11));
                let r = if kind == cmd3d::TRANSFER_TO_HOST_3D {
                    unsafe {
                        (api.transfer_write_iov)(
                            id,
                            ctx_id,
                            level as c_int,
                            stride,
                            layer_stride,
                            &mut b,
                            offset,
                            std::ptr::null_mut(),
                            0,
                        )
                    }
                } else {
                    let r = unsafe {
                        (api.transfer_read_iov)(
                            id,
                            ctx_id,
                            level,
                            stride,
                            layer_stride,
                            &mut b,
                            offset,
                            std::ptr::null_mut(),
                            0,
                        )
                    };
                    // virglrenderer wrote the backing directly.
                    if let Some(res) = self.resources.get(&id) {
                        for &(addr, len) in &res.backing {
                            mem.note_written(addr, len as u64);
                        }
                    }
                    r
                };
                ok(r)
            }
            cmd::SET_SCANOUT => {
                let (r, scanout, id) = (rect_at(body, 0), u32_at(body, 16), u32_at(body, 20));
                if scanout != 0 {
                    return (cmd::ERR_INVALID_SCANOUT_ID, vec![]);
                }
                if id == 0 {
                    self.scanout = None;
                    self.shown = (0, 0);
                    self.display.update(|f| {
                        f.width = 0;
                        f.height = 0;
                        f.pixels.clear();
                    });
                    return (cmd::OK_NODATA, vec![]);
                }
                let mut info = ResourceInfo::default();
                if !self.resources.contains_key(&id)
                    || unsafe { (api.resource_get_info)(id as c_int, &mut info) } != 0
                {
                    return (cmd::ERR_INVALID_RESOURCE_ID, vec![]);
                }
                let Some(format) = Format::from_virtio(info.virgl_format) else {
                    return (cmd::ERR_INVALID_PARAMETER, vec![]);
                };
                if r.width == 0
                    || r.height == 0
                    || r.x.saturating_add(r.width) > info.width
                    || r.y.saturating_add(r.height) > info.height
                {
                    return (cmd::ERR_INVALID_PARAMETER, vec![]);
                }
                if trace() {
                    eprintln!(
                        "virtio-gpu: scanout {id}: {}x{} format {} flags {:#x}, showing {}x{}+{}+{}\r",
                        info.width, info.height, info.virgl_format, info.flags, r.width, r.height, r.x, r.y
                    );
                }
                self.scanout = Some((id, r, format));
                self.present(r);
                (cmd::OK_NODATA, vec![])
            }
            cmd::RESOURCE_FLUSH => {
                let (r, id) = (rect_at(body, 0), u32_at(body, 16));
                if !self.resources.contains_key(&id) {
                    return (cmd::ERR_INVALID_RESOURCE_ID, vec![]);
                }
                if self.scanout.is_some_and(|(s, _, _)| s == id) {
                    self.present(r);
                }
                (cmd::OK_NODATA, vec![])
            }
            _ => (cmd::ERR_UNSPEC, vec![]),
        }
    }

    fn create_resource(&mut self, args: &mut CreateArgs) -> (u32, Vec<u8>) {
        if args.handle == 0 || self.resources.contains_key(&args.handle) {
            return (cmd::ERR_INVALID_RESOURCE_ID, vec![]);
        }
        // SAFETY: no backing yet; it is attached later.
        if unsafe { (self.api.resource_create)(args, std::ptr::null_mut(), 0) } != 0 {
            return (cmd::ERR_INVALID_PARAMETER, vec![]);
        }
        self.resources.insert(args.handle, Resource { iovs: Vec::new(), backing: Vec::new() });
        (cmd::OK_NODATA, vec![])
    }

    /// Reads rectangle `r` (resource coordinates) of the scanout back from the host GPU and
    /// shows it.
    fn present(&mut self, mut r: Rect) {
        let Some((id, view, format)) = self.scanout else { return };
        let resized = self.shown != (view.width, view.height);
        if resized {
            // A new mode: refresh all of it.
            r = view;
            self.shown = (view.width, view.height);
        }
        let x0 = r.x.max(view.x);
        let y0 = r.y.max(view.y);
        let x1 = r.x.saturating_add(r.width).min(view.x + view.width);
        let y1 = r.y.saturating_add(r.height).min(view.y + view.height);
        if x0 >= x1 || y0 >= y1 {
            return;
        }
        let (w, h) = (x1 - x0, y1 - y0);
        self.readback.resize((4 * w * h) as usize, 0);
        let mut iov = Iovec { base: self.readback.as_mut_ptr().cast(), len: self.readback.len() };
        let mut b = VirglBox { x: x0, y: y0, z: 0, w, h, d: 1 };
        // SAFETY: the buffer holds `w * h` pixels at a stride of `4 * w`.
        let r = unsafe { (self.api.transfer_read_iov)(id, 0, 0, 4 * w, 0, &mut b, 0, &mut iov, 1) };
        if r != 0 {
            if self.unreadable != Some(id) {
                eprintln!("virtio-gpu: cannot read back scanout resource {id} (error {r})\r");
                self.unreadable = Some(id);
            }
            return;
        }
        let pixels = &self.readback;
        self.display.update(|frame| {
            if resized || frame.width != view.width || frame.height != view.height {
                frame.width = view.width;
                frame.height = view.height;
                frame.pixels = vec![0; (view.width * view.height) as usize];
            }
            for y in y0..y1 {
                let src = ((y - y0) * w) as usize * 4;
                let dst = ((y - view.y) * view.width + (x0 - view.x)) as usize;
                let row = &pixels[src..src + 4 * w as usize];
                for (i, px) in row.as_chunks::<4>().0.iter().enumerate() {
                    frame.pixels[dst + i] = format.to_argb(u32::from_le_bytes(*px)) & 0x00ff_ffff;
                }
            }
        });
    }

    fn cursor(&mut self, len: usize) {
        let req = &request_bytes(&mut self.request)[..len];
        let kind = u32_at(req, 0);
        let body = &req[HEADER.min(req.len())..];
        let (x, y) = (u32_at(body, 4), u32_at(body, 8));
        match kind {
            cmd::UPDATE_CURSOR => {
                let (id, hot_x, hot_y) = (u32_at(body, 16), u32_at(body, 20), u32_at(body, 24));
                let mut image = None;
                if id != 0 && self.resources.contains_key(&id) {
                    let (mut w, mut h) = (0, 0);
                    // SAFETY: returns a malloc'd `w * h` array of 32-bit pixels (or null).
                    let data = unsafe { (self.api.get_cursor_data)(id, &mut w, &mut h) };
                    if !data.is_null() {
                        let src = unsafe { std::slice::from_raw_parts(data as *const u32, (w * h) as usize) };
                        let mut img = vec![0u32; (CURSOR_SIZE * CURSOR_SIZE) as usize];
                        for cy in 0..CURSOR_SIZE.min(h) {
                            for cx in 0..CURSOR_SIZE.min(w) {
                                img[(cy * CURSOR_SIZE + cx) as usize] = src[(cy * w + cx) as usize];
                            }
                        }
                        unsafe { free(data) };
                        image = Some(img);
                    }
                }
                self.display.update_cursor(|c| {
                    c.visible = image.is_some();
                    c.image = image.unwrap_or_default();
                    (c.hot_x, c.hot_y, c.x, c.y) = (hot_x, hot_y, x, y);
                });
            }
            cmd::MOVE_CURSOR => self.display.update_cursor(|c| (c.x, c.y) = (x, y)),
            _ => {}
        }
    }
}

fn request_bytes(words: &mut [u64]) -> &mut [u8] {
    // SAFETY: any bytes are valid u64s and vice versa.
    unsafe { std::slice::from_raw_parts_mut(words.as_mut_ptr().cast(), words.len() * 8) }
}

#[cfg(test)]
mod tests {
    use super::fix_glsl;

    #[test]
    fn glsl_for_core_410() {
        let src = "#version 130\n#extension GL_ARB_fragment_coord_conventions : require\n\
                   #extension GL_EXT_texture_array : require\nlayout(origin_upper_left) in vec4 gl_FragCoord;\n";
        assert_eq!(
            fix_glsl(src),
            "#version 410\n\n#extension GL_EXT_texture_array : require\nlayout(origin_upper_left) in vec4 gl_FragCoord;\n"
        );
        assert_eq!(fix_glsl("#version 430\nvoid main(){}"), "#version 430\nvoid main(){}");
        assert_eq!(fix_glsl("#version 300 es\n"), "#version 300 es\n");
    }
}
