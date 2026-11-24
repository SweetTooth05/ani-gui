//! Keeps the screen from blanking, dimming or locking while video plays.
//!
//! `KeepAwake` is a switch the player flips on while it is actually playing
//! and off when it pauses, ends or goes away. The platform calls run on a
//! worker thread so a slow or hung session bus can't stall the UI, and the
//! worker holds the platform `Inhibitor` (a guard: dropping it releases the
//! inhibition). Failing to inhibit never stops playback; it is logged once.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

use anyhow::Result;

#[cfg(target_os = "linux")]
use linux::Inhibitor;
#[cfg(target_os = "macos")]
use macos::Inhibitor;
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
use other::Inhibitor;

/// The application name desktops show next to the inhibition.
const APP_NAME: &str = "ani-gui";
const REASON: &str = "Playing video";

/// Keeps the screen awake while active. Idempotent: repeating a state does
/// nothing. Dropping it releases any inhibition.
pub struct KeepAwake {
    /// The worker, started on first activation.
    tx: Option<Sender<bool>>,
    active: bool,
    spawn: fn() -> Option<Sender<bool>>,
}

impl Default for KeepAwake {
    fn default() -> Self {
        KeepAwake::with_spawner(|| spawn_worker(|| Inhibitor::acquire(APP_NAME, REASON)))
    }
}

impl KeepAwake {
    fn with_spawner(spawn: fn() -> Option<Sender<bool>>) -> Self {
        KeepAwake {
            tx: None,
            active: false,
            spawn,
        }
    }

    /// Inhibits the screen saver while `active`, releases it otherwise.
    pub fn set_active(&mut self, active: bool) {
        if active == self.active {
            return;
        }
        self.active = active;
        if self.tx.is_none() {
            if !active {
                return;
            }
            self.tx = (self.spawn)();
        }
        if let Some(tx) = &self.tx
            && tx.send(active).is_err()
        {
            // The worker is gone (it couldn't start); try again next time.
            self.tx = None;
        }
    }
}

impl Drop for KeepAwake {
    fn drop(&mut self) {
        // Closing the channel ends the worker, which drops its guard.
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(false);
        }
    }
}

/// Starts a worker that holds a guard from `acquire` while the latest
/// message is `true`, and drops it on `false` or when the sender goes away.
fn spawn_worker<G, F>(acquire: F) -> Option<Sender<bool>>
where
    G: 'static,
    F: FnMut() -> Result<G> + Send + 'static,
{
    let (tx, rx) = mpsc::channel();
    let spawned = thread::Builder::new()
        .name("keep-awake".into())
        .spawn(move || run_worker(rx, acquire));
    match spawned {
        Ok(_) => Some(tx),
        Err(err) => {
            log_once(&format!("starting its thread: {err}"));
            None
        }
    }
}

fn run_worker<G>(rx: Receiver<bool>, mut acquire: impl FnMut() -> Result<G>) {
    let mut guard: Option<G> = None;
    while let Ok(mut want) = rx.recv() {
        // Only the newest state matters.
        while let Ok(newer) = rx.try_recv() {
            want = newer;
        }
        if !want {
            guard = None;
        } else if guard.is_none() {
            match acquire() {
                Ok(g) => guard = Some(g),
                Err(err) => log_once(&format!("{err:#}")),
            }
        }
    }
    drop(guard);
}

/// Reports the first failure only: a desktop without any inhibit service
/// would otherwise log on every play.
fn log_once(msg: &str) {
    static LOGGED: AtomicBool = AtomicBool::new(false);
    if !LOGGED.swap(true, Ordering::Relaxed) {
        eprintln!("ani-gui: can't keep the screen awake during playback: {msg}");
    }
}

#[cfg(target_os = "linux")]
mod linux {
    //! Freedesktop/GNOME inhibition over the session bus. The inhibition
    //! lives as long as the connection that asked for it, so the guard owns
    //! its connection.
    //!
    //! Tried in order: `org.freedesktop.ScreenSaver` (GNOME via
    //! gsd-screensaver-proxy, KDE, Xfce, LXQt…), the xdg-desktop-portal
    //! Inhibit interface (also reaches sandboxed setups), then GNOME's own
    //! session manager.

    use std::collections::HashMap;
    use std::time::Duration;

    use anyhow::{Context, Result, anyhow};
    use zbus::blocking::{Connection, connection};
    use zbus::zvariant::{OwnedObjectPath, Value};

    /// Bound on each bus call, so a wedged service can't hang the worker.
    const CALL_TIMEOUT: Duration = Duration::from_secs(5);
    /// "Inhibit the session being marked as idle" (portal and GNOME flags).
    const IDLE: u32 = 8;

    const SCREENSAVER: (&str, &str, &str) = (
        "org.freedesktop.ScreenSaver",
        "/org/freedesktop/ScreenSaver",
        "org.freedesktop.ScreenSaver",
    );
    const PORTAL: (&str, &str, &str) = (
        "org.freedesktop.portal.Desktop",
        "/org/freedesktop/portal/desktop",
        "org.freedesktop.portal.Inhibit",
    );
    const GNOME: (&str, &str, &str) = (
        "org.gnome.SessionManager",
        "/org/gnome/SessionManager",
        "org.gnome.SessionManager",
    );

    enum Held {
        ScreenSaver(u32),
        /// The portal's request handle; closing it ends the inhibition.
        Portal(OwnedObjectPath),
        Gnome(u32),
    }

    pub struct Inhibitor {
        conn: Connection,
        held: Held,
    }

    impl Inhibitor {
        pub fn acquire(app: &str, reason: &str) -> Result<Inhibitor> {
            let conn = connection::Builder::session()
                .and_then(|b| b.method_timeout(CALL_TIMEOUT).build())
                .context("connecting to the session bus")?;
            let mut errors = Vec::new();
            match screensaver(&conn, app, reason) {
                Ok(cookie) => return Ok(Inhibitor::new(conn, Held::ScreenSaver(cookie))),
                Err(err) => errors.push(format!("ScreenSaver: {err}")),
            }
            match portal(&conn, reason) {
                Ok(handle) => return Ok(Inhibitor::new(conn, Held::Portal(handle))),
                Err(err) => errors.push(format!("portal: {err}")),
            }
            match gnome(&conn, app, reason) {
                Ok(cookie) => return Ok(Inhibitor::new(conn, Held::Gnome(cookie))),
                Err(err) => errors.push(format!("GNOME session: {err}")),
            }
            Err(anyhow!(errors.join("; ")))
        }

        fn new(conn: Connection, held: Held) -> Self {
            Inhibitor { conn, held }
        }

        /// Which service holds the inhibition, for tests and diagnostics.
        #[cfg_attr(not(test), allow(dead_code))]
        pub fn mechanism(&self) -> &'static str {
            match self.held {
                Held::ScreenSaver(_) => "org.freedesktop.ScreenSaver",
                Held::Portal(_) => "org.freedesktop.portal.Inhibit",
                Held::Gnome(_) => "org.gnome.SessionManager",
            }
        }
    }

    impl Drop for Inhibitor {
        fn drop(&mut self) {
            let released = match &self.held {
                Held::ScreenSaver(cookie) => {
                    call(&self.conn, SCREENSAVER, "UnInhibit", &(*cookie,))
                }
                Held::Portal(handle) => self
                    .conn
                    .call_method(
                        Some(PORTAL.0),
                        handle.as_ref(),
                        Some("org.freedesktop.portal.Request"),
                        "Close",
                        &(),
                    )
                    .map(drop),
                Held::Gnome(cookie) => call(&self.conn, GNOME, "Uninhibit", &(*cookie,)),
            };
            // Closing the connection (as `conn` drops next) releases it anyway.
            if let Err(err) = released {
                eprintln!("ani-gui: releasing the screen inhibition: {err}");
            }
        }
    }

    fn call<B>(
        conn: &Connection,
        (dest, path, iface): (&str, &str, &str),
        method: &str,
        body: &B,
    ) -> zbus::Result<()>
    where
        B: serde::Serialize + zbus::zvariant::DynamicType,
    {
        conn.call_method(Some(dest), path, Some(iface), method, body)
            .map(drop)
    }

    fn screensaver(conn: &Connection, app: &str, reason: &str) -> zbus::Result<u32> {
        let (dest, path, iface) = SCREENSAVER;
        conn.call_method(Some(dest), path, Some(iface), "Inhibit", &(app, reason))?
            .body()
            .deserialize()
    }

    fn portal(conn: &Connection, reason: &str) -> zbus::Result<OwnedObjectPath> {
        let (dest, path, iface) = PORTAL;
        let options = HashMap::from([("reason", Value::from(reason))]);
        conn.call_method(
            Some(dest),
            path,
            Some(iface),
            "Inhibit",
            &("", IDLE, options),
        )?
        .body()
        .deserialize()
    }

    fn gnome(conn: &Connection, app: &str, reason: &str) -> zbus::Result<u32> {
        let (dest, path, iface) = GNOME;
        conn.call_method(
            Some(dest),
            path,
            Some(iface),
            "Inhibit",
            &(app, 0u32, reason, IDLE),
        )?
        .body()
        .deserialize()
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// The reasons of GNOME's current inhibitors.
        fn gnome_reasons() -> Vec<String> {
            let conn = Connection::session().expect("session bus");
            let (dest, path, iface) = GNOME;
            let paths: Vec<OwnedObjectPath> = conn
                .call_method(Some(dest), path, Some(iface), "GetInhibitors", &())
                .expect("GetInhibitors")
                .body()
                .deserialize()
                .expect("paths");
            paths
                .iter()
                .filter_map(|p| {
                    conn.call_method(
                        Some(dest),
                        p.as_ref(),
                        Some("org.gnome.SessionManager.Inhibitor"),
                        "GetReason",
                        &(),
                    )
                    .ok()?
                    .body()
                    .deserialize::<String>()
                    .ok()
                })
                .collect()
        }

        fn wait_listed(reason: &str, listed: bool) {
            let deadline = std::time::Instant::now() + Duration::from_secs(3);
            while gnome_reasons().iter().any(|r| r == reason) != listed {
                assert!(
                    std::time::Instant::now() < deadline,
                    "inhibitor {reason:?} never became listed={listed}"
                );
                std::thread::sleep(Duration::from_millis(50));
            }
        }

        fn session() -> Connection {
            connection::Builder::session()
                .and_then(|b| b.method_timeout(CALL_TIMEOUT).build())
                .expect("session bus")
        }

        // Against the real session bus of a GNOME desktop:
        // `cargo test keep_awake -- --ignored`.

        #[test]
        #[ignore = "needs a GNOME session bus"]
        fn inhibits_on_the_real_gnome_session() {
            let reason = format!("ani-gui test {}", std::process::id());
            let inhibitor = Inhibitor::acquire("ani-gui-test", &reason).expect("acquire");
            eprintln!("inhibiting via {}", inhibitor.mechanism());
            wait_listed(&reason, true);
            drop(inhibitor);
            wait_listed(&reason, false);
        }

        #[test]
        #[ignore = "needs a GNOME session bus"]
        fn portal_fallback_inhibits_and_releases() {
            let reason = format!("ani-gui portal test {}", std::process::id());
            let conn = session();
            let handle = portal(&conn, &reason).expect("portal inhibit");
            let inhibitor = Inhibitor::new(conn, Held::Portal(handle));
            wait_listed(&reason, true);
            drop(inhibitor);
            wait_listed(&reason, false);
        }

        #[test]
        #[ignore = "needs a GNOME session bus"]
        fn gnome_fallback_inhibits_and_releases() {
            let reason = format!("ani-gui gnome test {}", std::process::id());
            let conn = session();
            let cookie = gnome(&conn, "ani-gui-test", &reason).expect("gnome inhibit");
            let inhibitor = Inhibitor::new(conn, Held::Gnome(cookie));
            wait_listed(&reason, true);
            drop(inhibitor);
            wait_listed(&reason, false);
        }
    }
}

#[cfg(target_os = "macos")]
mod macos {
    //! An IOKit power assertion that stops the display from sleeping (and
    //! with it the screen saver and the lock). The assertion belongs to the
    //! process, not the thread, and lasts until it is released.

    use anyhow::{Result, bail};
    use core_foundation::base::TCFType;
    use core_foundation::string::{CFString, CFStringRef};

    type IOPMAssertionID = u32;
    type IOPMAssertionLevel = u32;
    type IOReturn = i32;

    /// `kIOPMAssertionLevelOn`.
    const LEVEL_ON: IOPMAssertionLevel = 255;
    /// `kIOReturnSuccess`.
    const SUCCESS: IOReturn = 0;
    /// The value of `kIOPMAssertionTypePreventUserIdleDisplaySleep`, a
    /// `CFSTR` macro in IOPMLib.h rather than a linkable symbol.
    const PREVENT_DISPLAY_SLEEP: &str = "PreventUserIdleDisplaySleep";

    #[link(name = "IOKit", kind = "framework")]
    unsafe extern "C" {
        fn IOPMAssertionCreateWithName(
            assertion_type: CFStringRef,
            level: IOPMAssertionLevel,
            name: CFStringRef,
            id: *mut IOPMAssertionID,
        ) -> IOReturn;
        fn IOPMAssertionRelease(id: IOPMAssertionID) -> IOReturn;
    }

    /// Holds one assertion; dropping it releases the assertion.
    pub struct Inhibitor {
        id: IOPMAssertionID,
    }

    impl Inhibitor {
        pub fn acquire(app: &str, reason: &str) -> Result<Inhibitor> {
            let kind = CFString::from_static_string(PREVENT_DISPLAY_SLEEP);
            // Shown by `pmset -g assertions`.
            let name = CFString::new(&format!("{app}: {reason}"));
            let mut id: IOPMAssertionID = 0;
            // SAFETY: both strings are valid CFStrings that outlive the call
            // (IOKit retains what it keeps), and `id` is a valid out pointer.
            let rc = unsafe {
                IOPMAssertionCreateWithName(
                    kind.as_concrete_TypeRef(),
                    LEVEL_ON,
                    name.as_concrete_TypeRef(),
                    &mut id,
                )
            };
            if rc != SUCCESS {
                bail!("IOPMAssertionCreateWithName failed: {rc:#x}");
            }
            Ok(Inhibitor { id })
        }
    }

    impl Drop for Inhibitor {
        fn drop(&mut self) {
            // SAFETY: `id` came from a successful IOPMAssertionCreateWithName
            // and is released exactly once, here.
            let rc = unsafe { IOPMAssertionRelease(self.id) };
            if rc != SUCCESS {
                eprintln!("ani-gui: keep awake: IOPMAssertionRelease failed: {rc:#x}");
            }
        }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
mod other {
    //! No inhibition on this platform; the guard does nothing.

    use anyhow::Result;

    pub struct Inhibitor;

    impl Inhibitor {
        pub fn acquire(_app: &str, _reason: &str) -> Result<Inhibitor> {
            Ok(Inhibitor)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicUsize;
    use std::time::{Duration, Instant};

    /// Live fake guards and total acquisitions, per test.
    struct Counts {
        live: AtomicUsize,
        acquired: AtomicUsize,
    }

    struct Guard(&'static Counts);

    impl Drop for Guard {
        fn drop(&mut self) {
            self.0.live.fetch_sub(1, Ordering::SeqCst);
        }
    }

    fn acquire(counts: &'static Counts) -> Result<Guard> {
        counts.live.fetch_add(1, Ordering::SeqCst);
        counts.acquired.fetch_add(1, Ordering::SeqCst);
        Ok(Guard(counts))
    }

    fn wait_for(what: &str, cond: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while !cond() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            thread::sleep(Duration::from_millis(5));
        }
    }

    // `with_spawner` takes a plain fn, so each test gets its own statics.
    macro_rules! fake {
        ($counts:ident) => {{
            static $counts: Counts = Counts {
                live: AtomicUsize::new(0),
                acquired: AtomicUsize::new(0),
            };
            (
                KeepAwake::with_spawner(|| spawn_worker(|| acquire(&$counts))),
                &$counts,
            )
        }};
    }

    #[test]
    fn holds_one_guard_while_active_and_releases_on_pause() {
        let (mut keep, counts) = fake!(PAUSE);
        keep.set_active(true);
        keep.set_active(true);
        wait_for("acquire", || counts.live.load(Ordering::SeqCst) == 1);
        keep.set_active(false);
        wait_for("release", || counts.live.load(Ordering::SeqCst) == 0);
        keep.set_active(true);
        wait_for("reacquire", || counts.live.load(Ordering::SeqCst) == 1);
        assert_eq!(counts.acquired.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn dropping_releases() {
        let (mut keep, counts) = fake!(DROPPED);
        keep.set_active(true);
        wait_for("acquire", || counts.live.load(Ordering::SeqCst) == 1);
        drop(keep);
        wait_for("release", || counts.live.load(Ordering::SeqCst) == 0);
    }

    #[test]
    fn never_active_never_starts_a_worker() {
        let (mut keep, counts) = fake!(IDLE_ONLY);
        keep.set_active(false);
        assert!(keep.tx.is_none());
        drop(keep);
        assert_eq!(counts.acquired.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn failing_to_acquire_is_harmless_and_retried() {
        static TRIES: Mutex<u32> = Mutex::new(0);
        let mut keep = KeepAwake::with_spawner(|| {
            spawn_worker(|| -> Result<()> {
                *TRIES.lock().unwrap() += 1;
                anyhow::bail!("no inhibit service")
            })
        });
        keep.set_active(true);
        wait_for("first try", || *TRIES.lock().unwrap() == 1);
        keep.set_active(false);
        keep.set_active(true);
        wait_for("retry", || *TRIES.lock().unwrap() == 2);
    }
}
