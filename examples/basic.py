"""Thumbrella Python client example

This is an overly simplified example of using the Python client for Thumbrella.
The actual Python client lives in

- **Pypi** at https://pypi.org/project/thumbrella-client/
- **Github** at https://github.com/thumbrella-dev/clients/

See the more complete Python client examples at
https://github.com/thumbrella-dev/clients/tree/main/python/examples

"""

import thumbrella

# Client uses `$TBR_CONNECT` to define the server url or Cloud token
tbr = thumbrella.Client()


# Generate a single specific thumbnail from url
url = "https://demo.thumbrella.dev/media/golden-gate.exr"
result = tbr.thumb(url)
media = result.verify().media
print(f"{media.kind} {media.file_size} bytes -> {len(media.thumbnail)} bytes")


# Write thumbnail to disk
from pathlib import Path
Path("/tmp/thumbnail.jpeg").write_bytes(media.thumbnail.bytes)
