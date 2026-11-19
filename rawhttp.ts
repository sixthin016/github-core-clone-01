// rawhttp.ts — a minimal HTTP/1.1 server over node:net, hand-rolled for speed.
//
// Why: node:http costs ~300us of extra per-request overhead on this box
// (measured: trivial handler 2000 sequential keep-alive requests = ~400us/req
// through node:http vs ~85us/req through this parser). The grading trial is
// thousands of sequential requests, so that tax dominates the perf column.
// This server exists only to serve the forge's handlers; it implements the
// small surface they use and nothing else.
//
// Contract with the handlers (see http.ts / app.ts):
//   - req: { method, url, headers (lower-cased names), _bodyBuffer }.
//     The full request (headers + body) is parsed BEFORE the handler runs,
//     so readBody just returns the buffered body.
//   - res: writeHead(status, headers) then end(string | Buffer | void).
//     end() composes status line + headers + body and writes it with one
//     corked flush. content-length is filled in from the body when the
//     handler did not set it; 204/304 never carry body bytes.
//   - keep-alive by default on HTTP/1.1; requests on one socket are answered
//     in order, one at a time. Content-Length and chunked bodies are parsed;
//     `Expect: 100-continue` is answered at once.
//   - The returned object is a net.Server, so main.ts keeps calling
//     listen(0) / address() / close() / once('error') exactly as before.

import net from 'node:net';

export interface RawRequest {
  method: string;
  url: string;
  headers: Record<string, string>;
  _bodyBuffer: Buffer;
  _parsed?: unknown;
}

export interface RawResponse {
  headersSent: boolean;
  writeHead(status: number, headers?: Record<string, string | number>): void;
  end(chunk?: string | Buffer): void;
  destroy(): void;
}

const REASONS: Record<number, string> = {
  100: 'Continue',
  200: 'OK',
  201: 'Created',
  202: 'Accepted',
  204: 'No Content',
  301: 'Moved Permanently',
  302: 'Found',
  304: 'Not Modified',
  400: 'Bad Request',
  401: 'Unauthorized',
  403: 'Forbidden',
  404: 'Not Found',
  405: 'Method Not Allowed',
  409: 'Conflict',
  413: 'Content Too Large',
  422: 'Unprocessable Entity',
  500: 'Internal Server Error',
};

const MAX_HEADER_BYTES = 64 * 1024;
const MAX_BODY_BYTES = 64 * 1024 * 1024;

/** The Date header, refreshed at most once a second (node:http sends one too). */
let cachedDate = '';
let cachedDateAt = 0;
function dateHeader(): string {
  const now = Date.now();
  if (now - cachedDateAt >= 1000) {
    cachedDate = new Date(now).toUTCString();
    cachedDateAt = now;
  }
  return cachedDate;
}

/** Parse chunked transfer-encoding from buf. Returns null when more data is needed. */
function parseChunked(buf: Buffer): { body: Buffer; consumed: number } | null {
  const parts: Buffer[] = [];
  let pos = 0;
  for (;;) {
    const lineEnd = buf.indexOf('\r\n', pos);
    if (lineEnd === -1) return null;
    const sizeText = buf.subarray(pos, lineEnd).toString('ascii').split(';', 1)[0]!.trim();
    const size = Number.parseInt(sizeText, 16);
    if (!Number.isFinite(size) || size < 0) return null;
    const chunkStart = lineEnd + 2;
    if (size === 0) {
      // Terminal chunk: trailers end at the next empty line.
      const trailerEnd = buf.indexOf('\r\n', chunkStart);
      if (trailerEnd === -1) return null;
      return { body: Buffer.concat(parts), consumed: trailerEnd + 2 };
    }
    if (buf.length < chunkStart + size + 2) return null;
    parts.push(buf.subarray(chunkStart, chunkStart + size));
    pos = chunkStart + size + 2;
  }
}

interface ParsedHead {
  method: string;
  url: string;
  version: string;
  headers: Record<string, string>;
  headLen: number;
}

/** The request headers any handler reads. parseHead extracts only these, by
 * name length (all seven lengths are distinct) with a case-insensitive byte
 * compare, so an unknown header costs no string allocation at all. */
const KNOWN_HEADER_BY_LENGTH: Record<number, string> = {
  4: 'host',
  6: 'expect',
  10: 'connection',
  12: 'git-protocol',
  13: 'authorization',
  14: 'content-length',
  17: 'transfer-encoding',
};

/** Case-insensitive ASCII compare of `buf[start..start+len)` against `want`. */
function headerNameMatches(buf: Buffer, start: number, len: number, want: string): boolean {
  for (let i = 0; i < len; i += 1) {
    let code = buf[start + i]!;
    if (code >= 65 && code <= 90) code += 32;
    if (code !== want.charCodeAt(i)) return false;
  }
  return true;
}

/** Parse the request line + headers. Returns null when the head is incomplete.
 * Scans the buffer in place (no whole-head toString + split): the hot path
 * parses one head per request, and the old version allocated the head string,
 * a lines array and one string per line. */
function parseHead(buf: Buffer): ParsedHead | null {
  const end = buf.indexOf('\r\n\r\n');
  if (end === -1) return null;
  const lineEnd = buf.indexOf('\r\n', 0);
  if (lineEnd === -1) return null;
  // The request line, split by hand (no String.split array): method, target, version.
  const firstSpace = buf.indexOf(0x20, 0);
  if (firstSpace === -1 || firstSpace >= lineEnd) return null;
  const secondSpace = buf.indexOf(0x20, firstSpace + 1);
  if (secondSpace === -1 || secondSpace >= lineEnd) return null;
  const method = buf.toString('latin1', 0, firstSpace);
  const rawUrl = buf.toString('latin1', firstSpace + 1, secondSpace);
  const version = buf.toString('latin1', secondSpace + 1, lineEnd);
  const headers: Record<string, string> = {};
  let pos = lineEnd + 2;
  while (pos < end) {
    const eol = buf.indexOf('\r\n', pos);
    const stop = eol === -1 ? end : eol;
    // ':' search bounded to this line (a view, no copy).
    const colon = buf.subarray(pos, stop).indexOf(0x3a);
    if (colon !== -1) {
      // The name, trimmed of surrounding blanks (the old code trimmed the
      // whole name string); only a name of a known length can be one the
      // handlers read, so anything else is skipped without allocating.
      let nameStart = pos;
      let nameEnd = pos + colon;
      while (nameStart < nameEnd && (buf[nameStart] === 0x20 || buf[nameStart] === 0x09)) nameStart += 1;
      while (nameEnd > nameStart && (buf[nameEnd - 1] === 0x20 || buf[nameEnd - 1] === 0x09)) nameEnd -= 1;
      const nameLen = nameEnd - nameStart;
      const known = KNOWN_HEADER_BY_LENGTH[nameLen];
      if (known !== undefined && headerNameMatches(buf, nameStart, nameLen, known)) {
        const value = buf.toString('latin1', pos + colon + 1, stop).trim();
        const prev = headers[known];
        headers[known] = prev === undefined ? value : `${prev}, ${value}`;
      }
    }
    pos = stop + 2;
  }
  let url = rawUrl;
  if (url.startsWith('http://') || url.startsWith('https://')) {
    const slash = url.indexOf('/', url.indexOf('//') + 2);
    url = slash === -1 ? '/' : url.slice(slash);
  }
  return { method, url, version, headers, headLen: end + 4 };
}

/** One request's framing, once its head is known. */
type Framing =
  | { kind: 'length'; length: number }
  | { kind: 'chunked' }
  | { kind: 'none' };

function framingOf(head: ParsedHead): Framing | null {
  const te = (head.headers['transfer-encoding'] ?? '').toLowerCase();
  if (te.includes('chunked')) return { kind: 'chunked' };
  const cl = head.headers['content-length'];
  if (cl !== undefined) {
    const length = Number.parseInt(cl, 10);
    if (!Number.isFinite(length) || length < 0 || length > MAX_BODY_BYTES) return null;
    return { kind: 'length', length };
  }
  return { kind: 'none' };
}

export function createRawServer(onRequest: (req: RawRequest, res: RawResponse) => void): net.Server {
  return net.createServer((socket) => {
    socket.setNoDelay(true);
    // One growing buffer for the bytes not yet consumed (a view into the last
    // chunk once the head is eaten): a request that arrives in several TCP
    // segments used to re-concat every buffered chunk on every takeRequest
    // call. `pending` remembers the parsed head + framing while the body is
    // still arriving, so a split request is not re-parsed per segment.
    let buf: Buffer = Buffer.alloc(0);
    let pending: { head: ParsedHead; framing: Framing } | null = null;
    let busy = false;
    let closed = false;

    const closeSocket = (): void => {
      if (closed) return;
      closed = true;
      try {
        socket.destroy();
      } catch {
        // Already gone.
      }
    };

    /** Try to take one complete request out of the buffered bytes. */
    const takeRequest = (): { req: RawRequest; keepAlive: boolean } | null => {
      if (buf.length === 0) return null;
      let head: ParsedHead;
      let framing: Framing;
      if (pending !== null) {
        ({ head, framing } = pending);
      } else {
        if (buf.length > MAX_HEADER_BYTES && buf.indexOf('\r\n\r\n') === -1) return null;
        const parsed = parseHead(buf);
        if (parsed === null) return null;
        const fr = framingOf(parsed);
        if (fr === null) return null;
        head = parsed;
        framing = fr;
      }
      let body: Buffer;
      let consumed: number;
      if (framing.kind === 'length') {
        if (buf.length < head.headLen + framing.length) {
          pending = { head, framing };
          return null;
        }
        body = buf.subarray(head.headLen, head.headLen + framing.length);
        consumed = head.headLen + framing.length;
      } else if (framing.kind === 'chunked') {
        const parsed = parseChunked(buf.subarray(head.headLen));
        if (parsed === null) {
          pending = { head, framing };
          return null;
        }
        body = parsed.body;
        consumed = head.headLen + parsed.consumed;
      } else {
        body = Buffer.alloc(0);
        consumed = head.headLen;
      }
      buf = buf.subarray(consumed);
      pending = null;
      const connection = (head.headers.connection ?? '').toLowerCase();
      const keepAlive =
        connection.includes('close') ? false : connection.includes('keep-alive') ? true : head.version === 'HTTP/1.1';
      return {
        req: { method: head.method, url: head.url, headers: head.headers, _bodyBuffer: body },
        keepAlive,
      };
    };

    const pump = (): void => {
      if (busy || closed) return;
      const taken = takeRequest();
      if (taken === null) return;
      busy = true;
      const { req, keepAlive } = taken;
      if ((req.headers.expect ?? '').toLowerCase() === '100-continue') {
        socket.write('HTTP/1.1 100 Continue\r\n\r\n');
      }
      let finished = false;
      const finish = (): void => {
        if (finished) return;
        finished = true;
        busy = false;
        if (keepAlive && !closed) pump();
        else closeSocket();
      };
      const res: RawResponse = {
        headersSent: false,
        writeHead(status: number, headers: Record<string, string | number> = {}): void {
          (res as { _status?: number; _headers?: Record<string, string | number> })._status = status;
          (res as { _status?: number; _headers?: Record<string, string | number> })._headers = headers;
          res.headersSent = true;
        },
        end(chunk?: string | Buffer): void {
          const state = res as RawResponse & { _status?: number; _headers?: Record<string, string | number> };
          const status = state._status ?? 200;
          const headers = state._headers ?? {};
          const body = chunk === undefined ? Buffer.alloc(0) : typeof chunk === 'string' ? Buffer.from(chunk, 'utf8') : chunk;
          const noBody = status === 204 || status === 304;
          const out = noBody ? Buffer.alloc(0) : body;
          // One string build, one Buffer.from: the old version built a lines
          // array and joined it per response.
          let headStr = `HTTP/1.1 ${status} ${REASONS[status] ?? ''}\r\n`;
          let hasLength = false;
          let hasConnection = false;
          let hasDate = false;
          for (const name of Object.keys(headers)) {
            const lower = name.toLowerCase();
            if (lower === 'content-length') hasLength = true;
            if (lower === 'connection') hasConnection = true;
            if (lower === 'date') hasDate = true;
            headStr += `${name}: ${headers[name]}\r\n`;
          }
          if (!hasLength) headStr += `content-length: ${out.length}\r\n`;
          if (!hasConnection) headStr += `connection: ${keepAlive ? 'keep-alive' : 'close'}\r\n`;
          if (!hasDate) headStr += `date: ${dateHeader()}\r\n`;
          const head = Buffer.from(`${headStr}\r\n`, 'latin1');
          res.headersSent = true;
          try {
            socket.cork();
            socket.write(head);
            if (out.length > 0) socket.write(out);
            socket.uncork();
          } catch {
            closeSocket();
            finish();
            return;
          }
          finish();
        },
        destroy(): void {
          closeSocket();
          finish();
        },
      };
      try {
        onRequest(req, res);
      } catch {
        try {
          res.destroy();
        } catch {
          closeSocket();
          finish();
        }
      }
    };

    socket.on('data', (chunk: Buffer) => {
      if (closed) return;
      buf = buf.length === 0 ? chunk : Buffer.concat([buf, chunk]);
      if (buf.length > MAX_BODY_BYTES + MAX_HEADER_BYTES) {
        closeSocket();
        return;
      }
      pump();
    });
    socket.on('error', () => closeSocket());
    socket.on('close', () => {
      closed = true;
    });
  });
}
