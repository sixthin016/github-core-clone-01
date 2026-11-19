// busypoll.ts — v8 experiment: best-effort socket/process tuning via the
// busypoll.node N-API addon.
//
//   tuneSocket(socket)  setsockopt SO_BUSY_POLL 50 + SO_PREFER_BUSY_POLL 1
//                       on every accepted socket (best effort, silent fail)
//   tuneProcess()       opt-in only (env-gated): sched_setaffinity via
//                       GCC_TUNE_AFFINITY=<cpu>, sysctls via GCC_TUNE_SYSCTL=1
//
// NOT SUBMITTABLE as an entry. RULES.txt ("Runtime and sandbox") says:
// "there is no install step, no package install and no native addon".
// A .node file IS a native addon. The addon is therefore loaded inside
// try/catch and every failure is ignored, so the server behaves exactly
// like v7 when the addon is absent, the wrong ABI, or the calls are
// refused (EPERM/ENOPROTOOPT in the trial pod: uid 65534, ALL caps
// dropped, gVisor netstack does not implement SO_BUSY_POLL — see
// github-core-clone-01-v8-NOTES.md). Nothing here may throw.

import { createRequire } from 'node:module';
import type { Socket } from 'node:net';
import { writeFileSync } from 'node:fs';

interface BusyPollAddon {
  setBusyPoll(fd: number, usecs: number): number;
  setPreferBusyPoll(fd: number, onoff: number): number;
  getBusyPoll(fd: number): number;
  setAffinity(cpu: number): number;
}

let addon: BusyPollAddon | null = null;
try {
  const require = createRequire(import.meta.url);
  const loaded: unknown = require('./busypoll.node');
  if (loaded !== null && typeof loaded === 'object') {
    addon = loaded as BusyPollAddon;
  }
} catch {
  addon = null;
}

/** Best-effort per-socket tuning. Never throws. True when SO_BUSY_POLL stuck. */
export function tuneSocket(socket: Socket): boolean {
  if (addon === null) return false;
  try {
    const fd = (socket as unknown as { _handle?: { fd?: unknown } })._handle?.fd;
    if (typeof fd !== 'number' || fd < 0) return false;
    const r = addon.setBusyPoll(fd, 50);
    // Prefer flag needs CAP_NET_ADMIN even where SO_BUSY_POLL itself sticks
    // (measured: EPERM as uid 0 in this sandbox); failure is ignored.
    addon.setPreferBusyPoll(fd, 1);
    return r === 0;
  } catch {
    return false;
  }
}

/**
 * Best-effort process tuning. Opt-in only via env, never throws.
 * GCC_TUNE_AFFINITY=<cpu> pins this process to one core.
 * GCC_TUNE_SYSCTL=1 attempts /proc/sys/net/* writes (needs CAP_SYS_ADMIN;
 * silently fails in the pod). Default: both off.
 */
export function tuneProcess(): void {
  if (addon === null) return;
  try {
    const cpu = Number.parseInt(process.env.GCC_TUNE_AFFINITY ?? '', 10);
    if (Number.isFinite(cpu) && cpu >= 0) addon.setAffinity(cpu);
  } catch {
    // ignore
  }
  if (process.env.GCC_TUNE_SYSCTL !== '1') return;
  const sysctls: Array<[string, string]> = [
    ['/proc/sys/net/ipv4/tcp_congestion_control', 'bbr'],
    ['/proc/sys/net/ipv4/tcp_notsent_lowat', '16384'],
    ['/proc/sys/net/core/netdev_budget', '65535'],
    ['/proc/sys/net/core/busy_poll', '50'],
    ['/proc/sys/net/core/busy_read', '50'],
  ];
  for (const [path, value] of sysctls) {
    try {
      writeFileSync(path, value);
    } catch {
      // ignore: read-only / no caps in the pod
    }
  }
}
