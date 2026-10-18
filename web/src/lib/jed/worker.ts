/// <reference lib="webworker" />
// OPFS file-manager utility only. Database SQL uses the shared TS async runtime.
import { EngineError } from '$jed/errors.ts';
import type { FileRequest, FileReply, OpfsFileInfo } from './protocol.ts';

function opfsRoot(): Promise<FileSystemDirectoryHandle> {
  if (typeof navigator === 'undefined' || !navigator.storage || !navigator.storage.getDirectory) {
    throw new EngineError('feature_not_supported', 'OPFS is not available in this browser');
  }
  return navigator.storage.getDirectory();
}

async function listFiles(): Promise<OpfsFileInfo[]> {
  const root = await opfsRoot();
  const files: OpfsFileInfo[] = [];
  for await (const [name, handle] of root.entries()) {
    if (handle.kind === 'file' && name.endsWith('.jed')) {
      const file = await (handle as FileSystemFileHandle).getFile();
      files.push({ name, size: file.size });
    }
  }
  files.sort((a, b) => a.name.localeCompare(b.name));
  return files;
}

async function exportBytes(name: string): Promise<Uint8Array> {
  const root = await opfsRoot();
  const handle = await root.getFileHandle(name);
  const file = await handle.getFile();
  return new Uint8Array(await file.arrayBuffer());
}

async function importBytes(name: string, bytes: Uint8Array, overwrite: boolean): Promise<void> {
  const root = await opfsRoot();
  if (!overwrite) {
    // Reject an existing name (mirrors create's 58P02), unless overwrite is requested.
    let exists = true;
    try {
      await root.getFileHandle(name);
    } catch {
      exists = false;
    }
    if (exists) throw new EngineError('duplicate_file', `database ${name} already exists`);
  }
  const handle = await root.getFileHandle(name, { create: true });
  // Write through a sync access handle (worker-only) — the portable path (Safari historically lacks
  // createWritable in OPFS). The same primitive the OPFS host uses (opfs.ts).
  const access = await handle.createSyncAccessHandle();
  try {
    access.truncate(0);
    access.write(bytes, { at: 0 });
    access.flush();
  } finally {
    access.close();
  }
}

async function deleteFile(name: string): Promise<void> {
  const root = await opfsRoot();
  await root.removeEntry(name);
}

function opfsSupported(): boolean {
  return (
    typeof navigator !== 'undefined' &&
    !!navigator.storage &&
    typeof navigator.storage.getDirectory === 'function'
  );
}

async function handle(req: FileRequest): Promise<unknown> {
  switch (req.op) {
    case 'opfsSupported':
      return opfsSupported();
    case 'listFiles':
      return listFiles();
    case 'exportBytes':
      return exportBytes(req.name);
    case 'importBytes':
      return importBytes(req.name, req.bytes, req.overwrite);
    case 'deleteFile':
      return deleteFile(req.name);
  }
}

// File mutations are ordered across asynchronous OPFS acquisition as well as writes.
let tail = Promise.resolve();
addEventListener('message', (event: MessageEvent<FileRequest>) => {
  const req = event.data;
  tail = tail.then(async () => {
    let reply: FileReply;
    try {
      reply = { rid: req.rid, ok: true, result: await handle(req) };
    } catch (error) {
      reply = {
        rid: req.rid,
        ok: false,
        error: {
          code: error instanceof EngineError ? error.code() : 'XX000',
          message: error instanceof Error ? error.message : String(error)
        }
      };
    }
    const transfer =
      reply.ok && reply.result instanceof Uint8Array ? [reply.result.buffer as ArrayBuffer] : [];
    postMessage(reply, transfer);
  });
});
