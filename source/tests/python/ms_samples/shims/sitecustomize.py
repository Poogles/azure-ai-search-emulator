# Test-harness shim: allow the Azure SDK to talk to a plain-HTTP local emulator.
# Mirrors source/tests/python/conftest.py. Applied automatically via PYTHONPATH.
try:
    import azure.search.documents.indexes._search_index_client as _sic
    import azure.core.pipeline.policies._authentication as _auth

    def _allow_http_endpoint(endpoint):
        if not endpoint.lower().startswith("http"):
            return "https://" + endpoint
        return endpoint

    def _no_op_enforce_https(request):
        return None

    _sic.normalize_endpoint = _allow_http_endpoint
    _auth._enforce_https = _no_op_enforce_https
except Exception:
    pass
