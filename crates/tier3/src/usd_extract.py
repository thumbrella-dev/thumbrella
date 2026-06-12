#!/usr/bin/env python3
"""Extract triangulated mesh geometry from a USD/USDZ file to OBJ.

Usage:
    python3 usd_extract.py input.usdz output.obj

The script walks all UsdGeomMesh prims, triangulates faces, applies
local-to-world transforms, and writes a single OBJ.  UVs and normals
are included when present.  No material or texture data is extracted.
"""
import sys
import os

sys.path.insert(0, "/home/thumbrella/.local/lib/python3.12/site-packages")
from pxr import Usd, UsdGeom, Vt, Gf


def extract_mesh(stage, output_path):
    """Walk the stage, collect mesh data, write OBJ."""
    all_vertices = []
    all_normals = []
    all_uvs = []
    all_faces = []
    vertex_offset = 0
    uv_offset = 0
    has_uvs = False
    has_normals = False

    # F3D renders OBJ with --up=+Z.  Rotate Y-up USD to Z-up so the
    # model's "up" axis matches what F3D expects for OBJ files.
    stage_up = UsdGeom.GetStageUpAxis(stage)
    up_rotation = Gf.Matrix4d(1.0)
    if stage_up == UsdGeom.Tokens.z:
        pass  # already Z-up
    elif stage_up == UsdGeom.Tokens.y:
        # (x, y, z) → (x, z, -y)  — +90° around X
        up_rotation = Gf.Matrix4d().SetRotate(Gf.Rotation(Gf.Vec3d(1, 0, 0), 90.0))
    # Unknown: leave as-is.

    for prim in stage.TraverseAll():
        if not prim.IsA(UsdGeom.Mesh):
            continue

        mesh = UsdGeom.Mesh(prim)

        points_attr = mesh.GetPointsAttr()
        if not points_attr.HasValue():
            continue
        points = points_attr.Get()

        fvi_attr = mesh.GetFaceVertexIndicesAttr()
        fvc_attr = mesh.GetFaceVertexCountsAttr()
        if not fvi_attr.HasValue() or not fvc_attr.HasValue():
            continue

        indices = fvi_attr.Get()
        counts = fvc_attr.Get()

        # UVs — skip entirely. Without an MTL they serve no purpose
        # and F3D may interpret them as vertex attributes causing
        # noisy faceted shading on dense meshes at thumbnail size.
        mesh_uvs = []

        # Normals — skip. USD sometimes authors per-face normals that
        # produce a noisy faceted look at thumbnail size.  Let F3D
        # auto-compute smooth normals from the geometry instead.
        mesh_normals = []

        # World transform + up-axis rotation.
        xform = UsdGeom.XformCache(Usd.TimeCode.Default()).GetLocalToWorldTransform(prim)
        combined = up_rotation * xform

        mesh_vertices = []
        for p in points:
            wp = combined.Transform(p)
            mesh_vertices.append((float(wp[0]), float(wp[1]), float(wp[2])))

        # Triangulate faces (fan method).
        tris = []
        idx = 0
        for count in counts:
            if count < 3:
                idx += count
                continue
            for j in range(1, count - 1):
                tris.append((indices[idx], indices[idx + j], indices[idx + j + 1]))
            idx += count

        # Accumulate with 1-based OBJ indexing.
        start_v = vertex_offset + 1
        start_vt = uv_offset + 1 if mesh_uvs else 1

        all_vertices.extend(mesh_vertices)
        if mesh_normals:
            all_normals.extend(mesh_normals)
        if mesh_uvs:
            all_uvs.extend(mesh_uvs)

        for t in tris:
            vi = (t[0] + start_v, t[1] + start_v, t[2] + start_v)
            vti = (t[0] + start_vt, t[1] + start_vt, t[2] + start_vt) if mesh_uvs else (0, 0, 0)
            vni = vi if mesh_normals else (0, 0, 0)
            all_faces.append((vi, vti, vni))

        vertex_offset += len(mesh_vertices)
        uv_offset += len(mesh_uvs)

    # Write OBJ file.
    with open(output_path, 'w') as f:
        f.write("# Extracted from USDZ by usd_extract.py\n")
        f.write(f"# {len(all_vertices)} vertices, {len(all_faces)} faces\n")

        for v in all_vertices:
            f.write(f"v {v[0]:.6f} {v[1]:.6f} {v[2]:.6f}\n")

        if has_uvs and all_uvs:
            for vt in all_uvs:
                f.write(f"vt {vt[0]:.6f} {vt[1]:.6f}\n")

        if has_normals and all_normals:
            for vn in all_normals:
                f.write(f"vn {vn[0]:.6f} {vn[1]:.6f} {vn[2]:.6f}\n")

        for face in all_faces:
            vi, vti, vni = face
            if has_uvs and has_normals:
                f.write(f"f {vi[0]}/{vti[0]}/{vni[0]} {vi[1]}/{vti[1]}/{vni[1]} {vi[2]}/{vti[2]}/{vni[2]}\n")
            elif has_uvs:
                f.write(f"f {vi[0]}/{vti[0]} {vi[1]}/{vti[1]} {vi[2]}/{vti[2]}\n")
            elif has_normals:
                f.write(f"f {vi[0]}//{vni[0]} {vi[1]}//{vni[1]} {vi[2]}//{vni[2]}\n")
            else:
                f.write(f"f {vi[0]} {vi[1]} {vi[2]}\n")

    return len(all_vertices), len(all_faces)


def main():
    if len(sys.argv) != 3:
        print(f"Usage: {sys.argv[0]} input.usdz output.obj", file=sys.stderr)
        sys.exit(1)

    input_path = sys.argv[1]
    output_path = sys.argv[2]

    if not os.path.exists(input_path):
        print(f"Error: input file not found: {input_path}", file=sys.stderr)
        sys.exit(1)

    stage = Usd.Stage.Open(input_path)
    if not stage:
        print(f"Error: could not open stage: {input_path}", file=sys.stderr)
        sys.exit(1)

    num_verts, num_faces = extract_mesh(stage, output_path)
    output_dir = os.path.dirname(output_path) or "."
    print(f"Extracted {num_verts} vertices, {num_faces} faces to {output_dir}/")


if __name__ == "__main__":
    main()



if __name__ == "__main__":
    main()
