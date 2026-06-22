package org.gameros.ansync

import java.nio.ByteBuffer
import java.nio.ByteOrder

/**
 * Tag-binary file-transfer progress wire mirrored from Rust
 * `encode_progress_for_kotlin` in `android/src/lib.rs`. Any change
 * requires a matching diff in that file in the same commit.
 *
 * Wire (little-endian throughout):
 *   batch_id           u64
 *   transfer_id        u64
 *   name_len           u16
 *   name               utf8[name_len]
 *   bytes              u64
 *   total              u64
 *   direction          u8     (0 send, 1 receive)
 *   batch_files        u32
 *   batch_files_done   u32
 *   batch_bytes_done   u64
 *   batch_total_bytes  u64
 */
data class WireProgress(
    val batchId: Long,
    val transferId: Long,
    val name: String,
    val bytes: Long,
    val total: Long,
    val direction: Direction,
    val batchFiles: Int,
    val batchFilesDone: Int,
    val batchBytesDone: Long,
    val batchTotalBytes: Long,
) {
    enum class Direction { Send, Receive }

    /** Percent done across the whole batch, clamped to 0..100. */
    fun batchPercent(): Int {
        if (batchTotalBytes <= 0L) return 100
        val raw = (batchBytesDone * 100L) / batchTotalBytes
        return raw.coerceIn(0L, 100L).toInt()
    }

    companion object {
        fun decode(bytes: ByteArray): WireProgress? {
            if (bytes.size < 8 + 8 + 2) return null
            val buf = ByteBuffer.wrap(bytes).order(ByteOrder.LITTLE_ENDIAN)
            val batchId = buf.long
            val transferId = buf.long
            val nameLen = (buf.short.toInt() and 0xFFFF)
            if (buf.remaining() < nameLen + 8 + 8 + 1 + 4 + 4 + 8 + 8) return null
            val nameBytes = ByteArray(nameLen)
            buf.get(nameBytes)
            val name = String(nameBytes, Charsets.UTF_8)
            val sent = buf.long
            val total = buf.long
            val direction = when (buf.get().toInt() and 0xFF) {
                1 -> Direction.Receive
                else -> Direction.Send
            }
            val batchFiles = buf.int
            val batchFilesDone = buf.int
            val batchBytesDone = buf.long
            val batchTotalBytes = buf.long
            return WireProgress(
                batchId = batchId,
                transferId = transferId,
                name = name,
                bytes = sent,
                total = total,
                direction = direction,
                batchFiles = batchFiles,
                batchFilesDone = batchFilesDone,
                batchBytesDone = batchBytesDone,
                batchTotalBytes = batchTotalBytes,
            )
        }
    }
}
