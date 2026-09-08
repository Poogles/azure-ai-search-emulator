"""Shared fixtures for the Python e2e harness."""

import subprocess
import time
import urllib.request

import pytest
from testcontainers.core.container import DockerContainer

IMAGE_NAME = "aisearch-emulator"
CONTAINER_PORT = 8080
API_KEY = "test-key"


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


@pytest.fixture(scope="session")
def emulator_image():
    """Build the Docker image once per session."""
    result = subprocess.run(
        ["docker", "build", "-t", IMAGE_NAME, "source/rust"],
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        raise RuntimeError(f"docker build failed:\n{result.stderr}")
    yield IMAGE_NAME


@pytest.fixture(scope="session")
def emulator_endpoint(emulator_image):
    """Start the emulator container and yield its base URL."""
    with DockerContainer(emulator_image) as container:
        container.with_exposed_port(CONTAINER_PORT)
        host = container.get_container_host_ip()
        port = container.exposed_port(CONTAINER_PORT)
        endpoint = f"http://{host}:{port}"
        _wait_for_health(endpoint)
        yield endpoint


@pytest.fixture()
def clean_emulator(emulator_endpoint):
    """Reset emulator state before each test."""
    _reset(emulator_endpoint)
    yield emulator_endpoint
