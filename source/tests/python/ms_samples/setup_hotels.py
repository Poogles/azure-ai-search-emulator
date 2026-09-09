"""Create the hotels index + seed data that the Microsoft samples expect."""
import os

from azure.core.credentials import AzureKeyCredential
from azure.search.documents import SearchClient
from azure.search.documents.indexes import SearchIndexClient
from azure.search.documents.indexes.models import (
    SearchField,
    SearchFieldDataType,
    SearchIndex,
)

endpoint = os.environ["AZURE_SEARCH_SERVICE_ENDPOINT"]
key = os.environ["AZURE_SEARCH_API_KEY"]
index_name = os.environ.get("AZURE_SEARCH_INDEX_NAME", "hotels-sample-index")

S = SearchFieldDataType.String
index = SearchIndex(
    name=index_name,
    fields=[
        SearchField(name="HotelId", type=S, key=True, filterable=True),
        SearchField(name="HotelName", type=S, searchable=True),
        SearchField(name="Description", type=S, searchable=True),
        SearchField(name="Description_fr", type=S, searchable=True),
        SearchField(name="Category", type=S, searchable=True, filterable=True, facetable=True),
        SearchField(name="Tags", type=SearchFieldDataType.Collection(S), searchable=True, filterable=True, facetable=True),
        SearchField(name="ParkingIncluded", type=SearchFieldDataType.Boolean, filterable=True, facetable=True),
        SearchField(name="IsDeleted", type=SearchFieldDataType.Boolean, filterable=True),
        SearchField(name="LastRenovationDate", type=SearchFieldDataType.DateTimeOffset, filterable=True, sortable=True),
        SearchField(name="Rating", type=SearchFieldDataType.Double, filterable=True, sortable=True),
        SearchField(name="Location", type=SearchFieldDataType.GeographyPoint, filterable=True),
        SearchField(
            name="Address",
            type=SearchFieldDataType.ComplexType,
            fields=[
                SearchField(name="City", type=S, searchable=True, filterable=True),
                SearchField(name="StateProvince", type=S, filterable=True),
                SearchField(name="Country", type=S, filterable=True),
            ],
        ),
    ],
)

docs = [
    {"HotelId": "1", "HotelName": "Historic Harbor Hotel",
     "Description": "A recently renovated waterfront hotel with free parking and a rooftop restaurant.",
     "Description_fr": "Hotel de front de mer récemment rénové.",
     "Category": "Luxury", "Tags": ["waterfront", "parking", "renovated", "spa"],
     "ParkingIncluded": True, "IsDeleted": False,
     "LastRenovationDate": "2025-02-10T00:00:00Z", "Rating": 4.8,
     "Location": {"type": "Point", "coordinates": [-122.332071, 47.601969]},
     "Address": {"City": "Seattle", "StateProvince": "WA", "Country": "USA"}},
    {"HotelId": "2", "HotelName": "City Center Hotel",
     "Description": "A downtown hotel near museums and restaurants with fast Wi-Fi and a spa.",
     "Description_fr": "Hôtel en centre-ville avec Wi-Fi rapide.",
     "Category": "Boutique", "Tags": ["downtown", "wifi", "spa"],
     "ParkingIncluded": False, "IsDeleted": False,
     "LastRenovationDate": "2023-06-15T00:00:00Z", "Rating": 4.2,
     "Location": {"type": "Point", "coordinates": [-80.191790, 25.761679]},
     "Address": {"City": "Miami", "StateProvince": "FL", "Country": "USA"}},
    {"HotelId": "3", "HotelName": "Mountain View Inn",
     "Description": "A quiet mountain hotel with trail access, breakfast, and free parking.",
     "Description_fr": "Hôtel de montagne calme avec petit-déjeuner.",
     "Category": "Resort", "Tags": ["mountain", "parking", "breakfast"],
     "ParkingIncluded": True, "IsDeleted": False,
     "LastRenovationDate": "2024-09-20T00:00:00Z", "Rating": 4.6,
     "Location": {"type": "Point", "coordinates": [-104.990251, 39.739236]},
     "Address": {"City": "Denver", "StateProvince": "CO", "Country": "USA"}},
    {"HotelId": "4", "HotelName": "Closed Airport Hotel",
     "Description": "A former airport hotel that is no longer available for booking.",
     "Description_fr": "Ancien hôtel d'aéroport fermé.",
     "Category": "Airport", "Tags": ["airport"],
     "ParkingIncluded": True, "IsDeleted": True,
     "LastRenovationDate": "2020-01-05T00:00:00Z", "Rating": 3.1,
     "Location": {"type": "Point", "coordinates": [-73.567257, 45.501699]},
     "Address": {"City": "Montreal", "StateProvince": "QC", "Country": "CAN"}},
]

cred = AzureKeyCredential(key)
idx = SearchIndexClient(endpoint, cred)
idx.create_or_update_index(index)
print(f"created index {index_name}")
sc = SearchClient(endpoint, index_name, cred)
res = sc.upload_documents(docs)
print(f"uploaded {sum(1 for r in res if r.succeeded)}/{len(res)} docs")
