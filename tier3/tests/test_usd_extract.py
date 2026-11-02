import importlib.util
from pathlib import Path
import tempfile
import unittest

from pxr import Gf, Usd, UsdGeom, Vt


spec = importlib.util.spec_from_file_location(
    "usd_extract", Path(__file__).parents[1] / "src" / "usd_extract.py"
)
extract = importlib.util.module_from_spec(spec)
spec.loader.exec_module(extract)


class ExtractionLimits(unittest.TestCase):
    def test_prim_limit(self):
        stage = Usd.Stage.CreateInMemory()
        for index in range(extract.MAX_PRIMS):
            stage.DefinePrim(f"/Prim{index}")
        with tempfile.TemporaryDirectory() as directory:
            path = str(Path(directory) / "output.obj")
            extract.extract_mesh(stage, path)
            stage.DefinePrim("/OneTooMany")
            with self.assertRaisesRegex(RuntimeError, "exceeds 100 prims"):
                extract.extract_mesh(stage, path)

    def test_triangle_limit(self):
        stage = Usd.Stage.CreateInMemory()
        mesh = UsdGeom.Mesh.Define(stage, "/Mesh")
        mesh.CreatePointsAttr(Vt.Vec3fArray([Gf.Vec3f(0), Gf.Vec3f(1, 0, 0), Gf.Vec3f(0, 1, 0)]))
        mesh.CreateFaceVertexCountsAttr(Vt.IntArray([3] * extract.MAX_TRIANGLES))
        mesh.CreateFaceVertexIndicesAttr(Vt.IntArray([0, 1, 2] * extract.MAX_TRIANGLES))
        with tempfile.TemporaryDirectory() as directory:
            path = str(Path(directory) / "output.obj")
            self.assertEqual(extract.extract_mesh(stage, path)[1], extract.MAX_TRIANGLES)
            mesh.GetFaceVertexCountsAttr().Set(Vt.IntArray([3] * (extract.MAX_TRIANGLES + 1)))
            mesh.GetFaceVertexIndicesAttr().Set(Vt.IntArray([0, 1, 2] * (extract.MAX_TRIANGLES + 1)))
            with self.assertRaisesRegex(RuntimeError, "exceeds 100000 triangulated polygons"):
                extract.extract_mesh(stage, path)

    def test_vertex_limit(self):
        stage = Usd.Stage.CreateInMemory()
        mesh = UsdGeom.Mesh.Define(stage, "/Mesh")
        mesh.CreatePointsAttr(Vt.Vec3fArray([Gf.Vec3f(0)] * (extract.MAX_VERTICES + 1)))
        mesh.CreateFaceVertexCountsAttr(Vt.IntArray([3]))
        mesh.CreateFaceVertexIndicesAttr(Vt.IntArray([0, 1, 2]))
        with tempfile.TemporaryDirectory() as directory:
            with self.assertRaisesRegex(RuntimeError, "exceeds 300000 vertices"):
                extract.extract_mesh(stage, str(Path(directory) / "output.obj"))


if __name__ == "__main__":
    unittest.main()
