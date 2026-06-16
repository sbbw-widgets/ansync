package org.gameros.ansync

import android.content.ContentResolver
import android.content.Context
import android.net.Uri
import android.provider.DocumentsContract
import android.system.Os
import android.system.OsConstants
import android.util.Log
import java.io.FileOutputStream
import java.nio.ByteBuffer
import kotlin.concurrent.thread

/**
 * Worker thread that bridges the remote `FsOpMessage` RPC stream
 * (delivered via `NativeBridge.nativePollFsRequest`) into Android's
 * Storage Access Framework.
 *
 * Architecture:
 *   1. User picks a folder once via `Intent.ACTION_OPEN_DOCUMENT_TREE`
 *      and we `takePersistableUriPermission` so the tree URI survives
 *      reboot.
 *   2. The worker polls native for the next request, decodes via
 *      `FsOpCodec`, dispatches to a SAF op against the tree URI, and
 *      replies via `nativeFsReply`.
 *
 * SAF wiring covers stat / readdir / open / read / write / create /
 * unlink / rename / truncate. `chmod` returns ENOSYS by design —
 * SAF doesn't expose Unix modes. Rename is restricted to same-dir
 * (SAF's `renameDocument` semantic); cross-dir moves return EXDEV.
 */
class AnsyncFsServer(
    private val ctx: Context,
    private val treeUri: Uri,
) {
    @Volatile private var running = false
    private var worker: Thread? = null
    private val resolver: ContentResolver = ctx.contentResolver

    /** Open file handles assigned to remote `Open` calls. */
    private val handles = HashMap<Long, Uri>()
    private var nextHandle: Long = 1

    fun start() {
        if (running) return
        running = true
        worker = thread(name = "ansync-fs-worker", isDaemon = true) { loop() }
    }

    fun stop() {
        running = false
        worker?.join(STOP_JOIN_MS)
        worker = null
    }

    private fun loop() {
        while (running) {
            val reqBytes = NativeBridge.nativePollFsRequest()
            if (reqBytes == null) {
                Log.i(TAG, "native poll returned null; sleeping")
                Thread.sleep(POLL_SLEEP_MS)
                continue
            }
            val reply = try {
                handle(FsOpCodec.decodeRequest(reqBytes))
            } catch (e: Exception) {
                Log.w(TAG, "fs handler threw", e)
                FsOpReply.Error(code = EIO, message = e.message ?: "exception")
            }
            val ok = NativeBridge.nativeFsReply(FsOpCodec.encodeReply(reply))
            if (!ok) {
                Log.w(TAG, "nativeFsReply failed; exiting worker")
                return
            }
        }
    }

    private fun handle(req: FsOpRequest): FsOpReply {
        return when (req) {
            is FsOpRequest.Stat -> statAt(req.path)
            is FsOpRequest.ReadDir -> readdirAt(req.path)
            is FsOpRequest.Open -> openAt(req.path, req.flags)
            is FsOpRequest.Read -> readAt(req.handle, req.offset, req.len)
            is FsOpRequest.Close -> closeAt(req.handle)
            is FsOpRequest.Write -> writeAt(req.handle, req.offset, req.data)
            is FsOpRequest.Create -> createAt(req.path, req.mode)
            is FsOpRequest.Unlink -> unlinkAt(req.path)
            is FsOpRequest.Rename -> renameAt(req.from, req.to)
            is FsOpRequest.Truncate -> truncateAt(req.path, req.size)
            // SAF has no Unix mode bit surface — keep ENOSYS so the FUSE
            // layer reports "operation not supported" instead of lying.
            is FsOpRequest.Chmod -> FsOpReply.Error(code = ENOSYS, message = "chmod unsupported on SAF")
        }
    }

    private fun resolveChildDocUri(relativePath: String): Uri? {
        // Tree URIs carry both `tree/<doc>` and `document/<doc>` parts.
        // Resolve `/a/b/c` by walking from the tree's root doc id and
        // matching display names. SAF doesn't have a direct "open by
        // path" API — walking is the canonical pattern.
        val treeDocId = DocumentsContract.getTreeDocumentId(treeUri)
        val parts = relativePath.trim('/').split('/').filter { it.isNotEmpty() }
        var currentDocId = treeDocId
        for (segment in parts) {
            val childrenUri = DocumentsContract.buildChildDocumentsUriUsingTree(treeUri, currentDocId)
            val cursor = resolver.query(
                childrenUri,
                arrayOf(
                    DocumentsContract.Document.COLUMN_DOCUMENT_ID,
                    DocumentsContract.Document.COLUMN_DISPLAY_NAME,
                ),
                null,
                null,
                null,
            ) ?: return null
            var found: String? = null
            cursor.use {
                while (it.moveToNext()) {
                    if (it.getString(1) == segment) {
                        found = it.getString(0)
                        break
                    }
                }
            }
            currentDocId = found ?: return null
        }
        return DocumentsContract.buildDocumentUriUsingTree(treeUri, currentDocId)
    }

    private fun statAt(path: String): FsOpReply {
        val docUri = resolveChildDocUri(path) ?: return FsOpReply.Error(ENOENT, "not found")
        val cursor = resolver.query(
            docUri,
            arrayOf(
                DocumentsContract.Document.COLUMN_SIZE,
                DocumentsContract.Document.COLUMN_LAST_MODIFIED,
                DocumentsContract.Document.COLUMN_MIME_TYPE,
            ),
            null,
            null,
            null,
        ) ?: return FsOpReply.Error(EIO, "query failed")
        cursor.use {
            if (!it.moveToFirst()) return FsOpReply.Error(ENOENT, "empty cursor")
            val size = it.getLong(0)
            val mtime = it.getLong(1) / 1000  // SAF returns ms, FUSE wants s
            val mime = it.getString(2) ?: ""
            val isDir = mime == DocumentsContract.Document.MIME_TYPE_DIR
            return FsOpReply.Stat(FsMeta(size = size, mode = if (isDir) 0x4_1ED else 0x8_1A4, mtime = mtime, isDir = isDir))
        }
    }

    private fun readdirAt(path: String): FsOpReply {
        val docUri = resolveChildDocUri(path) ?: return FsOpReply.Error(ENOENT, "not found")
        val docId = DocumentsContract.getDocumentId(docUri)
        val childrenUri = DocumentsContract.buildChildDocumentsUriUsingTree(treeUri, docId)
        val cursor = resolver.query(
            childrenUri,
            arrayOf(
                DocumentsContract.Document.COLUMN_DISPLAY_NAME,
                DocumentsContract.Document.COLUMN_SIZE,
                DocumentsContract.Document.COLUMN_LAST_MODIFIED,
                DocumentsContract.Document.COLUMN_MIME_TYPE,
            ),
            null,
            null,
            null,
        ) ?: return FsOpReply.Error(EIO, "query failed")
        val entries = mutableListOf<FsEntry>()
        cursor.use {
            while (it.moveToNext()) {
                val name = it.getString(0) ?: continue
                val size = it.getLong(1)
                val mtime = it.getLong(2) / 1000
                val mime = it.getString(3) ?: ""
                val isDir = mime == DocumentsContract.Document.MIME_TYPE_DIR
                entries.add(FsEntry(name, FsMeta(size, if (isDir) 0x4_1ED else 0x8_1A4, mtime, isDir)))
            }
        }
        return FsOpReply.ReadDir(entries)
    }

    private fun openAt(path: String, flags: Int): FsOpReply {
        val docUri = resolveChildDocUri(path) ?: return FsOpReply.Error(ENOENT, "not found")
        synchronized(handles) {
            val handle = nextHandle++
            handles[handle] = docUri
            return FsOpReply.Open(handle)
        }
    }

    private fun readAt(handle: Long, offset: Long, len: Int): FsOpReply {
        val docUri = synchronized(handles) { handles[handle] }
            ?: return FsOpReply.Error(EBADF, "bad handle")
        return try {
            resolver.openFileDescriptor(docUri, "r").use { pfd ->
                if (pfd == null) return FsOpReply.Error(EIO, "openFileDescriptor null")
                val fd = pfd.fileDescriptor
                // FileInputStream + skip(offset) + read. SAF doesn't
                // expose pread; we rely on stream seek semantics. For
                // sequential reads this is fine; random reads on big
                // files re-skip from 0 on every call — acceptable for
                // a first cut, optimisable with a per-handle stream
                // cache in the follow-up.
                java.io.FileInputStream(fd).use { fis ->
                    var skipped = 0L
                    while (skipped < offset) {
                        val n = fis.skip(offset - skipped)
                        if (n <= 0) break
                        skipped += n
                    }
                    val buf = ByteArray(len)
                    val read = fis.read(buf, 0, len)
                    val payload = if (read <= 0) ByteArray(0) else buf.copyOf(read)
                    FsOpReply.Read(payload)
                }
            }
        } catch (e: Exception) {
            Log.w(TAG, "readAt failed", e)
            FsOpReply.Error(EIO, e.message ?: "read failed")
        }
    }

    private fun closeAt(handle: Long): FsOpReply {
        synchronized(handles) { handles.remove(handle) }
        return FsOpReply.Ok
    }

    private fun writeAt(handle: Long, offset: Long, data: ByteArray): FsOpReply {
        val docUri = synchronized(handles) { handles[handle] }
            ?: return FsOpReply.Error(EBADF, "bad handle")
        return try {
            resolver.openFileDescriptor(docUri, "rw").use { pfd ->
                if (pfd == null) return FsOpReply.Error(EIO, "openFileDescriptor null")
                val fd = pfd.fileDescriptor
                Os.lseek(fd, offset, OsConstants.SEEK_SET)
                FileOutputStream(fd).channel.use { ch ->
                    val buf = ByteBuffer.wrap(data)
                    var written = 0
                    while (buf.hasRemaining()) {
                        val n = ch.write(buf)
                        if (n <= 0) break
                        written += n
                    }
                    FsOpReply.Write(written = written)
                }
            }
        } catch (e: Exception) {
            Log.w(TAG, "writeAt failed", e)
            FsOpReply.Error(EIO, e.message ?: "write failed")
        }
    }

    private fun createAt(path: String, @Suppress("UNUSED_PARAMETER") mode: Int): FsOpReply {
        // Split into parent + child name. SAF requires a parent dir
        // URI + display name; there's no atomic "create at absolute
        // path" call.
        val (parentPath, name) = splitParent(path)
            ?: return FsOpReply.Error(EINVAL, "invalid path $path")
        val parentUri = resolveChildDocUri(parentPath)
            ?: return FsOpReply.Error(ENOENT, "parent not found: $parentPath")
        val mime = guessMime(name)
        return try {
            val newUri = DocumentsContract.createDocument(resolver, parentUri, mime, name)
                ?: return FsOpReply.Error(EIO, "createDocument returned null")
            val handle = synchronized(handles) {
                val h = nextHandle++
                handles[h] = newUri
                h
            }
            FsOpReply.Create(handle = handle)
        } catch (e: Exception) {
            Log.w(TAG, "createAt failed path=$path", e)
            FsOpReply.Error(EIO, e.message ?: "create failed")
        }
    }

    private fun unlinkAt(path: String): FsOpReply {
        val docUri = resolveChildDocUri(path) ?: return FsOpReply.Error(ENOENT, "not found")
        return try {
            if (DocumentsContract.deleteDocument(resolver, docUri)) {
                FsOpReply.Ok
            } else {
                FsOpReply.Error(EIO, "deleteDocument returned false")
            }
        } catch (e: Exception) {
            Log.w(TAG, "unlinkAt failed path=$path", e)
            FsOpReply.Error(EIO, e.message ?: "unlink failed")
        }
    }

    private fun renameAt(from: String, to: String): FsOpReply {
        val (fromParent, _) = splitParent(from) ?: return FsOpReply.Error(EINVAL, "bad from")
        val (toParent, newName) = splitParent(to) ?: return FsOpReply.Error(EINVAL, "bad to")
        if (fromParent != toParent) {
            // SAF `renameDocument` is rename-in-place. Cross-dir
            // requires `moveDocument` which needs both source + target
            // parent URIs and is a stickier semantics match; surface
            // EXDEV so userspace can fall back to copy + unlink.
            return FsOpReply.Error(EXDEV, "cross-dir rename unsupported")
        }
        val docUri = resolveChildDocUri(from) ?: return FsOpReply.Error(ENOENT, "not found")
        return try {
            val newUri = DocumentsContract.renameDocument(resolver, docUri, newName)
                ?: return FsOpReply.Error(EIO, "renameDocument returned null")
            // Update any open handles pointing at the old URI so
            // subsequent reads/writes don't break on rename.
            synchronized(handles) {
                handles.entries.filter { it.value == docUri }.forEach { handles[it.key] = newUri }
            }
            FsOpReply.Ok
        } catch (e: Exception) {
            Log.w(TAG, "renameAt failed from=$from to=$to", e)
            FsOpReply.Error(EIO, e.message ?: "rename failed")
        }
    }

    private fun truncateAt(path: String, size: Long): FsOpReply {
        val docUri = resolveChildDocUri(path) ?: return FsOpReply.Error(ENOENT, "not found")
        return try {
            resolver.openFileDescriptor(docUri, "rw").use { pfd ->
                if (pfd == null) return FsOpReply.Error(EIO, "openFileDescriptor null")
                Os.ftruncate(pfd.fileDescriptor, size)
                FsOpReply.Ok
            }
        } catch (e: Exception) {
            Log.w(TAG, "truncateAt failed path=$path size=$size", e)
            FsOpReply.Error(EIO, e.message ?: "truncate failed")
        }
    }

    private fun splitParent(path: String): Pair<String, String>? {
        val trimmed = path.trim('/')
        if (trimmed.isEmpty()) return null
        val idx = trimmed.lastIndexOf('/')
        return if (idx < 0) {
            "" to trimmed
        } else {
            trimmed.substring(0, idx) to trimmed.substring(idx + 1)
        }
    }

    private fun guessMime(name: String): String {
        val ext = name.substringAfterLast('.', "").lowercase()
        return when (ext) {
            "txt", "md", "log", "json", "yaml", "yml", "toml" -> "text/plain"
            "png" -> "image/png"
            "jpg", "jpeg" -> "image/jpeg"
            "webp" -> "image/webp"
            "gif" -> "image/gif"
            "mp4" -> "video/mp4"
            "mp3" -> "audio/mpeg"
            "pdf" -> "application/pdf"
            else -> "application/octet-stream"
        }
    }

    companion object {
        private const val TAG = "ansync.fs"
        private const val STOP_JOIN_MS = 1_000L
        private const val POLL_SLEEP_MS = 100L
        // errno mirrors of common cases the FUSE layer translates.
        private const val ENOENT = 2
        private const val EIO = 5
        private const val EBADF = 9
        private const val EXDEV = 18
        private const val EINVAL = 22
        private const val ENOSYS = 38
    }
}
