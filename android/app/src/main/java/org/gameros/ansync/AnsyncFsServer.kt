package org.gameros.ansync

import android.content.ContentResolver
import android.content.Context
import android.net.Uri
import android.provider.DocumentsContract
import android.util.Log
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
 * SAF wiring is intentionally minimal in Step 9e: stat / readdir /
 * open / read have shipping handlers; create / write / unlink /
 * rename / truncate / chmod return `Error(ENOSYS)` until the
 * follow-up patch hooks them. Pipeline + Codec are validated
 * end-to-end already.
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
            is FsOpRequest.Write,
            is FsOpRequest.Create,
            is FsOpRequest.Unlink,
            is FsOpRequest.Rename,
            is FsOpRequest.Truncate,
            is FsOpRequest.Chmod -> FsOpReply.Error(code = ENOSYS, message = "not implemented yet")
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

    companion object {
        private const val TAG = "ansync.fs"
        private const val STOP_JOIN_MS = 1_000L
        private const val POLL_SLEEP_MS = 100L
        // errno mirrors of common cases the FUSE layer translates.
        private const val ENOENT = 2
        private const val EIO = 5
        private const val EBADF = 9
        private const val ENOSYS = 38
    }
}
