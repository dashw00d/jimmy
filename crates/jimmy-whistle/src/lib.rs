//! Rust bindings to the Cactus 4.1.4 Whistle ABI.
use anyhow::{Context, Result, bail, ensure};
use jimmy_core::{Recognition, Stt};
use jimmy_proto::Transcript;
use std::{
    ffi::{CStr, CString, c_char},
    path::Path,
    sync::{
        Mutex, OnceLock, TryLockError,
        atomic::{AtomicUsize, Ordering},
    },
};

type Load = unsafe extern "C" fn(*const u8, u64) -> i32;
type Transcribe = unsafe extern "C" fn(
    *const f32,
    i32,
    *const c_char,
    *const c_char,
    i32,
    *mut c_char,
    i32,
) -> i32;
type LastError = unsafe extern "C" fn() -> *const c_char;
type Models = unsafe extern "C" fn() -> i32;
type SetThreads = unsafe extern "C" fn(i32);

struct Api {
    load: Load,
    models: Models,
    transcribe: Transcribe,
    last_error: LastError,
    set_threads: SetThreads,
    #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
    _library: libloading::Library,
}

#[cfg(not(all(target_os = "macos", target_arch = "x86_64")))]
impl Api {
    fn open() -> Result<Self> {
        unsafe extern "C" {
            fn cactus_load(data: *const u8, count: u64) -> i32;
            fn cactus_transcribe(
                data: *const f32,
                count: i32,
                language: *const c_char,
                keywords: *const c_char,
                timestamps: i32,
                out: *mut c_char,
                capacity: i32,
            ) -> i32;
            fn cactus_models() -> i32;
            fn cactus_last_error() -> *const c_char;
            fn cactus_set_threads(threads: i32);
        }
        Ok(Self {
            load: cactus_load,
            models: cactus_models,
            transcribe: cactus_transcribe,
            last_error: cactus_last_error,
            set_threads: cactus_set_threads,
        })
    }
}

#[cfg(all(target_os = "macos", target_arch = "x86_64"))]
impl Api {
    fn open() -> Result<Self> {
        use std::io::Write;
        let mut file = tempfile::Builder::new().suffix(".dylib").tempfile()?;
        file.write_all(include_bytes!(concat!(env!("OUT_DIR"), "/libcactus.dylib")))?;
        // The embedded vendor wheel was verified by the build script. The Library
        // stays alive in the process-wide Runtime for the lifetime of all symbols.
        unsafe {
            let library = libloading::Library::new(file.path())?;
            Ok(Self {
                load: *library.get(b"cactus_load\0")?,
                models: *library.get(b"cactus_models\0")?,
                transcribe: *library.get(b"cactus_transcribe\0")?,
                last_error: *library.get(b"cactus_last_error\0")?,
                set_threads: *library.get(b"cactus_set_threads\0")?,
                _library: library,
            })
        }
    }
}

impl Api {
    fn error(&self) -> String {
        // The vendor returns a global NUL-terminated message under the runtime lock.
        let ptr = unsafe { (self.last_error)() };
        if ptr.is_null() {
            return "Cactus returned an unspecified error".into();
        }
        unsafe { CStr::from_ptr(ptr) }
            .to_string_lossy()
            .into_owned()
    }
}

pub const VERSION: &str = "4.1.4";
pub const MODEL: &str = "whistle.cact";
pub const SHA256: &str = "b6e02f048568ac5d01a2042556c658061e699acbc0aa2a1439f52f3d461dffeb";

struct Runtime {
    api: Api,
    _weights: Vec<u8>,
}

// The pinned engine exposes one global model and no independent C handles.
// Serializing calls also protects its scratch space and last-error pointer.
static RUNTIME: Mutex<()> = Mutex::new(());
static REQUIRED: AtomicUsize = AtomicUsize::new(0);
static MODEL_STATE: OnceLock<Runtime> = OnceLock::new();

#[derive(Clone, Copy)]
pub struct Whistle;

impl Whistle {
    pub fn load(path: &Path) -> Result<Self> {
        Self::load_with(|| std::fs::read(path).with_context(|| format!("read {}", path.display())))
    }

    pub fn load_with(read: impl FnOnce() -> Result<Vec<u8>>) -> Result<Self> {
        let _guard = RUNTIME
            .lock()
            .map_err(|_| anyhow::anyhow!("Whistle lock poisoned"))?;
        ensure!(
            MODEL_STATE.get().is_none(),
            "Whistle already has a model loaded; clone the existing handle"
        );
        let weights = read()?;
        let api = Api::open()?;
        if let Some(threads) = std::env::var("JIMMY_WHISTLE_THREADS")
            .ok()
            .and_then(|value| value.parse::<i32>().ok())
        {
            unsafe { (api.set_threads)(threads) };
        }
        // The backing bytes live as long as the loaded process-wide runtime.
        let status = unsafe { (api.load)(weights.as_ptr(), weights.len() as u64) };
        if status < 0 {
            bail!("{}", api.error());
        }
        ensure!(
            unsafe { (api.models)() } & 2 != 0,
            "Cactus artifact does not contain a speech model"
        );
        unsafe { (api.set_threads)(1) };
        let _ = MODEL_STATE.set(Runtime {
            api,
            _weights: weights,
        });
        Ok(Self)
    }

    fn recognize(&self, audio: &[i16], options: &Recognition) -> Result<Transcript> {
        if audio.is_empty() {
            return Ok(Transcript::default());
        }
        ensure!(
            audio.len() <= 30 * 16_000,
            "Whistle accepts at most 30 seconds per pass"
        );
        let language = options.language.as_deref().map(CString::new).transpose()?;
        let keywords = (!options.keywords.is_empty())
            .then(|| CString::new(options.keywords.join("\n")))
            .transpose()?;
        let audio: Vec<f32> = audio
            .iter()
            .map(|&sample| f32::from(sample) / 32768.0)
            .collect();
        let mut out = vec![0u8; 256 * 1024];
        let runtime = MODEL_STATE.get().context("Whistle has not been loaded")?;
        // Audio, strings and output remain valid for the call. Counts are bounded above.
        let status = unsafe {
            (runtime.api.transcribe)(
                audio.as_ptr(),
                audio.len() as i32,
                language.as_ref().map_or(std::ptr::null(), |s| s.as_ptr()),
                keywords.as_ref().map_or(std::ptr::null(), |s| s.as_ptr()),
                1,
                out.as_mut_ptr().cast(),
                out.len() as i32,
            )
        };
        if status < 0 {
            bail!("{}", runtime.api.error());
        }
        let json = CStr::from_bytes_until_nul(&out).context("unterminated Whistle response")?;
        serde_json::from_slice(json.to_bytes()).context("decode Whistle response")
    }
}

impl Stt for Whistle {
    fn transcribe(&mut self, audio: &[i16], options: &Recognition) -> Result<Transcript, String> {
        REQUIRED.fetch_add(1, Ordering::AcqRel);
        let guard = RUNTIME.lock();
        REQUIRED.fetch_sub(1, Ordering::AcqRel);
        let _guard = guard.map_err(|_| "Whistle lock poisoned".to_owned())?;
        self.recognize(audio, options).map_err(|e| format!("{e:#}"))
    }

    fn try_partial(
        &mut self,
        audio: &[i16],
        options: &Recognition,
    ) -> Result<Option<Transcript>, String> {
        if REQUIRED.load(Ordering::Acquire) > 0 {
            return Ok(None);
        }
        let _guard = match RUNTIME.try_lock() {
            Ok(guard) => guard,
            Err(TryLockError::WouldBlock) => return Ok(None),
            Err(TryLockError::Poisoned(_)) => return Err("Whistle lock poisoned".into()),
        };
        self.recognize(audio, options)
            .map(Some)
            .map_err(|e| format!("{e:#}"))
    }
}
