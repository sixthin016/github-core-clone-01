// http.ts — small request/response helpers shared by every surface.
//
// Every JSON reply is `application/json` and carries a non-empty
// `X-GitHub-Request-Id` (a per-process counter, so the header is stable for a
// given request order). Error bodies are always JSON with a string `message`.

import type { IncomingMessage, ServerResponse } from 'node:http';
import type { Store } from './store.ts';

export const JSON_TYPE = 'application/json; charset=utf-8';

/** Read a request body whole, as a Buffer. */
export function readBody(req: IncomingMessage): Promise<Buffer> {
  return new Promise<Buffer>((resolve, reject) => {
    const chunks: Buffer[] = [];
    req.on('data', (chunk: Buffer) => chunks.push(chunk));
    req.on('end', () => resolve(Buffer.concat(chunks)));
    req.on('error', reject);
  });
}

export function sendJson(
  store: Store,
  res: ServerResponse,
  status: number,
  body: unknown,
  extra: Record<string, string> = {},
): void {
  const text = `${JSON.stringify(body)}\n`;
  res.writeHead(status, {
    'content-type': JSON_TYPE,
    'content-length': Buffer.byteLength(text),
    'x-github-request-id': store.nextRequestId(),
    ...extra,
  });
  res.end(text);
}

export function sendRaw(
  store: Store,
  res: ServerResponse,
  status: number,
  body: Buffer,
  contentType: string,
  extra: Record<string, string> = {},
): void {
  res.writeHead(status, {
    'content-type': contentType,
    'content-length': body.length,
    'x-github-request-id': store.nextRequestId(),
    ...extra,
  });
  res.end(body);
}

export function sendError(store: Store, res: ServerResponse, status: number, message: string): void {
  sendJson(store, res, status, { message });
}

/** Parse a JSON body; returns null (and the caller answers 400) when malformed. */
export function parseJsonBody(body: Buffer): { ok: true; value: unknown } | { ok: false } {
  if (body.length === 0) return { ok: true, value: {} };
  try {
    return { ok: true, value: JSON.parse(body.toString('utf8')) as unknown };
  } catch {
    return { ok: false };
  }
}

/** The request's path segments, decoded, without empty leading/trailing parts. */
export function pathSegments(req: IncomingMessage): string[] {
  try {
    const url = new URL(req.url ?? '/', 'http://entry');
    return url.pathname.split('/').filter((part) => part !== '').map((part) => decodeURIComponent(part));
  } catch {
    return [];
  }
}

/** The request's query parameters. */
export function queryOf(req: IncomingMessage): URLSearchParams {
  try {
    return new URL(req.url ?? '/', 'http://entry').searchParams;
  } catch {
    return new URLSearchParams();
  }
}

/** A positive integer query value, or `fallback` when absent or invalid. */
export function queryInt(query: URLSearchParams, name: string, fallback: number): number {
  const raw = query.get(name);
  if (raw === null) return fallback;
  const value = Number.parseInt(raw, 10);
  return Number.isFinite(value) && value > 0 ? value : fallback;
}
