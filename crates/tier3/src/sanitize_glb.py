#!/usr/bin/env python3
"""Strip images, textures, materials, and KHR_texture_transform from a GLB.

VTK 9.1's GLTF importer crashes on these.  Reads a GLB from stdin or
a file argument, writes the sanitized GLB to stdout.
Usage: python3 sanitize_glb.py input.glb > output.glb
"""
import sys, struct, json

def strip_ext(obj):
    if isinstance(obj, dict):
        obj.pop("KHR_texture_transform", None)
        if "extensions" in obj and isinstance(obj["extensions"], dict):
            obj["extensions"].pop("KHR_texture_transform", None)
        for v in obj.values():
            strip_ext(v)
    elif isinstance(obj, list):
        for v in obj:
            strip_ext(v)

def sanitize(data):
    if len(data) < 20 or data[:4] != b"glTF":
        return data

    json_len = struct.unpack("<I", data[12:16])[0]
    json_bytes = data[20:20 + json_len]

    j = json.loads(json_bytes)
    if isinstance(j, dict):
        j.pop("images", None)
        j.pop("textures", None)
        j.pop("materials", None)
        j.pop("samplers", None)
    strip_ext(j)
    if "extensionsUsed" in j:
        j["extensionsUsed"] = [e for e in j["extensionsUsed"] if e != "KHR_texture_transform"]

    new_json = json.dumps(j, separators=(",", ":")).encode()
    while len(new_json) % 4 != 0:
        new_json += b" "

    # Rebuild GLB: 12-byte header (magic + version + total_len placeholder),
    # then JSON chunk, then original binary chunk.
    out = bytearray(data[:12])
    out += struct.pack("<I", len(new_json))
    out += b"JSON"
    out += new_json

    # Append original binary chunk.
    orig_aligned = ((20 + json_len + 3) // 4) * 4
    out += data[orig_aligned:]

    # Fix total length.
    struct.pack_into("<I", out, 8, len(out))
    return bytes(out)

if __name__ == "__main__":
    if len(sys.argv) > 1:
        with open(sys.argv[1], "rb") as f:
            data = f.read()
    else:
        data = sys.stdin.buffer.read()
    sys.stdout.buffer.write(sanitize(data))
