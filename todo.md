# How it works: Hetzner runners, controller in GCP

Target design for this repo: the `testflows.github.hetzner.runners` service
("the controller") runs as one pod in GKE staging, deployed by Arthur. It watches
one GitHub repository for queued jobs and creates ephemeral Hetzner Cloud VMs
that register themselves as GitHub Actions runners, run exactly one job, and
power off.

Upstream source: <https://github.com/testflows/TestFlows-GitHub-Hetzner-Runners>
(all references below are paths inside that repo).

```
GitHub API  <---- long poll ----  controller pod (GKE staging, 1 replica)
    |                                    |
    |  job queued                        |  Hetzner Cloud API: create/delete VM
    v                                    v
  queue                            Hetzner VM (ephemeral runner)
    |                                    |
    +--------- claims job <--------------+  long poll (no inbound needed)
```

## 1. What actually runs

A single Python process with a thread pool (`--workers`, default 10) running
three supervised loops:

| Loop | File | Job |
|---|---|---|
| scale up | `runners/scale_up.py` | find queued jobs, create VMs, configure runner over SSH |
| scale down | `runners/scale_down.py` | reap powered-off VMs, detect zombies/stolen runners, recycle |
| api watch | `runners/api_watch.py` | keep GitHub API rate-limit/caching healthy |

Started from `main()` in `runners/bin/github-hetzner-runners` (~line 1761):
log in to Hetzner, start Prometheus + dashboard servers, log in to GitHub, resolve
default image/location/server type/SSH key, then submit the three loops to the pool.

## 2. Scale-up loop (default every 60s)

1. `GET /repos/{repo}/actions/runs?status=queued` (v4 API via PyGithub) plus
   `status=in_progress`. The in-progress list exists only for stolen-runner
   detection, not for creating VMs.
2. Filter jobs whose `runs-on` labels contain `self-hosted` (`with_label`).
3. For each unmatched job: resolve VM shape from the job's own labels
   (`type-cpx22` → server type, `in-nbg1` → location, etc. — table below),
   pick a free name, and create the server
   (`client.servers.create(...)`, `scale_up.py` ~line 800).
4. Wait for SSH (`max_server_ready_time`, 180s).
5. `POST /repos/{repo}/actions/runners/registration-token` — this is the only
   GitHub write before the runner registers itself.
6. SSH in as `root@<public IPv4>` and run three things with env vars:
   - volume prep (`e2fsck`, `resize2fs`, `mount`, bind-mount apt caches),
   - `scripts/setup.sh` as root: creates `ubuntu`, docker group, fail2ban,
   - `scripts/startup-<arch>.sh` as `ubuntu` with `GITHUB_RUNNER_TOKEN`,
     `GITHUB_RUNNER_LABELS`, `SERVER_NAME`, `CACHE_DIR`.
7. The startup script downloads the actions runner (pinned version + sha256),
   runs `config.sh --ephemeral --replace --labels ...`, then
   `screen -d -m bash -c './run.sh; sudo poweroff'`.
8. Job runs, runner exits because it is ephemeral, VM powers off.

There is no cloud-init. Everything is pushed over SSH by the controller, which
is why the controller needs the **private** key (see §5).

## 3. Scale-down loop (default every 60s)

1. `GET /servers?label_selector=github-hetzner-runner=active` — this label query
   *is* the inventory. Nothing is remembered locally.
2. Powered off longer than `max_powered_off_time` (60s) → delete (unless recycle).
3. Running but no matching job, runner offline past `max_unused_runner_time`
   (180s) → treat as zombie, delete.
4. Running, runner never registered within `max_runner_registration_time` (180s)
   → delete.
5. Recycle (default on): power off, keep, then either rebuild from image
   (`recycle.sh`) or reuse as-is (`--recycle-without-rebuild`). Recycled servers
   are reused for the next job, which is what makes hourly billing tolerable.
6. Enforces `max_runners`, optional `max_runners_for_label`,
   `max_runners_in_workflow_run`.

## 4. Persistent state: none

Verified by reading the source. No database, no sqlite, no state file.

| Thing | Where it lives | Survives restart? |
|---|---|---|
| which VMs exist | Hetzner labels `github-hetzner-runner=active` | yes, read from API |
| which runner is which | GitHub self-hosted runners API | yes, read from API |
| recycle timestamps | Hetzner label `github-hetzner-recycle-timestamp` | yes |
| SSH key association | Hetzner label `github-hetzner-runner-ssh-key` | yes |
| dashboard graphs | in-process dict, 15-min ring buffer | **no** |
| rotated log (`/tmp/github-hetzner-runners.log`, 50MB × 10) | pod filesystem | no |

Consequences for the design:

- Restart is safe. A new pod re-reads both APIs and resumes; orphaned VMs get
  reaped by rule 2/3 above.
- **Exactly one replica.** Two controllers would each try to create a VM for the
  same queued job and race on deletes. Use `replicas: 1` with a `Recreate`
  strategy (no surge, no rolling update overlap), no HPA.
- No PVC needed. If you want dashboard history or logs to survive, mount emptyDir
  (logs only) or point `logger_config.handlers.rotating_logfile.filename` elsewhere.

## 5. Auth and secrets

| Secret | Used by | Notes |
|---|---|---|
| `GITHUB_TOKEN` | controller | **classic** PAT, fine-grained unsupported; needs admin on the repo to mint runner registration tokens. Expires — rotation is a manual runbook step. |
| `GITHUB_REPOSITORY` | controller | `owner/repo`. One repo per controller. Group/org runners are explicitly unsupported. |
| `HETZNER_TOKEN` | controller | Hetzner Cloud project API token. One project per repo (upstream requirement). |
| SSH keypair | controller | `--ssh-key <path/to/key.pub>` registers/attaches the key; the **private** key must be at `$HOME/.ssh/id_rsa` because `ssh_command` passes no `-i` and logs in as `root@ip`. Both halves go in GSM as one secret or two (`..._SSH_PUBLIC_KEY` / `..._SSH_PRIVATE_KEY`). |
| runner→GCP credential | runner VM | not needed by upstream; required if CI touches Artifact Registry/GCS. See §8. |

Config surface, in order of precedence: CLI flag → env var → YAML config
(`--config`, or `$GITHUB_HETZNER_RUNNERS_CONFIG`, or `~/.github-hetzner-runners/config.yaml`).

Only four env vars are read directly: `GITHUB_TOKEN`, `GITHUB_REPOSITORY`,
`HETZNER_TOKEN`, `GITHUB_HETZNER_RUNNERS_CONFIG`. Everything else is a flag or a
config-file key. The YAML config does interpolate `${ANY_VAR}` anywhere
(`config/config.py:33-47`), so a config file + env is the practical container
path: put non-secret knobs in the YAML, put `${GITHUB_TOKEN}` etc. in it and feed
the values as env from GSM/ExternalSecrets.

## 6. Labels: the only interface between workflow and VM

`runs-on: [self-hosted, type-cpx22, in-nbg1]`. Everything is a label, optional,
falling back to config defaults:

| Label | Effect |
|---|---|
| `self-hosted` | required to match (`with_label`) |
| `type-<hetzner type>` | server type, e.g. `type-cpx22`, `type-cx23` |
| `in-<location>` | location, e.g. `in-nbg1` (code uses prefix `in-`, not `location-`) |
| `image-<arch>-<type>-<name>` | image, e.g. `image-x86-system-ubuntu-24.04` |
| `volume-<name>-<size>` | attach/create a cache volume |
| `setup-<name>` / `startup-<name>` / `recycle-<name>` | pick alternative script from the scripts dir |
| `net-ipv6` | IPv6-only server |
| `meta-<name>` + `--meta-label` | expand one short label into many |

`--label-prefix` namespaces all of the above (useful if two controller
instances share a Hetzner project — though upstream says don't).

## 7. Ports

| Port | What | Default bind |
|---|---|---|
| 9090 | Prometheus `/metrics` | `127.0.0.1` |
| 8090 | Streamlit dashboard | `127.0.0.1` |

In a pod you must pass `--metrics-host 0.0.0.0 --dashboard-host 0.0.0.0`, or
nothing can scrape it. There is no `/health` route; if you want probes, use
`tcpSocket: 9090`. No inbound traffic from Hetzner is ever required — the
controller only makes outbound calls and outbound SSH to the VMs' public IPs.

## 8. The runner→GCP identity problem

Today's ARC runners get Workload Identity for free. Hetzner VMs get nothing, so
any job that pushes to Artifact Registry, reads GSM, or hits Firestore needs a
credential placed on the VM. Two options:

- **WIF (preferred).** Hetzner VMs have no GCP metadata server, so
  `google-github-actions/auth` needs `workload_identity_provider` + an OIDC token
  source. Practical shape: GitHub OIDC (`ACTIONS_ID_TOKEN_REQUEST_URL`) is
  available inside the job, so the job itself can exchange a GitHub OIDC token for
  a GCP access token — no key on the VM. Requires a WIF pool trusting
  `token.actions.githubusercontent.com` with attribute conditions on
  `repository`/`ref`. This is the same trust model other non-GCP CI has.
- **Service-account key.** Mount a JSON key into the VM via a custom setup/startup
  script (`scripts/setup-<name>.sh`), write it to disk, set
  `GOOGLE_APPLICATION_CREDENTIALS`. Fast, works, long-lived secret on every
  ephemeral VM.

Either way this lives in your own startup script, not upstream code.

## 9. Deploying the controller with Arthur (GKE staging)

Arthur components are Dockerfile-built workloads, so this is one component of
type long-running deployment. Sketch (mirrors `.arthur/toadsworth.staging.yaml`):

```yaml
apiVersion: enad.dev/2026-07
kind: App
environment: staging
metadata:
  name: github-hetzner-runners
  namespace: github-hetzner-runners
artifact_registry: github-action-runners
publish:
  chart: enad-workload
  argocd:
    project: default
    automated: true
components:
- name: controller
  dockerfile: Dockerfile
  image: github-hetzner-runners
  replicas: 1
  spot: false          # the only controller: don't take reclaims mid-create
  env:
    GITHUB_REPOSITORY: enadhq/<repo>
  externalSecrets:
    GITHUB_TOKEN: apps-<repo>-GITHUB_TOKEN
    HETZNER_TOKEN: apps-<repo>-HETZNER_TOKEN
    SSH_PRIVATE_KEY: apps-<repo>-HETZNER_SSH_PRIVATE_KEY
```

Container requirements:

- Start command: `github-hetzner-runners --metrics-host 0.0.0.0 --dashboard-host 0.0.0.0 --ssh-key /etc/runners/ssh/id_ed25519.pub [flags]`.
- Writable `/tmp` (rotated log) and a private key at `$HOME/.ssh/id_rsa` with
  `0600`. Easiest: run as root (the image already ssh's as root).
- `strategy: Recreate` and no HPA — Arthur sets these for `replicas: 1`, verify
  before trusting it.
- Non-`root` user needs `HOME` set and the key dropped at `$HOME/.ssh/id_rsa`.

## 10. Hetzner-side prerequisites

1. New Hetzner Cloud project per repo (upstream limitation, and it gives you
   per-repo cost separation for free).
2. API token, read/write on servers, volumes, SSH keys, images.
3. SSH keypair; the public half goes to GSM and gets auto-registered in the
   Hetzner project on first run (`check_ssh_key` creates it if missing).
4. Firewall: SSH 22 from the cluster's egress IPs only, everything else outbound
   open. The VMs are public-IP machines running a GitHub runner; leaving 22 open
   to the world with a key-only login is survivable but noisy.
5. A GKE egress IP that is stable (NAT/Cloud NAT) if you want that allowlist.
6. Optionally a snapshot image with Docker/Go/Node preinstalled — `setup-*.sh`
   runs `apt-get update` on every VM otherwise.

## 11. Knobs worth setting on day one

| Flag | Default | Why change |
|---|---|---|
| `--max-runners` | 10 | your cost ceiling; set explicitly |
| `--recycle` | on | keep on, it is the cost story |
| `--recycle-grace-period` | 1200 | minimum seconds in recycled state before end-of-life deletion |
| `--default-type` | cx23 | pin to what you actually want (`cpx22` for x64) |
| `--default-location` | none | pin, e.g. `nbg1` |
| `--end-of-life` | 50 | minutes of VM age after which a recyclable server is deleted |
| `--metrics-host` / `--dashboard-host` | 127.0.0.1 | must be 0.0.0.0 in a pod |
| `--workers` | 10 | concurrency of server creation |

## 12. Build order (what to do, in order)

1. Hetzner: create project, token, keypair, firewall. Record the token.
2. GitHub: classic PAT with repo admin on the target repository. Record it.
3. GSM: `apps-<repo>-GITHUB_TOKEN`, `apps-<repo>-HETZNER_TOKEN`,
   `apps-<repo>-HETZNER_SSH_PRIVATE_KEY`, `apps-<repo>-HETZNER_SSH_PUBLIC_KEY`.
4. Write `Dockerfile`: `python:3.12-slim`, `pip install testflows.github.hetzner.runners`,
   `openssh-client` (it shells out to `ssh`), entrypoint the console script.
5. Write `config/config.yaml` (labels, type, location, max runners, recycle) and
   ship it via a ConfigMap, or pass flags. Keep tokens as `${ENV}` in the YAML.
6. Write `.arthur/<name>.staging.yaml` with one component as in §9, plus the SSH
   key and config mounts.
7. `arthur gke validate --dir .arthur/<name>.staging` then `arthur gke publish`.
8. Smoke test: a workflow with `runs-on: [self-hosted, type-cpx22]`.

## 13. Verifying it works

```fish
kubectl logs -n github-hetzner-runners deploy/controller -f
kubectl port-forward -n github-hetzner-runners deploy/controller 8090:8090   # dashboard
kubectl port-forward -n github-hetzner-runners deploy/controller 9090:9090   # metrics
```

In-pod equivalents: `github-hetzner-runners list` (runners + servers), `... delete`
(tear down all runners and servers), `... estimate run <id>` (cost), `... cloud ...`
and `... service ...` only apply to its self-install path and are irrelevant here.

Healthy looks like: log shows `Looking for queued jobs` every 60s, a
`Create server ...` on queue, the runner appears online in GitHub, job runs, VM
goes `off`, then either `deleting` or `recycled`.

## 14. Failure modes to watch

| Symptom | Cause |
|---|---|
| VM created, runner never online | SSH failed or startup script died: check the pod's `ssh` output, runner version pin, and that port 22 is reachable |
| VM created and immediately deleted | `max_runner_registration_time` (180s) exceeded — slow image, no network |
| Queued jobs but no VMs | `with_label` mismatch, PAT lacks admin (registration token 403), or `max_runners` hit |
| VMs piling up powered off | scale-down wedged or restart storm; `list` then `delete` |
| Two VMs per job | two controllers running (stray `helm` release, old deployment) — the reason for `replicas: 1` |
| PAT expires | nothing scales up; rotate secret, restart |

Note the delete-on-restart-storm risk: `max_powered_off_time` is 60s, so a
controller that keeps crash-looping can delete and recreate VMs faster than a
queued job is claimed. Keep the pod stable (no spot, no aggressive probes).

## 15. What this replaces in the current repo

Delete, don't adapt: `.arthur/github-action.staging.yaml`,
`.arthur/github-action-runners.staging.yaml`, `.arthur/values/*`,
`.argocd/*`, `terraform/staging` (it only holds provider binaries and module
copies anyway), the `gha-runner-scale-set` values, and the GitHub App secrets in
GSM (`apps-github-action-runners-GITHUB_APP_*`) on the way out.

Keep: `.github/workflows/release.yaml` shape (build+push then publish) if you
want tagged releases of the controller image, and `labeler`/`release-drafter`.

## 16. Risks you are accepting

1. Repo-scoped only. Org-wide `runs-on: enad-action-runners` disappears; each
   repo needs its own app, Hetzner project, PAT, and secret set.
2. PAT instead of GitHub App: rotation burden and a broader-scoped long-lived
   credential than an installed App.
3. Runner identity in GCP is now a per-VM problem (§8).
4. Public-IP VMs with a GitHub runner and root SSH key-auth. Add the firewall.
5
