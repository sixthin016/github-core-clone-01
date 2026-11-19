// busypoll.cc — v8 experiment (NOT submittable: RULES.txt line 80 forbids native
// addons). Best-effort socket tuning via raw N-API, no dependencies.
//
//   setBusyPoll(fd, usecs)       setsockopt SOL_SOCKET/SO_BUSY_POLL
//   setPreferBusyPoll(fd, onoff)  setsockopt SOL_SOCKET/SO_PREFER_BUSY_POLL
//   getBusyPoll(fd)               getsockopt SOL_SOCKET/SO_BUSY_POLL (diagnostics)
//   setAffinity(cpu)              sched_setaffinity(0, ...) for this process
//
// Every function returns 0 on success, or -errno on failure (never throws).
// The JS wrapper (busypoll.ts) loads this file inside try/catch and ignores
// failures, so the server runs exactly like v7 when the addon is absent,
// the wrong ABI, or the calls are refused (EPERM/ENOPROTOOPT in the trial pod).

#include <node_api.h>
#include <cerrno>
#include <cstring>
#include <sched.h>
#include <sys/socket.h>
#include <unistd.h>

#ifndef SO_BUSY_POLL
#define SO_BUSY_POLL 46
#endif
#ifndef SO_PREFER_BUSY_POLL
#define SO_PREFER_BUSY_POLL 69
#endif

namespace {

bool GetInt32(napi_env env, napi_value value, int32_t *out) {
  return napi_get_value_int32(env, value, out) == napi_ok;
}

napi_value ReturnInt(napi_env env, int value) {
  napi_value out = nullptr;
  napi_create_int32(env, value, &out);
  return out;
}

napi_value SetBusyPoll(napi_env env, napi_callback_info info) {
  size_t argc = 2;
  napi_value argv[2] = {nullptr, nullptr};
  napi_get_cb_info(env, info, &argc, argv, nullptr, nullptr);
  int32_t fd = -1;
  int32_t usecs = 50;
  if (argc < 1 || !GetInt32(env, argv[0], &fd)) return ReturnInt(env, -EINVAL);
  if (argc > 1) GetInt32(env, argv[1], &usecs);
  if (setsockopt(fd, SOL_SOCKET, SO_BUSY_POLL, &usecs, sizeof(usecs)) != 0) {
    return ReturnInt(env, -errno);
  }
  return ReturnInt(env, 0);
}

napi_value SetPreferBusyPoll(napi_env env, napi_callback_info info) {
  size_t argc = 2;
  napi_value argv[2] = {nullptr, nullptr};
  napi_get_cb_info(env, info, &argc, argv, nullptr, nullptr);
  int32_t fd = -1;
  int32_t onoff = 1;
  if (argc < 1 || !GetInt32(env, argv[0], &fd)) return ReturnInt(env, -EINVAL);
  if (argc > 1) GetInt32(env, argv[1], &onoff);
  if (setsockopt(fd, SOL_SOCKET, SO_PREFER_BUSY_POLL, &onoff, sizeof(onoff)) != 0) {
    return ReturnInt(env, -errno);
  }
  return ReturnInt(env, 0);
}

napi_value GetBusyPoll(napi_env env, napi_callback_info info) {
  size_t argc = 1;
  napi_value argv[1] = {nullptr};
  napi_get_cb_info(env, info, &argc, argv, nullptr, nullptr);
  int32_t fd = -1;
  if (argc < 1 || !GetInt32(env, argv[0], &fd)) return ReturnInt(env, -EINVAL);
  int value = 0;
  socklen_t len = sizeof(value);
  if (getsockopt(fd, SOL_SOCKET, SO_BUSY_POLL, &value, &len) != 0) {
    return ReturnInt(env, -errno);
  }
  return ReturnInt(env, value);
}

napi_value SetAffinity(napi_env env, napi_callback_info info) {
  size_t argc = 1;
  napi_value argv[1] = {nullptr};
  napi_get_cb_info(env, info, &argc, argv, nullptr, nullptr);
  int32_t cpu = -1;
  if (argc < 1 || !GetInt32(env, argv[0], &cpu)) return ReturnInt(env, -EINVAL);
  cpu_set_t set;
  CPU_ZERO(&set);
  CPU_SET(static_cast<size_t>(cpu), &set);
  if (sched_setaffinity(0, sizeof(set), &set) != 0) {
    return ReturnInt(env, -errno);
  }
  return ReturnInt(env, 0);
}

napi_value Init(napi_env env, napi_value exports) {
  napi_property_descriptor desc[] = {
      {"setBusyPoll", nullptr, SetBusyPoll, nullptr, nullptr, nullptr, napi_default, nullptr},
      {"setPreferBusyPoll", nullptr, SetPreferBusyPoll, nullptr, nullptr, nullptr, napi_default, nullptr},
      {"getBusyPoll", nullptr, GetBusyPoll, nullptr, nullptr, nullptr, napi_default, nullptr},
      {"setAffinity", nullptr, SetAffinity, nullptr, nullptr, nullptr, napi_default, nullptr},
  };
  napi_define_properties(env, exports, sizeof(desc) / sizeof(desc[0]), desc);
  return exports;
}

}  // namespace

NAPI_MODULE(NODE_GYP_MODULE_NAME, Init)
