# Test-harness shim: allow the Azure SDK to talk to a plain-HTTP local emulator.
# Mirrors source/tests/python/conftest.py. Applied automatically via PYTHONPATH.
#
# The pinned SDK (azure-search-documents==12.0.0) uses the endpoint URL verbatim
# as the pipeline base URL, so http:// endpoints work for api-key auth without
# any shim. The one remaining scheme check is the bearer-token auth policy
# (_enforce_https); the samples use api-key auth, but the shim is kept so a
# bearer-token sample would still reach the emulator.
try:
    import azure.core.pipeline.policies._authentication as _auth

    def _no_op_enforce_https(request):
        return None

    _auth._enforce_https = _no_op_enforce_https
except Exception:
    pass
