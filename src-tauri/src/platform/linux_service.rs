//! Socket-activated input service. Only this dedicated account can open input
//! devices. Clients must belong to an active local graphical login session.
use std::{
    ffi::{CStr, CString},
    io, ptr,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

#[link(name = "systemd")]
unsafe extern "C" {
    fn sd_bus_open_system(bus: *mut *mut libc::c_void) -> libc::c_int;
    fn sd_bus_unref(bus: *mut libc::c_void) -> *mut libc::c_void;
    fn sd_bus_path_encode(
        prefix: *const libc::c_char,
        id: *const libc::c_char,
        path: *mut *mut libc::c_char,
    ) -> libc::c_int;
    fn sd_bus_set_method_call_timeout(bus: *mut libc::c_void, timeout: u64) -> libc::c_int;
    fn sd_bus_get_property_trivial(
        bus: *mut libc::c_void,
        destination: *const libc::c_char,
        path: *const libc::c_char,
        interface: *const libc::c_char,
        member: *const libc::c_char,
        error: *mut libc::c_void,
        kind: libc::c_char,
        value: *mut libc::c_void,
    ) -> libc::c_int;
    fn sd_pid_get_session(pid: libc::pid_t, session: *mut *mut libc::c_char) -> libc::c_int;
    fn sd_uid_get_display(uid: libc::uid_t, session: *mut *mut libc::c_char) -> libc::c_int;
    fn sd_session_get_uid(session: *const libc::c_char, uid: *mut libc::uid_t) -> libc::c_int;
    fn sd_session_is_active(session: *const libc::c_char) -> libc::c_int;
    fn sd_session_is_remote(session: *const libc::c_char) -> libc::c_int;
    fn sd_session_get_type(
        session: *const libc::c_char,
        kind: *mut *mut libc::c_char,
    ) -> libc::c_int;
}

fn session_is_unlocked(session: &CStr) -> bool {
    // LockedHint is a login1 property rather than part of sd-login. Keep the
    // read bounded and fail closed if login1 cannot confirm the session state.
    unsafe {
        let mut bus = ptr::null_mut();
        if sd_bus_open_system(&mut bus) < 0 {
            return false;
        }
        sd_bus_set_method_call_timeout(bus, 500_000);
        let mut path = ptr::null_mut();
        if sd_bus_path_encode(
            c"/org/freedesktop/login1/session".as_ptr(),
            session.as_ptr(),
            &mut path,
        ) < 0
        {
            sd_bus_unref(bus);
            return false;
        }
        let mut locked: libc::c_int = 1;
        let result = sd_bus_get_property_trivial(
            bus,
            c"org.freedesktop.login1".as_ptr(),
            path,
            c"org.freedesktop.login1.Session".as_ptr(),
            c"LockedHint".as_ptr(),
            ptr::null_mut(),
            b'b' as libc::c_char,
            (&mut locked as *mut libc::c_int).cast(),
        );
        libc::free(path.cast());
        sd_bus_unref(bus);
        result >= 0 && locked == 0
    }
}

fn active_local_session(session: &CStr, uid: libc::uid_t) -> bool {
    let mut owner = 0;
    let mut kind = ptr::null_mut();
    // libsystemd allocates the returned string; all pointers remain valid until free.
    unsafe {
        if sd_session_get_uid(session.as_ptr(), &mut owner) < 0
            || owner != uid
            || sd_session_is_active(session.as_ptr()) != 1
            || sd_session_is_remote(session.as_ptr()) != 0
            || sd_session_get_type(session.as_ptr(), &mut kind) < 0
        {
            return false;
        }
        let graphical = matches!(CStr::from_ptr(kind).to_bytes(), b"wayland" | b"x11");
        libc::free(kind.cast());
        graphical && session_is_unlocked(session)
    }
}

fn authorize_peer(fd: libc::c_int) -> io::Result<(CString, libc::uid_t)> {
    let mut peer: libc::ucred = unsafe { std::mem::zeroed() };
    let mut length = std::mem::size_of_val(&peer) as libc::socklen_t;
    if unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut peer as *mut libc::ucred).cast(),
            &mut length,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    if peer.pid <= 0 || peer.uid == 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Expected an unprivileged desktop client",
        ));
    }
    session_for(peer.pid, peer.uid).map(|session| (session, peer.uid))
}

pub(super) fn current_session_is_active() -> bool {
    session_for(unsafe { libc::getpid() }, unsafe { libc::getuid() }).is_ok()
}

fn session_for(pid: libc::pid_t, uid: libc::uid_t) -> io::Result<CString> {
    let mut session = ptr::null_mut();
    let result = unsafe { sd_pid_get_session(pid, &mut session) };
    if result < 0 {
        // Graphical apps launched by systemd --user are outside the login scope.
        // Only that case may use the user's display session. Never turn a known
        // remote/TTY session into a graphical authorization by falling back.
        if result != -libc::ENODATA || unsafe { sd_uid_get_display(uid, &mut session) } < 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "No graphical login session",
            ));
        }
    }
    let session = unsafe {
        let owned = CStr::from_ptr(session).to_owned();
        libc::free(session.cast());
        owned
    };
    if !active_local_session(&session, uid) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Input is available only in the active unlocked local graphical session",
        ));
    }
    Ok(session)
}

pub fn run_service() -> Result<(), String> {
    let result = (|| -> io::Result<()> {
        if unsafe { libc::geteuid() } == 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Input service must run as its dedicated unprivileged account",
            ));
        }
        let (session, uid) = authorize_peer(0)?;
        let stop = Arc::new(AtomicBool::new(false));
        let watcher_stop = stop.clone();
        let watcher = std::thread::spawn(move || {
            while !watcher_stop.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(250));
                if !active_local_session(&session, uid) {
                    // Wake the input loop and release grabs when the user logs
                    // out or switches away. No device access outlives the session.
                    unsafe {
                        libc::shutdown(0, libc::SHUT_RDWR);
                    }
                    break;
                }
            }
        });
        let result = super::linux_native::run_service_relay();
        stop.store(true, Ordering::Release);
        let _ = watcher.join();
        result
    })();
    if let Err(error) = &result {
        use std::io::Write;
        let event = serde_json::json!({"event":"engine-status","payload":{"available":false,"capturing":false,"message":error.to_string()}});
        let _ = writeln!(std::io::stdout(), "{event}");
    }
    result.map_err(|error| error.to_string())
}
