// rest.ts — the GitHub REST API v3 surface under /api/v3.
//
// Every reply is application/json with a non-empty X-GitHub-Request-Id. A
// missing or empty Bearer token is 401; a repository name under any owner
// other than the token's viewer is 404 (never 403); a malformed JSON body is
// 400; an unknown route is a JSON 404. A duplicate repository name is 422.

import type { RawRequest, RawResponse } from './rawhttp.ts';
import {
  flattenTree,
  lineDiff,
  parseCommit,
  parseTree,
} from './git.ts';
import {
  FIXED_ISO,
  VIEWER,
  globalId,
  mergeWouldConflict,
  squashMerge,
  type Issue,
  type Label,
  type PullRequest,
  type Repo,
  type Store,
} from './store.ts';
import { parseJsonBody, pathSegments, queryInt, queryOf, readBody, sendError, sendJson, sendJsonBuffer, sendJsonText } from './http.ts';

/** Count the lines of a text blob (a trailing newline ends the last line). */
function lineCount(text: string): number {
  if (text === '') return 0;
  const parts = text.split('\n');
  if (parts[parts.length - 1] === '') parts.pop();
  return parts.length;
}

export function repoJson(repo: Repo): Record<string, unknown> {
  return {
    id: repo.databaseId,
    node_id: repo.globalId,
    name: repo.name,
    full_name: repo.fullName,
    private: repo.isPrivate,
    owner: { login: repo.owner, id: VIEWER.id, type: 'User' },
    description: repo.description,
    default_branch: repo.defaultBranch,
    created_at: FIXED_ISO,
    updated_at: FIXED_ISO,
    pushed_at: FIXED_ISO,
    html_url: `https://example.invalid/${repo.fullName}`,
    url: `/api/v3/repos/${repo.fullName}`,
    clone_url: `https://example.invalid/${repo.fullName}.git`,
    git_url: `git://example.invalid/${repo.fullName}.git`,
    ssh_url: `git@example.invalid:${repo.fullName}.git`,
    fork: false,
    archived: false,
    disabled: false,
    visibility: repo.isPrivate ? 'private' : 'public',
    has_issues: true,
    has_projects: true,
    has_wiki: true,
    has_pages: false,
    has_downloads: true,
    license: null,
    topics: [],
    open_issues_count: [...repo.issues.values()].filter((issue) => issue.state === 'open').length,
  };
}

export function issueJson(repo: Repo, issue: Issue): Record<string, unknown> {
  return {
    id: issue.id,
    node_id: globalId('issue', `${repo.fullName}#${issue.number}`),
    number: issue.number,
    title: issue.title,
    body: issue.body,
    state: issue.state,
    labels: issue.labels.map((label) => ({ name: label.name, color: label.color })),
    user: { login: VIEWER.login, id: VIEWER.id, type: 'User' },
    created_at: FIXED_ISO,
    updated_at: FIXED_ISO,
    closed_at: issue.state === 'closed' ? FIXED_ISO : null,
    comments: issue.comments.length,
    html_url: `https://example.invalid/${repo.fullName}/issues/${issue.number}`,
    url: `/api/v3/repos/${repo.fullName}/issues/${issue.number}`,
    repository_url: `/api/v3/repos/${repo.fullName}`,
  };
}

/**
 * The serialised GET-issue body, cached on the issue and invalidated by
 * every mutation (PATCH, comment create, GraphQL close/comment): the read
 * workload GETs one unchanged issue 2,000 times in a row.
 */
export function issueJsonText(repo: Repo, issue: Issue): Buffer {
  if (issue.jsonCache !== null) return issue.jsonCache;
  const buf = Buffer.from(`${JSON.stringify(issueJson(repo, issue))}\n`, 'utf8');
  issue.jsonCache = buf;
  return buf;
}

/** Invalidate an issue's cached body after a mutation. */
export function touchIssue(issue: Issue): void {
  issue.rev += 1;
  issue.jsonCache = null;
}

export function pullJson(repo: Repo, pr: PullRequest): Record<string, unknown> {
  return {
    id: pr.id,
    node_id: globalId('pull', `${repo.fullName}#${pr.number}`),
    number: pr.number,
    title: pr.title,
    body: pr.body,
    state: pr.state,
    head: { ref: pr.headRef, sha: pr.headSha, repo: repoJson(repo) },
    base: { ref: pr.baseRef, sha: pr.baseSha, repo: repoJson(repo) },
    merged: pr.merged,
    mergeable: pr.state === 'open' ? !mergeWouldConflict(repo, pr) : null,
    merged_at: pr.mergedAt,
    merge_commit_sha: pr.mergeCommitSha,
    user: { login: VIEWER.login, id: VIEWER.id, type: 'User' },
    created_at: FIXED_ISO,
    updated_at: FIXED_ISO,
    closed_at: pr.state === 'closed' ? FIXED_ISO : null,
    html_url: `https://example.invalid/${repo.fullName}/pull/${pr.number}`,
    url: `/api/v3/repos/${repo.fullName}/pulls/${pr.number}`,
  };
}

/** The commits reachable from `tip`, newest first. */
function commitsFrom(repo: Repo, tip: string): Array<{ sha: string; message: string }> {
  const out: Array<{ sha: string; message: string }> = [];
  const seen = new Set<string>();
  const queue: string[] = [tip];
  let head = 0;
  while (head < queue.length) {
    const oid = queue[head++]!;
    if (seen.has(oid)) continue;
    seen.add(oid);
    const object = repo.objects.get(oid);
    if (!object || object.type !== 'commit') continue;
    const commit = parseCommit(object.data);
    out.push({ sha: oid, message: commit.message.replace(/\n+$/, '') });
    for (const parent of commit.parents) queue.push(parent);
  }
  return out;
}

/** The changed files between the PR's base and head tips. */
export function pullFiles(repo: Repo, pr: PullRequest): Array<Record<string, unknown>> {
  const lookup = repo.lookup;
  const baseFiles = pr.baseSha === '' ? new Map() : flattenTree(parseCommit(repo.objects.get(pr.baseSha)!.data).tree, lookup);
  const headFiles = flattenTree(parseCommit(repo.objects.get(pr.headSha)!.data).tree, lookup);
  const paths = new Set([...baseFiles.keys(), ...headFiles.keys()]);
  const out: Array<Record<string, unknown>> = [];
  for (const path of [...paths].sort()) {
    const base = baseFiles.get(path);
    const head = headFiles.get(path);
    if (base !== undefined && head !== undefined && base.oid === head.oid) continue;
    const baseText = base === undefined ? '' : repo.objects.get(base.oid)?.data.toString('utf8') ?? '';
    const headText = head === undefined ? '' : repo.objects.get(head.oid)?.data.toString('utf8') ?? '';
    let status: string;
    let additions: number;
    let deletions: number;
    if (base === undefined) {
      status = 'added';
      additions = lineCount(headText);
      deletions = 0;
    } else if (head === undefined) {
      status = 'removed';
      additions = 0;
      deletions = lineCount(baseText);
    } else {
      status = 'modified';
      const diff = lineDiff(baseText, headText);
      additions = diff.added;
      deletions = diff.removed;
    }
    out.push({ filename: path, status, additions, deletions, changes: additions + deletions });
  }
  return out;
}

/** A page of `items` with a Link header when more pages follow. */
function page<T>(
  store: Store,
  req: RawRequest,
  res: RawResponse,
  items: T[],
  render: (item: T) => unknown,
): void {
  const query = queryOf(req);
  const perPage = Math.min(Math.max(queryInt(query, 'per_page', 30), 1), 100);
  const pageNumber = Math.max(queryInt(query, 'page', 1), 1);
  const start = (pageNumber - 1) * perPage;
  const slice = items.slice(start, start + perPage);
  const extra: Record<string, string> = {};
  if (start + perPage < items.length) {
    const host = req.headers.host ?? 'localhost';
    const path = (req.url ?? '/').split('?')[0]!;
    const next = new URL(`http://${host}${path}`);
    for (const [key, value] of queryOf(req)) {
      if (key !== 'per_page' && key !== 'page') next.searchParams.set(key, value);
    }
    next.searchParams.set('per_page', String(perPage));
    next.searchParams.set('page', String(pageNumber + 1));
    extra.link = `<${next.toString()}>; rel="next"`;
  }
  sendJson(store, res, 200, slice.map(render), extra);
}

function isObject(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}

function stringField(body: Record<string, unknown>, field: string): string {
  const value = body[field];
  return typeof value === 'string' ? value : '';
}

/** Handle one REST request. Returns true when it answered. */
export async function handleRest(store: Store, req: RawRequest, res: RawResponse): Promise<boolean> {
  const segments = pathSegments(req);
  if (segments.length < 2 || segments[0] !== 'api' || segments[1] !== 'v3') return false;
  const rest = segments.slice(2);
  const method = req.method ?? 'GET';

  // Every REST route needs a bearer token.
  const authorization = req.headers.authorization ?? '';
  if (!authorization.startsWith('Bearer ') || authorization.slice('Bearer '.length).trim() === '') {
    sendError(store, res, 401, 'Requires authentication');
    return true;
  }

  const bodyBuffer = method === 'GET' || method === 'HEAD' ? Buffer.alloc(0) : await readBody(req);
  let body: unknown = {};
  if (bodyBuffer.length > 0) {
    const parsed = parseJsonBody(bodyBuffer);
    if (!parsed.ok) {
      sendError(store, res, 400, 'Problems parsing JSON');
      return true;
    }
    body = parsed.value;
  }
  const jsonBody: Record<string, unknown> = isObject(body) ? body : {};

  const repoAt = (owner: string, name: string): Repo | undefined => store.getRepo(owner, name);

  // GET /api/v3/
  if (method === 'GET' && rest.length === 0) {
    sendJson(store, res, 200, {
      current_user_url: '/api/v3/user',
      current_user_repositories_url: '/api/v3/user/repos',
      repository_url: '/api/v3/repos/{owner}/{repo}',
      meta_url: '/api/v3/meta',
    }, { 'x-oauth-scopes': 'repo, read:org' });
    return true;
  }

  // GET /api/v3/meta
  if (method === 'GET' && rest.length === 1 && rest[0] === 'meta') {
    sendJson(store, res, 200, {
      verifiable_password_authentication: false,
      installed_version: '1.0.0',
      hooks: [],
      git: [],
      packages: [],
      pages: [],
    });
    return true;
  }

  // GET /api/v3/user
  if (method === 'GET' && rest.length === 1 && rest[0] === 'user') {
    sendJson(store, res, 200, {
      login: VIEWER.login,
      id: VIEWER.id,
      node_id: Buffer.from(`user:${VIEWER.login}`, 'utf8').toString('base64url'),
      name: VIEWER.name,
      email: VIEWER.email,
      type: 'User',
    });
    return true;
  }

  // GET/POST /api/v3/user/repos
  if (rest.length === 2 && rest[0] === 'user' && rest[1] === 'repos') {
    if (method === 'GET') {
      page(store, req, res, store.viewerRepos(), (repo) => repoJson(repo));
      return true;
    }
    if (method === 'POST') {
      const name = stringField(jsonBody, 'name');
      if (name === '') {
        sendError(store, res, 422, 'Validation Failed: name is required');
        return true;
      }
      if (store.getRepo(VIEWER.login, name) !== undefined) {
        sendError(store, res, 422, 'Validation Failed: name already exists on this account');
        return true;
      }
      const isPrivate = jsonBody.private === true;
      const description = stringField(jsonBody, 'description');
      const repo = store.createRepo(VIEWER.login, name, description, isPrivate);
      sendJson(store, res, 201, repoJson(repo));
      return true;
    }
  }

  // /api/v3/repos/{owner}/{name}[...]
  if (rest.length >= 3 && rest[0] === 'repos') {
    const owner = rest[1]!;
    const name = rest[2]!;
    const repo = repoAt(owner, name);
    const tail = rest.slice(3);

    // The repository itself.
    if (tail.length === 0) {
      if (repo === undefined) {
        sendError(store, res, 404, 'Not Found');
        return true;
      }
      if (method === 'GET') {
        sendJson(store, res, 200, repoJson(repo));
        return true;
      }
      if (method === 'PATCH') {
        if (typeof jsonBody.description === 'string') repo.description = jsonBody.description;
        sendJson(store, res, 200, repoJson(repo));
        return true;
      }
      if (method === 'DELETE') {
        store.repos.delete(repo.fullName);
        res.writeHead(204, { 'x-github-request-id': store.nextRequestId(), 'content-length': 0 });
        res.end();
        return true;
      }
      sendError(store, res, 404, 'Not Found');
      return true;
    }

    if (repo === undefined) {
      sendError(store, res, 404, 'Not Found');
      return true;
    }

    // POST /repos/{o}/{r}/git/refs — create a branch (or tag) at a sha.
    if (tail.length === 2 && tail[0] === 'git' && tail[1] === 'refs' && method === 'POST') {
      const ref = stringField(jsonBody, 'ref');
      const sha = stringField(jsonBody, 'sha');
      if (!/^refs\/(heads|tags)\/\S+$/.test(ref) || !/^[0-9a-f]{40}$/i.test(sha) || !repo.objects.has(sha)) {
        sendError(store, res, 422, 'Validation Failed: ref or sha is invalid');
        return true;
      }
      if (repo.refs.has(ref)) {
        sendError(store, res, 422, 'Validation Failed: ref already exists');
        return true;
      }
      repo.setRef(ref, sha);
      sendJson(store, res, 201, { ref, object: { sha, type: repo.objects.get(sha)!.type, url: '' } });
      return true;
    }

    // GET /repos/{o}/{r}/branches
    if (tail.length === 1 && tail[0] === 'branches' && method === 'GET') {
      const branches = [...repo.refs.keys()]
        .filter((ref) => ref.startsWith('refs/heads/'))
        .sort()
        .map((ref) => ({ name: ref.slice('refs/heads/'.length), commit: { sha: repo.refs.get(ref)! } }));
      sendJson(store, res, 200, branches);
      return true;
    }

    // GET /repos/{o}/{r}/commits
    if (tail.length === 1 && tail[0] === 'commits' && method === 'GET') {
      const query = queryOf(req);
      const tip = repo.resolveRef(query.get('sha') ?? undefined);
      if (tip === undefined) {
        sendError(store, res, 404, 'Not Found');
        return true;
      }
      const commits = commitsFrom(repo, tip).map(({ sha, message }) => ({
        sha,
        commit: {
          message,
          author: { name: VIEWER.name, email: VIEWER.email, date: FIXED_ISO },
          committer: { name: VIEWER.name, email: VIEWER.email, date: FIXED_ISO },
          tree: { sha: parseCommit(repo.objects.get(sha)!.data).tree },
        },
        author: { login: VIEWER.login, id: VIEWER.id, type: 'User' },
        committer: { login: VIEWER.login, id: VIEWER.id, type: 'User' },
        parents: parseCommit(repo.objects.get(sha)!.data).parents.map((parent) => ({ sha: parent })),
      }));
      sendJson(store, res, 200, commits);
      return true;
    }

    // /repos/{o}/{r}/contents[/path...]
    if (tail.length >= 1 && tail[0] === 'contents') {
      const query = queryOf(req);
      const filePath = tail.slice(1).join('/');
      const branch = query.get('branch') ?? query.get('ref') ?? undefined;
      const tip = repo.resolveRef(branch);
      if (method === 'GET') {
        if (tip === undefined) {
          sendError(store, res, 404, 'Not Found');
          return true;
        }
        const treeOid = repo.treeAt(tip, filePath);
        if (treeOid !== undefined) {
          // A directory: list its entries.
          const entries: Array<Record<string, unknown>> = [];
          for (const entry of parseTree(repo.objects.get(treeOid)!.data)) {
            entries.push({
              type: entry.mode === '40000' ? 'dir' : 'file',
              name: entry.name,
              path: filePath === '' ? entry.name : `${filePath}/${entry.name}`,
              sha: entry.oid,
              size: entry.mode === '40000' ? 0 : repo.objects.get(entry.oid)?.data.length ?? 0,
            });
          }
          entries.sort((a, b) => String(a.name).localeCompare(String(b.name)));
          sendJson(store, res, 200, entries);
          return true;
        }
        const blobOid = repo.blobAt(tip, filePath);
        if (blobOid === undefined) {
          sendError(store, res, 404, 'Not Found');
          return true;
        }
        const data = repo.objects.get(blobOid)!.data;
        sendJson(store, res, 200, {
          type: 'file',
          encoding: 'base64',
          size: data.length,
          name: filePath.split('/').pop() ?? '',
          path: filePath,
          content: data.toString('base64'),
          sha: blobOid,
        });
        return true;
      }
      if (method === 'PUT') {
        const message = stringField(jsonBody, 'message');
        const contentB64 = stringField(jsonBody, 'content');
        const branchName = typeof jsonBody.branch === 'string' ? jsonBody.branch : repo.defaultBranch;
        const sha = typeof jsonBody.sha === 'string' ? jsonBody.sha : undefined;
        if (message === '' || contentB64 === '') {
          sendError(store, res, 422, 'Validation Failed: message and content are required');
          return true;
        }
        let content: Buffer;
        try {
          content = Buffer.from(contentB64, 'base64');
        } catch {
          sendError(store, res, 422, 'Validation Failed: content is not valid base64');
          return true;
        }
        const currentBlob = repo.branchTip(branchName) === undefined ? undefined : repo.blobAt(repo.branchTip(branchName)!, filePath);
        if (sha !== undefined && currentBlob !== undefined && sha.toLowerCase() !== currentBlob.toLowerCase()) {
          sendError(store, res, 409, 'Conflict: sha does not match the current file');
          return true;
        }
        if (sha !== undefined && currentBlob === undefined) {
          sendError(store, res, 409, 'Conflict: sha does not match the current file');
          return true;
        }
        const isUpdate = currentBlob !== undefined;
        const { commitSha, blobSha } = repo.commitFile(branchName, filePath, content, message);
        sendJson(store, res, isUpdate ? 200 : 201, {
          content: { name: filePath.split('/').pop() ?? '', path: filePath, sha: blobSha, size: content.length, encoding: 'base64' },
          commit: { sha: commitSha, message },
        });
        return true;
      }
      if (method === 'DELETE') {
        const sha = typeof jsonBody.sha === 'string' ? jsonBody.sha : undefined;
        const message = stringField(jsonBody, 'message');
        const branchName = typeof jsonBody.branch === 'string' ? jsonBody.branch : repo.defaultBranch;
        const tipOid = repo.branchTip(branchName);
        if (tipOid === undefined) {
          sendError(store, res, 404, 'Not Found');
          return true;
        }
        const currentBlob = repo.blobAt(tipOid, filePath);
        if (currentBlob === undefined) {
          sendError(store, res, 404, 'Not Found');
          return true;
        }
        if (sha !== undefined && sha.toLowerCase() !== currentBlob.toLowerCase()) {
          sendError(store, res, 409, 'Conflict: sha does not match the current file');
          return true;
        }
        const commitSha = repo.deleteFile(branchName, filePath, message === '' ? 'Delete file' : message);
        sendJson(store, res, 200, { commit: { sha: commitSha, message } });
        return true;
      }
      sendError(store, res, 404, 'Not Found');
      return true;
    }

    // POST /repos/{o}/{r}/labels
    if (tail.length === 1 && tail[0] === 'labels' && method === 'POST') {
      const name = stringField(jsonBody, 'name');
      const color = typeof jsonBody.color === 'string' ? jsonBody.color : 'ededed';
      if (name === '') {
        sendError(store, res, 422, 'Validation Failed: name is required');
        return true;
      }
      const existing = repo.labels.get(name);
      if (existing !== undefined) {
        sendJson(store, res, 201, { name: existing.name, color: existing.color });
        return true;
      }
      const label: Label = { name, color };
      repo.labels.set(name, label);
      sendJson(store, res, 201, { name, color });
      return true;
    }

    // /repos/{o}/{r}/issues[/{n}[/comments]]
    if (tail.length >= 1 && tail[0] === 'issues') {
      if (tail.length === 1) {
        if (method === 'GET') {
          const query = queryOf(req);
          const state = query.get('state') ?? 'open';
          const direction = query.get('direction') ?? 'desc';
          let issues = [...repo.issues.values()];
          if (state === 'open' || state === 'closed') issues = issues.filter((issue) => issue.state === state);
          issues.sort((a, b) => (direction === 'asc' ? a.createdSeq - b.createdSeq : b.createdSeq - a.createdSeq));
          page(store, req, res, issues, (issue) => issueJson(repo, issue));
          return true;
        }
        if (method === 'POST') {
          const title = stringField(jsonBody, 'title');
          const bodyText = stringField(jsonBody, 'body');
          const labelNames: string[] = Array.isArray(jsonBody.labels) ? jsonBody.labels.filter((item): item is string => typeof item === 'string') : [];
          const labels: Label[] = [];
          for (const labelName of labelNames) {
            const label = repo.labels.get(labelName);
            if (label === undefined) {
              sendError(store, res, 422, `Validation Failed: label ${JSON.stringify(labelName)} does not exist`);
              return true;
            }
            labels.push(label);
          }
          const number = repo.nextIssueNumber;
          repo.nextIssueNumber += 1;
          const issue: Issue = {
            id: number,
            number,
            title,
            body: bodyText,
            state: 'open',
            labels,
            createdSeq: repo.nextSeq(),
            comments: [],
            rev: 0,
            jsonCache: null,
          };
          repo.issues.set(number, issue);
          sendJson(store, res, 201, issueJson(repo, issue));
          return true;
        }
        sendError(store, res, 404, 'Not Found');
        return true;
      }
      const number = Number.parseInt(tail[1]!, 10);
      const issue = Number.isInteger(number) ? repo.issues.get(number) : undefined;
      if (tail.length === 2) {
        if (issue === undefined) {
          sendError(store, res, 404, 'Not Found');
          return true;
        }
        if (method === 'GET') {
          sendJsonBuffer(store, res, 200, issueJsonText(repo, issue));
          return true;
        }
        if (method === 'PATCH') {
          if (jsonBody.state === 'open' || jsonBody.state === 'closed') issue.state = jsonBody.state;
          if (typeof jsonBody.title === 'string') issue.title = jsonBody.title;
          if (typeof jsonBody.body === 'string') issue.body = jsonBody.body;
          touchIssue(issue);
          sendJson(store, res, 200, issueJson(repo, issue));
          return true;
        }
        sendError(store, res, 404, 'Not Found');
        return true;
      }
      if (tail.length === 3 && tail[2] === 'comments') {
        if (issue === undefined) {
          sendError(store, res, 404, 'Not Found');
          return true;
        }
        if (method === 'GET') {
          sendJson(store, res, 200, issue.comments.map((comment) => ({ id: comment.id, body: comment.body })));
          return true;
        }
        if (method === 'POST') {
          const commentBody = stringField(jsonBody, 'body');
          const comment = { id: repo.nextCommentId, body: commentBody };
          repo.nextCommentId += 1;
          issue.comments.push(comment);
          touchIssue(issue);
          sendJson(store, res, 201, { id: comment.id, body: comment.body });
          return true;
        }
        sendError(store, res, 404, 'Not Found');
        return true;
      }
      sendError(store, res, 404, 'Not Found');
      return true;
    }

    // /repos/{o}/{r}/pulls[/{n}[/merge|/files]]
    if (tail.length >= 1 && tail[0] === 'pulls') {
      if (tail.length === 1) {
        if (method === 'GET') {
          const query = queryOf(req);
          const state = query.get('state') ?? 'open';
          let pulls = [...repo.pulls.values()];
          if (state === 'open' || state === 'closed') pulls = pulls.filter((pr) => pr.state === state);
          pulls.sort((a, b) => b.createdSeq - a.createdSeq);
          page(store, req, res, pulls, (pr) => pullJson(repo, pr));
          return true;
        }
        if (method === 'POST') {
          const title = stringField(jsonBody, 'title');
          const bodyText = stringField(jsonBody, 'body');
          const headRef = stringField(jsonBody, 'head');
          const baseRef = stringField(jsonBody, 'base');
          const headSha = repo.branchTip(headRef);
          const baseSha = repo.branchTip(baseRef);
          if (headSha === undefined || baseSha === undefined) {
            sendError(store, res, 422, 'Validation Failed: head or base branch does not exist');
            return true;
          }
          const number = repo.nextPullNumber;
          repo.nextPullNumber += 1;
          const pr: PullRequest = {
            id: number,
            number,
            title,
            body: bodyText,
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
          sendJson(store, res, 201, pullJson(repo, pr));
          return true;
        }
        sendError(store, res, 404, 'Not Found');
        return true;
      }
      const number = Number.parseInt(tail[1]!, 10);
      const pr = Number.isInteger(number) ? repo.pulls.get(number) : undefined;
      if (tail.length === 2) {
        if (pr === undefined) {
          sendError(store, res, 404, 'Not Found');
          return true;
        }
        if (method === 'GET') {
          sendJson(store, res, 200, pullJson(repo, pr));
          return true;
        }
        sendError(store, res, 404, 'Not Found');
        return true;
      }
      if (tail.length === 3 && tail[2] === 'merge' && method === 'PUT') {
        if (pr === undefined) {
          sendError(store, res, 404, 'Not Found');
          return true;
        }
        if (pr.state !== 'open') {
          sendError(store, res, 405, 'Pull request is not open');
          return true;
        }
        const result = squashMerge(repo, pr);
        if (!result.ok) {
          sendError(store, res, 405, 'Merge conflict: the pull request cannot be merged cleanly');
          return true;
        }
        sendJson(store, res, 200, {
          sha: result.commitSha,
          merged: true,
          message: `Merge pull request #${pr.number} from ${pr.headRef}`,
        });
        return true;
      }
      if (tail.length === 3 && tail[2] === 'files' && method === 'GET') {
        if (pr === undefined) {
          sendError(store, res, 404, 'Not Found');
          return true;
        }
        sendJson(store, res, 200, pullFiles(repo, pr));
        return true;
      }
      sendError(store, res, 404, 'Not Found');
      return true;
    }
  }

  // Anything else under /api/v3 is an unknown route.
  sendError(store, res, 404, 'Not Found');
  return true;
}
