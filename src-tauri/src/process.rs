//! Background helper processes (model servers, speech-to-text): hidden, below-normal priority, and
//! tied to CursorAssist through a kill-on-close job object so they never outlive it, even on a crash.

use std::os::windows::io::AsRawHandle;
use std::os::windows::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::OnceLock;

use windows::Win32::Foundation::HANDLE;
use windows::Win32::Security::Cryptography::{BCryptGenRandom, BCRYPT_USE_SYSTEM_PREFERRED_RNG};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const BELOW_NORMAL_PRIORITY_CLASS: u32 = 0x0000_4000;

static JOB: OnceLock<Option<isize>> = OnceLock::new();

/// Spawns `command` hidden and at below-normal priority, with stderr going to `log`.
pub fn spawn(mut command: Command, log: std::fs::File) -> std::io::Result<Child> {
    let child = command
        .creation_flags(CREATE_NO_WINDOW | BELOW_NORMAL_PRIORITY_CLASS)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log)
        .spawn()?;
    if let Some(job) = *JOB.get_or_init(|| create_job().map_err(|e| eprintln!("[process] no job object: {e}")).ok()) {
        unsafe {
            let _ = AssignProcessToJobObject(HANDLE(job as *mut _), HANDLE(child.as_raw_handle()));
        }
    }
    Ok(child)
}

/// HTTP client for talking to helper servers on this machine. It ignores system and environment
/// proxy settings, so local traffic is never routed through a (corporate) proxy.
pub fn local_http_client() -> reqwest::Client {
    reqwest::Client::builder().no_proxy().build().expect("build HTTP client")
}

/// A random 256-bit key (hex) from the system CSPRNG. Helper servers are started with one and
/// reject requests without it, so other programs, and web pages in a browser (llama-server allows
/// any origin), can't use them. It only ever lives in memory.
pub fn random_key() -> String {
    let mut bytes = [0u8; 32];
    unsafe {
        BCryptGenRandom(None, &mut bytes, BCRYPT_USE_SYSTEM_PREFERRED_RNG)
            .ok()
            .expect("system random number generator unavailable");
    }
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn create_job() -> windows::core::Result<isize> {
    unsafe {
        let job = CreateJobObjectW(None, None)?;
        let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const _,
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )?;
        // Never closed: the OS closes it when CursorAssist exits, which kills every helper.
        Ok(job.0 as isize)
    }
}
