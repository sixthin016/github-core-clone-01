// mcp.ts — the Model Context Protocol surface at /mcp (Streamable HTTP,
// JSON-RPC 2.0, modern 2026-07-28 era via server/discover, with the legacy
// initialize handshake answered too).
//
// Six tools, named as github-mcp-server documents them, all sharing the one
// store with REST and GraphQL: create_repo, get_repo, create_issue,
// list_issues, create_pull_request, merge_pull_request. Every tool result is
// a text block carrying the object as JSON. An unknown tool is JSON-RPC
// -32602; a missing required argument is a tool result with isError:true; a
// body that is not JSON is HTTP 400 with JSON-RPC -32700; no Authorization
// header is HTTP 401.

import type { IncomingMessage, ServerResponse } from 'node:http';
import { readBody, sendJson } from './http.ts';
import { issueJson, pullJson, repoJson } from './rest.ts';
import {
  VIEWER,
  squashMerge,
  type Issue,
  type PullRequest,
  type Repo,
  type Store,
} from './store.ts';

const SERVER_NAME = 'makefun-git-forge';
const SERVER_VERSION = '1.0.0';
const PROTOCOL_VERSION = '2026-07-28';

interface ToolDef {
  name: string;
  description: string;
  inputSchema: {
    type: 'object';
    properties: Record<string, Record<string, unknown>>;
    required: string[];
  };
}

/** The tool list, frozen: two tools/list calls answer byte-identical JSON. */
const TOOLS: readonly ToolDef[] = Object.freeze([
  Object.freeze({
    name: 'create_repo',
    description: 'Create a repository owned by the authenticated user.',
    inputSchema: Object.freeze({
      type: 'object',
      properties: Object.freeze({
        name: Object.freeze({ type: 'string', description: 'The repository name.' }),
        description: Object.freeze({ type: 'string', description: 'A short description of the repository.' }),
        private: Object.freeze({ type: 'boolean', description: 'Whether the repository is private.' }),
      }),
      required: Object.freeze(['name']),
    }),
  }),
  Object.freeze({
    name: 'get_repo',
    description: 'Get a repository by owner and name.',
    inputSchema: Object.freeze({
      type: 'object',
      properties: Object.freeze({
        owner: Object.freeze({ type: 'string', description: 'The repository owner.' }),
        repo: Object.freeze({ type: 'string', description: 'The repository name.' }),
      }),
      required: Object.freeze(['owner', 'repo']),
    }),
  }),
  Object.freeze({
    name: 'create_issue',
    description: 'Create an issue in a repository.',
    inputSchema: Object.freeze({
      type: 'object',
      properties: Object.freeze({
        owner: Object.freeze({ type: 'string', description: 'The repository owner.' }),
        repo: Object.freeze({ type: 'string', description: 'The repository name.' }),
        title: Object.freeze({ type: 'string', description: 'The issue title.' }),
        body: Object.freeze({ type: 'string', description: 'The issue body.' }),
      }),
      required: Object.freeze(['owner', 'repo', 'title']),
    }),
  }),
  Object.freeze({
    name: 'list_issues',
    description: 'List the issues of a repository.',
    inputSchema: Object.freeze({
      type: 'object',
      properties: Object.freeze({
        owner: Object.freeze({ type: 'string', description: 'The repository owner.' }),
        repo: Object.freeze({ type: 'string', description: 'The repository name.' }),
        state: Object.freeze({ type: 'string', description: 'Filter by state: open, closed or all.' }),
      }),
      required: Object.freeze(['owner', 'repo']),
    }),
  }),
  Object.freeze({
    name: 'create_pull_request',
    description: 'Create a pull request from a head branch into a base branch.',
    inputSchema: Object.freeze({
      type: 'object',
      properties: Object.freeze({
        owner: Object.freeze({ type: 'string', description: 'The repository owner.' }),
        repo: Object.freeze({ type: 'string', description: 'The repository name.' }),
        title: Object.freeze({ type: 'string', description: 'The pull request title.' }),
        body: Object.freeze({ type: 'string', description: 'The pull request body.' }),
        head: Object.freeze({ type: 'string', description: 'The head branch name.' }),
        base: Object.freeze({ type: 'string', description: 'The base branch name.' }),
      }),
      required: Object.freeze(['owner', 'repo', 'title', 'head', 'base']),
    }),
  }),
  Object.freeze({
    name: 'merge_pull_request',
    description: 'Squash-merge a pull request into its base branch.',
    inputSchema: Object.freeze({
      type: 'object',
      properties: Object.freeze({
        owner: Object.freeze({ type: 'string', description: 'The repository owner.' }),
        repo: Object.freeze({ type: 'string', description: 'The repository name.' }),
        pull_number: Object.freeze({ type: 'number', description: 'The pull request number.' }),
      }),
      required: Object.freeze(['owner', 'repo', 'pull_number']),
    }),
  }),
]);

type ToolResult = { content: Array<{ type: 'text'; text: string }>; isError?: boolean };

function okResult(value: unknown): ToolResult {
  return { content: [{ type: 'text', text: `${JSON.stringify(value)}\n` }] };
}

function errorResult(message: string): ToolResult {
  return { content: [{ type: 'text', text: `${message}\n` }], isError: true };
}

function stringArg(args: Record<string, unknown>, name: string): string | undefined {
  const value = args[name];
  return typeof value === 'string' ? value : undefined;
}

function numberArg(args: Record<string, unknown>, name: string): number | undefined {
  const value = args[name];
  return typeof value === 'number' && Number.isFinite(value) ? value : undefined;
}

function callTool(store: Store, name: string, args: Record<string, unknown>): ToolResult {
  const tool = TOOLS.find((entry) => entry.name === name);
  if (tool === undefined) {
    throw { code: -32602, message: `Unknown tool: ${name}` };
  }
  for (const required of tool.inputSchema.required) {
    if (args[required] === undefined || args[required] === null || args[required] === '') {
      return errorResult(`Missing required argument: ${required}`);
    }
  }
  switch (name) {
    case 'create_repo': {
      const repoName = stringArg(args, 'name')!;
      if (store.getRepo(VIEWER.login, repoName) !== undefined) {
        return errorResult(`Repository ${VIEWER.login}/${repoName} already exists`);
      }
      const repo = store.createRepo(
        VIEWER.login,
        repoName,
        stringArg(args, 'description') ?? '',
        args.private === true,
      );
      return okResult(repoJson(repo));
    }
    case 'get_repo': {
      const repo = store.getRepo(stringArg(args, 'owner')!, stringArg(args, 'repo')!);
      if (repo === undefined) return errorResult('Repository not found');
      return okResult(repoJson(repo));
    }
    case 'create_issue': {
      const repo = store.getRepo(stringArg(args, 'owner')!, stringArg(args, 'repo')!);
      if (repo === undefined) return errorResult('Repository not found');
      const number = repo.nextIssueNumber;
      repo.nextIssueNumber += 1;
      const issue: Issue = {
        id: number,
        number,
        title: stringArg(args, 'title')!,
        body: stringArg(args, 'body') ?? '',
        state: 'open',
        labels: [],
        createdSeq: repo.nextSeq(),
        comments: [],
        rev: 0,
        jsonCache: null,
      };
      repo.issues.set(number, issue);
      return okResult(issueJson(repo, issue));
    }
    case 'list_issues': {
      const repo = store.getRepo(stringArg(args, 'owner')!, stringArg(args, 'repo')!);
      if (repo === undefined) return errorResult('Repository not found');
      const state = stringArg(args, 'state') ?? 'open';
      let issues = [...repo.issues.values()];
      if (state === 'open' || state === 'closed') issues = issues.filter((issue) => issue.state === state);
      issues.sort((a, b) => b.createdSeq - a.createdSeq);
      return okResult(issues.map((issue) => issueJson(repo, issue)));
    }
    case 'create_pull_request': {
      const repo = store.getRepo(stringArg(args, 'owner')!, stringArg(args, 'repo')!);
      if (repo === undefined) return errorResult('Repository not found');
      const headRef = stringArg(args, 'head')!;
      const baseRef = stringArg(args, 'base')!;
      const headSha = repo.branchTip(headRef);
      const baseSha = repo.branchTip(baseRef);
      if (headSha === undefined || baseSha === undefined) {
        return errorResult('Head or base branch not found');
      }
      const number = repo.nextPullNumber;
      repo.nextPullNumber += 1;
      const pr: PullRequest = {
        id: number,
        number,
        title: stringArg(args, 'title')!,
        body: stringArg(args, 'body') ?? '',
        state: 'open',
        headRef,
        baseRef,
        headSha,
        baseSha,
        merged: false,
        mergedAt: null,
        mergeCommitSha: null,
        createdSeq: repo.nextSeq(),
      };
      repo.pulls.set(number, pr);
      return okResult(pullJson(repo, pr));
    }
    case 'merge_pull_request': {
      const repo = store.getRepo(stringArg(args, 'owner')!, stringArg(args, 'repo')!);
      const number = numberArg(args, 'pull_number');
      const pr = repo?.pulls.get(number ?? -1);
      if (repo === undefined || pr === undefined) return errorResult('Pull request not found');
      if (pr.state !== 'open') return errorResult('Pull request is not open');
      const result = squashMerge(repo, pr);
      if (!result.ok) return errorResult('Merge conflict: the pull request cannot be merged cleanly');
      return okResult({ merged: true, sha: result.commitSha, number: pr.number });
    }
    default:
      return errorResult(`Unknown tool: ${name}`);
  }
}

function rpcResult(store: Store, res: ServerResponse, id: unknown, result: unknown): void {
  sendJson(store, res, 200, { jsonrpc: '2.0', id: id ?? null, result });
}

function rpcError(store: Store, res: ServerResponse, id: unknown, code: number, message: string): void {
  sendJson(store, res, 200, { jsonrpc: '2.0', id: id ?? null, error: { code, message } });
}

/** Handle one MCP request. Returns true when it answered. */
export async function handleMcp(store: Store, req: IncomingMessage, res: ServerResponse): Promise<boolean> {
  if (req.method !== 'POST') return false;
  const authorization = req.headers.authorization ?? '';
  if (!authorization.startsWith('Bearer ') || authorization.slice('Bearer '.length).trim() === '') {
    res.writeHead(401, { 'content-type': 'application/json; charset=utf-8', 'x-github-request-id': store.nextRequestId() });
    res.end(JSON.stringify({ message: 'Requires authentication' }));
    return true;
  }
  const bodyBuffer = await readBody(req);
  let body: unknown;
  try {
    body = JSON.parse(bodyBuffer.toString('utf8')) as unknown;
  } catch {
    sendJson(store, res, 400, { jsonrpc: '2.0', id: null, error: { code: -32700, message: 'Parse error' } });
    return true;
  }
  if (body === null || typeof body !== 'object' || Array.isArray(body)) {
    sendJson(store, res, 400, { jsonrpc: '2.0', id: null, error: { code: -32600, message: 'Invalid Request' } });
    return true;
  }
  const request = body as Record<string, unknown>;
  const id = request.id ?? null;
  const method = typeof request.method === 'string' ? request.method : '';
  const params = request.params !== null && typeof request.params === 'object' && !Array.isArray(request.params)
    ? (request.params as Record<string, unknown>)
    : {};

  // A notification (no id) gets no response body.
  if (request.id === undefined) {
    res.writeHead(202, { 'x-github-request-id': store.nextRequestId() });
    res.end();
    return true;
  }

  if (method === 'server/discover') {
    rpcResult(store, res, id, {
      protocolVersion: PROTOCOL_VERSION,
      serverInfo: { name: SERVER_NAME, version: SERVER_VERSION },
      capabilities: { tools: {} },
      supportedVersions: [PROTOCOL_VERSION],
    });
    return true;
  }
  if (method === 'initialize') {
    const offered = typeof params.protocolVersion === 'string' ? params.protocolVersion : PROTOCOL_VERSION;
    const negotiated = /^\d{4}-\d{2}-\d{2}$/.test(offered) ? offered : PROTOCOL_VERSION;
    rpcResult(store, res, id, {
      protocolVersion: negotiated,
      serverInfo: { name: SERVER_NAME, version: SERVER_VERSION },
      capabilities: { tools: {} },
    });
    return true;
  }
  if (method === 'tools/list') {
    rpcResult(store, res, id, { tools: TOOLS });
    return true;
  }
  if (method === 'tools/call') {
    const toolName = typeof params.name === 'string' ? params.name : '';
    const args = params.arguments !== null && typeof params.arguments === 'object' && !Array.isArray(params.arguments)
      ? (params.arguments as Record<string, unknown>)
      : {};
    try {
      rpcResult(store, res, id, callTool(store, toolName, args));
    } catch (err) {
      if (err !== null && typeof err === 'object' && 'code' in err && 'message' in err) {
        rpcError(store, res, id, (err as { code: number }).code, String((err as { message: unknown }).message));
      } else {
        rpcError(store, res, id, -32603, err instanceof Error ? err.message : String(err));
      }
    }
    return true;
  }
  rpcError(store, res, id, -32601, `Method not found: ${method}`);
  return true;
}
