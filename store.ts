// store.ts — the forge's in-memory state: repositories, git objects and refs,
// issues, pull requests, labels and comments.
//
// One process, one store, no persistence: the trial starts a fresh server and
// the suite creates everything it needs through the API. All ids are counters
// and all timestamps are fixed constants, so the same requests in the same
// order always leave the same state.

import {
  buildCommit,
  buildNestedTree,
  flattenTree,
  hashObject,
  isObjectId,
  mergeBase,
  parseCommit,
  parseTree,
  threeWayMerge,
  type GitObject,
  type GitObjectType,
} from './git.ts';

/** The login the bearer token names: the seeded scenario's viewer. */
export const VIEWER = Object.freeze({
  id: 1000,
  login: 'adaiversen',
  name: 'ada iversen',
  email: 'adaiversen@example.invalid',
});

/** The fixed identity every commit this forge authors carries. */
export const FORGE_IDENTITY = 'adaiversen <adaiversen@example.invalid>';
/** The fixed commit timestamp (seconds, +0000) for every forge-authored commit. */
export const FORGE_EPOCH = 1700000000;
/** The fixed ISO timestamp for created_at / updated_at / merged_at fields. */
export const FIXED_ISO = '2025-01-01T00:00:00Z';

export function forgeAuthorLine(): string {
  return `${FORGE_IDENTITY} ${FORGE_EPOCH} +0000`;
}

/** A base64url global (Relay) id: opaque, stable, never the plain database id. */
export function globalId(kind: string, key: string): string {
  return Buffer.from(`${kind}:${key}`, 'utf8').toString('base64url');
}

export function decodeGlobalId(id: string): { kind: string; key: string } | null {
  try {
    const text = Buffer.from(id, 'base64url').toString('utf8');
    const colon = text.indexOf(':');
    if (colon <= 0) return null;
    return { kind: text.slice(0, colon), key: text.slice(colon + 1) };
  } catch {
    return null;
  }
}

export interface Label {
  name: string;
  color: string;
}

export interface Issue {
  id: number;
  number: number;
  title: string;
  body: string;
  state: 'open' | 'closed';
  labels: Label[];
  createdSeq: number;
  comments: Comment[];
  /** Mutation counter; issueJsonText caches the serialised body per revision. */
  rev: number;
  /** Cached `${JSON.stringify(issueJson)}` + newline, or null when stale. */
  jsonCache: string | null;
}

export interface Comment {
  id: number;
  body: string;
}

export interface PullRequest {
  id: number;
  number: number;
  title: string;
  body: string;
  state: 'open' | 'closed';
  headRef: string;
  baseRef: string;
  headSha: string;
  baseSha: string;
  merged: boolean;
  mergedAt: string | null;
  mergeCommitSha: string | null;
  createdSeq: number;
}

export class Repo {
  readonly databaseId: number;
  readonly name: string;
  readonly owner: string;
  description: string;
  isPrivate: boolean;
  readonly defaultBranch = 'main';
  readonly objects = new Map<string, GitObject>();
  readonly refs = new Map<string, string>();
  readonly issues = new Map<number, Issue>();
  readonly pulls = new Map<number, PullRequest>();
  readonly labels = new Map<string, Label>();
  nextIssueNumber = 1;
  nextPullNumber = 1;
  nextCommentId = 1;
  private seq = 0;
  /**
   * Response caches for the git surface (clone workload: 50 concurrent
   * identical fetches of one unchanged repository). Every entry is keyed by
   * the git fingerprint below plus the request, so a key can only hit while
   * the repository's refs and object set are exactly the ones the response
   * was built from; objects are content-addressed and only ever added within
   * a repository's life, refs are named in the fingerprint, and deleting a
   * repository drops its Repo (and its caches) outright. Maps are capped so
   * stale fingerprints cannot grow them without bound.
   */
  readonly fetchCache = new Map<string, Buffer>();
  readonly lsRefsCache = new Map<string, Buffer>();
  readonly advCache = new Map<string, Buffer>();

  /** The git state a cached git response was built from. */
  gitFingerprint(): string {
    const refs = [...this.refs.entries()].sort((a, b) => (a[0] < b[0] ? -1 : a[0] > b[0] ? 1 : 0));
    let out = `${this.objects.size}|`;
    for (const [name, oid] of refs) out += `${name}=${oid};`;
    return out;
  }

  /** Store a cached git response, clearing the map when it grows too large. */
  cacheGit(cache: Map<string, Buffer>, key: string, value: Buffer): void {
    if (cache.size >= 64) cache.clear();
    cache.set(key, value);
  }

  constructor(databaseId: number, owner: string, name: string, description: string, isPrivate: boolean) {
    this.databaseId = databaseId;
    this.owner = owner;
    this.name = name;
    this.description = description;
    this.isPrivate = isPrivate;
  }

  get fullName(): string {
    return `${this.owner}/${this.name}`;
  }

  get globalId(): string {
    return globalId('repo', this.fullName);
  }

  nextSeq(): number {
    this.seq += 1;
    return this.seq;
  }

  put(type: GitObjectType, data: Buffer): string {
    const oid = hashObject(type, data);
    if (!this.objects.has(oid)) this.objects.set(oid, { type, data });
    return oid;
  }

  /** Store a pack object under its id (receive-pack's path into the store). */
  addObject(oid: string, object: GitObject): void {
    if (!this.objects.has(oid)) this.objects.set(oid, object);
  }

  /** Set any ref (heads, tags, …). */
  setRef(ref: string, oid: string): void {
    this.refs.set(ref, oid);
  }

  /** Delete any ref; false when it did not exist. */
  deleteRef(ref: string): boolean {
    return this.refs.delete(ref);
  }

  lookup = (oid: string): GitObject | undefined => this.objects.get(oid);

  refTip(ref: string): string | undefined {
    return this.refs.get(ref);
  }

  branchTip(branch: string): string | undefined {
    return this.refs.get(`refs/heads/${branch}`);
  }

  setBranch(branch: string, oid: string): void {
    this.refs.set(`refs/heads/${branch}`, oid);
  }

  /** The default branch's tip, or undefined while the repository is unborn. */
  defaultTip(): string | undefined {
    return this.refs.get(`refs/heads/${this.defaultBranch}`);
  }

  /** Resolve a branch name or a commit oid to a commit oid. */
  resolveRef(ref: string | undefined): string | undefined {
    if (ref === undefined) return this.defaultTip();
    if (isObjectId(ref)) return this.objects.has(ref) ? ref : undefined;
    return this.branchTip(ref);
  }

  /** Create a commit object on top of `parents` with the given tree, and store it. */
  makeCommit(treeOid: string, parents: readonly string[], message: string): string {
    const who = forgeAuthorLine();
    const text = message.endsWith('\n') ? message : `${message}\n`;
    return this.put('commit', buildCommit({ tree: treeOid, parents, message: text, author: who, committer: who }));
  }

  /**
   * Commit one file change on `branch`: write the blob, rebuild the trees from
   * the branch tip (or an empty tree when unborn), create the commit and move
   * the branch. A branch that does not exist is created from the default tip
   * (or as the root when the repository is unborn).
   */
  commitFile(branch: string, path: string, content: Buffer, message: string): { commitSha: string; blobSha: string } {
    const blobSha = this.put('blob', content);
    let baseFiles = new Map<string, { oid: string; mode: string }>();
    let parents: string[] = [];
    const tip = this.branchTip(branch);
    if (tip !== undefined) {
      const commit = parseCommit(this.objects.get(tip)!.data);
      baseFiles = flattenTree(commit.tree, this.lookup);
      parents = [tip];
    } else {
      const defaultTip = this.defaultTip();
      if (defaultTip !== undefined && branch !== this.defaultBranch) {
        const commit = parseCommit(this.objects.get(defaultTip)!.data);
        baseFiles = flattenTree(commit.tree, this.lookup);
        parents = [defaultTip];
      }
    }
    const files = new Map(baseFiles);
    files.set(path, { oid: blobSha, mode: '100644' });
    const treeOid = buildNestedTree(files, (type, data) => this.put(type, data));
    const commitSha = this.makeCommit(treeOid, parents, message);
    this.setBranch(branch, commitSha);
    return { commitSha, blobSha };
  }

  /** Commit the deletion of one file on `branch`. */
  deleteFile(branch: string, path: string, message: string): string {
    const tip = this.branchTip(branch);
    if (tip === undefined) throw new Error('branch has no tip');
    const commit = parseCommit(this.objects.get(tip)!.data);
    const files = flattenTree(commit.tree, this.lookup);
    files.delete(path);
    const treeOid = buildNestedTree(files, (type, data) => this.put(type, data));
    const commitSha = this.makeCommit(treeOid, [tip], message);
    this.setBranch(branch, commitSha);
    return commitSha;
  }

  /** The blob oid of `path` at `commitOid`, or undefined when absent. */
  blobAt(commitOid: string, path: string): string | undefined {
    const object = this.objects.get(commitOid);
    if (!object || object.type !== 'commit') return undefined;
    return flattenTree(parseCommit(object.data).tree, this.lookup).get(path)?.oid;
  }

  /** The tree oid of `path` (a directory) at `commitOid`, or undefined. */
  treeAt(commitOid: string, path: string): string | undefined {
    const object = this.objects.get(commitOid);
    if (!object || object.type !== 'commit') return undefined;
    if (path === '' || path === '/') return parseCommit(object.data).tree;
    let current = parseCommit(object.data).tree;
    for (const segment of path.split('/').filter((part) => part !== '')) {
      const tree = this.objects.get(current);
      if (!tree || tree.type !== 'tree') return undefined;
      let found: string | undefined;
      for (const entry of parseTree(tree.data)) {
        if (entry.name === segment && entry.mode === '40000') found = entry.oid;
      }
      if (!found) return undefined;
      current = found;
    }
    return current;
  }
}

export class Store {
  readonly repos = new Map<string, Repo>();
  private nextRepoId = 1;
  private requestSeq = 0;

  nextRequestId(): string {
    this.requestSeq += 1;
    return `req-${this.requestSeq}`;
  }

  createRepo(owner: string, name: string, description: string, isPrivate: boolean): Repo {
    const repo = new Repo(this.nextRepoId, owner, name, description, isPrivate);
    this.nextRepoId += 1;
    this.repos.set(`${owner}/${name}`, repo);
    return repo;
  }

  getRepo(owner: string, name: string): Repo | undefined {
    return this.repos.get(`${owner}/${name}`);
  }

  /** The viewer's repositories, in creation order. */
  viewerRepos(): Repo[] {
    return [...this.repos.values()]
      .filter((repo) => repo.owner === VIEWER.login)
      .sort((a, b) => a.databaseId - b.databaseId);
  }
}

/** Squash-merge a pull request: three-way merge onto the base tip, one commit. */
export function squashMerge(
  repo: Repo,
  pr: PullRequest,
): { ok: true; commitSha: string } | { ok: false; conflicts: string[] } {
  const headTip = repo.branchTip(pr.headRef);
  const baseTip = repo.branchTip(pr.baseRef);
  if (headTip === undefined || baseTip === undefined) return { ok: false, conflicts: [] };
  const base = mergeBase(baseTip, headTip, repo.lookup);
  const baseTree = base === null ? new Map() : flattenTree(parseCommit(repo.objects.get(base)!.data).tree, repo.lookup);
  const headFiles = flattenTree(parseCommit(repo.objects.get(headTip)!.data).tree, repo.lookup);
  const tipFiles = flattenTree(parseCommit(repo.objects.get(baseTip)!.data).tree, repo.lookup);
  const merged = threeWayMerge(baseTree, headFiles, tipFiles);
  if (merged.conflicts.length > 0) return { ok: false, conflicts: merged.conflicts };
  const treeOid = buildNestedTree(merged.files, (type, data) => repo.put(type, data));
  const commitSha = repo.makeCommit(treeOid, [baseTip], `Merge pull request #${pr.number} from ${pr.headRef}`);
  repo.setBranch(pr.baseRef, commitSha);
  pr.state = 'closed';
  pr.merged = true;
  pr.mergedAt = FIXED_ISO;
  pr.mergeCommitSha = commitSha;
  return { ok: true, commitSha };
}

/** Whether merging `pr` would conflict (without changing anything). */
export function mergeWouldConflict(repo: Repo, pr: PullRequest): boolean {
  const headTip = repo.branchTip(pr.headRef);
  const baseTip = repo.branchTip(pr.baseRef);
  if (headTip === undefined || baseTip === undefined) return false;
  const base = mergeBase(baseTip, headTip, repo.lookup);
  const baseTree = base === null ? new Map() : flattenTree(parseCommit(repo.objects.get(base)!.data).tree, repo.lookup);
  const headFiles = flattenTree(parseCommit(repo.objects.get(headTip)!.data).tree, repo.lookup);
  const tipFiles = flattenTree(parseCommit(repo.objects.get(baseTip)!.data).tree, repo.lookup);
  return threeWayMerge(baseTree, headFiles, tipFiles).conflicts.length > 0;
}
