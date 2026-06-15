# Importing a Blender scene

The 3D viewport loads **`assets/scenes/fixtures.glb`**. Any object named
`Light.000`, `Light.001`, … becomes a controllable light **fixture** mapped to
timeline light `0`, `1`, …. Every other mesh in the file is shown as **static set
geometry** (stage, trusses, floor, …). As you build the show, the fixtures glow
on/off to mirror the timeline at the current frame.

## Quick start

1. **Model your scene** in Blender — light fixtures plus any set pieces.
2. **Name each light fixture** `Light.000`, `Light.001`, … The trailing number is
   the timeline light index. (Double-click the object in the Outliner, or use
   Object Properties → name. Blender's automatic `.001` duplicate suffix already
   matches this convention, so duplicating one fixture gives you `Light.001`,
   `Light.002`, … for free.)
3. **Lay it out near the origin.** The app's camera looks from `(0, 3.5, 13)`
   toward the origin (Y-up). Put fixtures around `y ≈ 0.6` facing +Z. Scale to taste.
4. *(Recommended)* **Apply transforms:** `Object → Apply → All Transforms`, so the
   exported node transforms are clean.
5. **Export:** `File → Export → glTF 2.0 (.glb / .gltf)`
   - **Format:** glTF Binary (`.glb`)
   - **Include:** Selected Objects (make sure your `Light.*` objects *and* set
     pieces are selected) — or the whole scene.
   - **Transform:** +Y Up (the default; Bevy is Y-up).
   - **Geometry:** enable *Apply Modifiers*; keep *Normals* and *UVs* on; *Materials → Export*.
6. **Save it as `assets/scenes/fixtures.glb`** (overwrite the bundled sample).
7. Run `trunk serve` (it also rebuilds on change) and refresh the browser. Open a
   project — your scene appears and the fixtures light up with the timeline.

## How the mapping works

- Node name `Light.<n>` → timeline light `n`. Each fixture gets its **own material**;
  the app overrides its **emissive** every frame (on = warm glow, off = dark). The
  fixture's base color is replaced by the app's on/off colors — tweak them in
  `src/scene.rs` → `SceneConfig` (`on_color`, `off_color`, `emissive_strength`).
- Non-`Light` meshes keep their glTF materials and are rendered as static geometry.
- Lights beyond the project's light count stay off; a timeline light with no
  matching fixture simply has no 3D visual.

| Blender object name | Role |
|---|---|
| `Light.000` | timeline light 0 (toggleable) |
| `Light.001` | timeline light 1 (toggleable) |
| `Light.002` | timeline light 2 (toggleable) |
| `Stage`, `Truss`, `Floor`, … | static set geometry |

## Notes & limitations

- **Flat hierarchy assumed.** Node transforms are used directly, so deeply
  parented objects may be misplaced — un-parent them or apply transforms before export.
- Cameras, empties, and Blender lamps without a mesh are ignored; the app supplies
  its own camera and lighting.
- Only the **first** glTF scene in the file is loaded.
- If the `.glb` is missing or has no `Light.<n>` objects, the app falls back to a
  procedural row of fixtures.
- To load a different file/path, change `SCENE_GLB` in `src/scene.rs`.
