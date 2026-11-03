//! dsync#136 — the Npcap API, loaded at RUN time (Windows only).
//!
//! The `pcap` crate links `wpcap` at build time (`#[link(name = "wpcap")]`, pcap 2.4
//! `src/raw.rs`) and has no dynamic mode. Every application that links dsync-ptp then needed the
//! Npcap SDK's `wpcap.lib` to build and its own `/DELAYLOAD:wpcap.dll` to even START on a PC
//! without Npcap. This module replaces it: `wpcap.dll` is loaded on first use with
//! `LoadLibraryExW` and the functions we use are resolved with `GetProcAddress`. No SDK at build
//! time, no linker flag in the app, and a PC without Npcap gets a typed [`NpcapMissing`] error.
//!
//! The one owner of the Npcap API: the library's PTP capture (`net_pcap`) and the service's NTP
//! capture both go through [`devices`] and [`Capture`].

use log::info;
use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::ptr::NonNull;
use std::sync::OnceLock;

/// The Npcap runtime (`wpcap.dll`, or one of its functions) could not be loaded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NpcapMissing {
    /// What failed: the paths tried and the Windows error, or the missing function.
    pub detail: String,
}

impl std::fmt::Display for NpcapMissing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the Npcap runtime is not installed or not loadable ({}); install Npcap from \
             https://npcap.com",
            self.detail
        )
    }
}

impl std::error::Error for NpcapMissing {}

/// A pcap call failed; `0` is the libpcap error text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PcapError(pub String);

impl std::fmt::Display for PcapError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Npcap: {}", self.0)
    }
}

impl std::error::Error for PcapError {}

// ---- the C ABI (pcap.h; Windows: `long` is 32-bit) ----

const PCAP_ERRBUF_SIZE: usize = 256;
/// `PCAP_TSTAMP_HOST_HIPREC`: `KeQuerySystemTimePrecise()`, synchronized with the system clock.
const PCAP_TSTAMP_HOST_HIPREC: c_int = 2;
const PCAP_NETMASK_UNKNOWN: u32 = 0xffff_ffff;
const AF_INET: u16 = 2;
const AF_INET6: u16 = 23;

#[repr(C)]
struct RawIf {
    next: *mut RawIf,
    name: *mut c_char,
    description: *mut c_char,
    addresses: *mut RawAddr,
    flags: u32,
}

#[repr(C)]
struct RawAddr {
    next: *mut RawAddr,
    addr: *mut RawSockaddr,
    netmask: *mut RawSockaddr,
    broadaddr: *mut RawSockaddr,
    dstaddr: *mut RawSockaddr,
}

#[repr(C)]
struct RawSockaddr {
    family: u16,
    data: [u8; 14],
}

/// `struct timeval` on Windows: two `long`s (32-bit).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Timeval {
    pub tv_sec: i32,
    pub tv_usec: i32,
}

#[repr(C)]
struct RawPkthdr {
    ts: Timeval,
    caplen: u32,
    len: u32,
}

#[repr(C)]
struct BpfProgram {
    bf_len: u32,
    bf_insns: *mut c_void,
}

type PcapT = c_void;

/// The resolved functions. `extern "C"` = the x64 calling convention wpcap.dll exports.
struct Api {
    findalldevs: unsafe extern "C" fn(*mut *mut RawIf, *mut c_char) -> c_int,
    freealldevs: unsafe extern "C" fn(*mut RawIf),
    create: unsafe extern "C" fn(*const c_char, *mut c_char) -> *mut PcapT,
    set_snaplen: unsafe extern "C" fn(*mut PcapT, c_int) -> c_int,
    set_promisc: unsafe extern "C" fn(*mut PcapT, c_int) -> c_int,
    set_timeout: unsafe extern "C" fn(*mut PcapT, c_int) -> c_int,
    set_immediate_mode: unsafe extern "C" fn(*mut PcapT, c_int) -> c_int,
    set_tstamp_type: unsafe extern "C" fn(*mut PcapT, c_int) -> c_int,
    activate: unsafe extern "C" fn(*mut PcapT) -> c_int,
    compile: unsafe extern "C" fn(*mut PcapT, *mut BpfProgram, *const c_char, c_int, u32) -> c_int,
    setfilter: unsafe extern "C" fn(*mut PcapT, *mut BpfProgram) -> c_int,
    freecode: unsafe extern "C" fn(*mut BpfProgram),
    next_ex: unsafe extern "C" fn(*mut PcapT, *mut *mut RawPkthdr, *mut *const u8) -> c_int,
    geterr: unsafe extern "C" fn(*mut PcapT) -> *mut c_char,
    close: unsafe extern "C" fn(*mut PcapT),
}

/// The Npcap API, loaded once per process. Every call after a failed load returns the same error.
fn api() -> Result<&'static Api, NpcapMissing> {
    static API: OnceLock<Result<Api, NpcapMissing>> = OnceLock::new();
    API.get_or_init(load).as_ref().map_err(Clone::clone)
}

/// Whether the Npcap runtime loads on this PC (loads it on the first call).
pub fn available() -> Result<(), NpcapMissing> {
    api().map(|_| ())
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// `%SystemRoot%\System32\Npcap`: where the Npcap installer puts `wpcap.dll` and `Packet.dll`.
fn npcap_dir() -> Option<String> {
    use windows::Win32::System::SystemInformation::GetSystemDirectoryW;
    let mut buf = [0u16; 260];
    // SAFETY: a valid, writable buffer; the call writes at most its length.
    let n = unsafe { GetSystemDirectoryW(Some(&mut buf)) } as usize;
    (n > 0 && n < buf.len()).then(|| format!("{}\\Npcap", String::from_utf16_lossy(&buf[..n])))
}

fn load() -> Result<Api, NpcapMissing> {
    use windows::core::PCWSTR;
    use windows::Win32::System::LibraryLoader::{
        LoadLibraryExW, LOAD_LIBRARY_FLAGS, LOAD_WITH_ALTERED_SEARCH_PATH,
    };

    // The Npcap directory first, with its own directory searched for `Packet.dll`; then the
    // default search (a WinPcap-compatible install puts `wpcap.dll` in System32).
    let mut attempts: Vec<(String, LOAD_LIBRARY_FLAGS)> = Vec::new();
    if let Some(dir) = npcap_dir() {
        attempts.push((format!("{dir}\\wpcap.dll"), LOAD_WITH_ALTERED_SEARCH_PATH));
    }
    attempts.push(("wpcap.dll".to_string(), LOAD_LIBRARY_FLAGS(0)));

    let mut failures = Vec::new();
    let module = attempts.iter().find_map(|(path, flags)| {
        let w = wide(path);
        // SAFETY: `w` is a NUL-terminated UTF-16 path that outlives the call.
        match unsafe { LoadLibraryExW(PCWSTR(w.as_ptr()), None, *flags) } {
            Ok(m) => {
                info!("[Npcap] loaded {}", path);
                Some(m)
            }
            Err(e) => {
                failures.push(format!("{path}: {e}"));
                None
            }
        }
    });
    let Some(module) = module else {
        return Err(NpcapMissing {
            detail: failures.join("; "),
        });
    };
    // The module stays loaded for the process lifetime: the function pointers below point into it.

    type FarProc = unsafe extern "system" fn() -> isize;
    macro_rules! resolve {
        ($name:literal, $ty:ty) => {{
            use windows::core::PCSTR;
            use windows::Win32::System::LibraryLoader::GetProcAddress;
            // SAFETY: a NUL-terminated ASCII name; `module` is a loaded module handle.
            let f = unsafe { GetProcAddress(module, PCSTR(concat!($name, "\0").as_ptr())) }
                .ok_or_else(|| NpcapMissing {
                    detail: format!("wpcap.dll has no {}", $name),
                })?;
            // SAFETY: the libpcap function `$name` has exactly the signature of the field it is
            // assigned to (pcap.h); a function pointer transmute between same-size pointers.
            unsafe { std::mem::transmute::<FarProc, $ty>(f) }
        }};
    }

    Ok(Api {
        findalldevs: resolve!(
            "pcap_findalldevs",
            unsafe extern "C" fn(*mut *mut RawIf, *mut c_char) -> c_int
        ),
        freealldevs: resolve!("pcap_freealldevs", unsafe extern "C" fn(*mut RawIf)),
        create: resolve!(
            "pcap_create",
            unsafe extern "C" fn(*const c_char, *mut c_char) -> *mut PcapT
        ),
        set_snaplen: resolve!(
            "pcap_set_snaplen",
            unsafe extern "C" fn(*mut PcapT, c_int) -> c_int
        ),
        set_promisc: resolve!(
            "pcap_set_promisc",
            unsafe extern "C" fn(*mut PcapT, c_int) -> c_int
        ),
        set_timeout: resolve!(
            "pcap_set_timeout",
            unsafe extern "C" fn(*mut PcapT, c_int) -> c_int
        ),
        set_immediate_mode: resolve!(
            "pcap_set_immediate_mode",
            unsafe extern "C" fn(*mut PcapT, c_int) -> c_int
        ),
        set_tstamp_type: resolve!(
            "pcap_set_tstamp_type",
            unsafe extern "C" fn(*mut PcapT, c_int) -> c_int
        ),
        activate: resolve!("pcap_activate", unsafe extern "C" fn(*mut PcapT) -> c_int),
        compile: resolve!(
            "pcap_compile",
            unsafe extern "C" fn(*mut PcapT, *mut BpfProgram, *const c_char, c_int, u32) -> c_int
        ),
        setfilter: resolve!(
            "pcap_setfilter",
            unsafe extern "C" fn(*mut PcapT, *mut BpfProgram) -> c_int
        ),
        freecode: resolve!("pcap_freecode", unsafe extern "C" fn(*mut BpfProgram)),
        next_ex: resolve!(
            "pcap_next_ex",
            unsafe extern "C" fn(*mut PcapT, *mut *mut RawPkthdr, *mut *const u8) -> c_int
        ),
        geterr: resolve!(
            "pcap_geterr",
            unsafe extern "C" fn(*mut PcapT) -> *mut c_char
        ),
        close: resolve!("pcap_close", unsafe extern "C" fn(*mut PcapT)),
    })
}

// ---- devices ----

/// One address of a capture device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Address {
    pub addr: IpAddr,
    pub netmask: Option<IpAddr>,
}

/// A capture device (`\Device\NPF_{GUID}`), its description and its IP addresses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    pub name: String,
    pub desc: Option<String>,
    pub addresses: Vec<Address>,
}

/// An IPv4/IPv6 address from a `sockaddr` (family + the bytes after it); other families: `None`.
fn ip_from_sockaddr(family: u16, data: &[u8]) -> Option<IpAddr> {
    match family {
        // sockaddr_in: port (2), address (4)
        AF_INET if data.len() >= 6 => Some(IpAddr::V4(Ipv4Addr::new(
            data[2], data[3], data[4], data[5],
        ))),
        // sockaddr_in6: port (2), flowinfo (4), address (16)
        AF_INET6 if data.len() >= 22 => {
            let mut a = [0u8; 16];
            a.copy_from_slice(&data[6..22]);
            Some(IpAddr::V6(Ipv6Addr::from(a)))
        }
        _ => None,
    }
}

/// SAFETY: `sa` is null or points to a sockaddr of its family's full size (pcap fills them).
unsafe fn read_sockaddr(sa: *const RawSockaddr) -> Option<IpAddr> {
    if sa.is_null() {
        return None;
    }
    let family = (*sa).family;
    let len = match family {
        AF_INET => 14,
        AF_INET6 => 26, // sizeof(sockaddr_in6) - 2
        _ => return None,
    };
    let data = std::slice::from_raw_parts((sa as *const u8).add(2), len);
    ip_from_sockaddr(family, data)
}

/// SAFETY: `p` is null or a NUL-terminated C string.
unsafe fn opt_string(p: *const c_char) -> Option<String> {
    (!p.is_null()).then(|| CStr::from_ptr(p).to_string_lossy().into_owned())
}

fn errbuf_text(buf: &[c_char; PCAP_ERRBUF_SIZE]) -> String {
    // SAFETY: pcap NUL-terminates the error buffer; it was zeroed before the call.
    unsafe { CStr::from_ptr(buf.as_ptr()) }
        .to_string_lossy()
        .into_owned()
}

/// Every capture device Npcap sees (`pcap_findalldevs`).
pub fn devices() -> anyhow::Result<Vec<Device>> {
    let api = api()?;
    let mut list: *mut RawIf = std::ptr::null_mut();
    let mut err = [0 as c_char; PCAP_ERRBUF_SIZE];
    // SAFETY: valid out pointers; `err` holds PCAP_ERRBUF_SIZE bytes.
    if unsafe { (api.findalldevs)(&mut list, err.as_mut_ptr()) } != 0 {
        return Err(PcapError(errbuf_text(&err)).into());
    }
    let mut out = Vec::new();
    let mut cur = list;
    // SAFETY: a list pcap_findalldevs returned; freed once below, after the walk.
    unsafe {
        while !cur.is_null() {
            let d = &*cur;
            let mut addresses = Vec::new();
            let mut a = d.addresses;
            while !a.is_null() {
                if let Some(addr) = read_sockaddr((*a).addr) {
                    addresses.push(Address {
                        addr,
                        netmask: read_sockaddr((*a).netmask),
                    });
                }
                a = (*a).next;
            }
            out.push(Device {
                name: opt_string(d.name).unwrap_or_default(),
                desc: opt_string(d.description),
                addresses,
            });
            cur = d.next;
        }
        if !list.is_null() {
            (api.freealldevs)(list);
        }
    }
    Ok(out)
}

// ---- capture ----

/// How a capture is opened.
#[derive(Debug, Clone, Copy)]
pub struct CaptureSettings {
    pub snaplen: i32,
    pub promisc: bool,
    pub immediate: bool,
    pub timeout_ms: i32,
    /// `PCAP_TSTAMP_HOST_HIPREC` (`KeQuerySystemTimePrecise`) when true.
    pub host_hiprec: bool,
}

/// One received packet, valid until the next call on its capture.
pub struct Packet<'a> {
    pub ts: Timeval,
    pub data: &'a [u8],
}

/// An activated capture handle; closed on drop.
pub struct Capture {
    api: &'static Api,
    handle: NonNull<PcapT>,
}

// SAFETY: a pcap_t is used by one thread at a time (`&mut self` on every call); libpcap handles
// may move between threads.
unsafe impl Send for Capture {}

impl Capture {
    /// `pcap_create` + the settings + `pcap_activate`. A warning from `pcap_activate` (e.g. the
    /// timestamp type unsupported) is an error: the stamps would not be what the clock needs.
    pub fn open(device: &Device, s: CaptureSettings) -> anyhow::Result<Capture> {
        let api = api()?;
        let name = CString::new(device.name.as_str())?;
        let mut err = [0 as c_char; PCAP_ERRBUF_SIZE];
        // SAFETY: a NUL-terminated name; `err` holds PCAP_ERRBUF_SIZE bytes.
        let raw = unsafe { (api.create)(name.as_ptr(), err.as_mut_ptr()) };
        let handle = NonNull::new(raw).ok_or_else(|| PcapError(errbuf_text(&err)))?;
        let cap = Capture { api, handle };
        let h = cap.handle.as_ptr();
        // SAFETY: `h` is a created, not yet activated handle (the setters' precondition).
        unsafe {
            cap.check((api.set_snaplen)(h, s.snaplen), "pcap_set_snaplen")?;
            cap.check((api.set_promisc)(h, s.promisc as c_int), "pcap_set_promisc")?;
            cap.check((api.set_timeout)(h, s.timeout_ms), "pcap_set_timeout")?;
            cap.check(
                (api.set_immediate_mode)(h, s.immediate as c_int),
                "pcap_set_immediate_mode",
            )?;
            if s.host_hiprec {
                cap.check(
                    (api.set_tstamp_type)(h, PCAP_TSTAMP_HOST_HIPREC),
                    "pcap_set_tstamp_type",
                )?;
            }
            let rc = (api.activate)(h);
            if rc != 0 {
                return Err(PcapError(format!(
                    "pcap_activate returned {rc} on {}: {}",
                    device.name,
                    cap.last_error()
                ))
                .into());
            }
        }
        Ok(cap)
    }

    fn last_error(&self) -> String {
        // SAFETY: a live handle; pcap_geterr returns a NUL-terminated string owned by it.
        unsafe { opt_string((self.api.geterr)(self.handle.as_ptr())) }.unwrap_or_default()
    }

    fn check(&self, rc: c_int, what: &str) -> Result<(), PcapError> {
        if rc < 0 {
            Err(PcapError(format!(
                "{what} returned {rc}: {}",
                self.last_error()
            )))
        } else {
            Ok(())
        }
    }

    /// Compile (optimized) and apply a BPF filter.
    pub fn filter(&mut self, program: &str) -> anyhow::Result<()> {
        let text = CString::new(program)?;
        let mut bpf = BpfProgram {
            bf_len: 0,
            bf_insns: std::ptr::null_mut(),
        };
        let h = self.handle.as_ptr();
        // SAFETY: a live, activated handle; `bpf` is freed with pcap_freecode once compiled.
        unsafe {
            self.check(
                (self.api.compile)(h, &mut bpf, text.as_ptr(), 1, PCAP_NETMASK_UNKNOWN),
                "pcap_compile",
            )?;
            let set = (self.api.setfilter)(h, &mut bpf);
            (self.api.freecode)(&mut bpf);
            self.check(set, "pcap_setfilter")?;
        }
        Ok(())
    }

    /// The next packet; `Ok(None)` when the read timeout expired without one.
    pub fn next_packet(&mut self) -> anyhow::Result<Option<Packet<'_>>> {
        let mut hdr: *mut RawPkthdr = std::ptr::null_mut();
        let mut data: *const u8 = std::ptr::null();
        // SAFETY: a live, activated handle and valid out pointers.
        let rc = unsafe { (self.api.next_ex)(self.handle.as_ptr(), &mut hdr, &mut data) };
        match rc {
            1 if !hdr.is_null() && !data.is_null() => {
                // SAFETY: on 1 pcap returns a header and `caplen` bytes, valid until the next
                // call on this handle (the returned borrow of `self` enforces that).
                let (ts, caplen) = unsafe { ((*hdr).ts, (*hdr).caplen as usize) };
                let data = unsafe { std::slice::from_raw_parts(data, caplen) };
                Ok(Some(Packet { ts, data }))
            }
            0 | 1 => Ok(None),
            _ => {
                Err(PcapError(format!("pcap_next_ex returned {rc}: {}", self.last_error())).into())
            }
        }
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        // SAFETY: the handle is live and closed exactly once.
        unsafe { (self.api.close)(self.handle.as_ptr()) }
    }
}

#[cfg(test)]
mod tests;
