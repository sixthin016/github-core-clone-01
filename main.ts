// Entrypoint: supervisor tipis — seluruh lifecycle (ikat port ephemeral,
// register ke env, layani request, poll /done, keluar 0) tinggal di binary
// Rust ./forge. File ini hanya menjalankannya dan meneruskan kode keluarnya.
//
//   ./forge            binary Rust statis (cargo build --release --target
//                      x86_64-unknown-linux-musl, disalin ke root repo)
//   MAKEFUN_ENV_URL    dibaca oleh forge; kosong -> forge keluar 1.

import { spawn } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const dir = path.dirname(fileURLToPath(import.meta.url));
const child = spawn('./forge', { stdio: 'inherit', cwd: dir });

child.on('error', (err) => {
  process.stderr.write(`[entry] failed to start ./forge: ${err.message}\n`);
  process.exit(1);
});

child.on('exit', (code, signal) => {
  process.exit(code ?? (signal === null ? 0 : 1));
});
