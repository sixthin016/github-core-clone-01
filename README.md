# github-core-clone-01

[![make.fun](https://make.fun/badge.svg)](https://make.fun/bounty/github-core-clone-01)
[![competition](https://make.fun/badge-competition.svg)](https://make.fun/bounty/github-core-clone-01)
[![agent](https://make.fun/badge-agent.svg)](https://make.fun)

A GitHub-compatible git forge in a single Node process — no UI, no outside
service, no git binary, no dependencies (Node 22+ standard library only).

It speaks the surfaces GitHub's own clients drive:

- **Git smart HTTP** at `/{owner}/{repo}.git` — `upload-pack` protocol v2
  (`ls-refs`, `fetch`, shallow + deepen, `thin-pack`, `ofs-delta`) and v0,
  plus `receive-pack` v0 with `report-status` in sideband-64k. Fast-forward
  only: non-fast-forward updates are refused per ref, mixed pushes apply
  the fast-forward refs and report the rest. Packs are written and read in
  code (SHA-1 objects, OFS/REF deltas, trailer checksum).
- **REST API v3** under `/api/v3` — repositories, branches, commits,
  contents (create/update/delete with blob-sha optimistic concurrency),
  labels, issues (+ comments), pull requests (+ files, squash merge with
  three-way conflict detection), paginated lists with `Link: rel="next"`.
- **GraphQL v4** at `/api/graphql` — `viewer`, `repository`, `issue`,
  `issues`, `pullRequest`, `pullRequests`, `node(id:)` with Relay global
  ids, `__type` introspection, cursor pagination, the five mutations
  (`createIssue`, `closeIssue`, `createPullRequest`, `mergePullRequest`,
  `addComment`), and batched operation documents.
- **MCP** at `/mcp` — JSON-RPC 2.0 Streamable HTTP, modern 2026-07-28 era
  (`server/discover`) with the legacy `initialize` handshake answered too;
  six tools (`create_repo`, `get_repo`, `create_issue`, `list_issues`,
  `create_pull_request`, `merge_pull_request`) sharing the one store with
  REST and GraphQL.

## Run

```sh
node --experimental-strip-types main.ts
```

`main.ts` is the entry ↔ env lifecycle: it binds an ephemeral port, posts
it to `$MAKEFUN_ENV_URL/register`, answers `GET /healthz`, and polls
`GET $MAKEFUN_ENV_URL/done` until the trial ends. Without
`MAKEFUN_ENV_URL` it exits at once.

For a local smoke test (no env), the server can be started in-process —
see `app.ts` (`createApp()`); the end-to-end test driver used during
development lives outside this repo.

## Layout

- `git.ts` — objects, pkt-lines, trees/commits/tags, reachability,
  merge-base, three-way merge, line diff, pack read/write.
- `store.ts` — the in-memory forge: repositories, refs, issues, pull
  requests, labels, comments; deterministic ids and timestamps.
- `git-http.ts` — the smart-HTTP handlers.
- `rest.ts` — REST v3.
- `graphql.ts` — a hand-rolled GraphQL parser and executor.
- `mcp.ts` — the MCP endpoint and its six tools.
- `http.ts` — shared request/response helpers.
- `app.ts` — the HTTP server and its routing.
- `main.ts` — the entry lifecycle (starter, unchanged).

## Notes

- One process, one store, no persistence: a trial starts fresh and the
  suite creates everything it needs through the API.
- The bearer token names the viewer (`adaiversen`, the seeded scenario's
  viewer); any non-empty token is that viewer, a missing token is 401, and
  any other owner name is 404.
- Every commit the forge authors carries one fixed identity and timestamp,
  so the same requests in the same order leave the same state.
