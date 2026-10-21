# Running rfind in containers

rfindd was designed for the host of a Linux box. Three container
deployment modes make sense; a fourth, inviting one does not. Pick
the right one for your use case and skip the siren song of the
fourth.

| Mode | Index source | Live events | When to use |
|---|---|---|---|
| 1. Host-indexer sidecar | host's `/` | ✅ fanotify | you want host-wide search but prefer to keep rfindd out of the host namespace |
| 2. Per-container indexer | container's root fs | ⚠️ inotify only | an app container that wants fast local search over its own files |
| 3. Read-only query container | shared index volume | — | you want `rfind` CLI on a slim image, with the daemon running elsewhere |
| 4. Unprivileged + live events | — | — | **not supported** — fanotify refuses outside the init user namespace |

The image is published at `ghcr.io/yanivsaar/rfind` (Alpine musl,
~15 MiB). Build locally with `podman build -t rfind
-f packaging/container/Containerfile .`.

## Mode 1 — host-indexer sidecar

The daemon runs in a container but monitors and indexes the host's
filesystem. Needs `CAP_SYS_ADMIN` in the init user namespace, host
PID namespace, and the host's `/` mounted at `/` (or at `/host` if
you prefer path cleanliness).

### Podman

```sh
podman run -d \
    --name rfindd \
    --cap-add=SYS_ADMIN,DAC_READ_SEARCH \
    --security-opt=unmask=/proc/* \
    --security-opt=label=disable \
    --pid=host \
    --ipc=host \
    -v /:/:ro,rslave \
    -v rfindd-index:/var/lib/rfindd \
    -v rfindd-run:/run/rfindd \
    -v /etc/rfindd:/etc/rfindd:ro \
    ghcr.io/yanivsaar/rfind:latest
```

Flag by flag:

- `--cap-add=SYS_ADMIN,DAC_READ_SEARCH` — fanotify FS marks and
  `open_by_handle_at` do not work without these; the systemd unit
  requires the same.
- `--security-opt=unmask=/proc/*` — Podman's default masking hides
  `/proc/kcore` and other paths rfindd wants to statx when scanning
  under `/`. Remove the mask so stat succeeds; rfindd won't read
  these files, only stat them, and the `*` set it unmasks is a
  deliberate choice: don't go wider.
- `--security-opt=label=disable` — SELinux on the host will label-
  deny the daemon's reads otherwise. Load the host-side SELinux
  module (`packaging/selinux/rfindd.te`) instead if you prefer to
  keep enforcement.
- `--pid=host` — fanotify event resolution via `/proc/self/fd/N`
  needs the host PID view when the event comes from a host process.
- `--ipc=host` — Needed only if you want rfindd to share journald
  with the host; omit otherwise.
- `-v /:/:ro,rslave` — host root read-only. `rslave` makes new
  host mounts visible to the container (`watch_new_mounts=true`
  then works); without it, USB mounts appear only to the host.
- `rfindd-index` + `rfindd-run` named volumes — index + runtime
  survive container restarts.
- `-v /etc/rfindd:/etc/rfindd:ro` — host config bind-mounted
  read-only. Edit the config on the host; `podman kill -s HUP
  rfindd` reloads it.

### Docker

Same flags apply; replace `podman` with `docker` and drop
`label=disable` (Docker doesn't do SELinux labeling by default).

### What you give up vs. a host install

- No systemd supervision — restart on failure is on the container
  runtime (`--restart=on-failure`).
- No `rfind uninstall` story — you remove the container and its
  volumes.
- Signal reload (`SIGHUP`) still works via `podman kill -s HUP`.

## Mode 2 — per-container indexer

The daemon runs inside an app container and indexes that container's
own filesystem. fanotify will fail (no `CAP_SYS_ADMIN` in init
userns under rootless Podman / Kubernetes); the daemon falls back to
inotify. Appropriate when the app wants `rfind name foo` to be fast
over its own files.

```sh
# In your app Containerfile
RUN apk add --no-cache rfind    # if you've published Alpine packages,
# OR
COPY --from=ghcr.io/yanivsaar/rfind:latest /usr/local/bin/rfind* /usr/local/bin/
```

Start the daemon as a sidecar process in the container:

```sh
rfindd --pidfile=/run/rfindd/rfindd.pid &
exec your-app
```

Monitor startup detects the container runtime and logs:

```
INFO Runtime: podman | CAP_SYS_ADMIN=false CAP_DAC_READ_SEARCH=false userns=true
     | fanotify FS marks: skip (will use inotify)
INFO Skipping fanotify (runtime podman, caps insufficient) — using inotify
```

That's the correct behavior. Don't try to `--cap-add=SYS_ADMIN` an
app container to escape it — the capability is still bounded by the
user namespace.

## Mode 3 — read-only query container

The daemon runs on the host (or in Mode 1); you ship `rfind` alone
in a slim image and share the index volume read-only.

```sh
podman run --rm -it \
    -v rfindd-index:/var/lib/rfindd:ro \
    -v rfindd-run:/run/rfindd:ro \
    ghcr.io/yanivsaar/rfind:latest \
    rfind name '*.conf' /etc
```

Useful for CI pipelines that query an index built elsewhere.

## Mode 4 — unprivileged + live events (not supported)

fanotify filesystem marks require `CAP_SYS_ADMIN` **in the init user
namespace**. Running `--privileged` inside a rootless Podman still
isn't enough — the capability only applies within the namespace it
was granted in. If you need live events, you need Mode 1.

If you believe you've found a configuration where this works, please
file an issue — most reports we've seen were users confusing
"fanotify works" with "fanotify works correctly" (events are
delivered but with `/proc/self/fd/N` referring to the wrong file
system).

## Kubernetes

Mode 1 as a `DaemonSet` with `hostPID: true`,
`securityContext.capabilities.add: ["SYS_ADMIN", "DAC_READ_SEARCH"]`
and a hostPath volume at `/`. See the
[reference manifests](https://github.com/YanivSaar/rfind/blob/main/packaging/container/k8s/)
(coming in v1.9).

Pod Security Standards: this maps to `privileged` — if your cluster
enforces `baseline` or `restricted`, rfind as an in-cluster service
is not for you; run it on the host via systemd and query it over the
cluster's shared storage instead.
