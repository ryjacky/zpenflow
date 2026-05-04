package dev.penflow

import android.app.PendingIntent
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.hardware.usb.UsbAccessory
import android.hardware.usb.UsbManager
import android.os.Build
import android.os.ParcelFileDescriptor
import android.util.Log
import kotlinx.coroutines.suspendCancellableCoroutine
import java.io.FileInputStream
import java.io.FileOutputStream
import kotlin.coroutines.resume

/**
 * Glue between Android's `UsbManager` accessory-mode API and our protocol's
 * stream-oriented [PenflowClient.connectViaStreams].
 *
 * Lifecycle:
 *   1. The system delivers a `USB_ACCESSORY_ATTACHED` intent containing a
 *      [UsbAccessory] (created by the platform after the PC sends AOA
 *      `START_ACCESSORY`).
 *   2. The app calls [requestPermissionIfNeeded] — first time only,
 *      Android pops a permission dialog with our app name + accessory
 *      strings.
 *   3. Once permission is granted, [open] returns the bidirectional
 *      [Streams] backed by the kernel-side `ParcelFileDescriptor`. The
 *      underlying file descriptor talks directly to the kernel USB
 *      driver — no userspace daemon (no ADB, no socket marshalling).
 *
 * Cancellation: dropping the [Streams] handle closes the
 * `ParcelFileDescriptor`, which closes both stream halves and releases
 * the USB endpoint claim back to the kernel.
 */
object UsbAccessoryConnection {

    /** Permission-grant action our internal `BroadcastReceiver` listens for. */
    private const val ACTION_USB_PERMISSION = "dev.penflow.USB_PERMISSION"

    /** Bidirectional stream pair backed by a single `ParcelFileDescriptor`. */
    data class Streams(
        val input: FileInputStream,
        val output: FileOutputStream,
        val pfd: ParcelFileDescriptor,
        val accessoryLabel: String,
    ) : AutoCloseable {
        override fun close() {
            try { input.close() } catch (_: Throwable) {}
            try { output.close() } catch (_: Throwable) {}
            try { pfd.close() } catch (_: Throwable) {}
        }
    }

    /**
     * Pull the [UsbAccessory] out of an `Intent` that was either delivered
     * via `USB_ACCESSORY_ATTACHED` or fetched from the singleton
     * `UsbManager.getAccessoryList()` for "we just resumed and an
     * accessory is already plugged in" cases.
     */
    fun extractAccessoryFromIntent(intent: Intent?): UsbAccessory? {
        if (intent == null) return null
        if (intent.action != UsbManager.ACTION_USB_ACCESSORY_ATTACHED) {
            // System launched us via a different intent (e.g. LAUNCHER).
            // Caller will fall through to the LocalSocket transport.
            return null
        }
        @Suppress("DEPRECATION")
        return if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            intent.getParcelableExtra(UsbManager.EXTRA_ACCESSORY, UsbAccessory::class.java)
        } else {
            intent.getParcelableExtra(UsbManager.EXTRA_ACCESSORY) as? UsbAccessory
        }
    }

    /**
     * If `UsbManager` already reports an accessory connected (we missed
     * the intent because we were already foreground), return the first.
     */
    fun firstConnectedAccessory(ctx: Context): UsbAccessory? {
        val mgr = ctx.getSystemService(Context.USB_SERVICE) as UsbManager
        @Suppress("DEPRECATION")
        return mgr.accessoryList?.firstOrNull()
    }

    /**
     * Suspend until the user grants permission for [accessory], then
     * return. If permission is already granted, return immediately.
     * Throws on denial.
     */
    suspend fun requestPermissionIfNeeded(ctx: Context, accessory: UsbAccessory) {
        val mgr = ctx.getSystemService(Context.USB_SERVICE) as UsbManager
        if (mgr.hasPermission(accessory)) return

        suspendCancellableCoroutine<Unit> { cont ->
            val receiver = object : BroadcastReceiver() {
                override fun onReceive(context: Context?, intent: Intent?) {
                    if (intent?.action != ACTION_USB_PERMISSION) return
                    try {
                        ctx.unregisterReceiver(this)
                    } catch (_: Throwable) {}
                    val granted = intent.getBooleanExtra(UsbManager.EXTRA_PERMISSION_GRANTED, false)
                    if (granted) {
                        cont.resume(Unit)
                    } else {
                        cont.cancel(IllegalStateException("USB accessory permission denied"))
                    }
                }
            }
            val flags = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
                Context.RECEIVER_NOT_EXPORTED
            } else {
                0
            }
            // FLAG_NOT_EXPORTED equivalent for older API: register as
            // application-private via the Context. Pre-API 33 the system
            // doesn't enforce export semantics for our own app's PendingIntent.
            try {
                ctx.registerReceiver(receiver, IntentFilter(ACTION_USB_PERMISSION), flags)
            } catch (e: Throwable) {
                cont.cancel(e)
                return@suspendCancellableCoroutine
            }

            val piFlags = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
                PendingIntent.FLAG_MUTABLE or PendingIntent.FLAG_UPDATE_CURRENT
            } else {
                PendingIntent.FLAG_UPDATE_CURRENT
            }
            val pi = PendingIntent.getBroadcast(
                ctx, 0, Intent(ACTION_USB_PERMISSION).setPackage(ctx.packageName), piFlags
            )
            mgr.requestPermission(accessory, pi)

            cont.invokeOnCancellation {
                try { ctx.unregisterReceiver(receiver) } catch (_: Throwable) {}
            }
        }
    }

    /**
     * Open the kernel-side bulk file descriptor for [accessory] and
     * wrap it as Java IO streams. Caller must call [Streams.close]
     * when done.
     */
    fun open(ctx: Context, accessory: UsbAccessory): Streams {
        val mgr = ctx.getSystemService(Context.USB_SERVICE) as UsbManager
        val pfd = mgr.openAccessory(accessory)
            ?: throw IllegalStateException("UsbManager.openAccessory returned null — permission likely revoked")
        val fd = pfd.fileDescriptor
        val input = FileInputStream(fd)
        val output = FileOutputStream(fd)
        val label = "usb:${accessory.manufacturer}/${accessory.model}@${accessory.version}"
        Log.i(TAG, "opened USB accessory: $label")
        return Streams(input, output, pfd, label)
    }

    private const val TAG = "UsbAccessory"
}
