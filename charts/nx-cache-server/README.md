# Nx cache server Helm chart

This chart installs a Deployment, a ClusterIP Service on port 3000, and a
ServiceAccount. S3 storage, IAM, Secrets and external routing are managed outside
the chart. There are no Ingress resources or chart dependencies.

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
auth:
  readWriteSecret:
    name: cache-auth
    key: read-write
  readOnlySecret:
    name: cache-auth
    key: read-only
```

With workload identity configured for the server's ServiceAccount, run from the
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
an external router or trusted proxy for traffic outside the cluster.

## Credentials and workload identity

**Client authentication and storage identity are separate.** Nx clients use the
read-write or read-only bearer token to access this server. The server uses AWS
credentials to access S3. Linking the server's ServiceAccount to a cloud identity
can replace static AWS keys; it does not replace the Nx client tokens. The
application does not validate cloud identity tokens as client authentication.

`s3.credentialsSecret.name` defaults to empty, so the application uses its AWS SDK
default credential provider chain. The compiled SDK includes web-identity and
container credential providers:

- [EKS IRSA](https://docs.aws.amazon.com/eks/latest/userguide/pod-configuration.html)
  requires IAM trust, S3 permissions and the cluster injector to supply
  `AWS_ROLE_ARN`, `AWS_WEB_IDENTITY_TOKEN_FILE` and a readable projected token.
- [EKS Pod Identity](https://docs.aws.amazon.com/eks/latest/userguide/pod-id-how-it-works.html)
  requires an external ServiceAccount association and the Pod Identity Agent;
  EKS supplies the credential endpoint and projected token. It is not configured
  merely by adding an IRSA annotation.
- [GKE's Google ServiceAccount linking](https://cloud.google.com/kubernetes-engine/docs/how-to/workload-identity)
  supplies credentials for Google Cloud APIs, not AWS S3 credentials. That linking
  alone is not sufficient for this AWS SDK-based server.

The chart does not provision IAM trust, provider associations or projected identity
volumes. Those belong to the cluster's identity integration. Provider-specific
identity behavior has not been verified by the local chart tests.

Use `serviceAccount.annotations` for an account created by this chart. To use an
externally managed account, set `serviceAccount.create: false` and
`serviceAccount.name`; configure annotations on that existing account yourself.
The chart does not create RBAC or grant access to the Kubernetes API.
`serviceAccount.automountServiceAccountToken` defaults to false. Enable it only
if your identity integration requires the standard Kubernetes API token mount;
provider-injected audience-specific tokens may use separate projected volumes.

If workload identity is unavailable, `s3.credentialsSecret.name` can reference an
existing Secret with `AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY` keys. Override
`accessKeyIdKey` and `secretAccessKeyKey` when the Secret uses other key names.
For temporary credentials, also set `sessionTokenKey` to its session token key.
Static keys take precedence over workload identity, so omit this Secret when
using automatic credential discovery. Credential values never belong in Helm values.

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

## External routing and optional tracing

Configure routing, DNS and TLS outside this chart when clients need access from
outside the cluster. Route `/` without path rewriting to the ClusterIP Service
on port 3000. Proxy body-size limits, request buffering and client/upstream timeouts
depend on that routing infrastructure. Configure them to accommodate
`maxUploadBytes` and the slowest expected transfer plus S3 time.

Tracing is disabled unless `otlp.endpoint` is set. Use a base OTLP HTTP endpoint,
such as `https://collector.example.com:4318`; the exporter adds `/v1/traces`.
`otlp.serviceName` defaults to `nx-cache-server`. To authenticate the exporter,
reference an existing Secret through `otlp.headersSecret.name` and `.key`.
Its value uses the OTLP header format, for example
`Authorization=Bearer%20your-token`. Do not put that value in Helm values or
proxy configuration. This chart does not install a collector.

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
and optional tracing. `tests/optional-values.yaml` is a render
fixture, not a deployable environment. It is excluded from packaged charts.
