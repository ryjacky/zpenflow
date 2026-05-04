package dev.penflow

import android.media.MediaCodec
import android.media.MediaFormat
import android.os.Build
import android.os.Handler
import android.os.HandlerThread
import android.util.Log
import android.view.Surface
import java.nio.ByteBuffer
import java.util.ArrayDeque

// API level constants for Surface.setFrameRate (declared inline so the file
// builds on min-SDK without a separate compatibility shim).
private const val FRAME_RATE_COMPATIBILITY_FIXED_SOURCE = 1

/**
 * Async hardware decoder rendering directly into a Surface.
 *
 * **Setup philosophy:** start from the predecessor's exactly-working
 * configuration (KEY_OPERATING_RATE=240 + KEY_PRIORITY=0 on Qualcomm,
 * vendor.qti-ext-dec-low-latency.enable=1, simple `releaseOutputBuffer(idx,
 * true)` rendering) and add design.md §10 optimizations only behind
 * specific gates so they can't break working chips.
 *
 * Design.md §10.2 Adreno 620 SIGSEGV fix:
 *   The combination KEY_OPERATING_RATE=240 + KEY_PRIORITY=0 crashes
 *   Adreno 620 (Snapdragon 765G — Mi 10 lite 5G, Redmi K30i 5G), per
 *   moonlight-android's MediaCodecHelper.java:482. The dev rig (MovinkPad
 *   / Adreno 720, Snapdragon 8s Gen 3) is fine with the combo. We
 *   detect the affected chip via `Build.HARDWARE` and fall back to
 *   moonlight's Short.MAX_VALUE workaround on those devices only.
 *
 * Render strategy: simple `releaseOutputBuffer(idx, true)` per callback,
 * paired with a `Surface.setFrameRate(fps, FIXED_SOURCE)` hint to keep
 * the panel's VRR controller from downclocking during idle periods.
 *   We previously implemented design.md §10.6 MIN_LATENCY drain
 *   (newest-wins, drop the rest, post render at System.nanoTime()) but
 *   it caused a "ratchet" decoder degradation on the MovinkPad: dropping
 *   most buffers made SurfaceFlinger see few presented frames, the panel
 *   dropped to a low refresh rate, BufferQueue back-pressured the
 *   remaining renders, the codec callback handler stalled inside
 *   `releaseOutputBuffer`, and `decodedNs` (captured at callback entry)
 *   ratcheted up to 30-40 ms when idle with no recovery until user touch
 *   re-woke the panel. The simple form matches predecessor's measured
 *   7-8 ms baseline and the setFrameRate hint stops the panel from idling
 *   in the first place.
 *
 * Vendor key `vendor.qti-ext-dec-picture-order.enable=1`:
 *   Disables HEVC reorder buffering on Qualcomm. Saves 5-10 ms of decode
 *   delay (moonlight finding). On non-Qualcomm codecs it's a silent
 *   no-op since MediaFormat doesn't validate vendor keys.
 *
 * **Still deferred** (need lifecycle plumbing): §10.3 codec recovery
 * ladder, §10.4 hung-decoder watchdog, §10.5 surface-destroyed handler.
 *
 * Codec mime is chosen at construct time from the handshake's
 * `HELLO_PC.codec`: `video/avc` or `video/hevc`. The csd-0 layout is
 * opaque to us — MediaCodec consumes whatever the PC encoder produced.
 */
class VideoDecoder(
    private val width: Int,
    private val height: Int,
    private val fps: Int,
    private val codecId: Byte,
    private val surface: Surface,
    private val csd0: ByteArray,
    private val onDecoded: (decodedNs: Long) -> Unit = {},
) {

    private val mime: String = mimeFor(codecId)
    private val codec: MediaCodec = MediaCodec.createDecoderByType(mime)

    /** Owning codec callbacks; same handler is reused for posted render releases. */
    private val codecThread = HandlerThread("video-codec").apply { start() }
    private val codecHandler = Handler(codecThread.looper)

    // Single mutex protecting both INPUT queues. Producer (network thread)
    // calls feed(); consumer (codec callback thread) calls onInputBufferAvailable.
    private val lock = Any()
    private val pendingData = ArrayDeque<ByteArray>()
    private val parkedIndices = ArrayDeque<Int>()

    fun start() {
        // codec.name reflects the actual decoder we got from
        // createDecoderByType — known before configure() so we can
        // branch the format flags correctly.
        val codecName = codec.name.lowercase()
        val isQualcomm = codecName.startsWith("omx.qcom.") || codecName.startsWith("c2.qti.")
        val isAdreno620 = isAdreno620Hardware()

        val format = MediaFormat.createVideoFormat(mime, width, height).apply {
            setByteBuffer("csd-0", ByteBuffer.wrap(csd0))
            setInteger(MediaFormat.KEY_LOW_LATENCY, 1)
            setInteger(MediaFormat.KEY_FRAME_RATE, fps)

            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.M) {
                if (isAdreno620) {
                    // §10.2: KEY_OPERATING_RATE=240 + KEY_PRIORITY=0
                    // SIGSEGVs Adreno 620 (Snapdragon 765G). Use
                    // moonlight's Short.MAX_VALUE workaround alone.
                    setInteger(MediaFormat.KEY_OPERATING_RATE, Short.MAX_VALUE.toInt())
                    // do NOT set KEY_PRIORITY here.
                } else {
                    // Predecessor's combo. Validated working on Adreno 720
                    // (Snapdragon 8s Gen 3 — the dev rig).
                    setInteger(MediaFormat.KEY_OPERATING_RATE, 240)
                    setInteger(MediaFormat.KEY_PRIORITY, 0)
                }
            }

            if (isQualcomm && Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
                // Qualcomm-private low-latency pipeline switch.
                setInteger("vendor.qti-ext-dec-low-latency.enable", 1)
                // Disables HEVC reorder buffering — saves 5-10 ms per frame.
                setInteger("vendor.qti-ext-dec-picture-order.enable", 1)
            }
        }

        codec.setCallback(object : MediaCodec.Callback() {
            override fun onInputBufferAvailable(c: MediaCodec, index: Int) {
                val data: ByteArray? = synchronized(lock) {
                    if (pendingData.isNotEmpty()) {
                        pendingData.removeFirst()
                    } else {
                        parkedIndices.addLast(index)
                        null
                    }
                }
                if (data != null) {
                    feedBuffer(c, index, data)
                }
            }

            override fun onOutputBufferAvailable(
                c: MediaCodec,
                index: Int,
                info: MediaCodec.BufferInfo
            ) {
                val decodedNs = System.nanoTime()
                // Render every output buffer immediately at the next vsync.
                //
                // Earlier we used a §10.6 MIN_LATENCY drain (newest-buffer-
                // wins, drop the rest, render task posted on the codec
                // handler with `System.nanoTime()` as the PTS). In practice
                // it produced a "ratchet" decoder degradation when idle:
                // most outputs were released with render=false, the panel's
                // VRR controller saw few presented frames and dropped to a
                // low refresh rate, the BufferQueue then back-pressured the
                // remaining renders, and `releaseOutputBuffer(idx, ts)` ate
                // codec-handler time waiting for a slot. Since callbacks
                // share the same handler, `decodedNs` (captured at callback
                // entry) lagged by the queueing delay — pushing dec_us to
                // 30-40 ms with no path back down until the user touched
                // the screen and the panel jumped to 120 Hz again.
                //
                // The predecessor's simple `releaseOutputBuffer(idx, true)`
                // measured 7-8 ms steady, with the bonus that SurfaceFlinger
                // sees a continuous frame stream and keeps the panel awake
                // at the requested rate (paired with the
                // `Surface.setFrameRate` hint below).
                c.releaseOutputBuffer(index, true)
                onDecoded(decodedNs)
            }

            override fun onError(c: MediaCodec, e: MediaCodec.CodecException) {
                Log.e(
                    TAG,
                    "decoder error: code=${e.errorCode} diagnostic=${e.diagnosticInfo} " +
                        "recoverable=${e.isRecoverable} transient=${e.isTransient}",
                    e
                )
            }

            override fun onOutputFormatChanged(c: MediaCodec, format: MediaFormat) {
                Log.i(TAG, "decoder output format: $format")
            }
        }, codecHandler)

        codec.configure(format, surface, null, 0)
        codec.start()

        // Tell SurfaceFlinger our intended source frame rate. Without this
        // hint MovinkPad's VRR / Adaptive Refresh Rate controller can drop
        // the panel to a low rate when the rest of the system looks idle —
        // which then back-pressures our render path (the BufferQueue fills
        // because consumer is slower than producer), the codec callback
        // handler gets stuck waiting on `releaseOutputBuffer`, and dec_us
        // ratchets up with no recovery until user touch wakes the panel.
        // FRAME_RATE_COMPATIBILITY_FIXED_SOURCE = "I want this exact rate;
        // pick the panel mode that doesn't drop frames against my source."
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
            try {
                surface.setFrameRate(fps.toFloat(), FRAME_RATE_COMPATIBILITY_FIXED_SOURCE)
            } catch (t: Throwable) {
                Log.w(TAG, "Surface.setFrameRate($fps) failed; panel VRR may downclock", t)
            }
        }

        Log.i(
            TAG,
            "started $mime decoder ${width}x${height}@${fps} on $codecName " +
                "(qualcomm=$isQualcomm, adreno620=$isAdreno620)"
        )
    }

    /** Submit a coded video access unit (Annex-B framed). */
    fun feed(coded: ByteArray) {
        val parkedIndex: Int? = synchronized(lock) {
            if (parkedIndices.isNotEmpty()) {
                parkedIndices.removeFirst()
            } else {
                pendingData.addLast(coded)
                null
            }
        }
        if (parkedIndex != null) {
            feedBuffer(codec, parkedIndex, coded)
        }
    }

    private fun feedBuffer(c: MediaCodec, index: Int, data: ByteArray) {
        val buf = c.getInputBuffer(index) ?: return
        buf.clear()
        buf.put(data)
        c.queueInputBuffer(index, 0, data.size, System.nanoTime() / 1000, 0)
    }

    fun stop() {
        try {
            codec.stop()
        } catch (_: IllegalStateException) {
        }
        codec.release()
        codecThread.quitSafely()
    }

    companion object {
        private const val TAG = "VideoDecoder"

        fun mimeFor(codec: Byte): String = when (codec) {
            Protocol.CODEC_H264 -> "video/avc"
            Protocol.CODEC_HEVC -> "video/hevc"
            else -> throw IllegalArgumentException("unknown codec id: $codec")
        }

        /**
         * Identify Adreno 620 chips (Snapdragon 765G — Xiaomi Mi 10 lite
         * 5G, Redmi K30i 5G, etc.) where KEY_OPERATING_RATE=240 +
         * KEY_PRIORITY=0 SIGSEGVs the decoder. moonlight-android's
         * MediaCodecHelper.java enumerates the same set via Build.HARDWARE
         * being "lito" (Snapdragon 765G's hardware identifier).
         *
         * False negatives are safer than false positives: a missed
         * detection means we use the same combo predecessor uses today,
         * which works on every other Adreno we've tested.
         */
        private fun isAdreno620Hardware(): Boolean {
            val hw = Build.HARDWARE.lowercase()
            // "lito" = Snapdragon 765G platform = Adreno 620.
            return hw == "lito"
        }
    }
}
