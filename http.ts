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
  sendJsonText(store, res, status, `${JSON.stringify(body)}\n`, extra);
}

/** Send an already-serialised JSON text (hot path: a cached issue body). */
export function sendJsonText(
  store: Store,
  res: ServerResponse,
  status: number,
  text: string,
  extra: Record<string, string> = {},
): void {
  const buf = Buffer.from(text, 'utf8');
  res.writeHead(status, {
    'content-type': JSON_TYPE,
    'content-length': buf.length,
    'x-github-request-id': store.nextRequestId(),
    ...extra,
  });
  res.end(buf);
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

/** One URL parse per request, shared by every handler: app.ts stashes it on the
 * request (see parseRequest), and these helpers read the stash instead of
 * parsing the URL again (the old code parsed it up to four times per request:
 * pathOf + pathSegments in app.ts, pathSegments + queryOf in the handler). */
export interface ParsedRequest {
  segments: string[];
  path: string;
  query: URLSearchParams;
}

/** Parse a request's URL once; app.ts calls this and stashes the result. */
export function parseRequest(req: IncomingMessage): ParsedRequest {
  try {
    const url = new URL(req.url ?? '/', 'http://entry');
    return {
      segments: url.pathname.split('/').filter((part) => part !== '').map((part) => decodeURIComponent(part)),
      path: url.pathname,
      query: url.searchParams,
    };
  } catch {
    return { segments: [], path: '/', query: new URLSearchParams() };
  }
}

function parsed(req: IncomingMessage): ParsedRequest | undefined {
  return (req as IncomingMessage & { _parsed?: ParsedRequest })._parsed;
}

/** The request's path segments, decoded, without empty leading/trailing parts. */
export function pathSegments(req: IncomingMessage): string[] {
  const stash = parsed(req);
  if (stash !== undefined) return stash.segments;
  return parseRequest(req).segments;
}

/** The request's query parameters. */
export function queryOf(req: IncomingMessage): URLSearchParams {
  const stash = parsed(req);
  if (stash !== undefined) return stash.query;
  return parseRequest(req).query;
}

/** The request's path, or '/' when it does not parse (never a throw). */
export function pathOf(req: IncomingMessage): string {
  const stash = parsed(req);
  if (stash !== undefined) return stash.path;
  return parseRequest(req).path;
}

/** A positive integer query value, or `fallback` when absent or invalid. */
export function queryInt(query: URLSearchParams, name: string, fallback: number): number {
  const raw = query.get(name);
  if (raw === null) return fallback;
  const value = Number.parseInt(raw, 10);
  return Number.isFinite(value) && value > 0 ? value : fallback;
}
