"""Fixtures for running Microsoft's reference samples against the emulator.

The samples are the unmodified reference scripts from a sparse submodule of
``Azure/azure-sdk-for-python`` (``ms_samples/upstream``, narrowed to
``sdk/search/azure-search-documents/samples`` via ``make ms-samples``).
They are run as subprocesses exactly as Microsoft documents, with the
``AZURE_SEARCH_*`` environment variables pointed at the local emulator.

Two differences from the parent e2e harness:

* The samples do not pin an ``api-version``, so they use the SDK default
  (``2026-04-01`` for the pinned SDK 12.0.0). The emulator is therefore
  started with both ``2024-07-01`` and the SDK default accepted.
* The samples are run in a subprocess, so the plain-HTTP shims from the parent
  ``conftest.py`` are re-applied via ``ms_samples/shims/sitecustomize.py`` on
  ``PYTHONPATH``.
"""

import os

import pytest
from testcontainers.core.container import DockerContainer

from ._helpers import (
    API_VERSIONS,
    BUILD_CONTEXT,
    CONTAINER_PORT,
    IMAGE_NAME,
    image_exists,
    reset,
    wait_for_health,
)

os.environ.setdefault("TESTCONTAINERS_RYUK_DISABLED", "true")


@pytest.fixture(scope="session")
def ms_emulator_endpoint():
    """Start the emulator accepting both API versions and yield its base URL."""
    if not image_exists(IMAGE_NAME):
        import subprocess

        result = subprocess.run(
            ["docker", "build", "-t", IMAGE_NAME, str(BUILD_CONTEXT)],
            capture_output=True,
            text=True,
        )
        if result.returncode != 0:
            raise RuntimeError(f"docker build failed:\n{result.stderr}")
    container = (
        DockerContainer(IMAGE_NAME)
        .with_exposed_ports(CONTAINER_PORT)
        .with_env("EMULATOR_API_VERSIONS", API_VERSIONS)
    )
    with container:
        host = container.get_container_host_ip()
        port = container.get_exposed_port(CONTAINER_PORT)
        endpoint = f"http://{host}:{port}"
        wait_for_health(endpoint)
        yield endpoint


@pytest.fixture()
def ms_clean_emulator(ms_emulator_endpoint):
    """Reset emulator state before each sample run."""
    reset(ms_emulator_endpoint)
    yield ms_emulator_endpoint
