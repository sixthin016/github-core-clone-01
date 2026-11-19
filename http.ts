// http.ts — small request/response helpers shared by every surface.
//
// Every JSON reply is `application/json` and carries a non-empty
// `X-GitHub-Request-Id` (a per-process counter, so the header is stable for a
// given request order). Error bodies are always JSON with a string `message`.

import type { RawRequest, RawResponse } from './rawhttp.ts';
import type { Store } from './store.ts';

export const JSON_TYPE = 'application/json; charset=utf-8';

/** Read a request body whole, as a Buffer. */
export function readBody(req: RawRequest): Promise<Buffer> {
  // The raw server parses the whole request before the handler runs, so the
  // body is already buffered; the stream branch only survives as a fallback.
  const buffered = (req as RawRequest & { _bodyBuffer?: Buffer })._bodyBuffer;
  if (buffered !== undefined) return Promise.resolve(buffered);
  const stream = req as unknown as { on: (ev: string, fn: (...a: never[]) => void) => void };
  return new Promise<Buffer>((resolve, reject) => {
    const chunks: Buffer[] = [];
    stream.on('data', (chunk: Buffer) => chunks.push(chunk));
    stream.on('end', () => resolve(Buffer.concat(chunks)));
    stream.on('error', reject);
  });
}

export function sendJson(
  store: Store,
  res: RawResponse,
  status: number,
  body: unknown,
  extra: Record<string, string> = {},
): void {
  sendJsonText(store, res, status, `${JSON.stringify(body)}\n`, extra);
}

/** Send an already-serialised JSON Buffer (hot path: a cached issue body). */
export function sendJsonBuffer(
  store: Store,
  res: RawResponse,
  status: number,
  buf: Buffer,
  extra: Record<string, string> = {},
): void {
  res.writeHead(status, {
    'content-type': JSON_TYPE,
    'content-length': buf.length,
    'x-github-request-id': store.nextRequestId(),
    ...extra,
  });
  res.end(buf);
}

/** Send an already-serialised JSON text (hot path: a cached issue body). */
export function sendJsonText(
  store: Store,
  res: RawResponse,
  status: number,
  text: string,
  extra: Record<string, string> = {},
): void {
  sendJsonBuffer(store, res, status, Buffer.from(text, 'utf8'), extra);
}

export function sendRaw(
  store: Store,
  res: RawResponse,
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

export function sendError(store: Store, res: RawResponse, status: number, message: string): void {
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
  /** Raw query string (no leading '?'); the URLSearchParams is built lazily by queryOf. */
  rawQuery: string;
  /** Lazily built query params; undefined until queryOf first builds it. */
  query?: URLSearchParams;
}

/** Parse a request's URL once; app.ts calls this and stashes the result.
 * Hand-rolled (no `new URL`): the WHATWG parser costs tens of microseconds
 * per request and this runs on every request of every benchmark workload.
 * The fast path covers plain absolute-path request targets (everything the
 * suite's clients send); anything unusual (absolute-form URL, dot segments,
 * backslashes, a fragment) falls back to `new URL` so behaviour stays
 * identical. */
export function parseRequest(req: RawRequest): ParsedRequest {
  const fallback = (): ParsedRequest => {
    try {
      const url = new URL(req.url ?? '/', 'http://entry');
      return {
        segments: url.pathname.split('/').filter((part) => part !== '').map((part) => decodeURIComponent(part)),
        path: url.pathname,
        rawQuery: url.search.slice(1),
        query: url.searchParams,
      };
    } catch {
      return { segments: [], path: '/', rawQuery: '' };
    }
  };
  try {
    const raw = req.url ?? '/';
    if (
      !raw.startsWith('/') ||
      raw.includes('\\') ||
      raw.includes('/./') ||
      raw.includes('/../') ||
      raw.endsWith('/.') ||
      raw.endsWith('/..')
    ) {
      return fallback();
    }
    const hash = raw.indexOf('#');
    const noFrag = hash === -1 ? raw : raw.slice(0, hash);
    const q = noFrag.indexOf('?');
    const path = q === -1 ? noFrag : noFrag.slice(0, q);
    const rawQuery = q === -1 ? '' : noFrag.slice(q + 1);
    const parts = path.split('/');
    const segments: string[] = [];
    for (const part of parts) {
      if (part === '') continue;
      segments.push(part.includes('%') ? decodeURIComponent(part) : part);
    }
    return { segments, path, rawQuery };
  } catch {
    return fallback();
  }
}

function parsed(req: RawRequest): ParsedRequest | undefined {
  return (req as RawRequest & { _parsed?: ParsedRequest })._parsed;
}

/** The request's path segments, decoded, without empty leading/trailing parts. */
export function pathSegments(req: RawRequest): string[] {
  const stash = parsed(req);
  if (stash !== undefined) return stash.segments;
  return parseRequest(req).segments;
}

/** The request's query parameters (built lazily from the stashed raw query). */
export function queryOf(req: RawRequest): URLSearchParams {
  const stash = parsed(req);
  if (stash !== undefined) {
    if (stash.query === undefined) stash.query = new URLSearchParams(stash.rawQuery);
    return stash.query;
  }
  const fresh = parseRequest(req);
  if (fresh.query === undefined) fresh.query = new URLSearchParams(fresh.rawQuery);
  return fresh.query;
}

/** The request's path, or '/' when it does not parse (never a throw). */
export function pathOf(req: RawRequest): string {
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
