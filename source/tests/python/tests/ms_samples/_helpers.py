"""Shared constants and helpers for the Microsoft-samples compatibility probe.

The reference samples are not vendored: they live in a sparse, shallow
submodule of ``Azure/azure-sdk-for-python`` (``ms_samples/upstream``) that
checks out only ``sdk/search/azure-search-documents/samples``. See the
``make ms-samples`` target for (re)initialisation.
"""

import os
import subprocess
import sys
import time
import urllib.request
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[5]
MS_BASE_DIR = REPO_ROOT / "source" / "tests" / "python" / "ms_samples"
UPSTREAM_DIR = MS_BASE_DIR / "upstream"
MS_SAMPLES_DIR = UPSTREAM_DIR / "sdk" / "search" / "azure-search-documents" / "samples"
SETUP_SCRIPT = MS_BASE_DIR / "setup_hotels.py"
SHIMS_DIR = MS_BASE_DIR / "shims"

IMAGE_NAME = "aisearch-emulator"
CONTAINER_PORT = 8080
BUILD_CONTEXT = REPO_ROOT / "source" / "rust"

# The emulator's own default plus the SDK default the samples send.
API_VERSIONS = "2024-07-01,2025-09-01"
API_KEY = "test-key"
HOTELS_INDEX = "hotels-sample-index"

# Samples that are helpers or async mirrors, not standalone runnable samples.
NON_SAMPLES = {"sample_utils.py"}


def require_samples() -> None:
    """Fail fast with a helpful message if the submodule is not checked out."""
    if not (MS_SAMPLES_DIR / "sample_query_simple.py").is_file():
        raise RuntimeError(
            "Microsoft samples submodule is not checked out. "
            "Run `make ms-samples` (or: git submodule update --init "
            "source/tests/python/ms_samples/upstream && "
            f"git -C {UPSTREAM_DIR} sparse-checkout set "
            "sdk/search/azure-search-documents/samples)."
        )


def discover_samples() -> list[str]:
    """Return the sync reference samples (``sample_*.py``), sorted.

    Async mirrors (``*_async.py``) and helper modules are excluded: the
    emulator is a plain HTTP service, so the sync and async SDK paths exercise
    the same wire contract.
    """
    require_samples()
    return sorted(
        p.name
        for p in MS_SAMPLES_DIR.glob("sample_*.py")
        if p.name not in NON_SAMPLES and not p.name.endswith("_async.py")
    )


def wait_for_health(url: str, timeout: float = 30.0) -> None:
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


def reset(endpoint: str) -> None:
    req = urllib.request.Request(
        f"{endpoint}/admin/reset",
        data=b"",
        method="POST",
        headers={"Content-Type": "application/json"},
    )
    with urllib.request.urlopen(req, timeout=5):
        pass


def image_exists(name: str) -> bool:
    return subprocess.run(["docker", "image", "inspect", name], capture_output=True).returncode == 0


def seed_hotels(endpoint: str, index_name: str = HOTELS_INDEX) -> None:
    """Create the hotels index and seed documents the samples expect."""
    env = dict(
        os.environ,
        AZURE_SEARCH_SERVICE_ENDPOINT=endpoint,
        AZURE_SEARCH_API_KEY=API_KEY,
        AZURE_SEARCH_INDEX_NAME=index_name,
        PYTHONPATH=str(SHIMS_DIR),
    )
    result = subprocess.run(
        [sys.executable, str(SETUP_SCRIPT)],
        env=env,
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        raise RuntimeError(f"hotels seed failed:\n{result.stderr}\n{result.stdout}")


def run_sample(endpoint: str, sample: str, index_name: str = HOTELS_INDEX) -> subprocess.CompletedProcess:
    """Run one Microsoft sample script as a subprocess against the emulator."""
    env = dict(
        os.environ,
        AZURE_SEARCH_SERVICE_ENDPOINT=endpoint,
        AZURE_SEARCH_API_KEY=API_KEY,
        AZURE_SEARCH_INDEX_NAME=index_name,
        PYTHONPATH=str(SHIMS_DIR),
    )
    return subprocess.run(
        [sys.executable, str(MS_SAMPLES_DIR / sample)],
        env=env,
        capture_output=True,
        text=True,
        timeout=120,
    )
