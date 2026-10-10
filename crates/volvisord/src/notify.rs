//! The service-manager readiness notification (P7-B, ADR-0009's
//! service model): a hand-rolled `sd_notify(3)` client — no new
//! dependencies, `READY=1` and nothing else.
//!
//! Protocol (`sd_notify(3)`): a service manager that wants readiness
//! reports exports `NOTIFY_SOCKET` in the daemon's environment; the
//! daemon sends **one UNIX datagram** containing `READY=1\n` to that
//! socket when — and only when — it is ready. An `@`-prefixed socket
//! is an abstract-namespace socket: the protocol spells the leading
//! `@` as a NUL byte in the address (`SocketAddr::from_abstract_name`
//! performs that substitution); any other value is a filesystem path.
//!
//! Readiness semantics (both daemons, see [`crate::runtime::serve`]
//! and [`crate::witness::serve`]): `READY=1` is emitted only after
//! startup reconciliation completes — journal lock + replay, provider
//! construction and its reconcile — **and** the API listener is
//! bound. A daemon that is up but not reconciled, or reconciled but
//! not serving, is not ready and never says so.
//!
//! Failure discipline:
//!
//! * `NOTIFY_SOCKET` unset is a documented no-op — a standalone run
//!   from a shell, the test rigs, the campaign harness; nothing about
//!   the daemon's behavior changes.
//! * A send failure is **logged as a structured warning and never
//!   fails startup**. Under `Type=notify` the service manager kills
//!   the unit at its own start timeout and `Restart=on-failure`
//!   re-enters the (idempotent) startup path — the manager's timeout
//!   is the operator's signal, and the daemon keeps serving in the
//!   meantime. Failing startup here would turn a notification hiccup
//!   into an availability loss while adding no safety: readiness is
//!   the manager's gate, not the daemon's.
//!
//! Deliberately minimal: `READY=1` only — no `STATUS=`, `MAINPID=` or
//! watchdog traffic. The daemons' structured journal logs already
//! carry richer state than any `STATUS` string; a second channel
//! would only drift from them.

use std::os::unix::net::{SocketAddr, UnixDatagram};
use std::path::Path;

/// The entire readiness message, per the protocol: one datagram,
/// `READY=1\n`.
const READY: &[u8] = b"READY=1\n";

/// What a [`notify_ready`] attempt did.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum NotifyOutcome {
    /// `NOTIFY_SOCKET` is unset: a standalone run — the documented
    /// no-op.
    SocketUnset,
    /// The `READY=1` datagram was sent.
    Sent,
}

/// A readiness-notification failure. Contained in the [`Result`] of
/// [`notify_ready`] and logged by [`ready`]; never a startup failure
/// (see the module docs for why).
#[derive(Debug, thiserror::Error)]
pub(crate) enum NotifyError {
    /// `NOTIFY_SOCKET` is set but cannot be interpreted as a socket
    /// address.
    #[error("invalid NOTIFY_SOCKET {spec:?}: {reason}")]
    InvalidSocket {
        /// The offending socket specification.
        spec: String,
        /// Why it cannot be used as an address.
        reason: String,
    },
    /// Connecting to or sending on the notify socket failed.
    #[error("NOTIFY_SOCKET {spec:?}: {source}")]
    SendFailed {
        /// The socket specification.
        spec: String,
        /// The underlying I/O failure.
        source: std::io::Error,
    },
}

/// Translate a `NOTIFY_SOCKET` value into a socket address: a leading
/// `@` denotes an abstract-namespace socket (the protocol's spelling
/// — the `@` stands for the NUL byte the abstract form substitutes);
/// anything else is a filesystem path used verbatim.
fn socket_addr(spec: &str) -> Result<SocketAddr, NotifyError> {
    if let Some(name) = spec.strip_prefix('@') {
        return abstract_socket_addr(spec, name);
    }
    let path = Path::new(spec);
    if path.as_os_str().is_empty() {
        // std would accept an empty pathname; the protocol would not
        // — an empty NOTIFY_SOCKET is a typed refusal, not a connect
        // attempt against a nameless address.
        return Err(NotifyError::InvalidSocket {
            spec: spec.to_owned(),
            reason: "empty socket path".to_owned(),
        });
    }
    SocketAddr::from_pathname(path).map_err(|source| NotifyError::InvalidSocket {
        spec: spec.to_owned(),
        reason: source.to_string(),
    })
}

/// The abstract-namespace arm of [`socket_addr`]: Linux-only std
/// surface (the storage daemon's platform — LVM and DRBD are Linux
/// subsystems).
#[cfg(target_os = "linux")]
fn abstract_socket_addr(spec: &str, name: &str) -> Result<SocketAddr, NotifyError> {
    use std::os::linux::net::SocketAddrExt;
    SocketAddr::from_abstract_name(name).map_err(|source| NotifyError::InvalidSocket {
        spec: spec.to_owned(),
        reason: format!("abstract socket name rejected: {source}"),
    })
}

/// The non-Linux arm: abstract sockets are a Linux extension the
/// daemons do not support elsewhere — a typed refusal, not a panic.
#[cfg(not(target_os = "linux"))]
fn abstract_socket_addr(spec: &str, _name: &str) -> Result<SocketAddr, NotifyError> {
    Err(NotifyError::InvalidSocket {
        spec: spec.to_owned(),
        reason: "abstract sockets require Linux".to_owned(),
    })
}

/// Send the `READY=1` datagram to `spec`: one unbound UNIX datagram
/// socket, connect, send — the sender needs no name of its own (the
/// service manager only ever receives).
fn send_ready(spec: &str) -> Result<(), NotifyError> {
    let addr = socket_addr(spec)?;
    let socket = UnixDatagram::unbound().map_err(|source| NotifyError::SendFailed {
        spec: spec.to_owned(),
        source,
    })?;
    socket
        .connect_addr(&addr)
        .and_then(|()| socket.send(READY).map(|_| ()))
        .map_err(|source| NotifyError::SendFailed {
            spec: spec.to_owned(),
            source,
        })
}

/// Send `READY=1` per `$NOTIFY_SOCKET`: unset is the documented
/// no-op ([`NotifyOutcome::SocketUnset`]); otherwise exactly one
/// datagram ([`NotifyOutcome::Sent`]).
pub(crate) fn notify_ready() -> Result<NotifyOutcome, NotifyError> {
    match std::env::var("NOTIFY_SOCKET") {
        Err(std::env::VarError::NotPresent) => Ok(NotifyOutcome::SocketUnset),
        Err(std::env::VarError::NotUnicode(raw)) => Err(NotifyError::InvalidSocket {
            spec: raw.to_string_lossy().into_owned(),
            reason: "not valid unicode".to_owned(),
        }),
        Ok(spec) => send_ready(&spec).map(|()| NotifyOutcome::Sent),
    }
}

/// The daemons' startup readiness emission (see [`crate::runtime::serve`]
/// and [`crate::witness::serve`]): send `READY=1` per `$NOTIFY_SOCKET`
/// and log the outcome — sent, the standalone no-op, or the warning
/// that a send failed (never fatal; see the module docs). `daemon`
/// names the emitter in the structured log fields.
pub(crate) fn ready(daemon: &'static str) {
    match notify_ready() {
        Ok(NotifyOutcome::Sent) => tracing::info!(
            kind = "sd_notify",
            daemon,
            "READY=1 sent: startup reconciliation complete and the listener is bound"
        ),
        Ok(NotifyOutcome::SocketUnset) => tracing::debug!(
            kind = "sd_notify",
            daemon,
            "NOTIFY_SOCKET unset: standalone run, no readiness notification"
        ),
        Err(error) => tracing::warn!(
            kind = "sd_notify",
            daemon,
            %error,
            "the READY notification could not be sent; under Type=notify the service \
             manager restarts the unit at its own start timeout — the daemon keeps serving"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;
    use std::sync::Mutex;
    use std::time::Duration;

    /// `NOTIFY_SOCKET` is process-global and lib tests run in
    /// parallel: every test that reads or writes it holds this lock.
    /// The only reader is [`notify_ready`], exercised by these
    /// tests; the runtime tests' `serve` calls fail before the notify
    /// point, so nothing outside this module observes the variable.
    static ENV: Mutex<()> = Mutex::new(());

    /// Set `NOTIFY_SOCKET` for the current test.
    #[allow(unsafe_code)]
    fn set_notify_socket(spec: &OsStr) {
        // SAFETY: process-global environment mutation, serialized by
        // ENV and read only by the same serialized tests.
        unsafe { std::env::set_var("NOTIFY_SOCKET", spec) };
    }

    /// Clear `NOTIFY_SOCKET` for the current test.
    #[allow(unsafe_code)]
    fn clear_notify_socket() {
        // SAFETY: as `set_notify_socket`.
        unsafe { std::env::remove_var("NOTIFY_SOCKET") };
    }

    /// Receive one datagram, bounded, and assert it is exactly the
    /// protocol payload.
    fn expect_ready_datagram(receiver: &UnixDatagram) {
        receiver
            .set_read_timeout(Some(Duration::from_secs(10)))
            .expect("read timeout");
        // One byte past the payload: the datagram must be exact, not
        // merely prefixed.
        let mut buf = [0u8; READY.len() + 1];
        let (n, _) = receiver.recv_from(&mut buf).expect("the READY datagram");
        assert_eq!(n, READY.len(), "exactly READY=1, nothing more");
        assert_eq!(&buf[..n], READY);
    }

    #[test]
    fn an_unset_notify_socket_is_a_documented_no_op() {
        let _env = ENV.lock().expect("env lock");
        clear_notify_socket();
        assert_eq!(
            notify_ready().ok(),
            Some(NotifyOutcome::SocketUnset),
            "a standalone run (no NOTIFY_SOCKET) is a no-op, not an error"
        );
    }

    #[test]
    fn a_filesystem_notify_socket_receives_exactly_ready() {
        let _env = ENV.lock().expect("env lock");
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("notify.sock");
        let receiver = UnixDatagram::bind(&path).expect("bind the notify socket");
        set_notify_socket(path.as_os_str());
        assert_eq!(notify_ready().ok(), Some(NotifyOutcome::Sent));
        expect_ready_datagram(&receiver);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn an_abstract_notify_socket_receives_ready_with_the_nul_spelling() {
        use std::os::linux::net::SocketAddrExt;

        let _env = ENV.lock().expect("env lock");
        // Abstract names are system-global: carry the pid so a
        // parallel `cargo test` of another binary cannot collide.
        let name = format!("volvisor-notify-test-{}", std::process::id());
        let addr = SocketAddr::from_abstract_name(&name).expect("abstract address");
        let receiver = UnixDatagram::bind_addr(&addr).expect("bind the abstract socket");
        set_notify_socket(OsStr::new(&format!("@{name}")));
        assert_eq!(notify_ready().ok(), Some(NotifyOutcome::Sent));
        expect_ready_datagram(&receiver);
        // And the conversion itself: the '@' really becomes the
        // NUL-led abstract form, never a literal "@..." pathname.
        assert_eq!(
            socket_addr(&format!("@{name}"))
                .expect("abstract conversion")
                .as_abstract_name(),
            Some(name.as_bytes())
        );
    }

    #[test]
    fn a_dead_notify_socket_is_a_contained_typed_error() {
        let _env = ENV.lock().expect("env lock");
        let dir = tempfile::tempdir().expect("tempdir");
        // A socket path nothing ever bound: the connect fails — and
        // the failure is contained in the Result (the daemon's
        // wrapper only logs it; startup never fails on a
        // notification).
        set_notify_socket(dir.path().join("never-bound.sock").as_os_str());
        let result = notify_ready();
        assert!(
            matches!(result, Err(NotifyError::SendFailed { .. })),
            "a dead notify socket is a typed send failure: {result:?}"
        );
    }

    #[test]
    fn socket_specs_translate_per_the_protocol() {
        // Filesystem paths pass through verbatim.
        let addr = socket_addr("/run/volvisor/notify.sock").expect("pathname conversion");
        assert_eq!(
            addr.as_pathname(),
            Some(Path::new("/run/volvisor/notify.sock"))
        );
        // The empty spec is a typed refusal, not a panic.
        assert!(matches!(
            socket_addr(""),
            Err(NotifyError::InvalidSocket { .. })
        ));
        // A path with an interior NUL cannot be a pathname either.
        assert!(matches!(
            socket_addr("/a\0b"),
            Err(NotifyError::InvalidSocket { .. })
        ));
    }
}
