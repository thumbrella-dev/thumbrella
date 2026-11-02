import importlib.util
from pathlib import Path
import unittest

import bpy


spec = importlib.util.spec_from_file_location(
    "blender_render", Path(__file__).parents[1] / "src" / "blender_render.py"
)
renderer = importlib.util.module_from_spec(spec)
spec.loader.exec_module(renderer)


class BlenderAppearance(unittest.TestCase):
    def setUp(self):
        bpy.ops.wm.read_factory_settings(use_empty=True)

    def mesh(self, name):
        mesh = bpy.data.meshes.new(name)
        mesh.from_pydata([(0, 0, 0), (1, 0, 0), (0, 1, 0)], [], [(0, 1, 2)])
        obj = bpy.data.objects.new(name, mesh)
        bpy.context.scene.collection.objects.link(obj)
        return obj

    def has_appearance(self, objects):
        bpy.context.view_layer.update()
        return renderer.has_authored_appearance(objects)

    def test_unmaterialed_mesh(self):
        self.assertFalse(self.has_appearance([self.mesh("plain")]))

    def test_assigned_material_preserves_a_mixed_scene(self):
        plain = self.mesh("plain")
        colored = self.mesh("colored")
        colored.data.materials.append(bpy.data.materials.new("colored"))
        self.assertTrue(self.has_appearance([plain, colored]))

    def test_unused_material_slot_does_not_count(self):
        obj = self.mesh("plain")
        obj.data.materials.append(None)
        obj.data.materials.append(bpy.data.materials.new("unused"))
        self.assertFalse(self.has_appearance([obj]))
        obj.data.polygons[0].material_index = 1
        self.assertTrue(self.has_appearance([obj]))

    def test_vertex_colors_are_preserved(self):
        obj = self.mesh("vertex-colored")
        colors = obj.data.color_attributes.new(name="Color", type="FLOAT_COLOR", domain="POINT")
        obj.data.color_attributes.active_color = colors
        self.assertTrue(self.has_appearance([obj]))


if __name__ == "__main__":
    result = unittest.main(argv=[__file__], exit=False)
    if not result.result.wasSuccessful():
        raise SystemExit(1)
