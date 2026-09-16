# Test-harness shims. Applied automatically via PYTHONPATH to the sample
# subprocesses (see source/tests/python/tests/ms_samples/conftest.py).
from typing import Any

#
# Shim 1: allow the Azure SDK to talk to a plain-HTTP local emulator.
# Mirrors source/tests/python/conftest.py.
#
# The pinned SDK (azure-search-documents==12.0.0) uses the endpoint URL verbatim
# as the pipeline base URL, so http:// endpoints work for api-key auth without
# any shim. The one remaining scheme check is the bearer-token auth policy
# (_enforce_https); the samples use api-key auth, but the shim is kept so a
# bearer-token sample would still reach the emulator.
try:
    from azure.core.pipeline.policies import _authentication as _auth

    def _no_op_enforce_https(request: object) -> None:
        return None

    _auth._enforce_https = _no_op_enforce_https
except Exception:  # noqa: BLE001, S110 - the shim must never break sample startup
    pass

# Shim 2: work around an azure-search-documents 12.0.0 bug.
# `SearchClient.search()` has no `query_language` / `query_speller` parameters,
# so the values `sample_query_semantic.py` passes for them are captured by
# `**kwargs` (the pipeline-options pass-through), never reach the request body,
# and are forwarded down the pipeline into `requests.Session.request`, whose
# fixed signature rejects them with `TypeError: Session.request() got an
# unexpected keyword argument 'query_language'`. The filter below drops any
# transport-bound kwarg `requests.Session.request` does not accept, so the
# request proceeds with exactly what the SDK puts on the wire (the two options
# are dropped, which is what the SDK's request builder already does).
#
# REMOVE THIS SHIM when the pinned SDK implements `query_language` /
# `query_speller` (or rejects unknown search options at the `search()` call
# site) and re-pin `azure-search-documents` in pyproject.toml.
try:
    import inspect

    import requests
    from azure.core.pipeline.transport import _requests_basic as _req_basic

    _session_params = set(inspect.signature(requests.Session.request).parameters)
    _original_send = _req_basic.RequestsTransport.send

    def _filtered_send(self: Any, request: Any, **kwargs: Any) -> Any:
        for key in [k for k in kwargs if k not in _session_params]:
            del kwargs[key]
        return _original_send(self, request, **kwargs)

    _req_basic.RequestsTransport.send = _filtered_send  # type: ignore[method-assign]
except Exception:  # noqa: BLE001, S110 - the shim must never break sample startup
    pass
