//! Wireless ADB helpers — `adb pair`, `adb connect`, `adb disconnect`.
//!
//! Wireless ADB (Android 11+) is a thin layer in front of the same
//! daemon → device protocol that USB uses. Once the tablet is paired
//! (one-time) and `adb connect <host>:<port>` has registered it as a
//! transport, *every* downstream adb command — including the
//! `adb reverse localabstract:...` call that
//! [`crate::adb::AdbLocalAbstractTransport`] makes — works exactly the
//! same as it would over USB.
//!
//! That means we **don't need a new `Transport` impl**. We just need to
//! arrange for `adb connect` to succeed before
//! [`AdbLocalAbstractTransport::bind_with_adb`] runs its first reverse
//! call. This module is the helper layer the service / GUI calls to
//! perform that arrangement.
//!
//! ## Pairing flow (Android 11+)
//!
//! On the tablet: *Developer options → Wireless debugging → Pair device
//! with pairing code*. Android shows an `IP:PAIR_PORT` and a 6-digit
//! code, both ephemeral. The pair port differs from the *connect* port
//! that the same screen lists once pairing succeeds.
//!
//! 1. [`pair`] runs `adb pair <host>:<pair_port> <code>`. This survives
//!    a daemon restart but does NOT establish a live connection — it
//!    only stores the host's ed25519 key on the tablet's allowlist.
//! 2. [`connect`] runs `adb connect <host>:<port>` against the tablet's
//!    *connect* port (the one shown once pairing finishes). This opens
//!    the live transport; from here, `adb devices` shows the tablet and
//!    every other adb command targets it.
//!
//! On modern Wacom MovinkPad firmware the connect port stays stable
//! across reboots once pairing is done, so users typically pair once
//! and reuse the connect host:port forever.

use std::io;

use crate::util::{resolve_through_shim, run_adb};

/// Run `adb pair <host>:<port> <code>` synchronously. Use this for the
/// first-time pairing flow when the user opens *Pair device with
/// pairing code* on the tablet.
///
/// `host:port` is the **pair port** (the one Android shows on the
/// pairing-code screen), NOT the connect port. Codes are 6 digits.
///
/// Returns the trimmed stdout of the adb invocation on success — adb
/// prints something like `Successfully paired to 192.168.1.42:39855
/// [guid=…]` which is occasionally useful for the GUI to surface.
pub async fn pair(adb_path: &str, host: &str, port: u16, code: &str) -> io::Result<String> {
    let adb_path = resolve_through_shim(adb_path);
    let host = host.to_string();
    let code = code.to_string();
    tokio::task::spawn_blocking(move || {
        let endpoint = format!("{host}:{port}");
        let out = run_adb(&adb_path, &["pair", &endpoint, &code])?;
        Ok::<_, io::Error>(String::from_utf8_lossy(&out.stdout).trim().to_string())
    })
    .await
    .map_err(|e| io::Error::other(format!("spawn_blocking join: {e}")))?
}

/// Run `adb connect <host>:<port>` synchronously and check the textual
/// status line adb prints to stdout.
///
/// `adb connect` is annoying: it returns exit code 0 even when the
/// connect fails ("failed to connect to 192.168.1.42:5555: …"). The
/// authoritative signal is the **stdout text**, which we parse and
/// surface as a hard error so callers don't move on to
/// `bind_with_adb()` against a phantom device.
pub async fn connect(adb_path: &str, host: &str, port: u16) -> io::Result<String> {
    let adb_path = resolve_through_shim(adb_path);
    let host = host.to_string();
    tokio::task::spawn_blocking(move || {
        let endpoint = format!("{host}:{port}");
        let out = run_adb(&adb_path, &["connect", &endpoint])?;
        let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
        match parse_connect_status(&stdout) {
            ConnectStatus::Ok => Ok::<_, io::Error>(stdout),
            ConnectStatus::AlreadyConnected => Ok::<_, io::Error>(stdout),
            ConnectStatus::Failed(reason) => Err(io::Error::other(format!(
                "adb connect {endpoint} failed: {reason}"
            ))),
        }
    })
    .await
    .map_err(|e| io::Error::other(format!("spawn_blocking join: {e}")))?
}

/// Run `adb disconnect <host>:<port>` synchronously. Best-effort —
/// failures are surfaced but callers can usually ignore them (cleanup
/// path).
pub async fn disconnect(adb_path: &str, host: &str, port: u16) -> io::Result<()> {
    let adb_path = resolve_through_shim(adb_path);
    let host = host.to_string();
    tokio::task::spawn_blocking(move || {
        let endpoint = format!("{host}:{port}");
        run_adb(&adb_path, &["disconnect", &endpoint]).map(|_| ())
    })
    .await
    .map_err(|e| io::Error::other(format!("spawn_blocking join: {e}")))?
}

/// One row of `adb devices -l`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdbDevice {
    /// Serial as adb reports it. For wireless devices this is the
    /// `host:port` of the connect endpoint; for USB it's an opaque
    /// device id.
    pub serial: String,
    /// `device`, `offline`, `unauthorized`, `recovery`, etc.
    pub state: String,
    /// True when [`serial`](Self::serial) looks like `<host>:<port>`
    /// (wireless transport).
    pub is_wireless: bool,
}

/// Run `adb devices -l` and return the parsed list of currently-known
/// transports. Used by the GUI's wireless panel to show whether the
/// tablet is actually visible to adb after a connect attempt.
pub async fn list_devices(adb_path: &str) -> io::Result<Vec<AdbDevice>> {
    let adb_path = resolve_through_shim(adb_path);
    tokio::task::spawn_blocking(move || {
        let out = run_adb(&adb_path, &["devices", "-l"])?;
        Ok::<_, io::Error>(parse_devices(&String::from_utf8_lossy(&out.stdout)))
    })
    .await
    .map_err(|e| io::Error::other(format!("spawn_blocking join: {e}")))?
}

#[derive(Debug, PartialEq, Eq)]
enum ConnectStatus {
    Ok,
    AlreadyConnected,
    Failed(String),
}

/// Parse the stdout of `adb connect <host>:<port>`.
///
/// adb's output isn't formally specified; we match on the prefixes that
/// every adb release since ~30.0.0 has used. Known shapes:
///
///   `connected to 192.168.1.42:5555`
///   `already connected to 192.168.1.42:5555`
///   `failed to connect to 192.168.1.42:5555: Connection refused`
///   `cannot connect to 192.168.1.42:5555: …` (older adb)
fn parse_connect_status(stdout: &str) -> ConnectStatus {
    let lower = stdout.to_ascii_lowercase();
    if lower.starts_with("already connected") {
        ConnectStatus::AlreadyConnected
    } else if lower.starts_with("connected to") {
        ConnectStatus::Ok
    } else {
        ConnectStatus::Failed(stdout.trim().to_string())
    }
}

/// Parse the multi-line output of `adb devices -l`. Header line and
/// blank lines are skipped; each remaining line is split on whitespace
/// with column 0 = serial, column 1 = state.
fn parse_devices(stdout: &str) -> Vec<AdbDevice> {
    let mut out = Vec::new();
    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with("List of devices") || line.starts_with('*') {
            continue;
        }
        let mut cols = line.split_whitespace();
        let Some(serial) = cols.next() else {
            continue;
        };
        let Some(state) = cols.next() else {
            continue;
        };
        let is_wireless = looks_like_wireless_serial(serial);
        out.push(AdbDevice {
            serial: serial.to_string(),
            state: state.to_string(),
            is_wireless,
        });
    }
    out
}

/// Heuristic: wireless adb serials are `<host>:<port>` where host is an
/// IPv4 / IPv6 literal or DNS name and port is a u16. USB serials never
/// contain `:` in any adb version we've seen. We just check for a
/// colon followed by all-digits, which is robust to both `1.2.3.4:5555`
/// and `[::1]:5555`.
fn looks_like_wireless_serial(serial: &str) -> bool {
    let Some(colon) = serial.rfind(':') else {
        return false;
    };
    let port = &serial[colon + 1..];
    !port.is_empty() && port.chars().all(|c| c.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connect_status_ok() {
        assert_eq!(
            parse_connect_status("connected to 192.168.1.42:5555"),
            ConnectStatus::Ok
        );
    }

    #[test]
    fn connect_status_already_connected_is_ok_equivalent() {
        assert_eq!(
            parse_connect_status("already connected to 192.168.1.42:5555"),
            ConnectStatus::AlreadyConnected
        );
    }

    #[test]
    fn connect_status_failure_surfaces_reason() {
        match parse_connect_status("failed to connect to 192.168.1.42:5555: Connection refused") {
            ConnectStatus::Failed(msg) => assert!(msg.contains("Connection refused")),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn parse_devices_skips_header_and_separators() {
        let sample = "List of devices attached\n\
                      192.168.1.42:39855  device product:movink model:WacomMovink\n\
                      ABCD1234            device usb:1-2 product:phone\n";
        let devs = parse_devices(sample);
        assert_eq!(devs.len(), 2);
        assert_eq!(devs[0].serial, "192.168.1.42:39855");
        assert_eq!(devs[0].state, "device");
        assert!(devs[0].is_wireless);
        assert_eq!(devs[1].serial, "ABCD1234");
        assert!(!devs[1].is_wireless);
    }

    #[test]
    fn parse_devices_handles_empty_output() {
        let devs = parse_devices("List of devices attached\n\n");
        assert!(devs.is_empty());
    }

    #[test]
    fn ipv6_serial_recognised_as_wireless() {
        assert!(looks_like_wireless_serial("[::1]:5555"));
    }

    #[test]
    fn usb_serial_not_wireless() {
        assert!(!looks_like_wireless_serial("ABCD1234"));
    }
}
