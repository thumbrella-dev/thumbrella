"""Probe Blender importers or render a geometry thumbnail."""
import json
import math
import os
import sys

import bpy
from mathutils import Vector


IMPORTERS = {
    "fbx": ("import_scene", "fbx"),
    "glb": ("import_scene", "gltf"),
    "gltf": ("import_scene", "gltf"),
    "stl": ("wm", "stl_import"),
    "obj": ("wm", "obj_import"),
    "ply": ("wm", "ply_import"),
    "dae": ("wm", "collada_import"),
    "abc": ("wm", "alembic_import"),
    "usd": ("wm", "usd_import"),
    "usda": ("wm", "usd_import"),
    "usdc": ("wm", "usd_import"),
    "usdz": ("wm", "usd_import"),
}


def supported_formats():
    supported = ["blend"]
    for extension, (group, name) in IMPORTERS.items():
        if extension in ("usd", "usda", "usdc", "usdz") and not getattr(bpy.app.build_options, "usd", False):
            continue
        if extension == "abc" and not getattr(bpy.app.build_options, "alembic", False):
            continue
        try:
            getattr(getattr(bpy.ops, group), name).get_rna_type()
        except (AttributeError, KeyError):
            continue
        supported.append(extension)
    return supported


def has_authored_appearance(objects):
    depsgraph = bpy.context.evaluated_depsgraph_get()
    for obj in objects:
        evaluated = obj.evaluated_get(depsgraph)
        materials = {index for index, slot in enumerate(evaluated.material_slots)
                     if slot.material is not None}
        if evaluated.type == "MESH":
            mesh = evaluated.data
            colors = getattr(mesh, "color_attributes", None)
            if colors and colors.active_color and len(colors.active_color.data):
                return True
            if materials and any(face.material_index in materials for face in mesh.polygons):
                return True
        elif materials:
            return True
    return False


def prepare_alembic_scene(scene):
    frame = scene.frame_start + round((scene.frame_end - scene.frame_start) * 0.1)
    scene.frame_set(frame)
    if scene.camera is not None:
        return scene.camera
    cameras = [obj for obj in scene.objects if obj.type == "CAMERA"]
    return cameras[0] if len(cameras) == 1 else None


def render(source, output, extension):
    bpy.ops.wm.read_factory_settings(use_empty=True)
    if extension not in supported_formats():
        raise RuntimeError(f"Blender importer unavailable for {extension}")
    if extension == "blend":
        result = bpy.ops.wm.open_mainfile(filepath=source, load_ui=False)
    else:
        group, name = IMPORTERS[extension]
        options = {"filepath": source}
        if extension == "fbx":
            options["use_anim"] = False
        elif extension in ("usd", "usda", "usdc", "usdz"):
            options.update(import_cameras=False, import_lights=False,
                           import_usd_preview=True, import_textures_mode="IMPORT_PACK")
        result = getattr(getattr(bpy.ops, group), name)(**options)
    if "FINISHED" not in result:
        raise RuntimeError(f"Import failed: {result}")
    scene = bpy.context.scene
    camera = prepare_alembic_scene(scene) if extension == "abc" else None
    bpy.context.view_layer.update()

    objects = [obj for obj in bpy.context.scene.objects
               if obj.type in ("MESH", "CURVE", "SURFACE", "FONT") and not obj.hide_render]
    if not objects:
        raise RuntimeError("Import produced no renderable geometry")
    has_appearance = has_authored_appearance(objects)
    print("THUMBRELLA_HAS_APPEARANCE=" + json.dumps(has_appearance), flush=True)
    if extension == "abc":
        depsgraph = bpy.context.evaluated_depsgraph_get()
        evaluated = [obj.evaluated_get(depsgraph) for obj in objects]
    else:
        evaluated = objects
    corners = [obj.matrix_world @ Vector(corner) for obj in evaluated for corner in obj.bound_box]
    minimum = Vector(tuple(min(c[i] for c in corners) for i in range(3)))
    maximum = Vector(tuple(max(c[i] for c in corners) for i in range(3)))
    center = (minimum + maximum) / 2
    radius = (maximum - minimum).length / 2
    if not math.isfinite(radius) or radius <= 0:
        raise RuntimeError("Geometry has empty or invalid bounds")

    if camera is None:
        angle = math.radians(20)
        direction = Vector((math.sin(angle) * math.cos(angle),
                            -math.cos(angle) * math.cos(angle), math.sin(angle)))
        distance = radius / math.sin(math.radians(45 / 2)) * 1.2
        camera_data = bpy.data.cameras.new("ThumbnailCamera")
        camera = bpy.data.objects.new("ThumbnailCamera", camera_data)
        scene.collection.objects.link(camera)
        camera.location = center + direction * distance
        camera.rotation_euler = (center - camera.location).to_track_quat("-Z", "Y").to_euler()
        camera_data.sensor_fit = "HORIZONTAL"
        camera_data.lens = camera_data.sensor_width / (2 * math.tan(math.radians(45 / 2)))
        camera_data.clip_start = max(distance / 1000, 0.00001)
        camera_data.clip_end = distance + radius * 2

    scene.camera = camera
    scene.render.engine = "BLENDER_WORKBENCH"
    scene.display.render_aa = "OFF"
    scene.display.shading.light = "STUDIO"
    scene.display.shading.color_type = "TEXTURE"
    scene.display.shading.show_shadows = False
    scene.display.shading.show_cavity = False
    scene.render.resolution_x = 512
    scene.render.resolution_y = 512
    scene.render.resolution_percentage = 100
    scene.render.film_transparent = True
    scene.render.use_compositing = False
    scene.render.use_sequencer = False
    scene.render.image_settings.file_format = "PNG"
    scene.render.image_settings.color_mode = "RGBA"
    scene.render.filepath = output
    scene.view_settings.view_transform = "Standard"
    scene.view_settings.exposure = 1.0 if has_appearance else 0.0
    bpy.ops.render.render(write_still=True)
    if not os.path.isfile(output):
        raise RuntimeError("Blender did not produce an image")


def main():
    args = sys.argv[sys.argv.index("--") + 1:]
    if args == ["probe"]:
        print("THUMBRELLA_FORMATS=" + json.dumps(supported_formats()), flush=True)
    elif len(args) == 4 and args[0] == "render":
        render(*args[1:])
    else:
        raise RuntimeError("Expected probe or render INPUT OUTPUT EXTENSION")


if __name__ == "__main__":
    main()
