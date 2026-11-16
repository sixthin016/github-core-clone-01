// git-http.ts — the git smart-HTTP surface: info/refs advertisements (protocol
// v0 and v2), upload-pack (ls-refs and fetch, v0 and v2) and receive-pack
// (push with report-status in sideband-64k).
//
// The wire shapes follow the suite's own client: a v2 advertisement opens with
// the service preamble and `version 2`; a v0 advertisement opens with the
// service preamble and lists refs with capabilities after a NUL on the first
// line; a v2 fetch answers its sections (acknowledgments, shallow-info,
// packfile) separated by flush packets, the pack carried in sideband band 1.

import type { IncomingMessage, ServerResponse } from 'node:http';
import {
  ZERO_OID,
  collectReachable,
  decodePktLines,
  encodePktLine,
  encodePktLines,
  isAncestor,
  isObjectId,
  parseCommit,
  parseTag,
  readPack,
  writePack,
  type GitObject,
  type ObjectLookup,
} from './git.ts';
import { readBody, sendError, sendJson, sendRaw } from './http.ts';
import { VIEWER, type Repo, type Store } from './store.ts';

const AGENT = 'makefun-git-forge/1.0';
const UPLOAD_CAPS_V0 =
  'symref=HEAD:refs/heads/main side-band-64k thin-pack ofs-delta multi_ack multi_ack_detailed shallow agent=makefun-git-forge/1.0';
const RECEIVE_CAPS_V0 =
  'symref=HEAD:refs/heads/main report-status side-band-64k delete-refs ofs-delta agent=makefun-git-forge/1.0';

/** The refs a repository advertises, in advertisement order. */
function advertisedRefs(repo: Repo): Array<{ oid: string; name: string; peeled?: string }> {
  const out: Array<{ oid: string; name: string; peeled?: string }> = [];
  const heads = [...repo.refs.keys()].filter((ref) => ref.startsWith('refs/heads/')).sort();
  const tags = [...repo.refs.keys()].filter((ref) => ref.startsWith('refs/tags/')).sort();
  for (const ref of heads) out.push({ oid: repo.refs.get(ref)!, name: ref });
  for (const ref of tags) {
    const oid = repo.refs.get(ref)!;
    const entry: { oid: string; name: string; peeled?: string } = { oid, name: ref };
    const object = repo.objects.get(oid);
    if (object?.type === 'tag') {
      try {
        entry.peeled = parseTag(object.data).object;
      } catch {
        // a malformed tag object advertises without a peel
      }
    }
    out.push(entry);
  }
  return out;
}

function v0Advertisement(service: 'git-upload-pack' | 'git-receive-pack', repo: Repo): Buffer {
  const caps = service === 'git-upload-pack' ? UPLOAD_CAPS_V0 : RECEIVE_CAPS_V0;
  const refs = advertisedRefs(repo);
  const lines: Array<string | 'flush'> = [`# service=${service}\n`, 'flush'];
  const defaultTip = repo.defaultTip();
  if (refs.length === 0) {
    lines.push(`${ZERO_OID} capabilities^{}\0${caps}\n`);
  } else {
    lines.push(`${defaultTip ?? ZERO_OID} HEAD\0${caps}\n`);
    for (const ref of refs) {
      lines.push(`${ref.oid} ${ref.name}\n`);
      if (ref.peeled !== undefined) lines.push(`${ref.peeled} ${ref.name}^{}\n`);
    }
  }
  lines.push('flush');
  return encodePktLines(lines);
}

function v2Advertisement(repo: Repo): Buffer {
  void repo;
  return encodePktLines([
    '# service=git-upload-pack\n',
    'flush',
    'version 2\n',
    `agent=${AGENT}\n`,
    'ls-refs=unborn\n',
    'fetch=shallow filter\n',
    'server-option\n',
    'object-format=sha1\n',
    'flush',
  ]);
}

/**
 * The objects the client already holds, from its `have` lines: the closure of
 * the haves, truncated at its shallow boundaries — a boundary commit's parents
 * are not held, so they must not be subtracted from what the fetch sends.
 */
function haveClosure(repo: Repo, haves: readonly string[], oldShallow: readonly string[]): Set<string> {
  const boundary = new Set(oldShallow);
  const held = new Set<string>();
  const queue: string[] = [];
  for (const oid of haves) {
    if (isObjectId(oid) && repo.objects.has(oid) && !held.has(oid)) {
      held.add(oid);
      queue.push(oid);
    }
  }
  while (queue.length > 0) {
    const oid = queue.shift()!;
    const object = repo.objects.get(oid);
    if (!object) continue;
    if (object.type === 'commit') {
      const commit = parseCommit(object.data);
      for (const child of collectReachable([commit.tree], repo.lookup)) held.add(child);
      if (boundary.has(oid)) continue; // shallow boundary: parents are not held
      for (const parent of commit.parents) {
        if (!held.has(parent)) {
          held.add(parent);
          queue.push(parent);
        }
      }
    } else if (object.type === 'tag') {
      try {
        const target = parseTag(object.data).object;
        if (!held.has(target)) {
          held.add(target);
          queue.push(target);
        }
      } catch {
        // ignore a malformed tag
      }
    }
  }
  return held;
}

interface FetchRequest {
  wants: string[];
  haves: string[];
  depth?: number;
  /** `deepen-relative`: the depth counts from the current shallow boundary. */
  deepenRelative: boolean;
  oldShallow: string[];
}

interface FetchPlan {
  objects: Map<string, GitObject>;
  shallow: string[];
  unshallow: string[];
  acks: string[];
}

/**
 * Work out what a fetch sends: the reachable closure of the wants, minus what
 * the client already holds, honouring a depth limit (shallow) with its
 * boundary list. `oldShallow` is the client's current shallow boundary.
 */
function planFetch(repo: Repo, request: FetchRequest): FetchPlan {
  const lookup: ObjectLookup = repo.lookup;
  const wants = request.wants.filter((oid) => isObjectId(oid) && repo.objects.has(oid));
  const haves = request.haves.filter((oid) => isObjectId(oid) && repo.objects.has(oid));
  const held = haveClosure(repo, haves, request.oldShallow);

  // The effective depth: an absolute `deepen N`, or, with `deepen-relative`,
  // N commits beyond the want's current shallow boundary.
  let depth = request.depth;
  if (depth !== undefined && request.deepenRelative) {
    const boundary = new Set(request.oldShallow);
    let best = 0;
    for (const want of wants) {
      // BFS distance (1-based) from the want to its nearest old boundary.
      const seen = new Set<string>([want]);
      const queue: Array<{ oid: string; dist: number }> = [{ oid: want, dist: 1 }];
      while (queue.length > 0) {
        const { oid, dist } = queue.shift()!;
        if (boundary.has(oid)) {
          best = Math.max(best, dist + depth);
          break;
        }
        const object = repo.objects.get(oid);
        if (object?.type !== 'commit') continue;
        for (const parent of parseCommit(object.data).parents) {
          if (!seen.has(parent)) {
            seen.add(parent);
            queue.push({ oid: parent, dist: dist + 1 });
          }
        }
      }
    }
    if (best > 0) depth = best;
  }

  const included = new Set<string>();
  const boundaries: string[] = [];
  if (depth === undefined) {
    for (const oid of collectReachable(wants, lookup)) included.add(oid);
  } else {
    // BFS over commit parents: distance 1 is the want itself. Only commits
    // within the depth and their trees are included; parents past a boundary
    // commit are never reached.
    const distance = new Map<string, number>();
    const queue: string[] = [];
    for (const want of wants) {
      if (!distance.has(want)) {
        distance.set(want, 1);
        queue.push(want);
      }
    }
    const commitSet = new Set<string>();
    while (queue.length > 0) {
      const oid = queue.shift()!;
      const dist = distance.get(oid)!;
      const object = repo.objects.get(oid);
      if (!object) continue;
      commitSet.add(oid);
      if (object.type === 'tag') {
        try {
          const target = parseTag(object.data).object;
          if (!distance.has(target)) {
            distance.set(target, dist);
            queue.push(target);
          }
        } catch {
          // ignore a malformed tag
        }
        continue;
      }
      if (object.type !== 'commit') continue;
      if (dist === depth) {
        boundaries.push(oid);
        continue;
      }
      for (const parent of parseCommit(object.data).parents) {
        if (!distance.has(parent)) {
          distance.set(parent, dist + 1);
          queue.push(parent);
        }
      }
    }
    // Each included commit contributes itself and its tree closure only.
    for (const oid of commitSet) {
      included.add(oid);
      const object = repo.objects.get(oid);
      if (object?.type === 'commit') {
        for (const child of collectReachable([parseCommit(object.data).tree], lookup)) included.add(child);
      }
    }
    // Tags elsewhere in the repo that point at an included commit.
    for (const [oid, object] of repo.objects) {
      if (object.type !== 'tag') continue;
      try {
        if (commitSet.has(parseTag(object.data).object)) included.add(oid);
      } catch {
        // ignore
      }
    }
  }

  const objects = new Map<string, GitObject>();
  for (const oid of included) {
    if (held.has(oid)) continue;
    const object = repo.objects.get(oid);
    if (object) objects.set(oid, object);
  }

  // Shallow-info: new boundaries, surviving old boundaries, deepened old ones.
  const shallow: string[] = [];
  const unshallow: string[] = [];
  if (request.depth !== undefined) {
    const boundarySet = new Set(boundaries);
    for (const oid of boundaries) shallow.push(oid);
    for (const old of request.oldShallow) {
      if (!isObjectId(old)) continue;
      if (boundarySet.has(old)) continue;
      // Deepened: the old boundary now lies strictly inside the new depth,
      // so its parents are delivered and it is no longer a boundary. (The
      // boundary commit itself is always held by the client; that is what
      // makes it a boundary, so holding it does not block unshallowing.)
      const deepened = included.has(old) && !boundarySet.has(old);
      if (deepened) {
        // Its parents are now delivered: it is no longer a boundary.
        if (!unshallow.includes(old)) unshallow.push(old);
        continue;
      }
      // Still a boundary when it is an ancestor of a want and its parents
      // were not delivered.
      if (wants.some((want) => isAncestor(old, want, lookup))) {
        if (!shallow.includes(old)) shallow.push(old);
      }
    }
  }

  const acks: string[] = [];
  for (const have of haves) {
    if (wants.some((want) => isAncestor(have, want, lookup))) acks.push(have);
  }
  return { objects, shallow, unshallow, acks };
}

/** Split pack bytes into sideband band-1 pkt-lines (65515 data bytes each). */
function sidebandPack(pack: Buffer): Buffer[] {
  const parts: Buffer[] = [];
  const CHUNK = 65515;
  for (let offset = 0; offset < pack.length; offset += CHUNK) {
    const slice = pack.subarray(offset, Math.min(pack.length, offset + CHUNK));
    parts.push(encodePktLine(Buffer.concat([Buffer.from([1]), slice])));
  }
  if (parts.length === 0) parts.push(encodePktLine(Buffer.from([1])));
  return parts;
}

function v2FetchResponse(repo: Repo, request: FetchRequest): Buffer {
  const plan = planFetch(repo, request);
  // Deterministic pack order: sort the objects by id.
  const sorted = [...plan.objects.entries()].sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0));
  const orderedPack = writePack(sorted.map(([, object]) => object));
  const parts: Buffer[] = [];
  // Real git reads the acknowledgments section only when the request carried
  // haves to negotiate: with no haves it reads the packfile section header
  // first, so an acknowledgments section here would be read as the section
  // header and die with "expected 'packfile'". The kit client accepts both
  // shapes (it treats a missing acknowledgments section as NAK).
  if (request.haves.length > 0) {
    const ackLines: string[] = ['acknowledgments\n'];
    if (plan.acks.length > 0) {
      for (const ack of plan.acks) ackLines.push(`ACK ${ack}\n`);
      ackLines.push('ready\n');
    } else {
      ackLines.push('NAK\n');
    }
    parts.push(encodePktLines([...ackLines, 'delim']));
  }
  if (request.depth !== undefined) {
    const shallowLines: string[] = ['shallow-info\n'];
    for (const oid of plan.shallow) shallowLines.push(`shallow ${oid}\n`);
    for (const oid of plan.unshallow) shallowLines.push(`unshallow ${oid}\n`);
    parts.push(encodePktLines([...shallowLines, 'delim']));
  }
  parts.push(encodePktLine('packfile\n'));
  for (const chunk of sidebandPack(orderedPack)) parts.push(chunk);
  parts.push(encodePktLines(['flush']));
  return Buffer.concat(parts);
}

function v0FetchResponse(repo: Repo, request: FetchRequest): Buffer {
  const plan = planFetch(repo, request);
  const sorted = [...plan.objects.entries()].sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0));
  const pack = writePack(sorted.map(([, object]) => object));
  const lines: Array<string | 'flush'> = [];
  // v0 knows no `ready` line: only ACK lines or NAK.
  if (plan.acks.length > 0) {
    for (const ack of plan.acks) lines.push(`ACK ${ack}\n`);
  } else {
    lines.push('NAK\n');
  }
  if (request.depth !== undefined) {
    for (const oid of plan.shallow) lines.push(`shallow ${oid}\n`);
    for (const oid of plan.unshallow) lines.push(`unshallow ${oid}\n`);
  }
  const parts: Buffer[] = [encodePktLines(lines)];
  for (const chunk of sidebandPack(pack)) parts.push(chunk);
  parts.push(encodePktLines(['flush']));
  return Buffer.concat(parts);
}

function parseFetchArgs(lines: string[]): FetchRequest {
  const request: FetchRequest = { wants: [], haves: [], deepenRelative: false, oldShallow: [] };
  for (const line of lines) {
    const text = line.replace(/\n$/, '');
    if (text.startsWith('want ')) request.wants.push(text.slice(5).split(' ')[0]!);
    else if (text.startsWith('have ')) request.haves.push(text.slice(5).trim());
    else if (text.startsWith('shallow ')) request.oldShallow.push(text.slice(8).trim());
    else if (text === 'deepen-relative') request.deepenRelative = true;
    else if (text.startsWith('deepen ')) {
      const depth = Number.parseInt(text.slice(7).trim(), 10);
      if (Number.isFinite(depth) && depth > 0) request.depth = depth;
    }
  }
  return request;
}

function handleLsRefs(repo: Repo, lines: string[]): Buffer {
  const args = lines.map((line) => line.replace(/\n$/, ''));
  const peel = args.includes('peel');
  const symrefs = args.includes('symrefs');
  const prefixes: string[] = [];
  for (const arg of args) {
    if (arg.startsWith('ref-prefix ')) prefixes.push(arg.slice('ref-prefix '.length));
  }
  const keep = (name: string): boolean => prefixes.length === 0 || prefixes.some((prefix) => name.startsWith(prefix));
  const out: string[] = [];
  const defaultTip = repo.defaultTip();
  if (defaultTip !== undefined && keep('HEAD')) {
    const symref = symrefs ? ' symref-target:refs/heads/main' : '';
    out.push(`${defaultTip} HEAD${symref}\n`);
  }
  for (const ref of advertisedRefs(repo)) {
    if (!keep(ref.name)) continue;
    const peeled = peel && ref.peeled !== undefined ? ` peeled:${ref.peeled}` : '';
    out.push(`${ref.oid} ${ref.name}${peeled}\n`);
  }
  return encodePktLines([...out, 'flush']);
}

interface PushCommand {
  oldOid: string;
  newOid: string;
  ref: string;
}

/**
 * Parse the receive-pack command list: pkt-lines up to the first flush packet.
 * The pack follows the flush as raw binary, so the body must not be decoded
 * as pkt-lines past it.
 */
function parseReceivePackCommands(body: Buffer): { commands: PushCommand[]; packStart: number } {
  const commands: PushCommand[] = [];
  let offset = 0;
  for (;;) {
    if (offset + 4 > body.length) throw new Error('truncated pkt-line stream');
    const header = body.subarray(offset, offset + 4).toString('ascii');
    if (header === '0000') {
      offset += 4;
      break;
    }
    const length = Number.parseInt(header, 16);
    if (!Number.isFinite(length) || length < 4 || offset + length > body.length) {
      throw new Error(`bad pkt-line length ${JSON.stringify(header)}`);
    }
    const payload = body.subarray(offset + 4, offset + length);
    const text = payload.toString('utf8');
    const nul = text.indexOf('\0');
    const head = nul >= 0 ? text.slice(0, nul) : text;
    const match = /^([0-9a-f]{40}) ([0-9a-f]{40}) (\S+)\n?$/.exec(head);
    if (!match) throw new Error(`bad receive-pack command line ${JSON.stringify(head.slice(0, 60))}`);
    commands.push({ oldOid: match[1]!, newOid: match[2]!, ref: match[3]! });
    offset += length;
  }
  return { commands, packStart: offset };
}

function handleReceivePack(repo: Repo, body: Buffer): Buffer {
  const { commands, packStart } = parseReceivePackCommands(body);
  if (commands.length === 0) throw new Error('receive-pack sent no commands');

  const packBytes = body.subarray(packStart);
  let unpackError: string | null = null;
  if (packBytes.length > 0) {
    try {
      const read = readPack(packBytes, repo.lookup);
      for (const [oid, object] of read.objects) {
        if (!repo.objects.has(oid)) repo.objects.set(oid, object);
      }
    } catch (err) {
      unpackError = err instanceof Error ? err.message : String(err);
    }
  }

  const results: Array<{ ref: string; ok: boolean; reason: string }> = [];
  if (unpackError !== null) {
    for (const command of commands) results.push({ ref: command.ref, ok: false, reason: 'unpack failed' });
  } else {
    for (const command of commands) {
      const current = repo.refs.get(command.ref);
      if (command.newOid === ZERO_OID) {
        // Delete.
        if (current === undefined) results.push({ ref: command.ref, ok: false, reason: 'ref does not exist' });
        else if (current !== command.oldOid) results.push({ ref: command.ref, ok: false, reason: 'old oid mismatch' });
        else {
          repo.refs.delete(command.ref);
          results.push({ ref: command.ref, ok: true, reason: '' });
        }
        continue;
      }
      if (command.oldOid === ZERO_OID) {
        // Create.
        if (current !== undefined) {
          results.push({ ref: command.ref, ok: false, reason: 'ref already exists' });
          continue;
        }
      } else {
        if (current === undefined) {
          results.push({ ref: command.ref, ok: false, reason: 'ref does not exist' });
          continue;
        }
        if (current !== command.oldOid) {
          results.push({ ref: command.ref, ok: false, reason: 'old oid mismatch' });
          continue;
        }
      }
      const target = repo.objects.get(command.newOid);
      if (target === undefined) {
        results.push({ ref: command.ref, ok: false, reason: 'object not found' });
        continue;
      }
      if (command.oldOid !== ZERO_OID && command.ref.startsWith('refs/heads/')) {
        if (!isAncestor(current!, command.newOid, repo.lookup)) {
          results.push({ ref: command.ref, ok: false, reason: 'non-fast-forward' });
          continue;
        }
      }
      repo.refs.set(command.ref, command.newOid);
      results.push({ ref: command.ref, ok: true, reason: '' });
    }
  }

  const reportLines: Array<string | 'flush'> = [
    unpackError !== null ? `unpack ${unpackError}\n` : 'unpack ok\n',
  ];
  for (const result of results) {
    reportLines.push(result.ok ? `ok ${result.ref}\n` : `ng ${result.ref} ${result.reason}\n`);
  }
  reportLines.push('flush');
  const report = encodePktLines(reportLines);
  const chunks: Buffer[] = [];
  const CHUNK = 65515;
  for (let offset = 0; offset < report.length; offset += CHUNK) {
    chunks.push(encodePktLine(Buffer.concat([Buffer.from([1]), report.subarray(offset, Math.min(report.length, offset + CHUNK))])));
  }
  // The sideband stream ends with a flush packet of its own: without it a
  // real git client reports a sideband demultiplexer error.
  chunks.push(encodePktLines(['flush']));
  return Buffer.concat(chunks);
}

/** Handle one git smart-HTTP request. Returns true when it answered. */
export async function handleGit(
  store: Store,
  req: IncomingMessage,
  res: ServerResponse,
  segments: string[],
): Promise<boolean> {
  if (segments.length < 2) return false;
  const owner = segments[0]!;
  const repoSegment = segments[1]!;
  if (!repoSegment.endsWith('.git')) return false;
  const repoName = repoSegment.slice(0, -4);
  const method = req.method ?? 'GET';
  const rest = segments.slice(2);

  // info/refs
  if (method === 'GET' && rest.length === 2 && rest[0] === 'info' && rest[1] === 'refs') {
    const service = new URL(req.url ?? '/', 'http://entry').searchParams.get('service');
    if (service !== 'git-upload-pack' && service !== 'git-receive-pack') {
      sendError(store, res, 400, 'unknown service');
      return true;
    }
    const repo = store.getRepo(owner, repoName);
    if (repo === undefined || owner !== VIEWER.login) {
      sendError(store, res, 404, 'Not Found');
      return true;
    }
    const wantV2 = service === 'git-upload-pack' && (req.headers['git-protocol'] ?? '') === 'version=2';
    const body = wantV2 ? v2Advertisement(repo) : v0Advertisement(service, repo);
    sendRaw(store, res, 200, body, `application/x-${service}-advertisement`);
    return true;
  }

  // upload-pack / receive-pack POST
  if (method === 'POST' && rest.length === 1 && (rest[0] === 'git-upload-pack' || rest[0] === 'git-receive-pack')) {
    const service = rest[0] as 'git-upload-pack' | 'git-receive-pack';
    const repo = store.getRepo(owner, repoName);
    if (repo === undefined || owner !== VIEWER.login) {
      sendError(store, res, 404, 'Not Found');
      return true;
    }
    const body = await readBody(req);
    try {
      if (service === 'git-receive-pack') {
        const report = handleReceivePack(repo, body);
        sendRaw(store, res, 200, report, 'application/x-git-receive-pack-result');
        return true;
      }
      // upload-pack: v2 when the client asked for it, else v0.
      const decoded = decodePktLines(body);
      if (decoded === null) {
        res.writeHead(400, { 'content-type': 'text/plain; charset=utf-8' });
        res.end('malformed pkt-line stream');
        return true;
      }
      const texts = decoded.packets
        .filter((packet) => packet.kind === 'data')
        .map((packet) => (packet as { kind: 'data'; payload: Buffer }).payload.toString('utf8'));
      const isV2 = (req.headers['git-protocol'] ?? '') === 'version=2';
      if (isV2) {
        const command = texts[0]?.replace(/\n$/, '') ?? '';
        if (command === 'command=ls-refs') {
          sendRaw(store, res, 200, handleLsRefs(repo, texts.slice(1)), 'application/x-git-upload-pack-result');
          return true;
        }
        if (command === 'command=fetch') {
          const request = parseFetchArgs(texts.slice(1));
          sendRaw(store, res, 200, v2FetchResponse(repo, request), 'application/x-git-upload-pack-result');
          return true;
        }
        res.writeHead(400, { 'content-type': 'text/plain; charset=utf-8' });
        res.end(`unknown v2 command ${JSON.stringify(command)}`);
        return true;
      }
      const request = parseFetchArgs(texts);
      sendRaw(store, res, 200, v0FetchResponse(repo, request), 'application/x-git-upload-pack-result');
      return true;
    } catch (err) {
      res.writeHead(400, { 'content-type': 'text/plain; charset=utf-8' });
      res.end(err instanceof Error ? err.message : String(err));
      return true;
    }
  }

  return false;
}
