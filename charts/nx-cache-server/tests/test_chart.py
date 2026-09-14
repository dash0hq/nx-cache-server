"""Chart contract checks. Requires Helm 3.19+ and PyYAML; no cluster access."""

import pathlib
import subprocess
import unittest

import yaml


CHART = pathlib.Path(__file__).resolve().parents[1]
BASE = ["--set", "s3.bucket=test-cache,auth.readWriteSecret.name=test-auth"]
OPTIONAL = ["-f", str(CHART / "tests/optional-values.yaml")]


def helm(*args):
    return subprocess.run(
        ["helm", *map(str, args)], capture_output=True, text=True, check=False
    )


class ChartTest(unittest.TestCase):
    def render(self, *args):
        result = helm("template", "cache", CHART, *BASE, *args)
        self.assertEqual(result.returncode, 0, result.stderr)
        docs = list(yaml.safe_load_all(result.stdout))
        self.assertTrue(all(docs))
        return {doc["kind"]: doc for doc in docs}

    def env(self, docs):
        container = docs["Deployment"]["spec"]["template"]["spec"]["containers"][0]
        entries = container["env"]
        self.assertEqual(len(entries), len({entry["name"] for entry in entries}))
        self.assertTrue(all(isinstance(e["value"], str) for e in entries if "value" in e))
        return {entry["name"]: entry for entry in entries}

    def test_lint(self):
        for values in (BASE, OPTIONAL):
            with self.subTest(values=values):
                result = helm("lint", CHART, "--strict", *values)
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_defaults(self):
        docs = self.render()
        self.assertEqual(set(docs), {"Deployment", "Service", "ServiceAccount"})
        dep = docs["Deployment"]["spec"]
        pod = dep["template"]["spec"]
        container = pod["containers"][0]
        env = self.env(docs)
        self.assertEqual(dep["replicas"], 1)
        self.assertEqual(dep["selector"]["matchLabels"], dep["template"]["metadata"]["labels"])
        self.assertEqual(docs["Service"]["spec"]["selector"], dep["selector"]["matchLabels"])
        self.assertEqual(docs["Service"]["spec"]["type"], "ClusterIP")
        self.assertEqual(docs["Service"]["spec"]["ports"], [{"name": "http", "port": 3000, "targetPort": "http"}])
        self.assertEqual(container["ports"], [{"name": "http", "containerPort": 3000}])
        self.assertEqual(pod["serviceAccountName"], "cache")
        self.assertFalse(pod["automountServiceAccountToken"])
        self.assertEqual(pod["securityContext"], {
            "runAsNonRoot": True, "runAsUser": 65532, "runAsGroup": 65532,
            "fsGroup": 65532, "seccompProfile": {"type": "RuntimeDefault"},
        })
        self.assertEqual(container["securityContext"], {
            "allowPrivilegeEscalation": False, "readOnlyRootFilesystem": True,
            "capabilities": {"drop": ["ALL"]},
        })
        self.assertEqual(pod["volumes"], [{"name": "spool", "emptyDir": {"sizeLimit": "1Gi"}}])
        self.assertEqual(container["volumeMounts"], [{"name": "spool", "mountPath": "/spool"}])
        self.assertEqual(env["TMPDIR"]["value"], "/spool")
        self.assertEqual(env["MAX_UPLOAD_BYTES"]["value"], "268435456")
        self.assertEqual(env["S3_TIMEOUT"]["value"], "30")
        self.assertEqual(env["S3_BUCKET_NAME"]["value"], "test-cache")
        self.assertEqual(env["SERVICE_ACCESS_TOKEN"]["valueFrom"]["secretKeyRef"], {"name": "test-auth", "key": "token"})
        self.assertFalse(any(name.startswith(("AWS_", "OTEL_", "READ_ONLY_")) for name in env))
        self.assertEqual(container["resources"]["requests"]["ephemeral-storage"], "1Gi")
        self.assertEqual(container["resources"]["limits"]["ephemeral-storage"], "2Gi")
        self.assertRegex(container["image"], r"^ghcr.io/dash0hq/nx-cache-server@sha256:[a-f0-9]{64}$")
        self.assertEqual(pod["nodeSelector"]["kubernetes.io/arch"], "amd64")
        for probe in ("readinessProbe", "livenessProbe"):
            self.assertEqual(container[probe]["httpGet"], {"path": "/health", "port": "http"})

    def test_optional_configuration(self):
        # --set overrides -f, so explicitly override the required BASE values too.
        docs = self.render(*OPTIONAL, "--set", "s3.bucket=render-test-cache,auth.readWriteSecret.name=cache-tokens")
        pod = docs["Deployment"]["spec"]["template"]["spec"]
        env = self.env(docs)
        self.assertEqual(docs["Deployment"]["spec"]["replicas"], 3)
        expected_values = {
            "AWS_REGION": "eu-west-1", "S3_ENDPOINT_URL": "https://s3.example.test",
            "S3_TIMEOUT": "47", "MAX_UPLOAD_BYTES": "5368709120",
            "OTEL_EXPORTER_OTLP_ENDPOINT": "https://otel.example.test:4318",
            "OTEL_SERVICE_NAME": "cache-render-test",
        }
        for name, value in expected_values.items():
            self.assertEqual(env[name]["value"], value)
        expected_refs = {
            "SERVICE_ACCESS_TOKEN": ("cache-tokens", "rw"),
            "READ_ONLY_ACCESS_TOKEN": ("cache-tokens", "ro"),
            "AWS_ACCESS_KEY_ID": ("aws-credentials", "access-id"),
            "AWS_SECRET_ACCESS_KEY": ("aws-credentials", "secret-key"),
            "AWS_SESSION_TOKEN": ("aws-credentials", "session-token"),
            "OTEL_EXPORTER_OTLP_HEADERS": ("otlp-auth", "exporter-headers"),
        }
        for name, (secret, key) in expected_refs.items():
            self.assertEqual(env[name], {"name": name, "valueFrom": {"secretKeyRef": {"name": secret, "key": key}}})
        self.assertEqual(pod["serviceAccountName"], "cache-identity")
        self.assertEqual(docs["ServiceAccount"]["metadata"]["annotations"], {"eks.amazonaws.com/role-arn": "arn:aws:iam::123456789012:role/example"})
        self.assertEqual(docs["Deployment"]["spec"]["template"]["metadata"]["annotations"], {"example.test/rollout": "2"})
        self.assertEqual(pod["terminationGracePeriodSeconds"], 120)
        self.assertEqual(pod["volumes"][0]["emptyDir"], {"sizeLimit": "16Gi"})
        self.assertEqual(pod["containers"][0]["resources"]["limits"]["ephemeral-storage"], "20Gi")
        ingress = docs["Ingress"]["spec"]
        self.assertEqual(ingress["ingressClassName"], "example")
        self.assertEqual(ingress["tls"], [{"hosts": ["cache.example.test"], "secretName": "cache-tls"}])
        self.assertEqual(ingress["rules"], [{"host": "cache.example.test", "http": {"paths": [{
            "path": "/", "pathType": "Prefix", "backend": {"service": {"name": "cache", "port": {"name": "http"}}},
        }]}}])

    def test_existing_identity_and_plain_ingress(self):
        docs = self.render("--set", "serviceAccount.create=false,serviceAccount.name=existing,serviceAccount.automountServiceAccountToken=true,ingress.enabled=true,ingress.host=cache.example.test")
        self.assertNotIn("ServiceAccount", docs)
        pod = docs["Deployment"]["spec"]["template"]["spec"]
        self.assertEqual(pod["serviceAccountName"], "existing")
        self.assertTrue(pod["automountServiceAccountToken"])
        self.assertNotIn("tls", docs["Ingress"]["spec"])
        self.assertNotIn("ingressClassName", docs["Ingress"]["spec"])

    def test_credentials_without_session_token_and_otlp_without_headers(self):
        docs = self.render("--set", "s3.credentialsSecret.name=aws,otlp.endpoint=http://collector:4318")
        env = self.env(docs)
        self.assertIn("AWS_ACCESS_KEY_ID", env)
        self.assertIn("AWS_SECRET_ACCESS_KEY", env)
        self.assertNotIn("AWS_SESSION_TOKEN", env)
        self.assertIn("OTEL_EXPORTER_OTLP_ENDPOINT", env)
        self.assertNotIn("OTEL_EXPORTER_OTLP_HEADERS", env)

    def test_upload_lower_boundary(self):
        self.assertEqual(self.env(self.render("--set", "maxUploadBytes=1"))["MAX_UPLOAD_BYTES"]["value"], "1")

    def test_reject_invalid_values(self):
        cases = [
            ("s3.bucket=", "s3.bucket"),
            ("auth.readWriteSecret.name=", "auth.readWriteSecret.name"),
            ("auth.readWriteSecret.key=", "auth.readWriteSecret.key"),
            ("serviceAccount.create=false", "serviceAccount.name"),
            ("ingress.enabled=true", "ingress.host"),
            ("otlp.headersSecret.name=headers", "otlp.endpoint"),
            ("s3.credentialsSecret.secretAccessKeyKey=", "secretAccessKeyKey"),
            ("maxUploadBytes=0", "maxUploadBytes"),
            ("maxUploadBytes=5368709121", "maxUploadBytes"),
            ("replicaCount=0", "replicaCount"),
            ("replicaCount=two", "replicaCount"),
            ("spool.sizeLimit=0Gi", "spool.sizeLimit"),
            ("s3.timeoutSeconds=0", "s3.timeoutSeconds"),
            ("s3.endpoint=ftp://invalid", "s3.endpoint"),
            ("image.digest=latest", "image.digest"),
            ("repilcaCount=2", "repilcaCount"),
        ]
        for value, diagnostic in cases:
            with self.subTest(value=value):
                result = helm("template", "cache", CHART, *BASE, "--set", value)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(diagnostic.replace(".", "/"), result.stderr)
        result = helm("template", "cache", CHART)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("s3/bucket", result.stderr)
        self.assertIn("auth/readWriteSecret/name", result.stderr)


if __name__ == "__main__":
    unittest.main(verbosity=2)
