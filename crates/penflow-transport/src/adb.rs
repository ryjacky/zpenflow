//! ADB reverse-tunnel transport.
//!
//! The Android client opens `LocalSocket("penflow", ABSTRACT)`; ADB's reverse
//! tunnel forwards that to a TCP port on the host, which we listen on with
//! a tokio `TcpListener`. Bootstrap matches design.md §8.2:
//!
//!   1. `adb start-server` (idempotent — no-op if a daemon is already running).
//!   2. Bind a TCP listener on `127.0.0.1:0` (let the OS pick a free port).
//!   3. `adb reverse localabstract:penflow tcp:<assigned_port>`.
//!   4. `accept()` returns the first connection.
//!
//! Shutdown removes the reverse rule so subsequent runs / other tools can
//! re-bind the local-abstract name.
//!
//! HANDOFF §4.4 calls out the trap that ADB happily accepts a TCP `connect()`
//! while the Android app is still launching — the server then sits waiting
//! for `HELLO_ANDROID` that never comes. Today we punt on that (the
//! `HELLO_ANDROID` read times out and the operator restarts); design §7.3's
//! `READY_BYTE = 0xA5` probe is still TODO and lands when the Android client
//! adds support.

use std::io;
use std::process::{Command, Stdio};
use std::time::Duration;

use async_trait::async_trait;
use tokio::net::TcpListener;
use tokio::sync::Mutex;

use crate::util::{resolve_through_shim, run_adb, silent};
use crate::{Transport, TransportStream};

/// Default `localabstract:` name. Must match
/// `PenflowClient.kt`'s `abstractName = "penflow"`.
pub const DEFAULT_ABSTRACT_NAME: &str = "penflow";

/// ADB-reverse-tunnel transport. Build with [`AdbLocalAbstractTransport::bind`].
pub struct AdbLocalAbstractTransport {
    abstract_name: String,
    /// `Mutex<Option<TcpListener>>` because `Transport::accept` takes `&self`,
    /// but `TcpListener::accept` is `&self`-callable too — so really the
    /// mutex only protects the "listener has been taken / shut down" state.
    listener: Mutex<Option<TcpListener>>,
    bound_port: u16,
    /// `adb` executable path. Lets tests / packaged installers override.
    adb_path: String,
    /// Device serial to target with `adb -s <serial>`, or `None` to
    /// let adb pick the only attached device. Required whenever more
    /// than one transport is registered (e.g. USB + wireless both
    /// active) — without it, `adb reverse` fails with `more than one
    /// device/emulator`.
    target_serial: Option<String>,
    reverse_active: Mutex<bool>,
}

impl AdbLocalAbstractTransport {
    /// Bind a TCP listener on `127.0.0.1:0`, then run
    /// `adb reverse localabstract:<name> tcp:<port>`.
    pub async fn bind(abstract_name: impl Into<String>) -> io::Result<Self> {
        Self::bind_with_adb(abstract_name, "adb").await
    }

    /// Like [`bind`] but with a custom `adb` executable path. Used by the
    /// MSI installer to point at a bundled adb, and by tests to point at a
    /// shim.
    pub async fn bind_with_adb(
        abstract_name: impl Into<String>,
        adb_path: impl Into<String>,
    ) -> io::Result<Self> {
        Self::bind_with_adb_targeting(abstract_name, adb_path, None::<String>).await
    }

    /// Like [`bind_with_adb`] but targets a specific device serial with
    /// `adb -s <serial>`. Pass `Some(host:port)` for wireless ADB so
    /// the reverse-tunnel call doesn't fail with "more than one
    /// device/emulator" when a USB tablet is also plugged in.
    pub async fn bind_with_adb_targeting(
        abstract_name: impl Into<String>,
        adb_path: impl Into<String>,
        target_serial: Option<impl Into<String>>,
    ) -> io::Result<Self> {
        let abstract_name = abstract_name.into();
        let target_serial: Option<String> = target_serial.map(Into::into);
        // Resolve scoop-style shims to their underlying executable. See
        // [`resolve_through_shim`] for the why; tl;dr: scoop's shimexe
        // wrapper falls back to console-mode handle inheritance when it
        // can't determine the target's subsystem, and a windows_subsystem
        // = "windows" parent (Penflow's release build) has no console
        // for it to inherit, which makes CreateProcess fail with
        // "Could not create process". Going straight to the underlying
        // adb.exe sidesteps the entire shim.
        let adb_path: String = resolve_through_shim(&adb_path.into());

        // 1. Start the adb daemon (idempotent — `start-server` is a no-op
        //    if one is already running). On a fresh install the very
        //    first start-server invocation can take 10–30s while Windows
        //    Defender scans adb.exe and the daemon initializes its
        //    keyring; wrap in spawn_blocking so the tokio executor
        //    thread isn't pinned. Without this wrap, every other async
        //    task on that worker (Tauri command handlers, the event
        //    pump, etc.) stalls until daemon startup completes — and
        //    that's the most plausible explanation for the "first
        //    launch can't connect to ADB" symptom users hit immediately
        //    after MSI install.
        {
            let adb_path = adb_path.clone();
            tokio::task::spawn_blocking(move || run_adb(&adb_path, &["start-server"]))
                .await
                .map_err(|e| io::Error::other(format!("spawn_blocking join: {e}")))??;
        }

        // 2. Bind a TCP listener on a kernel-assigned port.
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let bound_port = listener.local_addr()?.port();

        // 3. Set up the reverse tunnel. Also defensively remove any
        //    stale rule from a prior crashed Penflow that didn't run
        //    its Drop impl — that rule would point at a defunct TCP
        //    port, and overwriting via plain `adb reverse` should be
        //    fine in practice but let's be explicit.
        //
        //    When `target_serial` is set, every adb invocation is
        //    prefixed with `-s <serial>`. Without it, `adb reverse`
        //    errors with "more than one device/emulator" whenever
        //    multiple transports are registered (the canonical case:
        //    USB tablet AND wireless tablet both showing up in
        //    `adb devices`).
        {
            let adb_path = adb_path.clone();
            let abstract_name = abstract_name.clone();
            let target_serial = target_serial.clone();
            tokio::task::spawn_blocking(move || {
                let local_abs = format!("localabstract:{abstract_name}");
                let tcp_arg = format!("tcp:{bound_port}");
                // Best-effort cleanup; ignore errors (no rule present
                // is fine).
                let mut remove_args: Vec<&str> = Vec::new();
                if let Some(serial) = target_serial.as_deref() {
                    remove_args.push("-s");
                    remove_args.push(serial);
                }
                remove_args.extend_from_slice(&["reverse", "--remove", &local_abs]);
                let _ = run_adb(&adb_path, &remove_args);

                let mut bind_args: Vec<&str> = Vec::new();
                if let Some(serial) = target_serial.as_deref() {
                    bind_args.push("-s");
                    bind_args.push(serial);
                }
                bind_args.extend_from_slice(&["reverse", &local_abs, &tcp_arg]);
                run_adb(&adb_path, &bind_args)
            })
            .await
            .map_err(|e| io::Error::other(format!("spawn_blocking join: {e}")))??;
        }

        Ok(Self {
            abstract_name,
            listener: Mutex::new(Some(listener)),
            bound_port,
            adb_path,
            target_serial,
            reverse_active: Mutex::new(true),
        })
    }

    /// The PC-side TCP port that ADB is forwarding to.
    pub fn bound_port(&self) -> u16 {
        self.bound_port
    }

    /// The `localabstract:` name on the Android side.
    pub fn abstract_name(&self) -> &str {
        &self.abstract_name
    }
}

#[async_trait]
impl Transport for AdbLocalAbstractTransport {
    async fn accept(&self) -> io::Result<TransportStream> {
        let mut g = self.listener.lock().await;
        let listener = g.as_mut().ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotConnected, "transport already shut down")
        })?;
        let (sock, peer) = listener.accept().await?;
        // Disable Nagle so small input messages (PEN_EVENT, TIME_SYNC_REQ)
        // ship immediately. ADB's USB tunnel is already low-latency; we
        // don't want the kernel to coalesce.
        sock.set_nodelay(true).ok();

        let (read_half, write_half) = sock.into_split();
        Ok(TransportStream {
            reader: Box::new(read_half),
            writer: Box::new(write_half),
            peer_label: format!("adb:{peer}"),
        })
    }

    async fn shutdown(&self) -> io::Result<()> {
        // Drop the listener first so accept() unblocks if anything is
        // currently waiting.
        {
            let mut g = self.listener.lock().await;
            *g = None;
        }
        // Best-effort remove the reverse rule. If adb is gone or the rule
        // isn't there any more, nothing useful to surface; log-and-ignore.
        let mut active = self.reverse_active.lock().await;
        if *active {
            // Use a short timeout so we don't hang shutdown on an unresponsive
            // adb daemon.
            let _ = tokio::time::timeout(
                Duration::from_secs(2),
                tokio::task::spawn_blocking({
                    let adb_path = self.adb_path.clone();
                    let abstract_name = self.abstract_name.clone();
                    let target_serial = self.target_serial.clone();
                    move || {
                        let mut cmd = Command::new(&adb_path);
                        if let Some(serial) = target_serial.as_deref() {
                            cmd.arg("-s").arg(serial);
                        }
                        cmd.args([
                            "reverse",
                            "--remove",
                            &format!("localabstract:{abstract_name}"),
                        ])
                        .stdout(Stdio::null())
                        .stderr(Stdio::null());
                        let _ = silent(&mut cmd).status();
                    }
                }),
            )
            .await;
            *active = false;
        }
        Ok(())
    }
}

impl Drop for AdbLocalAbstractTransport {
    fn drop(&mut self) {
        // Best-effort cleanup. `try_lock` because we may be in a sync drop
        // context and shouldn't deadlock if shutdown() is also racing.
        if let Ok(mut active) = self.reverse_active.try_lock() {
            if *active {
                let mut cmd = Command::new(&self.adb_path);
                if let Some(serial) = self.target_serial.as_deref() {
                    cmd.arg("-s").arg(serial);
                }
                cmd.args([
                    "reverse",
                    "--remove",
                    &format!("localabstract:{}", self.abstract_name),
                ])
                .stdout(Stdio::null())
                .stderr(Stdio::null());
                let _ = silent(&mut cmd).status();
                *active = false;
            }
        }
    }
}
