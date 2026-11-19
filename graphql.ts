// graphql.ts — a small hand-rolled GraphQL executor for the v4 surface at
// /api/graphql, covering exactly the documents the suite sends (gh's query
// shapes): viewer, repository, issue, issues, pullRequest, pullRequests,
// node, __type introspection, the five mutations, and batched bodies.
//
// The schema is fixed: Repository, Issue, PullRequest, User, and the
// connections between them. Unknown fields answer HTTP 200 with an `errors`
// array naming the field; an unknown repository answers data.repository null
// with no errors; a missing token is 401.

import type { IncomingMessage, ServerResponse } from 'node:http';
import { readBody, sendError, sendJson } from './http.ts';
import {
  FIXED_ISO,
  VIEWER,
  decodeGlobalId,
  globalId,
  mergeWouldConflict,
  squashMerge,
  type Issue,
  type PullRequest,
  type Repo,
  type Store,
} from './store.ts';
import { pullFiles as restPullFiles } from './rest.ts';

// -------------------------------------------------------------------------------------------
// Tokenizer and parser
// -------------------------------------------------------------------------------------------

type TokenKind = 'name' | 'int' | 'float' | 'string' | 'punct' | 'eof';
interface Token {
  kind: TokenKind;
  value: string;
}

function tokenize(source: string): Token[] {
  const tokens: Token[] = [];
  let i = 0;
  while (i < source.length) {
    const ch = source[i]!;
    if (ch === ' ' || ch === '\t' || ch === '\n' || ch === '\r' || ch === ',' || ch === '#') {
      if (ch === '#') {
        while (i < source.length && source[i] !== '\n') i += 1;
      } else {
        i += 1;
      }
      continue;
    }
    if (ch === '"') {
      let value = '';
      i += 1;
      while (i < source.length && source[i] !== '"') {
        if (source[i] === '\\' && i + 1 < source.length) {
          const escape = source[i + 1]!;
          value += escape === 'n' ? '\n' : escape === 't' ? '\t' : escape === 'r' ? '\r' : escape;
          i += 2;
        } else {
          value += source[i]!;
          i += 1;
        }
      }
      i += 1;
      tokens.push({ kind: 'string', value });
      continue;
    }
    if (ch === '-' || (ch >= '0' && ch <= '9')) {
      let value = '';
      let isFloat = false;
      if (ch === '-') {
        value += ch;
        i += 1;
      }
      while (i < source.length && /[0-9.]/.test(source[i]!)) {
        if (source[i] === '.') isFloat = true;
        value += source[i]!;
        i += 1;
      }
      tokens.push({ kind: isFloat ? 'float' : 'int', value });
      continue;
    }
    if (/[A-Za-z_]/.test(ch)) {
      let value = '';
      while (i < source.length && /[A-Za-z0-9_]/.test(source[i]!)) {
        value += source[i]!;
        i += 1;
      }
      tokens.push({ kind: 'name', value });
      continue;
    }
    if (ch === '.' && source.slice(i, i + 3) === '...') {
      tokens.push({ kind: 'punct', value: '...' });
      i += 3;
      continue;
    }
    tokens.push({ kind: 'punct', value: ch });
    i += 1;
  }
  tokens.push({ kind: 'eof', value: '' });
  return tokens;
}

type ValueNode =
  | { kind: 'string'; value: string }
  | { kind: 'int'; value: number }
  | { kind: 'float'; value: number }
  | { kind: 'boolean'; value: boolean }
  | { kind: 'null' }
  | { kind: 'enum'; value: string }
  | { kind: 'variable'; name: string }
  | { kind: 'list'; values: ValueNode[] }
  | { kind: 'object'; fields: Array<{ name: string; value: ValueNode }> };

interface FieldNode {
  kind: 'field';
  alias: string;
  name: string;
  args: Array<{ name: string; value: ValueNode }>;
  selections: SelectionNode[] | null;
}

interface FragmentNode {
  kind: 'fragment';
  typeCondition: string | null;
  selections: SelectionNode[];
}

type SelectionNode = FieldNode | FragmentNode;

interface OperationNode {
  operation: 'query' | 'mutation';
  name: string | null;
  variables: Array<{ name: string; defaultValue: ValueNode | null }>;
  selections: SelectionNode[];
}

class ParseError extends Error {}

class Parser {
  private tokens: Token[];
  private pos = 0;

  constructor(source: string) {
    this.tokens = tokenize(source);
  }

  private peek(): Token {
    return this.tokens[this.pos]!;
  }

  private next(): Token {
    const token = this.tokens[this.pos]!;
    this.pos += 1;
    return token;
  }

  private expectPunct(value: string): void {
    const token = this.next();
    if (token.kind !== 'punct' || token.value !== value) {
      throw new ParseError(`expected ${JSON.stringify(value)}, got ${JSON.stringify(token.value)}`);
    }
  }

  private expectName(value?: string): string {
    const token = this.next();
    if (token.kind !== 'name' || (value !== undefined && token.value !== value)) {
      throw new ParseError(`expected name ${value ?? ''}, got ${JSON.stringify(token.value)}`);
    }
    return token.value;
  }

  parseDocument(): OperationNode[] {
    const operations: OperationNode[] = [];
    while (this.peek().kind !== 'eof') {
      const token = this.peek();
      if (token.kind === 'punct' && token.value === '{') {
        operations.push({
          operation: 'query',
          name: null,
          variables: [],
          selections: this.parseSelections(),
        });
        continue;
      }
      if (token.kind === 'name' && (token.value === 'query' || token.value === 'mutation')) {
        const operation = this.next().value as 'query' | 'mutation';
        let name: string | null = null;
        if (this.peek().kind === 'name') name = this.next().value;
        const variables: OperationNode['variables'] = [];
        if (this.peek().kind === 'punct' && this.peek().value === '(') {
          this.next();
          while (!(this.peek().kind === 'punct' && this.peek().value === ')')) {
            if (this.peek().kind === 'eof') throw new ParseError('unterminated variable definitions');
            this.expectPunct('$');
            const varName = this.expectName();
            this.expectPunct(':');
            this.parseType();
            let defaultValue: ValueNode | null = null;
            if (this.peek().kind === 'punct' && this.peek().value === '=') {
              this.next();
              defaultValue = this.parseValue();
            }
            variables.push({ name: varName, defaultValue });
          }
          this.next();
        }
        operations.push({ operation, name, variables, selections: this.parseSelections() });
        continue;
      }
      if (token.kind === 'name' && token.value === 'fragment') {
        // Named fragments are not used by the suite; skip the definition.
        this.next();
        this.expectName();
        this.expectName('on');
        this.expectName();
        this.parseSelections();
        continue;
      }
      throw new ParseError(`unexpected token ${JSON.stringify(token.value)}`);
    }
    return operations;
  }

  private parseType(): void {
    if (this.peek().kind === 'punct' && this.peek().value === '[') {
      this.next();
      this.parseType();
      this.expectPunct(']');
    } else {
      this.expectName();
    }
    if (this.peek().kind === 'punct' && this.peek().value === '!') this.next();
  }

  private parseSelections(): SelectionNode[] {
    this.expectPunct('{');
    const selections: SelectionNode[] = [];
    while (!(this.peek().kind === 'punct' && this.peek().value === '}')) {
      if (this.peek().kind === 'eof') throw new ParseError('unterminated selection set');
      selections.push(this.parseSelection());
    }
    this.next();
    return selections;
  }

  private parseSelection(): SelectionNode {
    if (this.peek().kind === 'punct' && this.peek().value === '...') {
      this.next();
      let typeCondition: string | null = null;
      if (this.peek().kind === 'name' && this.peek().value === 'on') {
        this.next();
        typeCondition = this.expectName();
      } else if (this.peek().kind === 'name') {
        // A named fragment spread: not used by the suite; skip its selections.
        this.next();
        return { kind: 'fragment', typeCondition: null, selections: [] };
      }
      return { kind: 'fragment', typeCondition, selections: this.parseSelections() };
    }
    const first = this.expectName();
    let alias = first;
    let name = first;
    if (this.peek().kind === 'punct' && this.peek().value === ':') {
      this.next();
      name = this.expectName();
    }
    const args: FieldNode['args'] = [];
    if (this.peek().kind === 'punct' && this.peek().value === '(') {
      this.next();
      while (!(this.peek().kind === 'punct' && this.peek().value === ')')) {
        if (this.peek().kind === 'eof') throw new ParseError('unterminated arguments');
        const argName = this.expectName();
        this.expectPunct(':');
        args.push({ name: argName, value: this.parseValue() });
      }
      this.next();
    }
    let selections: SelectionNode[] | null = null;
    if (this.peek().kind === 'punct' && this.peek().value === '{') {
      selections = this.parseSelections();
    }
    return { kind: 'field', alias, name, args, selections };
  }

  private parseValue(): ValueNode {
    const token = this.next();
    switch (token.kind) {
      case 'string':
        return { kind: 'string', value: token.value };
      case 'int':
        return { kind: 'int', value: Number.parseInt(token.value, 10) };
      case 'float':
        return { kind: 'float', value: Number.parseFloat(token.value) };
      case 'name':
        if (token.value === 'true') return { kind: 'boolean', value: true };
        if (token.value === 'false') return { kind: 'boolean', value: false };
        if (token.value === 'null') return { kind: 'null' };
        return { kind: 'enum', value: token.value };
      case 'punct':
        if (token.value === '$') return { kind: 'variable', name: this.expectName() };
        if (token.value === '[') {
          const values: ValueNode[] = [];
          while (!(this.peek().kind === 'punct' && this.peek().value === ']')) {
            if (this.peek().kind === 'eof') throw new ParseError('unterminated list');
            values.push(this.parseValue());
          }
          this.next();
          return { kind: 'list', values };
        }
        if (token.value === '{') {
          const fields: Array<{ name: string; value: ValueNode }> = [];
          while (!(this.peek().kind === 'punct' && this.peek().value === '}')) {
            if (this.peek().kind === 'eof') throw new ParseError('unterminated object');
            const name = this.expectName();
            this.expectPunct(':');
            fields.push({ name, value: this.parseValue() });
          }
          this.next();
          return { kind: 'object', fields };
        }
        throw new ParseError(`unexpected punct ${JSON.stringify(token.value)} in value`);
      default:
        throw new ParseError(`unexpected token ${JSON.stringify(token.value)} in value`);
    }
  }
}

function valueFromNode(node: ValueNode, variables: Record<string, unknown>): unknown {
  switch (node.kind) {
    case 'string':
    case 'int':
    case 'float':
    case 'boolean':
      return node.value;
    case 'null':
      return null;
    case 'enum':
      return node.value;
    case 'variable':
      return variables[node.name] ?? null;
    case 'list':
      return node.values.map((value) => valueFromNode(value, variables));
    case 'object': {
      const out: Record<string, unknown> = {};
      for (const field of node.fields) out[field.name] = valueFromNode(field.value, variables);
      return out;
    }
  }
}

function argValue(field: FieldNode, name: string, variables: Record<string, unknown>): unknown {
  const arg = field.args.find((entry) => entry.name === name);
  return arg === undefined ? undefined : valueFromNode(arg.value, variables);
}

// -------------------------------------------------------------------------------------------
// The schema: field lists for introspection
// -------------------------------------------------------------------------------------------

const SCHEMA_FIELDS: Record<string, string[]> = {
  Query: ['viewer', 'repository', 'node'],
  Mutation: ['createIssue', 'closeIssue', 'createPullRequest', 'mergePullRequest', 'addComment'],
  Repository: [
    'id', 'databaseId', 'name', 'nameWithOwner', 'owner', 'description', 'defaultBranchRef',
    'viewerPermission', 'hasIssuesEnabled', 'issue', 'issues', 'pullRequest', 'pullRequests',
    'createdAt', 'updatedAt', 'url', 'isPrivate',
  ],
  Issue: ['id', 'number', 'title', 'body', 'state', 'author', 'createdAt', 'updatedAt', 'url', 'repository'],
  PullRequest: [
    'id', 'number', 'title', 'body', 'state', 'headRefName', 'baseRefName', 'headRef', 'baseRef',
    'mergeable', 'merged', 'mergedAt', 'files', 'author', 'createdAt', 'updatedAt', 'url', 'repository',
  ],
  User: ['id', 'login', 'name', 'email'],
  IssueConnection: ['nodes', 'edges', 'pageInfo', 'totalCount'],
  PullRequestConnection: ['nodes', 'edges', 'pageInfo', 'totalCount'],
  PullRequestFileConnection: ['nodes', 'edges', 'pageInfo', 'totalCount'],
  PageInfo: ['hasNextPage', 'hasPreviousPage', 'startCursor', 'endCursor'],
  Ref: ['name', 'target'],
  IssueComment: ['id', 'body', 'author', 'createdAt'],
  IssueCommentEdge: ['node', 'cursor'],
  __Type: ['name', 'kind', 'description', 'fields', 'enumValues'],
  __Field: ['name', 'description'],
  __EnumValue: ['name', 'description'],
};

const SCHEMA_ENUMS: Record<string, string[]> = {
  SearchType: ['DISCUSSION', 'ISSUE', 'MILESTONE', 'PULL_REQUEST', 'REPOSITORY', 'USER'],
  IssueState: ['OPEN', 'CLOSED'],
  PullRequestState: ['OPEN', 'CLOSED', 'MERGED'],
  MergeableState: ['CONFLICTING', 'MERGEABLE', 'UNKNOWN'],
  RepositoryPermission: ['ADMIN', 'MAINTAIN', 'READ', 'TRIAGE', 'WRITE'],
};

// -------------------------------------------------------------------------------------------
// Resolvers
// -------------------------------------------------------------------------------------------

type GqlObject =
  | { __type: 'Viewer'; login: string }
  | { __type: 'Repository'; repo: Repo }
  | { __type: 'Issue'; repo: Repo; issue: Issue }
  | { __type: 'PullRequest'; repo: Repo; pr: PullRequest }
  | { __type: 'IssueConnection'; repo: Repo; issues: Issue[]; pageInfo: PageInfo }
  | { __type: 'PullRequestConnection'; repo: Repo; prs: PullRequest[]; pageInfo: PageInfo }
  | { __type: 'PullRequestFileConnection'; files: Array<{ path: string }> }
  | { __type: 'PageInfo'; info: PageInfo }
  | { __type: 'Ref'; name: string }
  | { __type: 'User'; login: string }
  | { __type: 'Type'; name: string }
  | { __type: 'Comment'; id: string; body: string }
  | { __type: 'CommentEdge'; id: string; body: string }
  | { __type: 'CreateIssuePayload'; repo: Repo; issue: Issue }
  | { __type: 'CloseIssuePayload'; repo: Repo; issue: Issue }
  | { __type: 'CreatePullRequestPayload'; repo: Repo; pr: PullRequest }
  | { __type: 'MergePullRequestPayload'; repo: Repo; pr: PullRequest }
  | { __type: 'AddCommentPayload'; id: string; body: string };

interface PageInfo {
  hasNextPage: boolean;
  hasPreviousPage: boolean;
  startCursor: string | null;
  endCursor: string | null;
}

interface Executor {
  store: Store;
  errors: Array<{ message: string }>;
}

function stateOf(issue: Issue): string {
  return issue.state === 'open' ? 'OPEN' : 'CLOSED';
}

function prStateOf(pr: PullRequest): string {
  return pr.state === 'open' ? 'OPEN' : 'CLOSED';
}

function cursorFor(seq: number): string {
  return Buffer.from(`cursor:${seq}`, 'utf8').toString('base64url');
}

function seqFromCursor(cursor: unknown): number | null {
  if (typeof cursor !== 'string' || cursor === '') return null;
  try {
    const text = Buffer.from(cursor, 'base64url').toString('utf8');
    if (!text.startsWith('cursor:')) return null;
    const seq = Number.parseInt(text.slice('cursor:'.length), 10);
    return Number.isFinite(seq) ? seq : null;
  } catch {
    return null;
  }
}

/** The issues connection for a repository, honouring states, orderBy, first and after. */
function issuesConnection(repo: Repo, field: FieldNode, variables: Record<string, unknown>): GqlObject {
  let issues = [...repo.issues.values()];
  const states = argValue(field, 'states', variables);
  if (Array.isArray(states) && states.length > 0) {
    const wanted = new Set(states.map((state) => String(state).toLowerCase()));
    issues = issues.filter((issue) => wanted.has(issue.state));
  }
  const orderBy = argValue(field, 'orderBy', variables);
  let direction = 'DESC';
  if (orderBy !== null && typeof orderBy === 'object' && !Array.isArray(orderBy)) {
    const record = orderBy as Record<string, unknown>;
    if (typeof record.direction === 'string') direction = record.direction;
  }
  issues.sort((a, b) => (direction === 'ASC' ? a.createdSeq - b.createdSeq : b.createdSeq - a.createdSeq));
  const afterSeq = seqFromCursor(argValue(field, 'after', variables));
  if (afterSeq !== null) {
    issues = direction === 'ASC'
      ? issues.filter((issue) => issue.createdSeq > afterSeq)
      : issues.filter((issue) => issue.createdSeq < afterSeq);
  }
  const first = argValue(field, 'first', variables);
  const limit = typeof first === 'number' && Number.isFinite(first) && first >= 0 ? Math.floor(first) : issues.length;
  const page = issues.slice(0, limit);
  const hasNextPage = issues.length > page.length;
  const last = page[page.length - 1];
  const pageInfo: PageInfo = {
    hasNextPage,
    hasPreviousPage: afterSeq !== null,
    startCursor: page.length > 0 ? cursorFor(page[0]!.createdSeq) : null,
    endCursor: last !== undefined ? cursorFor(last.createdSeq) : null,
  };
  return { __type: 'IssueConnection', repo, issues: page, pageInfo };
}

function pullsConnection(repo: Repo, field: FieldNode, variables: Record<string, unknown>): GqlObject {
  let prs = [...repo.pulls.values()];
  const states = argValue(field, 'states', variables);
  if (Array.isArray(states) && states.length > 0) {
    const wanted = new Set(states.map((state) => String(state).toLowerCase()));
    prs = prs.filter((pr) => wanted.has(pr.state));
  } else {
    prs = prs.filter((pr) => pr.state === 'open');
  }
  prs.sort((a, b) => b.createdSeq - a.createdSeq);
  const first = argValue(field, 'first', variables);
  const limit = typeof first === 'number' && Number.isFinite(first) && first >= 0 ? Math.floor(first) : prs.length;
  const page = prs.slice(0, limit);
  const pageInfo: PageInfo = {
    hasNextPage: prs.length > page.length,
    hasPreviousPage: false,
    startCursor: page.length > 0 ? cursorFor(page[0]!.createdSeq) : null,
    endCursor: page.length > 0 ? cursorFor(page[page.length - 1]!.createdSeq) : null,
  };
  return { __type: 'PullRequestConnection', repo, prs: page, pageInfo };
}

function resolveField(exec: Executor, obj: GqlObject, field: FieldNode, variables: Record<string, unknown>): unknown {
  const store = exec.store;
  switch (obj.__type) {
    case 'Viewer':
      if (field.name === 'login') return VIEWER.login;
      if (field.name === 'id') return globalId('user', VIEWER.login);
      if (field.name === 'name') return VIEWER.name;
      if (field.name === 'email') return VIEWER.email;
      break;
    case 'User':
      if (field.name === 'login') return obj.login;
      if (field.name === 'id') return globalId('user', obj.login);
      if (field.name === 'name') return obj.login === VIEWER.login ? VIEWER.name : obj.login;
      if (field.name === 'email') return obj.login === VIEWER.login ? VIEWER.email : null;
      break;
    case 'Repository': {
      const repo = obj.repo;
      switch (field.name) {
        case 'id': return repo.globalId;
        case 'databaseId': return repo.databaseId;
        case 'name': return repo.name;
        case 'nameWithOwner': return repo.fullName;
        case 'description': return repo.description;
        case 'isPrivate': return repo.isPrivate;
        case 'createdAt': return FIXED_ISO;
        case 'updatedAt': return FIXED_ISO;
        case 'url': return `https://example.invalid/${repo.fullName}`;
        case 'owner': return { __type: 'User', login: repo.owner } as GqlObject;
        case 'defaultBranchRef': {
          const tip = repo.defaultTip();
          return tip === undefined ? null : ({ __type: 'Ref', name: repo.defaultBranch } as GqlObject);
        }
        case 'viewerPermission': return 'ADMIN';
        case 'hasIssuesEnabled': return true;
        case 'issue': {
          const number = argValue(field, 'number', variables);
          if (typeof number !== 'number') return null;
          const issue = repo.issues.get(number);
          return issue === undefined ? null : ({ __type: 'Issue', repo, issue } as GqlObject);
        }
        case 'issues': return issuesConnection(repo, field, variables);
        case 'pullRequest': {
          const number = argValue(field, 'number', variables);
          if (typeof number !== 'number') return null;
          const pr = repo.pulls.get(number);
          return pr === undefined ? null : ({ __type: 'PullRequest', repo, pr } as GqlObject);
        }
        case 'pullRequests': return pullsConnection(repo, field, variables);
      }
      break;
    }
    case 'Issue': {
      const issue = obj.issue;
      switch (field.name) {
        case 'id': return globalId('issue', `${obj.repo.fullName}#${issue.number}`);
        case 'number': return issue.number;
        case 'title': return issue.title;
        case 'body': return issue.body;
        case 'state': return stateOf(issue);
        case 'author': return { __type: 'User', login: VIEWER.login } as GqlObject;
        case 'createdAt': return FIXED_ISO;
        case 'updatedAt': return FIXED_ISO;
        case 'url': return `https://example.invalid/${obj.repo.fullName}/issues/${issue.number}`;
        case 'repository': return { __type: 'Repository', repo: obj.repo } as GqlObject;
      }
      break;
    }
    case 'PullRequest': {
      const pr = obj.pr;
      switch (field.name) {
        case 'id': return globalId('pull', `${obj.repo.fullName}#${pr.number}`);
        case 'number': return pr.number;
        case 'title': return pr.title;
        case 'body': return pr.body;
        case 'state': return prStateOf(pr);
        case 'headRefName': return pr.headRef;
        case 'baseRefName': return pr.baseRef;
        case 'headRef': return { __type: 'Ref', name: pr.headRef } as GqlObject;
        case 'baseRef': return { __type: 'Ref', name: pr.baseRef } as GqlObject;
        case 'mergeable': return mergeWouldConflict(obj.repo, pr) ? 'CONFLICTING' : 'MERGEABLE';
        case 'merged': return pr.merged;
        case 'mergedAt': return pr.mergedAt;
        case 'author': return { __type: 'User', login: VIEWER.login } as GqlObject;
        case 'createdAt': return FIXED_ISO;
        case 'updatedAt': return FIXED_ISO;
        case 'url': return `https://example.invalid/${obj.repo.fullName}/pull/${pr.number}`;
        case 'repository': return { __type: 'Repository', repo: obj.repo } as GqlObject;
        case 'files': {
          const first = argValue(field, 'first', variables);
          const limit = typeof first === 'number' && Number.isFinite(first) && first >= 0 ? Math.floor(first) : 100;
          const files = restPullFiles(obj.repo, pr).slice(0, limit).map((file) => ({ path: String(file.filename) }));
          return { __type: 'PullRequestFileConnection', files } as GqlObject;
        }
      }
      break;
    }
    case 'IssueConnection':
      if (field.name === 'nodes') return obj.issues.map((issue) => ({ __type: 'Issue', repo: obj.repo, issue } as GqlObject));
      if (field.name === 'edges') return obj.issues.map((issue) => ({ node: { __type: 'Issue', repo: obj.repo, issue } as GqlObject, cursor: cursorFor(issue.createdSeq) }));
      if (field.name === 'pageInfo') return { __type: 'PageInfo', info: obj.pageInfo } as GqlObject;
      if (field.name === 'totalCount') return obj.issues.length;
      break;
    case 'PullRequestConnection':
      if (field.name === 'nodes') return obj.prs.map((pr) => ({ __type: 'PullRequest', repo: obj.repo, pr } as GqlObject));
      if (field.name === 'edges') return obj.prs.map((pr) => ({ node: { __type: 'PullRequest', repo: obj.repo, pr } as GqlObject, cursor: cursorFor(pr.createdSeq) }));
      if (field.name === 'pageInfo') return { __type: 'PageInfo', info: obj.pageInfo } as GqlObject;
      if (field.name === 'totalCount') return obj.prs.length;
      break;
    case 'PullRequestFileConnection':
      if (field.name === 'nodes') return obj.files.map((file) => file);
      if (field.name === 'edges') return obj.files.map((file) => ({ node: file, cursor: cursorFor(0) }));
      if (field.name === 'pageInfo') {
        return { __type: 'PageInfo', info: { hasNextPage: false, hasPreviousPage: false, startCursor: null, endCursor: null } } as GqlObject;
      }
      if (field.name === 'totalCount') return obj.files.length;
      break;
    case 'PageInfo':
      if (field.name === 'hasNextPage') return obj.info.hasNextPage;
      if (field.name === 'hasPreviousPage') return obj.info.hasPreviousPage;
      if (field.name === 'startCursor') return obj.info.startCursor;
      if (field.name === 'endCursor') return obj.info.endCursor;
      break;
    case 'Ref':
      if (field.name === 'name') return obj.name;
      if (field.name === 'target') return null;
      break;
    case 'Type': {
      const name = obj.name;
      if (field.name === 'name') return name;
      if (field.name === 'kind') return SCHEMA_ENUMS[name] !== undefined ? 'ENUM' : 'OBJECT';
      if (field.name === 'description') return null;
      if (field.name === 'fields') {
        const fields = SCHEMA_FIELDS[name];
        if (fields === undefined) return null;
        return fields.map((fieldName) => ({ name: fieldName, description: null }));
      }
      if (field.name === 'enumValues') {
        const values = SCHEMA_ENUMS[name];
        if (values === undefined) return null;
        return values.map((value) => ({ name: value, description: null }));
      }
      break;
    }
    case 'Comment':
      if (field.name === 'id') return obj.id;
      if (field.name === 'body') return obj.body;
      if (field.name === 'author') return { __type: 'User', login: VIEWER.login } as GqlObject;
      if (field.name === 'createdAt') return FIXED_ISO;
      break;
    case 'CommentEdge':
      if (field.name === 'node') return { __type: 'Comment', id: obj.id, body: obj.body } as GqlObject;
      if (field.name === 'cursor') return cursorFor(0);
      break;
    case 'CreateIssuePayload':
      if (field.name === 'issue') return { __type: 'Issue', repo: obj.repo, issue: obj.issue } as GqlObject;
      break;
    case 'CloseIssuePayload':
      if (field.name === 'issue') return { __type: 'Issue', repo: obj.repo, issue: obj.issue } as GqlObject;
      break;
    case 'CreatePullRequestPayload':
      if (field.name === 'pullRequest') return { __type: 'PullRequest', repo: obj.repo, pr: obj.pr } as GqlObject;
      break;
    case 'MergePullRequestPayload':
      if (field.name === 'pullRequest') return { __type: 'PullRequest', repo: obj.repo, pr: obj.pr } as GqlObject;
      break;
    case 'AddCommentPayload':
      if (field.name === 'commentEdge') return { __type: 'CommentEdge', id: obj.id, body: obj.body } as GqlObject;
      break;
  }
  return undefined;
}

/** The GraphQL type name of a resolved object, for fragments and __typename. */
function typeNameOf(obj: GqlObject): string {
  switch (obj.__type) {
    case 'Viewer': return 'User';
    case 'User': return 'User';
    case 'Repository': return 'Repository';
    case 'Issue': return 'Issue';
    case 'PullRequest': return 'PullRequest';
    case 'IssueConnection': return 'IssueConnection';
    case 'PullRequestConnection': return 'PullRequestConnection';
    case 'PullRequestFileConnection': return 'PullRequestFileConnection';
    case 'PageInfo': return 'PageInfo';
    case 'Ref': return 'Ref';
    case 'Type': return '__Type';
    case 'Comment': return 'IssueComment';
    case 'CommentEdge': return 'IssueCommentEdge';
    default: return obj.__type;
  }
}

/** Resolve a selection set against an object, collecting field errors. */
function executeSelections(
  exec: Executor,
  obj: GqlObject,
  selections: SelectionNode[],
  variables: Record<string, unknown>,
): Record<string, unknown> {
  const out: Record<string, unknown> = {};
  const typeName = typeNameOf(obj);
  for (const selection of selections) {
    if (selection.kind === 'fragment') {
      if (selection.typeCondition !== null && selection.typeCondition !== typeName) continue;
      Object.assign(out, executeSelections(exec, obj, selection.selections, variables));
      continue;
    }
    if (selection.name === '__typename') {
      out[selection.alias] = typeName;
      continue;
    }
    const value = resolveField(exec, obj, selection, variables);
    if (value === undefined) {
      exec.errors.push({ message: `Cannot query field "${selection.name}" on type "${typeName}".` });
      out[selection.alias] = null;
      continue;
    }
    out[selection.alias] = resolveValue(exec, value, selection.selections, variables);
  }
  return out;
}

/** Recurse into a resolved value: scalars pass through, objects/lists resolve selections. */
function resolveValue(
  exec: Executor,
  value: unknown,
  selections: SelectionNode[] | null,
  variables: Record<string, unknown>,
): unknown {
  if (value === null || typeof value !== 'object') return value;
  if (Array.isArray(value)) {
    return value.map((entry) => resolveValue(exec, entry, selections, variables));
  }
  const obj = value as GqlObject;
  if (obj.__type === undefined) {
    // A plain record (a file node, a __Type field/enum entry, an edge): its
    // own keys are the fields.
    if (selections === null) return value;
    const out: Record<string, unknown> = {};
    for (const selection of selections) {
      if (selection.kind === 'fragment') continue;
      const record = value as Record<string, unknown>;
      const entry = record[selection.name];
      out[selection.alias] = entry === undefined ? null : resolveValue(exec, entry, selection.selections, variables);
    }
    return out;
  }
  if (selections === null) return null;
  return executeSelections(exec, obj, selections, variables);
}

function findRepoById(store: Store, id: string): Repo | undefined {
  const decoded = decodeGlobalId(id);
  if (decoded === null || decoded.kind !== 'repo') return undefined;
  return store.repos.get(decoded.key);
}

function findIssueById(store: Store, id: string): { repo: Repo; issue: Issue } | undefined {
  const decoded = decodeGlobalId(id);
  if (decoded === null || decoded.kind !== 'issue') return undefined;
  const hash = decoded.key.lastIndexOf('#');
  if (hash < 0) return undefined;
  const repo = store.repos.get(decoded.key.slice(0, hash));
  if (repo === undefined) return undefined;
  const issue = repo.issues.get(Number.parseInt(decoded.key.slice(hash + 1), 10));
  return issue === undefined ? undefined : { repo, issue };
}

function findPullById(store: Store, id: string): { repo: Repo; pr: PullRequest } | undefined {
  const decoded = decodeGlobalId(id);
  if (decoded === null || decoded.kind !== 'pull') return undefined;
  const hash = decoded.key.lastIndexOf('#');
  if (hash < 0) return undefined;
  const repo = store.repos.get(decoded.key.slice(0, hash));
  if (repo === undefined) return undefined;
  const pr = repo.pulls.get(Number.parseInt(decoded.key.slice(hash + 1), 10));
  return pr === undefined ? undefined : { repo, pr };
}

/** Execute one root field of a query or mutation. */
function executeRootField(
  exec: Executor,
  operation: OperationNode,
  field: FieldNode,
  variables: Record<string, unknown>,
): { data: Record<string, unknown>; handled: boolean } {
  const store = exec.store;
  const data: Record<string, unknown> = {};
  if (operation.operation === 'query') {
    if (field.name === 'viewer') {
      data[field.alias] = resolveValue(exec, { __type: 'Viewer', login: VIEWER.login } as GqlObject, field.selections, variables);
      return { data, handled: true };
    }
    if (field.name === 'repository') {
      const owner = argValue(field, 'owner', variables);
      const name = argValue(field, 'name', variables);
      const repo = typeof owner === 'string' && typeof name === 'string' ? store.getRepo(owner, name) : undefined;
      if (repo === undefined) {
        data[field.alias] = null;
      } else {
        data[field.alias] = resolveValue(exec, { __type: 'Repository', repo } as GqlObject, field.selections, variables);
      }
      return { data, handled: true };
    }
    if (field.name === 'node') {
      const id = argValue(field, 'id', variables);
      let obj: GqlObject | null = null;
      if (typeof id === 'string') {
        const repo = findRepoById(store, id);
        if (repo !== undefined) obj = { __type: 'Repository', repo };
        else {
          const found = findIssueById(store, id);
          if (found !== undefined) obj = { __type: 'Issue', repo: found.repo, issue: found.issue };
          else {
            const pull = findPullById(store, id);
            if (pull !== undefined) obj = { __type: 'PullRequest', repo: pull.repo, pr: pull.pr };
          }
        }
      }
      data[field.alias] = obj === null ? null : resolveValue(exec, obj, field.selections, variables);
      return { data, handled: true };
    }
    if (field.name === '__type') {
      const name = argValue(field, 'name', variables);
      const obj: GqlObject = { __type: 'Type', name: typeof name === 'string' ? name : '' };
      const known = SCHEMA_FIELDS[obj.name] !== undefined || SCHEMA_ENUMS[obj.name] !== undefined;
      data[field.alias] = known ? resolveValue(exec, obj, field.selections, variables) : null;
      return { data, handled: true };
    }
    return { data, handled: false };
  }

  // Mutations.
  const input = argValue(field, 'input', variables);
  const inputRecord = input !== null && typeof input === 'object' && !Array.isArray(input) ? (input as Record<string, unknown>) : {};
  if (field.name === 'createIssue') {
    const repo = typeof inputRecord.repositoryId === 'string' ? findRepoById(store, inputRecord.repositoryId) : undefined;
    if (repo === undefined) {
      exec.errors.push({ message: 'createIssue: repository not found' });
      data[field.alias] = null;
      return { data, handled: true };
    }
    const number = repo.nextIssueNumber;
    repo.nextIssueNumber += 1;
    const issue: Issue = {
      id: number,
      number,
      title: typeof inputRecord.title === 'string' ? inputRecord.title : '',
      body: typeof inputRecord.body === 'string' ? inputRecord.body : '',
      state: 'open',
      labels: [],
      createdSeq: repo.nextSeq(),
      comments: [],
      rev: 0,
      jsonCache: null,
    };
    repo.issues.set(number, issue);
    const payload: GqlObject = { __type: 'CreateIssuePayload', repo, issue };
    data[field.alias] = resolveValue(exec, payload, field.selections, variables);
    return { data, handled: true };
  }
  if (field.name === 'closeIssue') {
    const found = typeof inputRecord.issueId === 'string' ? findIssueById(store, inputRecord.issueId) : undefined;
    if (found === undefined) {
      exec.errors.push({ message: 'closeIssue: issue not found' });
      data[field.alias] = null;
      return { data, handled: true };
    }
    found.issue.state = 'closed';
    found.issue.rev += 1;
    found.issue.jsonCache = null;
    const payload: GqlObject = { __type: 'CloseIssuePayload', repo: found.repo, issue: found.issue };
    data[field.alias] = resolveValue(exec, payload, field.selections, variables);
    return { data, handled: true };
  }
  if (field.name === 'createPullRequest') {
    const repo = typeof inputRecord.repositoryId === 'string' ? findRepoById(store, inputRecord.repositoryId) : undefined;
    const headRef = typeof inputRecord.headRefName === 'string' ? inputRecord.headRefName : '';
    const baseRef = typeof inputRecord.baseRefName === 'string' ? inputRecord.baseRefName : '';
    const headSha = repo?.branchTip(headRef);
    const baseSha = repo?.branchTip(baseRef);
    if (repo === undefined || headSha === undefined || baseSha === undefined) {
      exec.errors.push({ message: 'createPullRequest: repository or branch not found' });
      data[field.alias] = null;
      return { data, handled: true };
    }
    const number = repo.nextPullNumber;
    repo.nextPullNumber += 1;
    const pr: PullRequest = {
      id: number,
      number,
      title: typeof inputRecord.title === 'string' ? inputRecord.title : '',
      body: typeof inputRecord.body === 'string' ? inputRecord.body : '',
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
    const payload: GqlObject = { __type: 'CreatePullRequestPayload', repo, pr };
    data[field.alias] = resolveValue(exec, payload, field.selections, variables);
    return { data, handled: true };
  }
  if (field.name === 'mergePullRequest') {
    const found = typeof inputRecord.pullRequestId === 'string' ? findPullById(store, inputRecord.pullRequestId) : undefined;
    if (found === undefined) {
      exec.errors.push({ message: 'mergePullRequest: pull request not found' });
      data[field.alias] = null;
      return { data, handled: true };
    }
    if (found.pr.state !== 'open') {
      exec.errors.push({ message: 'mergePullRequest: pull request is not open' });
      data[field.alias] = null;
      return { data, handled: true };
    }
    const result = squashMerge(found.repo, found.pr);
    if (!result.ok) {
      exec.errors.push({ message: 'mergePullRequest: merge conflict' });
      data[field.alias] = null;
      return { data, handled: true };
    }
    const payload: GqlObject = { __type: 'MergePullRequestPayload', repo: found.repo, pr: found.pr };
    data[field.alias] = resolveValue(exec, payload, field.selections, variables);
    return { data, handled: true };
  }
  if (field.name === 'addComment') {
    const found = typeof inputRecord.subjectId === 'string' ? findIssueById(store, inputRecord.subjectId) : undefined;
    if (found === undefined) {
      exec.errors.push({ message: 'addComment: subject not found' });
      data[field.alias] = null;
      return { data, handled: true };
    }
    const comment = { id: found.repo.nextCommentId, body: typeof inputRecord.body === 'string' ? inputRecord.body : '' };
    found.repo.nextCommentId += 1;
    found.issue.comments.push(comment);
    found.issue.rev += 1;
    found.issue.jsonCache = null;
    const commentId = globalId('comment', `${found.repo.fullName}#${found.issue.number}#${comment.id}`);
    const payload: GqlObject = { __type: 'AddCommentPayload', id: commentId, body: comment.body };
    data[field.alias] = resolveValue(exec, payload, field.selections, variables);
    return { data, handled: true };
  }
  return { data, handled: false };
}

function executeOperation(store: Store, operation: OperationNode, variables: Record<string, unknown>): Record<string, unknown> {
  const exec: Executor = { store, errors: [] };
  const data: Record<string, unknown> = {};
  for (const selection of operation.selections) {
    if (selection.kind === 'fragment') continue;
    const { data: fieldData, handled } = executeRootField(exec, operation, selection, variables);
    if (!handled) {
      exec.errors.push({ message: `Cannot query field "${selection.name}" on type "${operation.operation === 'query' ? 'Query' : 'Mutation'}".` });
      data[selection.alias] = null;
      continue;
    }
    Object.assign(data, fieldData);
  }
  const out: Record<string, unknown> = { data };
  if (exec.errors.length > 0) out.errors = exec.errors;
  return out;
}

/** Handle one GraphQL request. Returns true when it answered. */
export async function handleGraphQL(store: Store, req: IncomingMessage, res: ServerResponse): Promise<boolean> {
  if (req.method !== 'POST') return false;
  const authorization = req.headers.authorization ?? '';
  if (!authorization.startsWith('Bearer ') || authorization.slice('Bearer '.length).trim() === '') {
    sendError(store, res, 401, 'Requires authentication');
    return true;
  }
  const bodyBuffer = await readBody(req);
  let body: unknown;
  try {
    body = JSON.parse(bodyBuffer.toString('utf8')) as unknown;
  } catch {
    sendJson(store, res, 400, { errors: [{ message: 'Request body is not valid JSON' }] });
    return true;
  }
  const items = Array.isArray(body) ? body : [body];
  const results: Array<Record<string, unknown>> = [];
  for (const item of items) {
    const query = item !== null && typeof item === 'object' && !Array.isArray(item) ? (item as Record<string, unknown>).query : undefined;
    const variables = item !== null && typeof item === 'object' && !Array.isArray(item) ? (item as Record<string, unknown>).variables : undefined;
    if (typeof query !== 'string' || query.trim() === '') {
      results.push({ errors: [{ message: 'Must provide query string.' }] });
      continue;
    }
    let operations: OperationNode[];
    try {
      operations = new Parser(query).parseDocument();
    } catch (err) {
      results.push({ errors: [{ message: err instanceof Error ? err.message : String(err) }] });
      continue;
    }
    const operation = operations[0];
    if (operation === undefined) {
      results.push({ errors: [{ message: 'Must provide an operation.' }] });
      continue;
    }
    const vars: Record<string, unknown> = {};
    for (const variable of operation.variables) {
      if (variables !== null && typeof variables === 'object' && !Array.isArray(variables) && variable.name in (variables as Record<string, unknown>)) {
        vars[variable.name] = (variables as Record<string, unknown>)[variable.name];
      } else if (variable.defaultValue !== null) {
        vars[variable.name] = valueFromNode(variable.defaultValue, {});
      }
    }
    results.push(executeOperation(store, operation, vars));
  }
  sendJson(store, res, 200, Array.isArray(body) ? results : results[0]);
  return true;
}
