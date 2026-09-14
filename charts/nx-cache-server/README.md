# Nx cache server Helm chart

This chart installs a Deployment, a ClusterIP Service on port 3000, and a
ServiceAccount on EKS. It supports AWS S3 with IAM roles for service accounts
(IRSA). The bucket, IAM role and trust policy, cluster OIDC setup, Secrets and
external routing are managed outside the chart. There are no Ingress resources
or chart dependencies.

## Install from a checkout

Use Helm 3.19 or later and Kubernetes 1.25 or later. The default image is
Linux/amd64, so the chart selects Linux/amd64 nodes. Chart version `0.1.0` is
independent of the application version. `appVersion` identifies the source
commit; `image.digest` pins its published GHCR image. There is no Helm repository
or OCI chart publication in this initial version.

Before installing:

1. Create an AWS S3 bucket and grant the IRSA role `s3:GetObject` and
   `s3:PutObject` for its objects. Configure encryption permissions, lifecycle
   expiration and network access as required by your bucket configuration.
2. Create a namespace and an existing Secret containing a strong read-write
   token. Optionally add a distinct read-only token. Give only the read-only
   token to untrusted builds. The application rejects empty or identical tokens.
3. Configure the cluster OIDC provider and IAM role trust for the release namespace
   and ServiceAccount name, as described below.

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
serviceAccount:
  name: nx-cache
  roleArn: arn:aws:iam::123456789012:role/ci/nx-cache
auth:
  readWriteSecret:
    name: cache-auth
    key: read-write
  readOnlySecret:
    name: cache-auth
    key: read-only
```

With IRSA trust configured for the server's ServiceAccount, run from the
repository root:

```sh
helm lint charts/nx-cache-server --strict -f cache-values.yaml
helm upgrade --install nx-cache charts/nx-cache-server \
  --namespace nx-cache -f cache-values.yaml --wait --timeout 5m
```

An unconfigured install intentionally fails validation. `s3.bucket`, `s3.region`,
`serviceAccount.roleArn` and `auth.readWriteSecret.name` are required. Helm checks
the values, not IAM trust or the existence and contents of referenced Secrets.
All Secrets must be in the release namespace. Missing Secret keys prevent the
container starting.

Clients inside the cluster can use `http://nx-cache.nx-cache.svc:3000` as
`NX_SELF_HOSTED_REMOTE_CACHE_SERVER`. Set
`NX_SELF_HOSTED_REMOTE_CACHE_ACCESS_TOKEN` to the appropriate token. Use TLS at
an external router or trusted proxy for traffic outside the cluster.

## IRSA and client authentication

**Client authentication and storage identity are separate.** Nx clients use the
read-write or read-only bearer token to access this server. The server uses IRSA
to obtain short-lived AWS credentials for S3. IRSA does not replace the Nx client tokens. The
application does not validate cloud identity tokens as client authentication.

The chart always creates the ServiceAccount and annotates it with
`eks.amazonaws.com/role-arn` from the required `serviceAccount.roleArn` value.
`serviceAccount.name` defaults to the Helm release name; set it explicitly to keep
the IAM trust binding stable. There is no existing-account mode.

Configure the IAM role outside Helm to trust the EKS cluster's OIDC provider with
`sts:AssumeRoleWithWebIdentity`. The trust conditions must match:

- `sub`: `system:serviceaccount:<release-namespace>:<serviceAccount-name>`
- `aud`: `sts.amazonaws.com`

For the example above, the subject is `system:serviceaccount:nx-cache:nx-cache`.
Changing either the release namespace or the ServiceAccount name requires updating
the role's trust policy. Restrict trust to the intended namespace and account, and
grant the role only the required bucket permissions. Follow the
[AWS IRSA setup guide](https://docs.aws.amazon.com/eks/latest/userguide/associate-service-account-role.html)
for the cluster OIDC provider and trust policy configuration.

The EKS IRSA webhook supplies `AWS_ROLE_ARN`, `AWS_WEB_IDENTITY_TOKEN_FILE` and a
readable projected token. The application still uses the AWS SDK default
credential chain, including its web-identity provider; it is not an IRSA-only
credential resolver. IRSA is the supported chart configuration, and the chart
does not inject static AWS keys or custom S3 endpoints. The explicit `s3.region`
selects the bucket's AWS region.

Ordinary Kubernetes API-token automount is disabled on both the ServiceAccount
and pod. This does not prohibit the IRSA webhook's separate projected token
volume. The chart grants no Kubernetes API access and does not create IAM roles,
trust policies or cluster OIDC providers. Local rendering and Kubernetes admission
checks do not prove that IRSA can assume the role; verify authenticated S3 access
on the target EKS cluster before sending CI traffic.

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

Helm checks resource quantity types and that `spool.sizeLimit` is a nonempty
string. Kubernetes validates quantity syntax and resource constraints, including
malformed quantity strings that Helm accepts. Use Kubernetes quantities such as
`100m`, `128Mi` and `1Gi`; size the spool and resource budgets together.

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

Client tokens and OTLP headers enter the container through environment variables.
Updating an external Secret does not restart pods; perform a rollout after token
or OTLP header rotation. A changed `podAnnotations` value can trigger that rollout
through Helm. Use a rollout after changing `serviceAccount.roleArn` too, so the
IRSA webhook reinjects the role configuration. Uninstalling removes the chart's
ServiceAccount but leaves external Secrets, the IAM role/trust and S3 objects untouched.

## Local validation

From the repository root, with Helm on `PATH`:

```sh
helm lint charts/nx-cache-server --strict \
  --set s3.bucket=test-cache,s3.region=eu-west-1,auth.readWriteSecret.name=test-auth \
  --set serviceAccount.roleArn=arn:aws:iam::123456789012:role/ci/nx-cache
helm lint charts/nx-cache-server --strict \
  -f charts/nx-cache-server/tests/optional-values.yaml
helm template cache charts/nx-cache-server \
  -f charts/nx-cache-server/tests/optional-values.yaml
```

`values.schema.json` validates values during linting, rendering and installation.
An unconfigured `helm template cache charts/nx-cache-server` must fail with missing
bucket, region, role ARN and read-write Secret name diagnostics. Inspect the
rendered resources when changing templates, including their IRSA annotation,
Secret references, security settings and spool. Use a Kubernetes server-side dry
run to validate quantities; Helm intentionally does not parse their grammar.
`tests/optional-values.yaml` exercises optional configuration for local rendering;
it is not a deployable environment and is excluded from packaged charts.
