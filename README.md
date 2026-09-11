# Nx Custom Remote Cache Server

A lightweight Nx cache server that bridges Nx CLI clients with S3-compatible storage.

## Features

- **AWS S3 Integration**: Direct streaming integration with AWS S3 and S3-compatible services
- **Bounded uploads**: Uploads are streamed through temporary files with a configurable size limit
- **High Performance**: Built with Rust and Axum for maximum throughput
- **OpenTelemetry**: Optional OTLP HTTP traces for Dash0 or any compatible backend
- **Nx API Compliant**: Full implementation of the [Nx custom remote cache OpenAPI specification](https://nx.dev/recipes/running-tasks/self-hosted-caching#build-your-own-caching-server)
- **Security First**: Bearer token authentication with constant-time comparison
- **Self-Hosted & Private**: Telemetry is disabled unless an OTLP endpoint is configured

## Quick Start

### Prerequisites

Access to AWS S3 (or S3-compatible service like MinIO)

### Installation

#### Step 1: Pull the image
```bash
docker pull ghcr.io/dash0hq/nx-cache-server:latest
```

#### Step 2: Configure the server

The server supports configuration via environment variables, command-line arguments, or both.

##### Option A: Environment Variables (Recommended)
```bash
# Required
export S3_BUCKET_NAME="your-s3-bucket-name"
export SERVICE_ACCESS_TOKEN="your-bearer-token"

# AWS Credentials (optional - auto-discovered from IAM roles, config files, SSO if not provided)
export AWS_ACCESS_KEY_ID="your-aws-access-key-id"
export AWS_SECRET_ACCESS_KEY="your-aws-secret-access-key"
export AWS_SESSION_TOKEN="your-session-token"  # If you are using temporary credentials

# AWS Region (optional - auto-discovered from AWS config, EC2/ECS metadata if not provided)
export AWS_REGION="us-west-2"

# Optional
export S3_ENDPOINT_URL="your-s3-endpoint-url"   # For S3-compatible services like MinIO
export S3_TIMEOUT="30"                          # S3 operation timeout in seconds (default: 30)
export PORT="3000"                              # Server port (default: 3000)
export BIND_ADDRESS="0.0.0.0"                   # IP to bind to (default: 0.0.0.0). Use "::" for IPv6/dual-stack
export READ_ONLY_ACCESS_TOKEN="your-ro-token"   # Read-only token for untrusted CI jobs (see "Protecting against cache poisoning")
export MAX_UPLOAD_BYTES="268435456"              # Maximum upload size (default: 256 MiB)
export OTEL_EXPORTER_OTLP_ENDPOINT="https://your-dash0-otlp-http-endpoint:4318"
export OTEL_EXPORTER_OTLP_HEADERS="Authorization=Bearer%20your-token"
```

##### Option B: Command Line Arguments
```bash
./nx-cache-aws \
  --region "your-aws-region" \
  --access-key-id "your-aws-access-key-id" \
  --secret-access-key "your-aws-secret-access-key" \
  --bucket-name "your-s3-bucket-name" \
  --session-token "your-session-token" \
  --endpoint-url "your-s3-endpoint-url" \
  --service-access-token "your-bearer-token" \
  --timeout-seconds 30 \
  --port 3000 \
  --bind-address 0.0.0.0
```

##### Option C: Mixed Configuration
You can also combine both methods. Command line arguments will override environment variables:
```bash
# Set common config via environment
export AWS_REGION="us-west-2"
export S3_BUCKET_NAME="my-cache-bucket"
export SERVICE_ACCESS_TOKEN="my-secure-token"

# Specify other values via CLI
./nx-cache-aws --port 8080
```

> **Note:** AWS credentials and region are optional when running on AWS infrastructure (EC2, ECS, Lambda) or when AWS config files are present. The server will auto-discover them from your environment.

#### Step 3: Run the server
```bash
docker run --rm -p 3000:3000 \
  -e AWS_REGION -e AWS_ACCESS_KEY_ID -e AWS_SECRET_ACCESS_KEY -e AWS_SESSION_TOKEN \
  -e S3_BUCKET_NAME -e S3_ENDPOINT_URL -e S3_TIMEOUT -e PORT -e BIND_ADDRESS \
  -e SERVICE_ACCESS_TOKEN -e READ_ONLY_ACCESS_TOKEN \
  -e MAX_UPLOAD_BYTES -e OTEL_EXPORTER_OTLP_ENDPOINT -e OTEL_EXPORTER_OTLP_HEADERS \
  ghcr.io/dash0hq/nx-cache-server:latest
```

#### Step 4 (optional): Verify the service is up and running
```bash
curl http://localhost:3000/health
```
You should receive an "OK" response.

### Client Configuration

To configure your Nx workspace to use this cache server, set the following environment variables:

```bash
# Point Nx to your cache server
export NX_SELF_HOSTED_REMOTE_CACHE_SERVER="http://localhost:3000"

# Authentication token (must match SERVICE_ACCESS_TOKEN from server config,
# or READ_ONLY_ACCESS_TOKEN for jobs that should not write to the cache)
export NX_SELF_HOSTED_REMOTE_CACHE_ACCESS_TOKEN="your-bearer-token"

# Optional: Disable TLS certificate validation (e.g. for development/testing environment)
export NODE_TLS_REJECT_UNAUTHORIZED="0"
```

Once configured, Nx will automatically use your cache server for storing and retrieving build artifacts.

For more details, see the [Nx documentation](https://nx.dev/recipes/running-tasks/self-hosted-caching#usage-notes).

### Protecting against cache poisoning (CVE-2025-36852 / CREEP)

If untrusted contributors can run CI with cache **write** access (typically pull request builds), they can pre-seed the cache entry for a hash that a trusted branch will later compute — and the trusted build will replay the poisoned artifact ([CVE-2025-36852, "CREEP"](https://nx.dev/blog/cve-2025-36852-critical-cache-poisoning-vulnerability-creep)). Write-once semantics don't prevent this: the attack writes *first*, it never overwrites.

The mitigation is to keep untrusted jobs read-only. Configure a second token on the server:

```bash
export SERVICE_ACCESS_TOKEN="your-rw-token"     # trusted builds (main/release): read-write
export READ_ONLY_ACCESS_TOKEN="your-ro-token"   # untrusted builds (PRs): read-only
```

Then set `NX_SELF_HOSTED_REMOTE_CACHE_ACCESS_TOKEN` to the read-only token in PR pipelines and to the read-write token only in trusted-branch pipelines. A read-only token can retrieve artifacts as usual but gets `403 Forbidden` on writes, so untrusted jobs still benefit from cache hits without being able to poison the cache.
