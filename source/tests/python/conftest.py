"""Shared fixtures for the Python e2e harness."""

import os
import subprocess
import time
import urllib.request
from pathlib import Path

import azure.core.pipeline.policies._authentication as _authentication
import azure.search.documents.indexes._search_index_client as _search_index_client
import pytest
from testcontainers.core.container import DockerContainer

# Disable testcontainers' Ryuk cleanup container. It fails to mount the Docker
# socket on Docker Desktop for macOS, and is unnecessary here: the emulator
# fixture uses a context manager that stops and removes the container.
os.environ.setdefault("TESTCONTAINERS_RYUK_DISABLED", "true")


def _allow_http_endpoint(endpoint: str) -> str:
    if not endpoint.lower().startswith("http"):
        return "https://" + endpoint
    return endpoint


def _no_op_enforce_https(request: object) -> None:
    return None


# The pinned SDK (azure-search-documents==11.6.0) rejects non-TLS endpoints in
# two places: SearchIndexClient.normalize_endpoint (only accepts https:// URLs)
# and the bearer-token auth policy (_enforce_https). The emulator is a local
# plain-HTTP service, so the harness allows http:// in both. These are
# test-harness-only shims: the full HTTP contract is still exercised, only the
# SDK's endpoint-scheme validation is bypassed.
_search_index_client.normalize_endpoint = _allow_http_endpoint
_authentication._enforce_https = _no_op_enforce_https

IMAGE_NAME = "aisearch-emulator"
CONTAINER_PORT = 8080
REPO_ROOT = Path(__file__).resolve().parents[2]
BUILD_CONTEXT = REPO_ROOT / "source" / "rust"


def _wait_for_health(url: str, timeout: float = 30.0) -> None:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            with urllib.request.urlopen(f"{url}/health", timeout=2) as resp:
                if resp.status == 200:
                    return
        except Exception:
            pass
        time.sleep(0.5)
    raise RuntimeError(f"emulator did not become healthy within {timeout}s")


def _reset(endpoint: str) -> None:
    req = urllib.request.Request(
        f"{endpoint}/admin/reset",
        data=b"",
        method="POST",
        headers={"Content-Type": "application/json"},
    )
    with urllib.request.urlopen(req, timeout=5):
        pass


def _image_exists(name: str) -> bool:
    result = subprocess.run(
        ["docker", "image", "inspect", name],
        capture_output=True,
    )
    return result.returncode == 0


@pytest.fixture(scope="session")
def emulator_image():
    """Use the Docker image, building it first if it is not already present.

    CI loads a pre-built image (see .github/workflows/ci.yml); local runs build
    from source/rust on first use.
    """
    if not _image_exists(IMAGE_NAME):
        result = subprocess.run(
            ["docker", "build", "-t", IMAGE_NAME, str(BUILD_CONTEXT)],
            capture_output=True,
            text=True,
        )
        if result.returncode != 0:
            raise RuntimeError(f"docker build failed:\n{result.stderr}")
    yield IMAGE_NAME


@pytest.fixture(scope="session")
def emulator_endpoint(emulator_image):
    """Start the emulator container and yield its base URL."""
    container = DockerContainer(emulator_image).with_exposed_ports(CONTAINER_PORT)
    with container:
        host = container.get_container_host_ip()
        port = container.get_exposed_port(CONTAINER_PORT)
        endpoint = f"http://{host}:{port}"
        _wait_for_health(endpoint)
        yield endpoint


@pytest.fixture()
def clean_emulator(emulator_endpoint):
    """Reset emulator state before each test."""
    _reset(emulator_endpoint)
    yield emulator_endpoint
