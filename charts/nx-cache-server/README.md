# Nx cache server Helm chart

This chart installs a Deployment, a ClusterIP Service on port 3000, and a
ServiceAccount. Ingress is optional. S3 storage, IAM, Secrets and ingress
controllers are managed outside the chart. There are no chart dependencies.

## Install from a checkout

Use Helm 3.19 or later and Kubernetes 1.25 or later. The default image is
Linux/amd64, so the chart selects Linux/amd64 nodes. Chart version `0.1.0` is
independent of the application version. `appVersion` identifies the source
commit; `image.digest` pins its published GHCR image. There is no Helm repository
or OCI chart publication in this initial version.

Before installing:

1. Create an S3 bucket and grant the server identity `s3:GetObject` and
   `s3:PutObject` for its objects. S3-compatible storage must support conditional
   `PutObject` with `If-None-Match: *`. Configure encryption permissions, lifecycle
   expiration and network access as required by your storage provider.
2. Create a namespace and an existing Secret containing a strong read-write
   token. Optionally add a distinct read-only token. Give only the read-only
   token to untrusted builds. The application rejects empty or identical tokens.
3. Arrange AWS credentials or a supported workload identity, as described below.

For example, provision a token Secret from files without putting token values in
Helm values or shell history. The files must contain the exact tokens without a
trailing newline:

```sh
kubectl create namespace nx-cache
kubectl -n nx-cache create secret generic cache-auth \
  --from-file=read-write=/secure/path/read-write-token \
  --from-file=read-only=/secure/path/read-only-token
```

Create `cache-values.yaml` with your non-secret configuration:

```yaml
s3:
  bucket: your-existing-cache-bucket
  region: eu-west-1
  credentialsSecret:
    name: cache-aws # Omit when using workload identity.
auth:
  readWriteSecret:
    name: cache-auth
    key: read-write
  readOnlySecret:
    name: cache-auth
    key: read-only
```

With `cache-aws` provisioned or workload identity configured, run from the
repository root:

```sh
helm lint charts/nx-cache-server --strict -f cache-values.yaml
helm upgrade --install nx-cache charts/nx-cache-server \
  --namespace nx-cache -f cache-values.yaml --wait --timeout 5m
```

An unconfigured install intentionally fails validation. `s3.bucket` and
`auth.readWriteSecret.name` are required. Helm checks the values, not the existence
or contents of referenced Secrets. All Secrets and existing ServiceAccounts must
be in the release namespace. Missing Secret keys prevent the container starting.

Clients inside the cluster can use `http://nx-cache.nx-cache.svc:3000` as
`NX_SELF_HOSTED_REMOTE_CACHE_SERVER`. Set
`NX_SELF_HOSTED_REMOTE_CACHE_ACCESS_TOKEN` to the appropriate token. Use TLS at
the ingress or another trusted proxy for traffic outside the cluster.

## Credentials and workload identity

For static AWS credentials, `s3.credentialsSecret.name` references a Secret with
`AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY` keys by default. Override
`accessKeyIdKey` and `secretAccessKeyKey` when your Secret uses other key names.
For temporary credentials, also set `sessionTokenKey` to the Secret's session
token key. Credential values never belong in Helm values.

When the Secret name is empty, the application uses its AWS SDK credential
provider chain. Workload identity needs the provider's cluster integration,
identity trust policy and S3 permissions. For example, an IRSA setup must inject
the role environment and projected web-identity token into the pod. Adding a
ServiceAccount annotation alone does not establish that trust or install the
injector. Provider-specific identity behavior is not verified by this chart's
local tests.

Use `serviceAccount.annotations` for an account created by this chart. To use an
externally managed account, set `serviceAccount.create: false` and
`serviceAccount.name`; configure annotations on that existing account yourself.
The chart does not create RBAC or grant access to the Kubernetes API.
`serviceAccount.automountServiceAccountToken` defaults to false. Enable it only
if your identity integration requires the standard Kubernetes API token mount;
provider-injected audience-specific tokens may use separate projected volumes.

Set `s3.region` explicitly unless your environment supplies region discovery.
For MinIO or another S3-compatible provider, set `s3.endpoint` to an HTTP or HTTPS
URL. The application uses path-style addressing for custom endpoints.

## Storage, limits and shutdown

The container runs as UID/GID 65532 with no capabilities, no privilege escalation,
a read-only root filesystem and the runtime's default seccomp profile. A
disk-backed `emptyDir` at `/spool` is writable through `fsGroup: 65532`; no root
init container is needed. This is temporary upload storage, not the cache's
durable store. Pod replacement loses in-flight uploads but not S3 objects.

`maxUploadBytes` defaults to 268435456 bytes, or 256 MiB, per request. It must be
between 1 byte and 5 GiB. **It does not bound concurrent requests.** Budget
`spool.sizeLimit`, node disk capacity, and `resources.requests.ephemeral-storage`
for the number and size of concurrent uploads. Allow headroom for logs in
`resources.limits.ephemeral-storage`. The defaults are a 1 GiB spool, a 1 GiB
ephemeral-storage request and a 2 GiB limit per pod. They are starting values,
not a concurrency guarantee. Kubernetes enforces local storage limits through
accounting and eviction, not a strict reservation for each upload. Disk pressure
can fail uploads or evict the pod before all clients finish.

Both probes call the public `/health` route. It returns `OK` when the HTTP server
is running; **it does not test S3 credentials, permissions or connectivity**.
Check an authenticated upload and download before sending CI traffic.

SIGTERM starts graceful shutdown. `terminationGracePeriodSeconds` defaults to
60 seconds. Size it for the expected client transfer time, S3 operation timeout
and telemetry flush time. Kubernetes can terminate remaining uploads after that
deadline. `s3.timeoutSeconds` defaults to 30 and limits each S3 operation, not
the time a client spends uploading to the server.

## Optional ingress and tracing

The ingress routes one hostname at `/`, without rewriting paths, to the Service:

```yaml
ingress:
  enabled: true
  className: your-installed-controller
  host: cache.example.com
  tls:
    secretName: cache-tls
```

Create the TLS Secret and configure DNS separately. Ingress annotations, body
size limits, request buffering and client/upstream timeouts depend on your
controller. Set them to accommodate `maxUploadBytes` and the slowest expected
transfer plus S3 time. The chart does not guess controller-specific annotations.
Without a TLS Secret, TLS termination must be configured elsewhere. No ingress
controller or certificate issuer is installed by this chart.

Tracing is disabled unless `otlp.endpoint` is set. Use a base OTLP HTTP endpoint,
such as `https://collector.example.com:4318`; the exporter adds `/v1/traces`.
`otlp.serviceName` defaults to `nx-cache-server`. To authenticate the exporter,
reference an existing Secret through `otlp.headersSecret.name` and `.key`.
Its value uses the OTLP header format, for example
`Authorization=Bearer%20your-token`. Do not put that value in Helm values or
ingress annotations. This chart does not install a collector.

## Upgrade and uninstall

Keep a complete values file and pass it on every upgrade. For application
upgrades, select a verified published `image.digest`. If using an image for
another architecture, also change `nodeSelector`. The chart always uses a digest,
never `latest`. Application upgrades need their own compatibility validation.

```sh
helm upgrade nx-cache charts/nx-cache-server \
  --namespace nx-cache -f cache-values.yaml --wait --timeout 5m
helm uninstall nx-cache --namespace nx-cache
```

Secret values enter the container through environment variables. Updating an
external Secret does not restart pods; perform a rollout after token, credential
or OTLP header rotation. A changed `podAnnotations` value can trigger that rollout
through Helm. Uninstalling leaves external Secrets, existing ServiceAccounts,
the S3 bucket and objects untouched.

## Local validation

From the repository root, with Helm on `PATH`:

```sh
helm lint charts/nx-cache-server --strict \
  --set s3.bucket=test-cache,auth.readWriteSecret.name=test-auth
uv run --with PyYAML python3 charts/nx-cache-server/tests/test_chart.py
```

The test suite checks invalid values, exact environment strings and Secret
references, hardened security, selectors, disk storage, existing ServiceAccounts,
and optional ingress/TLS and tracing. `tests/optional-values.yaml` is a render
fixture, not a deployable environment. It is excluded from packaged charts.
