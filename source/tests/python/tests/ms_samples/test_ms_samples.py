"""Compatibility probe: Microsoft's reference samples vs. the emulator.

The reference samples are pulled from a sparse submodule of
``Azure/azure-sdk-for-python`` (see ``make ms-samples``) and run, unmodified,
as subprocesses against the emulator. The suite is green while the emulator
behaves as documented; it turns red when a documented gap is closed (a sample
starts passing) or when a new upstream sample needs triage, so the registry
below stays honest as the emulator catches up.

Each discovered sync sample is classified:

* not in ``KNOWN_ISSUES``  -> expected to PASS.
* ``("gap", signature)``   -> expected to FAIL with that Azure error signature
  (the emulator does not implement the feature; the signature documents the gap).
* ``("falsepass", ...)``   -> expected to exit 0 while the operation silently
  did not take effect (the SDK swallowed a per-document error).
* ``("skip", reason)``     -> cannot run here for reasons unrelated to the
  emulator (newer SDK than the pinned one, or an external service).
"""

import pytest
from azure.core.credentials import AzureKeyCredential
from azure.search.documents import SearchClient

from ._helpers import (
    HOTELS_INDEX,
    API_KEY,
    discover_samples,
    run_sample,
    seed_hotels,
)

try:
    SAMPLES = discover_samples()
    _MISSING = None
except RuntimeError as exc:  # submodule not checked out
    SAMPLES = []
    _MISSING = str(exc)


# sample filename -> (category, detail)
KNOWN_ISSUES = {
    # --- Cannot run here: needs azure-search-documents >= 12.0.0 (harness pins 11.6.0).
    "sample_agentic_retrieval.py": ("skip", "needs azure-search-documents>=12 (knowledgebases)"),
    "sample_knowledge_service_stats_preview.py": ("skip", "needs azure-search-documents>=12 (typed service stats model)"),
    "sample_index_alias_crud.py": ("skip", "needs azure-search-documents>=12 (SearchAlias)"),
    "sample_index_client_custom_request.py": ("skip", "needs azure-search-documents>=12 (DEFAULT_VERSION)"),
    "sample_index_crud.py": ("skip", "needs azure-search-documents>=12 (SearchFieldDataType.STRING)"),
    "sample_query_semantic.py": ("skip", "needs azure-search-documents>=12 (semantic query kwargs)"),
    "sample_query_vector.py": ("skip", "needs azure-search-documents>=12 (SearchFieldDataType.STRING)"),
    "sample_search_client_custom_request.py": ("skip", "needs azure-search-documents>=12 (DEFAULT_VERSION)"),
    "sample_knowledge_base_configuration_preview.py": ("skip", "needs azure-search-documents>=12 (knowledge bases)"),
    "sample_knowledge_base_crud.py": ("skip", "needs azure-search-documents>=12 (knowledge bases)"),
    "sample_knowledge_retrieval_response_preview.py": ("skip", "needs azure-search-documents>=12 (knowledge bases)"),
    "sample_knowledge_source_crud.py": ("skip", "needs azure-search-documents>=12 (knowledge sources)"),
    "sample_knowledge_source_fabric_data_agent_preview.py": ("skip", "needs azure-search-documents>=12 (knowledge sources)"),
    "sample_knowledge_source_fabric_ontology_preview.py": ("skip", "needs azure-search-documents>=12 (knowledge sources)"),
    "sample_knowledge_source_file_preview.py": ("skip", "needs azure-search-documents>=12 (knowledge sources)"),
    "sample_knowledge_source_freshness_preview.py": ("skip", "needs azure-search-documents>=12 (knowledge sources)"),
    "sample_knowledge_source_mcp_server_preview.py": ("skip", "needs azure-search-documents>=12 (knowledge sources)"),
    "sample_knowledge_source_workiq_preview.py": ("skip", "needs azure-search-documents>=12 (knowledge sources)"),
    # --- Cannot run here: needs azure-identity and a real Azure AD environment.
    "sample_authentication.py": ("skip", "needs azure-identity + real AAD for the AAD half (API-key half passes)"),
    # --- Cannot run here: needs an external Azure Storage account.
    "sample_indexer_crud.py": ("skip", "needs AZURE_STORAGE_CONNECTION_STRING"),
    "sample_indexer_datasource_crud.py": ("skip", "needs AZURE_STORAGE_CONNECTION_STRING"),
    "sample_indexer_workflow.py": ("skip", "needs AZURE_STORAGE_CONNECTION_STRING"),
}


@pytest.mark.skipif(not SAMPLES, reason=_MISSING or "no samples discovered")
@pytest.mark.parametrize("sample", SAMPLES)
def test_ms_sample(ms_clean_emulator, sample):
    category, detail = KNOWN_ISSUES.get(sample, ("pass", None))

    if category == "skip":
        pytest.skip(detail)

    seed_hotels(ms_clean_emulator)
    proc = run_sample(ms_clean_emulator, sample)

    if category == "pass":
        assert proc.returncode == 0, (
            f"{sample} failed (new upstream sample? triage it):\n{proc.stderr}"
        )
    elif category == "gap":
        assert proc.returncode != 0, (
            f"{sample} unexpectedly passed (gap closed? remove it from KNOWN_ISSUES):\n{proc.stdout}"
        )
        assert detail in proc.stderr, (
            f"{sample}: expected Azure signature {detail!r} in stderr\n{proc.stderr}"
        )
    elif category == "falsepass":
        assert proc.returncode == 0, (
            f"{sample} should exit 0 (buffered sender swallows per-doc errors)\n{proc.stderr}"
        )
        client = SearchClient(
            endpoint=ms_clean_emulator,
            index_name=HOTELS_INDEX,
            credential=AzureKeyCredential(API_KEY),
        )
        with pytest.raises(Exception):
            client.get_document(key="100")
