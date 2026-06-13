package org.gameros.ansync

import java.nio.ByteBuffer
import java.nio.ByteOrder

/**
 * Kotlin mirror of the Rust `encode_fs_req_for_kotlin` /
 * `decode_fs_reply_from_kotlin` tag-binary wire format defined in
 * `android/src/lib.rs`. Schema lives in two places — every change
 * must touch both files in the same commit.
 *
 * Encoding primitives:
 *   - integers: little-endian
 *   - strings: u32 length + UTF-8 bytes
 *   - blobs:   u32 length + bytes
 */

sealed class FsOpRequest {
    data class Stat(val path: String) : FsOpRequest()
    data class ReadDir(val path: String) : FsOpRequest()
    data class Open(val path: String, val flags: Int) : FsOpRequest()
    data class Read(val handle: Long, val offset: Long, val len: Int) : FsOpRequest()
    data class Write(val handle: Long, val offset: Long, val data: ByteArray) : FsOpRequest()
    data class Close(val handle: Long) : FsOpRequest()
    data class Create(val path: String, val mode: Int) : FsOpRequest()
    data class Unlink(val path: String) : FsOpRequest()
    data class Rename(val from: String, val to: String) : FsOpRequest()
    data class Truncate(val path: String, val size: Long) : FsOpRequest()
    data class Chmod(val path: String, val mode: Int) : FsOpRequest()
}

data class FsMeta(
    val size: Long,
    val mode: Int,
    val mtime: Long,
    val isDir: Boolean,
)

data class FsEntry(val name: String, val meta: FsMeta)

sealed class FsOpReply {
    data object Ok : FsOpReply()
    data class Stat(val meta: FsMeta) : FsOpReply()
    data class ReadDir(val entries: List<FsEntry>) : FsOpReply()
    data class Open(val handle: Long) : FsOpReply()
    data class Read(val data: ByteArray) : FsOpReply()
    data class Write(val written: Int) : FsOpReply()
    data class Create(val handle: Long) : FsOpReply()
    data class Error(val code: Int, val message: String) : FsOpReply()
}

object FsOpCodec {
    fun decodeRequest(bytes: ByteArray): FsOpRequest {
        require(bytes.isNotEmpty()) { "empty request" }
        val c = Cursor(bytes)
        return when (val tag = c.u8()) {
            0 -> FsOpRequest.Stat(c.str())
            1 -> FsOpRequest.ReadDir(c.str())
            2 -> FsOpRequest.Open(c.str(), c.i32())
            3 -> FsOpRequest.Read(c.i64(), c.i64(), c.i32())
            4 -> FsOpRequest.Write(c.i64(), c.i64(), c.blob())
            5 -> FsOpRequest.Close(c.i64())
            6 -> FsOpRequest.Create(c.str(), c.i32())
            7 -> FsOpRequest.Unlink(c.str())
            8 -> FsOpRequest.Rename(c.str(), c.str())
            9 -> FsOpRequest.Truncate(c.str(), c.i64())
            10 -> FsOpRequest.Chmod(c.str(), c.i32())
            else -> throw IllegalArgumentException("unknown FsOpRequest tag $tag")
        }
    }

    fun encodeReply(reply: FsOpReply): ByteArray {
        val out = Buffer()
        when (reply) {
            is FsOpReply.Ok -> out.u8(0)
            is FsOpReply.Stat -> { out.u8(1); writeMeta(out, reply.meta) }
            is FsOpReply.ReadDir -> {
                out.u8(2)
                out.u32(reply.entries.size)
                for (e in reply.entries) {
                    out.str(e.name)
                    writeMeta(out, e.meta)
                }
            }
            is FsOpReply.Open -> { out.u8(3); out.i64(reply.handle) }
            is FsOpReply.Read -> { out.u8(4); out.blob(reply.data) }
            is FsOpReply.Write -> { out.u8(5); out.u32(reply.written) }
            is FsOpReply.Create -> { out.u8(6); out.i64(reply.handle) }
            is FsOpReply.Error -> { out.u8(7); out.i32(reply.code); out.str(reply.message) }
        }
        return out.toByteArray()
    }

    private fun writeMeta(out: Buffer, meta: FsMeta) {
        out.i64(meta.size)
        out.i32(meta.mode)
        out.i64(meta.mtime)
        out.u8(if (meta.isDir) 1 else 0)
    }
}

private class Cursor(private val bytes: ByteArray) {
    private val buf = ByteBuffer.wrap(bytes).order(ByteOrder.LITTLE_ENDIAN)

    fun u8(): Int = buf.get().toInt() and 0xFF
    fun i32(): Int = buf.int
    fun i64(): Long = buf.long
    fun str(): String {
        val n = buf.int
        val out = ByteArray(n)
        buf.get(out)
        return String(out, Charsets.UTF_8)
    }
    fun blob(): ByteArray {
        val n = buf.int
        val out = ByteArray(n)
        buf.get(out)
        return out
    }
}

private class Buffer {
    private val out = ByteArray(256)
    private val bb = ByteBuffer.wrap(out).order(ByteOrder.LITTLE_ENDIAN)
    private val grow = mutableListOf<Byte>()

    fun u8(v: Int) { grow.add(v.toByte()) }
    fun u32(v: Int) {
        val b = ByteBuffer.allocate(4).order(ByteOrder.LITTLE_ENDIAN).putInt(v).array()
        b.forEach { grow.add(it) }
    }
    fun i32(v: Int) = u32(v)
    fun i64(v: Long) {
        val b = ByteBuffer.allocate(8).order(ByteOrder.LITTLE_ENDIAN).putLong(v).array()
        b.forEach { grow.add(it) }
    }
    fun str(v: String) {
        val bytes = v.toByteArray(Charsets.UTF_8)
        u32(bytes.size)
        bytes.forEach { grow.add(it) }
    }
    fun blob(v: ByteArray) {
        u32(v.size)
        v.forEach { grow.add(it) }
    }
    fun toByteArray(): ByteArray = grow.toByteArray()
}
