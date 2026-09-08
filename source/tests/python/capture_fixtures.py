"""Capture HTTP fixtures from the Azure AI Search Python SDK.

Run against a live emulator instance:
    python capture_fixtures.py --endpoint http://localhost:8080

Outputs sanitized request/response pairs to fixtures/.
"""

import argparse
import json
import re
import uuid
from pathlib import Path

from azure.core.pipeline import PipelineContext
from azure.core.pipeline.transport import HttpRequest, HttpResponse
from azure.search.documents import SearchClient
from azure.search.documents.indexes import SearchIndexClient
from azure.search.documents.indexes.models import (
    SearchField,
    SearchFieldDataType,
    SearchIndex,
)

API_KEY = "fixture-key"
INDEX_NAME = "fixture-index"


class RecordingTransport:
    """Wraps the default transport and records every request/response pair."""

    def __init__(self):
        self._inner = None
        self.records: list[dict] = []

    def open(self):
        from azure.core.pipeline.transport import RequestsTransport

        self._inner = RequestsTransport()
        self._inner.open()

    def close(self):
        if self._inner:
            self._inner.close()

    def send(self, request: HttpRequest, **kwargs) -> HttpResponse:
        response = self._inner.send(request, **kwargs)
        body = response.read()
        self.records.append(
            {
                "method": request.method,
                "url": request.url,
                "headers": {k: v for k, v in request.headers},
                "body": body.decode("utf-8", errors="replace") if body else None,
                "status_code": response.status_code,
                "response_headers": dict(response.headers),
                "response_body": body.decode("utf-8", errors="replace") if body else None,
            }
        )
        # Re-wrap so the SDK can read the body again
        response.set_content(body)
        return response


def sanitize(record: dict) -> dict:
    """Strip dynamic values from a recorded exchange."""
    rec = dict(record)
    # Remove x-ms-client-request-id
    rec["headers"] = {k: v for k, v in rec["headers"].items() if k.lower() != "x-ms-client-request-id"}
    rec["response_headers"] = {
        k: v for k, v in rec["response_headers"].items() if k.lower() != "x-ms-client-request-id"
    }
    # Mask any UUIDs in URLs
    rec["url"] = re.sub(r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}", "<UUID>", rec["url"])
    return rec


def main():
    parser = argparse.ArgumentParser(description="Capture SDK HTTP fixtures")
    parser.add_argument("--endpoint", default="http://localhost:8080")
    parser.add_argument("--output", default="fixtures")
    args = parser.parse_args()

    transport = RecordingTransport()
    transport.open()

    index_client = SearchIndexClient(
        endpoint=args.endpoint,
        credential=API_KEY,
        transport=transport,
    )
    search_client = SearchClient(
        endpoint=args.endpoint,
        index_name=INDEX_NAME,
        credential=API_KEY,
        transport=transport,
    )

    # Create index
    index_client.create_index(
        SearchIndex(
            name=INDEX_NAME,
            fields=[
                SearchField(name="id", type=SearchFieldDataType.STRING, key=True),
                SearchField(name="title", type=SearchFieldDataType.STRING, searchable=True),
            ],
        )
    )

    # Upload documents
    search_client.upload_documents(
        documents=[
            {"id": "1", "title": "hello world"},
            {"id": "2", "title": "foo bar"},
        ]
    )

    # Search
    list(search_client.search(search_text="hello", top=10))

    # Delete index
    index_client.delete_index(INDEX_NAME)

    transport.close()

    # Write sanitized fixtures
    out_dir = Path(args.output)
    out_dir.mkdir(parents=True, exist_ok=True)
    for i, record in enumerate(transport.records):
        sanitized = sanitize(record)
        path = out_dir / f"{i:02d}_{record['method']}_{INDEX_NAME}.json"
        path.write_text(json.dumps(sanitized, indent=2))
        print(f"  {path}")

    print(f"\nCaptured {len(transport.records)} exchanges → {out_dir}/")


if __name__ == "__main__":
    main()
