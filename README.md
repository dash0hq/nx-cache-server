# Nx remote cache server

A small Rust/Axum service backed by S3. This fork retains the architecture and
Apache-2.0 license of [nxcite/nx-cache-server](https://github.com/nxcite/nx-cache-server).
See [LICENSE.txt](LICENSE.txt). Archives are opaque bytes, never unpacked on the server.

## Local development

Install rustup, Docker with Compose v2, Python 3, and the Node version in
`.node-version`. Rust 1.94.1 is pinned because the locked AWS SDK requires it.

```sh
make check                       # fmt, Clippy, unit and raw-TCP protocol tests
cp .env.example .env             # disposable local credentials only
make dev-up                      # pinned MinIO, persistent local Docker volume
make run                        # in another terminal
make test-s3                     # separate disposable MinIO + two servers
make test-nx                     # npm ci, native nx@23.1.1 + disposable MinIO
make dev-down                   # preserves dev data
make dev-clean                  # explicitly deletes this dev project's volume
```

Tests create unique Docker project names, S3 prefixes, local caches, and output
directories. They never clear a shared cache. Allow at least 2 GiB disk and 2 GiB
Docker memory for fixtures, plus Rust build space. MinIO community is unmaintained;
the image is an isolated test dependency, not a production recommendation.
`compose.yaml` pins the tested release and digest and binds only loopback.

The Nx fixture verifies actual S3 writes, exact output restoration from fresh
local caches without running the task, RO hit/miss, >8 MiB rejected writes,
outage failure, and bypass. Its execution marker is outside the hash inputs.
The fixed Nx 23.1.1 dependency tree currently has npm audit findings in
`brace-expansion` and `smol-toml`; use this fixture only with its trusted inputs.

## Configuration and limits

Flags are also available via `nx-cache-aws --help`. Required values are
`SERVICE_ACCESS_TOKEN` and `S3_BUCKET_NAME`. AWS credentials and region can use
the standard provider chains; explicitly configure both for local tests and set
`AWS_EC2_METADATA_DISABLED=true`. Supply both `AWS_ACCESS_KEY_ID` and
`AWS_SECRET_ACCESS_KEY`, optionally `AWS_SESSION_TOKEN`.

| Variable | Default | Meaning |
| --- | --- | --- |
| `READ_ONLY_ACCESS_TOKEN` | unset | Optional nonempty token, different from the RW token |
| `AWS_REGION` | provider chain | Use `us-east-1` for the fixture |
| `S3_ENDPOINT_URL` | AWS S3 | Custom HTTP/S endpoint; path-style addressing |
| `S3_PREFIX` | `nx-cache` | Server-owned workspace segment, 1–128 ASCII letters/digits/`-`/`_` |
| `S3_TIMEOUT` | `30` | Seconds per SDK operation, 1–300 |
| `PORT` / `BIND_ADDRESS` | `3000` / `0.0.0.0` | HTTP listener; `::` supports IPv6 |
| `MAX_UPLOAD_BYTES` | `268435456` | 256 MiB; configurable up to single-PUT 5 GiB |
| `MAX_UPLOADS` | `4` | Active writes and RO drains; no application wait queue |
| `UPLOAD_TIMEOUT_SECONDS` | `120` | Absolute inbound deadline, 1–3600 seconds |
| `SPOOL_DIRECTORY` | `/tmp/nx-cache-spool` | Private, dedicated directory; 0700 on Unix |
| `DEBUG` | false | App debug logs; SDK request dumps remain disabled |

Keys are `S3_PREFIX/hash`. The hash is one 1–128 byte ASCII segment with the
same character set. The new prefix intentionally does not read old unprefixed
objects; those become cold misses. Use a separate server/prefix for each workspace.
This is not a multi-tenant authorization service.

Uploads stream into secure temporary files. The server checks declared and actual
length and waits for complete S3 persistence before returning 200. It does not
hold whole artifacts in memory. Downloads stream directly from S3. There is no
measured sub-4-MB memory guarantee.

S3 `PutObject` uses `If-None-Match: *`, without a HEAD check. Only a 412
`PreconditionFailed` means collision. A 409 `ConditionalRequestConflict` retries
the complete file with the same condition, at most three total attempts and
150 ms total backoff. Other failures return 500. PUT SDK retries are disabled so
this bound is explicit. Persistence may finish even if the client disconnects;
an ambiguous lost response is safe to retry because objects are immutable.

The admitted spool payload bound is `MAX_UPLOADS × MAX_UPLOAD_BYTES`, 1 GiB by
default. Use a dedicated quota-limited volume for the hard physical disk limit;
filesystem overhead and briefly outstanding OS/SDK handles are outside that
payload accounting. Do not place the spool on a shared network filesystem.
The process locks its directory, reaps abandoned `upload-*` files on startup,
and removes request spools on success, rejection, or error. Detached upload work
holds capacity through cleanup when a caller disconnects. SIGTERM/SIGINT stops
accepting connections and waits for uploads. Allow inbound timeout plus three
S3 operation timeouts when configuring termination grace. A stalled local disk
can delay cleanup; an inbound timeout cannot cancel kernel filesystem I/O.

## Nx clients and failure behavior

```sh
export NX_NO_CLOUD=true
export NX_SELF_HOSTED_REMOTE_CACHE_SERVER="https://cache.example.com" # no trailing slash
export NX_SELF_HOSTED_REMOTE_CACHE_ACCESS_TOKEN="your-ro-or-rw-token"
```

Nx 23.1.1 treats GET 404 as a miss; other GET failures are fatal. PUT 403 and
409 are nonfatal but Nx still attempts a PUT for each newly cached task. Other
PUT errors can be retried up to six times by Nx. There is no implicit fail-open.
For a known outage, explicitly set `NX_SKIP_REMOTE_CACHE=true`.

Missing/invalid auth returns 401 with exactly `Content-Type: text/plain`.
Valid RO writes drain within the configured limits and return 403. Collisions
return 409 after receiving the body. Draining preserves status delivery for
clients still uploading. Overload returns 503; oversized or slow uploads return
413/408. Those early rejections may appear as transport errors to a client still
sending. Unauthenticated bodies are never drained. S3 permission and outage errors
are never disguised as misses or read-only denials.

## Operations and image

`/health` is public process liveness, not a bucket probe. No readiness endpoint
is added: use an authenticated synthetic PUT/GET with disposable keys to check
the storage path. Configure bucket lifecycle expiration separately.

Structured log events have bounded labels: `request` records method/status and
header-response latency, `s3` records operation/latency/error and object bytes,
`uploads` records active count, and `spool` records completed spool bytes added
and removed. GET 200/404 counts are hits/misses. These events can feed log-derived
metrics; there is no metrics server. Byte fields are object sizes, not confirmed
client delivery. Inbound partial spools are bounded by active count × upload limit.
No token, hash, artifact content, or SDK request detail is logged.

```sh
docker build -t nx-cache-local .
docker run --rm --read-only --cap-drop=ALL --security-opt=no-new-privileges \
  --tmpfs /spool:rw,noexec,nosuid,size=1100m,mode=0700,uid=65532,gid=65532 \
  --env-file .env -e SPOOL_DIRECTORY=/spool -e BIND_ADDRESS=0.0.0.0 \
  -p 127.0.0.1:3000:3000 nx-cache-local
```

For containers, set `BIND_ADDRESS=0.0.0.0` and a reachable S3 endpoint; host
loopback in `.env.example` does not address MinIO from inside a container.
Both pinned base images support Linux amd64/arm64. Build natively on each
architecture or use `docker buildx build --platform linux/amd64,linux/arm64`.
The runtime is non-root with no shell. Run it read-only with only `/spool`
writable, and account for tmpfs in the container memory limit. Publish/deploy
by immutable image digest, not a mutable tag. No image publishing workflow is added.

Manual binary release packaging remains. It gates on checks for the exact
candidate, requires `master` and a matching Cargo version/new tag, pins actions,
and produces checksums. Release notes pass as data through an environment variable
and file, never interpolated into shell source. Nothing publishes automatically.

Before production rollout: enforce TLS, private bucket/IAM and lifecycle policy,
protect RW secrets from every PR-editable workflow, restrict releases, and measure
cost and hit rate in a pilot. RO protects integrity, not confidentiality. Strict RO
also loses Nx Cloud's isolated PR writes. Production S3/IAM and Dash0 CI migration
are separate work, not covered by the local fixture.
