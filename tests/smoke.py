"""Disposable MinIO + real HTTP/native Nx checks. No cloud credentials or shared caches."""
import concurrent.futures
import http.client
import json
import os
from pathlib import Path
import shutil
import socket
import subprocess
import sys
import tempfile
import time
import uuid

ROOT = Path(__file__).resolve().parents[1]
RW, RO = "fixture-read-write", "fixture-read-only"


def run(*args, **kwargs):
    return subprocess.run(args, check=True, text=True, capture_output=True, **kwargs).stdout.strip()


def free_port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def request(port, method, key, data=None, token=RW):
    conn = http.client.HTTPConnection("127.0.0.1", port, timeout=15)
    try:
        conn.request(method, f"/v1/cache/{key}", data, {"Authorization": f"Bearer {token}"})
        response = conn.getresponse()
        return response.status, response.read()
    finally:
        conn.close()


def wait_for(check):
    for _ in range(100):
        try:
            if check():
                return
        except (OSError, http.client.HTTPException):
            pass
        time.sleep(0.1)
    raise AssertionError("local fixture did not become ready")


class Fixture:
    def __init__(self, directory):
        self.directory = directory
        self.project = "nx-test-" + uuid.uuid4().hex
        self.prefix = uuid.uuid4().hex
        self.env = {**os.environ, "MINIO_PORT": str(free_port())}
        self.servers = []

    def compose(self, *args):
        return run("docker", "compose", "-f", str(ROOT / "compose.yaml"), "-p", self.project, *args, env=self.env)

    def start(self):
        self.compose("up", "-d", "--wait", "minio")
        self.compose("run", "--rm", "init")
        self.endpoint = "http://" + self.compose("port", "minio", "9000")

    def server(self):
        port = free_port()
        spool = self.directory / f"spool-{port}"
        log = open(self.directory / f"server-{port}.log", "w+")
        env = {key: value for key, value in os.environ.items() if not key.startswith(("AWS_", "S3_"))}
        env.update(AWS_REGION="us-east-1", AWS_ACCESS_KEY_ID="local-cache",
                   AWS_SECRET_ACCESS_KEY="local-cache-password", AWS_EC2_METADATA_DISABLED="true",
                   S3_ENDPOINT_URL=self.endpoint, S3_BUCKET_NAME="nx-cache", S3_PREFIX=self.prefix,
                   SERVICE_ACCESS_TOKEN=RW, READ_ONLY_ACCESS_TOKEN=RO, BIND_ADDRESS="127.0.0.1",
                   PORT=str(port), SPOOL_DIRECTORY=str(spool), S3_TIMEOUT="2",
                   UPLOAD_TIMEOUT_SECONDS="3", MAX_UPLOADS="2", MAX_UPLOAD_BYTES=str(16 * 1024 * 1024))
        process = subprocess.Popen([ROOT / "target/debug/nx-cache-aws"], env=env, stdout=log, stderr=log)
        self.servers.append((process, log))
        wait_for(lambda: request(port, "GET", "ready")[0] == 404)
        return port

    def objects(self):
        output = self.compose("run", "--rm", "--no-deps", "init", "ls", "--json", "--recursive", f"local/nx-cache/{self.prefix}/")
        return sorted(json.loads(line)["key"] for line in output.splitlines() if line)

    def close(self, failed):
        for process, log in self.servers:
            process.terminate()
            process.wait(timeout=20)
            if failed:
                log.seek(0)
                print(log.read())
            log.close()
        self.compose("down", "--volumes", "--remove-orphans")


def s3(fixture):
    first, second = fixture.server(), fixture.server()
    payload = b"opaque\x00\xff\x17" * 20003
    assert request(first, "PUT", "roundtrip", payload)[0] == 200
    assert request(second, "GET", "roundtrip") == (200, payload)
    assert request(first, "PUT", "roundtrip", b"different")[0] == 409
    assert request(first, "PUT", "forbidden", payload, RO)[0] == 403
    assert request(first, "GET", "forbidden")[0] == 404
    fixture.compose("restart", "minio")
    fixture.compose("up", "-d", "--wait", "minio")
    wait_for(lambda: request(second, "GET", "roundtrip", token=RO) == (200, payload))
    assert request(second, "GET", "roundtrip", token=RO) == (200, payload)

    # Two independent servers contend for one fresh object with different bytes.
    with concurrent.futures.ThreadPoolExecutor(2) as pool:
        for number in range(5):
            key = f"race-{number}"
            left = pool.submit(request, first, "PUT", key, b"left" * 70111)
            right = pool.submit(request, second, "PUT", key, b"right" * 99333)
            statuses = [left.result()[0], right.result()[0]]
            assert sorted(statuses) == [200, 409], statuses
            winner = b"left" * 70111 if statuses[0] == 200 else b"right" * 99333
            assert request(first, "GET", key) == (200, winner)
            assert request(second, "PUT", key, b"third")[0] == 409
            assert request(second, "GET", key) == (200, winner)

    # An in-flight and then interrupted upload must never be visible.
    with socket.create_connection(("127.0.0.1", first)) as sock:
        sock.sendall(f"PUT /v1/cache/partial HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {RW}\r\nContent-Length: 1000000\r\n\r\n".encode() + b"partial")
        assert request(second, "GET", "partial")[0] == 404
    wait_for(lambda: not list(fixture.directory.glob("spool-*/upload-*")))
    assert request(second, "GET", "partial")[0] == 404

    # SIGTERM must wait for an admitted body, persistence, and spool removal.
    conn = http.client.HTTPConnection("127.0.0.1", first, timeout=10)
    conn.putrequest("PUT", "/v1/cache/shutdown")
    conn.putheader("Authorization", f"Bearer {RW}")
    conn.putheader("Content-Length", str(len(payload)))
    conn.endheaders(payload[:17])
    wait_for(lambda: bool(list(fixture.directory.glob(f"spool-{first}/upload-*"))))
    process = fixture.servers[0][0]
    process.terminate()
    time.sleep(0.1)
    assert process.poll() is None, "SIGTERM discarded an admitted upload"
    conn.send(payload[17:])
    response = conn.getresponse()
    assert response.status == 200
    response.read()
    conn.close()
    assert process.wait(timeout=10) == 0
    assert not list(fixture.directory.glob(f"spool-{first}/upload-*"))
    assert request(fixture.server(), "GET", "shutdown") == (200, payload)
    print("PASS S3: exact bytes, RO denial, persistent restart, 5 two-server races, no partial visibility, spool cleanup, active-upload SIGTERM")


def nx(fixture):
    port = fixture.server()
    workspace = fixture.directory / "workspace"
    shutil.copytree(ROOT / "tests/nx", workspace, ignore=shutil.ignore_patterns("node_modules"))
    (workspace / "node_modules").symlink_to(ROOT / "tests/nx/node_modules", target_is_directory=True)
    marker = fixture.directory / "executions"
    content = b"independent expected bytes\x00\xff" * 137
    (workspace / "input.bin").write_bytes(content)
    count = 0

    def execute(token=RW, expected=True, bypass=False, address=None):
        nonlocal count
        count += 1
        shutil.rmtree(workspace / "out", ignore_errors=True)
        env = {key: value for key, value in os.environ.items() if not key.startswith("NX_")}
        env.update(NX_NO_CLOUD="true", NX_DAEMON="false", NX_TASKS_RUNNER_DYNAMIC_OUTPUT="false",
                   NX_SELF_HOSTED_REMOTE_CACHE_SERVER=f"http://127.0.0.1:{address or port}",
                   NX_SELF_HOSTED_REMOTE_CACHE_ACCESS_TOKEN=token, NX_FIXTURE_MARKER=str(marker),
                   NX_CACHE_DIRECTORY=str(fixture.directory / f"cache-{count}"),
                   NX_WORKSPACE_DATA_DIRECTORY=str(fixture.directory / f"data-{count}"))
        if bypass:
            env["NX_SKIP_REMOTE_CACHE"] = "true"
        result = subprocess.run(["node", "node_modules/.bin/nx", "run", "artifact:build"],
                                cwd=workspace, env=env, text=True, capture_output=True, timeout=120)
        assert (result.returncode == 0) == expected, result.stdout + result.stderr
        if expected:
            assert (workspace / "out/artifact.bin").read_bytes() == (workspace / "input.bin").read_bytes()

    execute()
    original = fixture.objects()
    assert len(original) == 1 and marker.read_text() == "executed\n"
    execute()
    execute(RO)
    assert marker.read_text() == "executed\n", "remote restore executed the task"
    (workspace / "input.bin").write_bytes(b"RO miss is not persisted")
    execute(RO)
    assert marker.read_text().count("executed") == 2
    assert fixture.objects() == original

    # Incompressible >8 MiB archive, not an upload that fits socket buffers.
    (workspace / "input.bin").write_bytes(os.urandom(9 * 1024 * 1024))
    execute(RO)
    assert fixture.objects() == original
    execute()
    assert len(fixture.objects()) == 2

    # Force a native Nx GET miss followed by a real PUT collision using a local proxy.
    import http.server
    import threading
    collisions = []

    class MissProxy(http.server.BaseHTTPRequestHandler):
        def do_GET(self):
            self.send_response(404)
            self.end_headers()

        def do_PUT(self):
            body = self.rfile.read(int(self.headers["Content-Length"]))
            status, _ = request(port, "PUT", self.path.rsplit("/", 1)[1], body)
            assert status == 409
            assert len(body) > 8 * 1024 * 1024
            collisions.append(status)
            self.send_response(status)
            self.end_headers()

        def log_message(self, *_):
            pass

    with http.server.ThreadingHTTPServer(("127.0.0.1", 0), MissProxy) as proxy:
        thread = threading.Thread(target=proxy.serve_forever, daemon=True)
        thread.start()
        try:
            execute(address=proxy.server_port)
        finally:
            proxy.shutdown()
            thread.join()
    assert collisions == [409], collisions

    fixture.compose("stop", "minio")
    execute(expected=False)
    execute(bypass=True)
    execute(token="invalid", expected=False)
    assert not list(fixture.directory.glob("spool-*/upload-*"))
    print("PASS Nx 23.1.1: RW upload, fresh-cache RW/RO restoration without execution, RO miss/no writes, >8MiB 403/409, outage failure, explicit bypass, invalid auth")


def main():
    run("docker", "info")
    assert shutil.disk_usage(ROOT).free > 2 * 1024**3, "need 2 GiB free for disposable tests"
    with tempfile.TemporaryDirectory(prefix="nx-cache-test-") as directory:
        fixture = Fixture(Path(directory))
        failed = True
        try:
            fixture.start()
            {"s3": s3, "nx": nx}[sys.argv[1]](fixture)
            failed = False
        finally:
            fixture.close(failed)


if __name__ == "__main__":
    main()
