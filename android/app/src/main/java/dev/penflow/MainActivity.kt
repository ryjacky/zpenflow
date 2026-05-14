package dev.penflow

import android.app.Activity
import android.content.Context
import android.graphics.Rect
import android.net.ConnectivityManager
import android.net.LinkProperties
import android.net.Network
import android.net.NetworkCapabilities
import android.os.Bundle
import android.provider.Settings
import android.util.Log
import android.view.Gravity
import android.view.MotionEvent
import android.view.SurfaceHolder
import android.view.SurfaceView
import android.view.View
import android.view.WindowManager
import android.widget.FrameLayout
import android.widget.TextView
import java.net.Inet4Address
import java.net.NetworkInterface

/**
 * Top-level entry. Wires the Surface, the network client, and the pen
 * capture together. Phase 1 is intentionally minimal — connect on launch,
 * forward pen events, render incoming video.
 */
class MainActivity : Activity() {

    private lateinit var surfaceView: SurfaceView
    private lateinit var statusView: TextView
    private lateinit var client: PenflowClient
    private lateinit var penCapture: PenInputCapture
    private lateinit var touchCapture: TouchInputCapture

    @Volatile
    private var currentSurface: android.view.Surface? = null

    /** Rect (root-view pixels) the SurfaceView covers; smaller than the
     *  root when source aspect ≠ panel. Recomputed on each Connected. */
    @Volatile
    private var activeRect: Rect = Rect()

    /** Mirrors the PC's CLIENT_CONFIG SCREEN_OFF bit. Sticky per
     *  session; re-evaluated on reconnect. */
    @Volatile
    private var screenOff: Boolean = false

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        window.addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)

        setContentView(R.layout.activity_main)
        surfaceView = findViewById(R.id.video_surface)
        statusView = findViewById(R.id.status)

        // Hide system UI for fullscreen pen-display experience.
        window.decorView.systemUiVisibility = (
            View.SYSTEM_UI_FLAG_HIDE_NAVIGATION or
            View.SYSTEM_UI_FLAG_FULLSCREEN or
            View.SYSTEM_UI_FLAG_IMMERSIVE_STICKY or
            View.SYSTEM_UI_FLAG_LAYOUT_STABLE or
            View.SYSTEM_UI_FLAG_LAYOUT_HIDE_NAVIGATION or
            View.SYSTEM_UI_FLAG_LAYOUT_FULLSCREEN
        )

        surfaceView.holder.addCallback(object : SurfaceHolder.Callback {
            override fun surfaceCreated(holder: SurfaceHolder) {
                currentSurface = holder.surface
                Log.i(TAG, "surface ready ${surfaceView.width}x${surfaceView.height}")
            }

            override fun surfaceChanged(holder: SurfaceHolder, fmt: Int, w: Int, h: Int) {
                currentSurface = holder.surface
            }

            override fun surfaceDestroyed(holder: SurfaceHolder) {
                currentSurface = null
            }
        })

        // Capture pen events anywhere on the root view.
        val root = findViewById<View>(android.R.id.content)
        root.isFocusable = true
        root.isFocusableInTouchMode = true

        penCapture = PenInputCapture(
            activeRect = { activeRect },
            onEvent = { sample ->
                client.sendPenEvent(sample)
            }
        )

        touchCapture = TouchInputCapture(
            activeRect = { activeRect },
            onSnapshot = { snap ->
                client.sendTouchSnapshot(snap)
            }
        )

        // Both touch and hover events go through dispatchGenericMotionEvent /
        // dispatchTouchEvent. Subclassing the root view would be cleaner;
        // for now we override the activity-level hooks below.

        val hud = findViewById<HudView>(R.id.hud)

        client = PenflowClient(
            abstractName = "penflow",
            onState = { st -> runOnUiThread { renderState(st) } },
            surfaceProvider = { currentSurface },
            hud = hud,
            onClientConfig = { cfg ->
                runOnUiThread {
                    val vis = if (cfg.hudEnabled) android.view.View.VISIBLE
                              else android.view.View.GONE
                    // The HUD toggle hides BOTH overlays the user sees on the
                    // tablet: the right-side latency panel (HudView) and the
                    // top-left status / resolution readout. They're separate
                    // Views but conceptually one "instrumentation overlay".
                    hud.visibility = vis
                    statusView.visibility = vis

                    // Screen-off: hide the video surface and dim the panel
                    // (no video will arrive). Pen + touch input still flow.
                    screenOff = cfg.screenOff
                    surfaceView.visibility =
                        if (cfg.screenOff) android.view.View.GONE
                        else android.view.View.VISIBLE
                    applyScreenBrightness(cfg.screenOff)
                }
            },
        )
    }

    override fun onStart() {
        super.onStart()
        client.connect(detectDeviceCaps())
    }

    override fun onStop() {
        client.disconnect()
        super.onStop()
    }

    override fun onDestroy() {
        client.close()
        super.onDestroy()
    }

    override fun dispatchTouchEvent(ev: MotionEvent): Boolean {
        // Pen events first (they use a different toolType so don't conflict with
        // touch). If the pen capture rejects the event (toolType=FINGER), fall
        // through to touch capture.
        if (penCapture.consume(ev)) return true
        if (touchCapture.consume(ev)) return true
        return super.dispatchTouchEvent(ev)
    }

    override fun dispatchGenericMotionEvent(ev: MotionEvent): Boolean {
        // Hover events from the pen go through here while not contacting.
        if (penCapture.consume(ev)) return true
        return super.dispatchGenericMotionEvent(ev)
    }

    private fun renderState(st: PenflowClient.State) {
        // Compose a one-line state summary, then unconditionally append
        // the LAN IP + wireless-debugging port. Showing wireless info
        // on every state (including Connected) avoids the bug from the
        // first attempt at this feature: the previous code only
        // injected wireless info on Disconnected/Error, but in normal
        // use the app jumps straight from Connecting → Connected and
        // the info was never visible. The user reads the IP off the
        // tablet at any time to fill in Penflow's desktop wireless
        // panel, so making it persistent is the correct trade-off.
        val state = when (st) {
            PenflowClient.State.Disconnected -> "disconnected"
            PenflowClient.State.Connecting -> "connecting…"
            is PenflowClient.State.Connected -> if (screenOff)
                "pen tablet — display off (${st.width}x${st.height} target)"
            else
                "connected ${st.width}x${st.height}@${st.fps}"
            is PenflowClient.State.Error -> "error: ${st.message}"
        }
        statusView.text = withWirelessInfo(state)
        if (st is PenflowClient.State.Connected) {
            // Run the contain layout in both modes so activeRect preserves
            // the target monitor's aspect ratio — otherwise pen strokes
            // would be stretched in screen_off when panel and monitor
            // aspects differ. SurfaceView resize is a no-op when GONE.
            // TODO: expose a "Mapping" setting in the GUI (aspect-fit /
            // stretch / custom rect) so power users can pick.
            applyContainLayout(st.width, st.height)
        }
    }

    /** Per-window brightness override; no WRITE_SETTINGS needed. */
    private fun applyScreenBrightness(dim: Boolean) {
        val lp = window.attributes
        val target = if (dim) {
            WindowManager.LayoutParams.BRIGHTNESS_OVERRIDE_OFF
        } else {
            WindowManager.LayoutParams.BRIGHTNESS_OVERRIDE_NONE
        }
        if (lp.screenBrightness != target) {
            lp.screenBrightness = target
            window.attributes = lp
            Log.i(TAG, "screen_off=$dim — brightness override = $target")
        }
    }

    /** Contain-fit the SurfaceView to source dimensions. Posted to root
     *  so layout has finished — `Connected` can fire before `onLayout`. */
    private fun applyContainLayout(sourceWidth: Int, sourceHeight: Int) {
        if (sourceWidth <= 0 || sourceHeight <= 0) return
        val root = findViewById<View>(android.R.id.content)
        root.post {
            val pw = root.width
            val ph = root.height
            if (pw <= 0 || ph <= 0) return@post

            // contain: smaller scale fits both axes; other axis = bars.
            val scale = minOf(pw.toFloat() / sourceWidth, ph.toFloat() / sourceHeight)
            val rectW = (sourceWidth * scale).toInt().coerceAtLeast(1)
            val rectH = (sourceHeight * scale).toInt().coerceAtLeast(1)
            val left = (pw - rectW) / 2
            val top = (ph - rectH) / 2

            activeRect = Rect(left, top, left + rectW, top + rectH)

            val lp = surfaceView.layoutParams as? FrameLayout.LayoutParams
                ?: FrameLayout.LayoutParams(rectW, rectH)
            lp.width = rectW
            lp.height = rectH
            lp.gravity = Gravity.CENTER
            surfaceView.layoutParams = lp

            Log.i(TAG, "contain layout: panel=${pw}x${ph} source=${sourceWidth}x${sourceHeight} active=$activeRect")
        }
    }

    /**
     * Reports our static device capabilities to the PC. These are read
     * from the actual InputDevice when possible, with safe defaults for
     * the Wacom Pro Pen 3 if no device is enumerated yet.
     */
    private fun detectDeviceCaps(): PenflowClient.DeviceCaps {
        val display = windowManager.defaultDisplay
        val size = android.graphics.Point()
        @Suppress("DEPRECATION")
        display.getRealSize(size)

        // Defaults match Wacom Pro Pen 3 specs. Android InputDevice
        // normalizes pressure to 0..1, so reading getMotionRange().max
        // for AXIS_PRESSURE always yields 1.0 — useless. We hardcode the
        // raw resolution because PEN_EVENT carries normalized floats over
        // the wire anyway, and this field is informational for the PC.
        val pressureMax = 8191
        var tiltMin = -90
        var tiltMax = 90
        val buttons = 3

        // Read real tilt range from any present stylus InputDevice.
        for (id in android.view.InputDevice.getDeviceIds()) {
            val dev = android.view.InputDevice.getDevice(id) ?: continue
            if (dev.sources and android.view.InputDevice.SOURCE_STYLUS == 0) continue
            dev.getMotionRange(MotionEvent.AXIS_TILT)?.let {
                // AXIS_TILT in Android is reported in radians.
                tiltMin = Math.toDegrees(it.min.toDouble()).toInt()
                tiltMax = Math.toDegrees(it.max.toDouble()).toInt()
            }
            break
        }

        return PenflowClient.DeviceCaps(
            displayWidth = size.x,
            displayHeight = size.y,
            penMaxPressure = pressureMax,
            penTiltMinDeg = tiltMin,
            penTiltMaxDeg = tiltMax,
            penButtonsCount = buttons,
            codecCaps = MediaCodecCaps.queryHardwareDecodeBitmask(),
        )
    }

    /**
     * Append the tablet's LAN IP and (when readable) the wireless-debugging
     * port to a status string. The user reads these off the tablet panel
     * and types them into the Penflow desktop GUI's *Wireless* panel.
     *
     * The pairing code itself is generated and held inside Android's
     * system *Wireless debugging → Pair device with pairing code* dialog
     * — it is intentionally **not** exposed to third-party apps, so we
     * can't display it here. The IP we surface IS reusable for both the
     * connect endpoint and the pair endpoint (Android binds them to the
     * same address); only the ports differ.
     */
    private fun withWirelessInfo(prefix: String): String {
        val (ip, diag) = readLanIpv4WithDiag()
        val port = readAdbWifiPort()
        val portFragment = if (port != null) ":$port" else ""
        val hint = if (port == null) {
            "(open Settings → Wireless debugging for port + pair code)"
        } else {
            "(pair code from Settings → Wireless debugging)"
        }
        val line = if (ip != null) {
            "wifi: $ip$portFragment  $hint"
        } else {
            // Surface the diagnostic so we can see WHY lookup failed
            // without having to plug in adb logcat. This stays visible
            // until lookup starts working.
            "wifi: lookup failed — $diag"
        }
        return "$prefix\n$line"
    }

    /**
     * Return the first non-loopback IPv4 address bound to any active
     * interface, or `null` if none are up. `NetworkInterface` doesn't
     * need a manifest permission (unlike `WifiManager.connectionInfo`,
     * which now needs `ACCESS_FINE_LOCATION` on Android 10+), and it
     * works on Ethernet-only setups too.
     */
    /**
     * Try every Android API we have for "what is my IPv4 address" and
     * return the first hit along with a one-line diagnostic when no
     * strategy worked. We surface the diagnostic in the UI so failures
     * are visible without `adb logcat`.
     *
     * Strategy 1: `ConnectivityManager` — the canonical API. Walks
     *   all active networks, picks the one with WIFI / ETHERNET /
     *   anything-but-loopback transport, reads its `LinkProperties.
     *   linkAddresses` for the first IPv4.
     *
     * Strategy 2: `NetworkInterface` enumeration — fallback that
     *   doesn't need ConnectivityManager and works on some weird OEM
     *   builds where the CM path returns no LinkAddresses.
     */
    private fun readLanIpv4WithDiag(): Pair<String?, String> {
        val notes = StringBuilder()
        try {
            val cm = applicationContext.getSystemService(Context.CONNECTIVITY_SERVICE)
                as? ConnectivityManager
            if (cm == null) {
                notes.append("CM null; ")
            } else {
                val networks: Array<Network> = cm.allNetworks
                if (networks.isEmpty()) notes.append("no networks; ")
                // Sort by transport preference: WiFi > Ethernet > Cellular > other.
                val scored = networks.map { n ->
                    val caps = cm.getNetworkCapabilities(n)
                    val score = when {
                        caps == null -> 0
                        caps.hasTransport(NetworkCapabilities.TRANSPORT_WIFI) -> 100
                        caps.hasTransport(NetworkCapabilities.TRANSPORT_ETHERNET) -> 90
                        caps.hasTransport(NetworkCapabilities.TRANSPORT_CELLULAR) -> 30
                        else -> 10
                    }
                    Triple(score, n, caps)
                }.sortedByDescending { it.first }
                for ((score, net, _) in scored) {
                    val lp: LinkProperties = cm.getLinkProperties(net) ?: continue
                    for (la in lp.linkAddresses) {
                        val a = la.address
                        if (a is Inet4Address && !a.isLoopbackAddress) {
                            val host = a.hostAddress
                            if (host != null) {
                                Log.i(TAG, "readLanIpv4: CM hit iface=${lp.interfaceName} score=$score addr=$host")
                                return host to ""
                            }
                        }
                    }
                }
                notes.append("CM found no IPv4; ")
            }
        } catch (e: Exception) {
            notes.append("CM exception ${e.javaClass.simpleName}; ")
            Log.w(TAG, "readLanIpv4: CM path threw", e)
        }

        // Fallback: NetworkInterface enumeration.
        try {
            val ifs = NetworkInterface.getNetworkInterfaces()
            if (ifs == null) {
                notes.append("NI null")
                return null to notes.toString()
            }
            val candidates = mutableListOf<Triple<Int, String, String>>()
            for (iface in ifs.toList()) {
                val name = iface.name ?: ""
                val up = try { iface.isUp } catch (_: Exception) { false }
                val lb = try { iface.isLoopback } catch (_: Exception) { false }
                if (lb) continue
                val score = when {
                    name.startsWith("wlan") -> 100
                    name.startsWith("eth") -> 90
                    name.startsWith("rmnet") -> 30
                    else -> 10
                }
                for (addr in iface.inetAddresses.toList()) {
                    if (addr !is Inet4Address) continue
                    if (addr.isLoopbackAddress) continue
                    val host = addr.hostAddress ?: continue
                    Log.i(TAG, "readLanIpv4: NI candidate iface=$name up=$up score=$score addr=$host")
                    // Prefer "up" interfaces but don't exclude others —
                    // on some OEMs wlan0.isUp returns false even with
                    // a valid bound address.
                    val effective = score + (if (up) 5 else 0)
                    candidates += Triple(effective, name, host)
                }
            }
            val pick = candidates.maxByOrNull { it.first }
            return if (pick != null) {
                pick.third to ""
            } else {
                notes.append("NI no IPv4")
                null to notes.toString()
            }
        } catch (e: Exception) {
            notes.append("NI exception ${e.javaClass.simpleName}")
            Log.w(TAG, "readLanIpv4: NI path threw", e)
            return null to notes.toString()
        }
    }

    /**
     * Best-effort read of the Android 11+ wireless-debugging *connect*
     * port from `Settings.Global`. The key isn't part of the public SDK
     * and OEMs are inconsistent: AOSP / Pixel uses `adb_wifi_port`,
     * other OEMs may use a different key or hide the value. Returns
     * `null` whenever we can't get a positive integer back, and the
     * caller falls back to "see system Wireless debugging screen".
     *
     * The pair port is deliberately not probed — it's ephemeral, only
     * exists while the system pairing dialog is open, and isn't
     * exposed via Settings at all.
     */
    private fun readAdbWifiPort(): Int? {
        // Iterate every plausible vendor key we've seen mentioned in
        // AOSP, Samsung, and Xiaomi sources. Picking the first
        // non-zero hit keeps this resilient as OEMs rename.
        for (key in arrayOf("adb_wifi_port", "wifi_adb_port", "wireless_adb_port")) {
            try {
                val v = Settings.Global.getInt(contentResolver, key, -1)
                if (v in 1..65535) return v
            } catch (_: Exception) {
                // Some OEM builds throw SecurityException on unknown
                // keys instead of returning the default. Swallow and
                // try the next candidate.
            }
        }
        return null
    }

    companion object {
        private const val TAG = "MainActivity"
    }
}
