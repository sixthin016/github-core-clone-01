// app.ts — the forge's HTTP server: one store, four surfaces.
//
//   GET  /healthz            the env's boot probe: 200 {"ok":true}
//   /{owner}/{repo}.git/*    git smart HTTP (info/refs, upload-pack, receive-pack)
//   POST /api/graphql        GraphQL v4
//   POST /mcp                Model Context Protocol (Streamable HTTP)
//   /api/v3/*                GitHub REST API v3
//
// Anything else is a JSON 404. A handler that throws answers a JSON 500 and
// never takes the server down with it.

import { createServer, type IncomingMessage, type Server, type ServerResponse } from 'node:http';
import { handleGit } from './git-http.ts';
import { handleGraphQL } from './graphql.ts';
import { pathSegments, sendError, sendJson } from './http.ts';
import { handleMcp } from './mcp.ts';
import { handleRest } from './rest.ts';
import { Store } from './store.ts';

/** The path of a request URL, or '/' when it does not parse (never a throw). */
function pathOf(req: IncomingMessage): string {
  try {
    return new URL(req.url ?? '/', 'http://entry').pathname;
  } catch {
    return '/';
  }
}

/** Build the server. It binds nothing until the caller listens; the entry listens on an ephemeral port. */
export function createApp(): Server {
  const store = new Store();
  return createServer((req: IncomingMessage, res: ServerResponse) => {
    void (async () => {
      try {
        const segments = pathSegments(req);
        const method = req.method ?? 'GET';
        const path = pathOf(req);

        // The env's boot check calls this twice: 200 {"ok":true}.
        if (method === 'GET' && path === '/healthz') {
          sendJson(store, res, 200, { ok: true });
          return;
        }

        // Git smart HTTP: /{owner}/{repo}.git/info/refs, git-upload-pack, git-receive-pack.
        if (segments.length >= 2 && segments[1]!.endsWith('.git')) {
          if (await handleGit(store, req, res, segments)) return;
        }

        // Model Context Protocol.
        if (path === '/mcp') {
          if (await handleMcp(store, req, res)) return;
        }

        // GraphQL v4.
        if (path === '/api/graphql') {
          if (await handleGraphQL(store, req, res)) return;
        }

        // REST API v3.
        if (segments.length >= 2 && segments[0] === 'api' && segments[1] === 'v3') {
          if (await handleRest(store, req, res)) return;
        }

        // Everything else, every method: GitHub's 404 body.
        sendError(store, res, 404, 'Not Found');
      } catch (err) {
        if (!res.headersSent) {
          try {
            sendError(store, res, 500, err instanceof Error ? err.message : 'Internal Server Error');
          } catch {
            res.destroy();
          }
        } else {
          res.destroy();
        }
      }
    })();
  });
}
