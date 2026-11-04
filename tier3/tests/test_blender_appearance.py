import importlib.util
from pathlib import Path
import tempfile
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

    def test_alembic_frame_ignores_authored_camera(self):
        scene = bpy.context.scene
        scene.frame_start = 10
        scene.frame_end = 110
        camera = bpy.data.objects.new("authored", bpy.data.cameras.new("authored"))
        scene.collection.objects.link(camera)
        self.assertIsNone(renderer.prepare_alembic_scene(scene))
        self.assertEqual(scene.frame_current, 20)

    def test_imported_alembic_animation_ignores_camera(self):
        if "abc" not in renderer.supported_formats():
            self.skipTest("Blender has no Alembic importer")
        scene = bpy.context.scene
        scene.frame_start = 1
        scene.frame_end = 11
        obj = self.mesh("animated")
        obj.location.x = -2
        obj.keyframe_insert(data_path="location", frame=1)
        obj.location.x = 2
        obj.keyframe_insert(data_path="location", frame=11)
        camera = bpy.data.objects.new("authored", bpy.data.cameras.new("authored"))
        scene.collection.objects.link(camera)
        with tempfile.TemporaryDirectory() as directory:
            path = str(Path(directory) / "animated.abc")
            self.assertEqual(bpy.ops.wm.alembic_export(filepath=path, start=1, end=11), {"FINISHED"})
            bpy.ops.wm.read_factory_settings(use_empty=True)
            self.assertEqual(bpy.ops.wm.alembic_import(filepath=path), {"FINISHED"})
            scene = bpy.context.scene
            self.assertEqual((scene.frame_start, scene.frame_end), (1, 11))
            # The export carried a camera through, but thumbnails must ignore it.
            self.assertTrue(any(obj.type == "CAMERA" for obj in scene.objects))
            self.assertIsNone(renderer.prepare_alembic_scene(scene))
            self.assertEqual(scene.frame_current, 2)
            imported = next(obj for obj in scene.objects if obj.type == "MESH")
            evaluated = imported.evaluated_get(bpy.context.evaluated_depsgraph_get())
            self.assertGreater(evaluated.matrix_world.translation.x, -2)
            self.assertLess(evaluated.matrix_world.translation.x, 2)


if __name__ == "__main__":
    result = unittest.main(argv=[__file__], exit=False)
    if not result.result.wasSuccessful():
        raise SystemExit(1)
