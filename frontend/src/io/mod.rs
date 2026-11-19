pub mod obj;
pub mod stl;
pub mod threemf;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use bevy::prelude::*;
use crate::bridge::ffi::{self, DecomposedPartInfo, RenderVertex};
use crate::color_optics::{format_hex_rgba, rgba8_to_srgba, unpack_rgba8};
use crate::config::{
    ObjectFilamentMode, SlicerConfig, ThreeMfFilamentSlot, DEFAULT_FILAMENT_SLOT_PALETTE,
};
use crate::hull::{
    build_surface_patches, SimplifiedConvexHull, SurfacePatch, MAX_SIMPLIFIED_HULL_FACES,
};
use crate::io::threemf::compute_vertex_filament_slots_from_triangles;

/// Metadata for a single build plate parsed from a `.3mf` archive.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoadedPlateInfo {
    pub name: String,
}

/// Complete CPU-side payload for a single object loaded and processed by the C++ backend.
#[derive(Clone, Debug)]
pub struct LoadedObjectData {
    pub object_id: u64,
    pub name: String,
    pub source_file: String,
    pub has_brep: bool,
    /// Hierarchical STEP assembly tree path from root assembly to leaf component/body.
    pub assembly_path: Vec<String>,
    /// Original 3D CAD assembly coordinates `(X, Y, Z)` (centered in XY, with relative Z height preserved)
    /// for Orca-style 3D Assembly View inspection and subset picking.
    pub assembly_translation: Vec3,
    /// STEP material / appearance name extracted from XCAF or STEP product metadata (if present).
    pub step_material_name: String,
    /// STEP material density ($\text{g/cm}^3$) extracted from XCAF material metadata (`0.0` if unspecified).
    pub step_material_density_g_cm3: f32,
    /// Whether this object is included in slicing and G-code generation (default `true`; imported from `.3mf`).
    pub printable: bool,
    /// Zero-based index of the build plate this object belongs to (`0` = Plate 1).
    pub plate_index: usize,
    pub initial_translation: Vec3,
    pub initial_rotation: Quat,
    /// Original unscaled 100% mesh vertices in object-local coordinates.
    pub base_vertices: Vec<RenderVertex>,
    /// Per-axis scale multipliers (`Vec3::ONE` = `100%` across X, Y, Z).
    pub scale_factors: Vec3,
    /// Unit quaternion frame in which `scale_factors` is applied (`Quat::IDENTITY` by default).
    pub scale_rotation: Quat,
    /// Whether X, Y, Z scale axes are locked together (proportional scaling).
    pub lock_scale_axes: bool,
    pub vertices: Vec<RenderVertex>,
    pub indices: Vec<u32>,
    pub local_min: Vec3,
    pub local_max: Vec3,
    pub hull: SimplifiedConvexHull,
    pub surface_patches: Vec<SurfacePatch>,
    pub triangle_to_patch: Vec<u32>,
    /// Per-triangle 0-based 3MF filament/colour slot indices (`0..255`), or empty for single-colour objects.
    pub triangle_filament_slots: Vec<u8>,
    /// Per-vertex 0-based 3MF filament/colour slot indices (`0..255`), or empty for single-colour objects.
    pub vertex_filament_slots: Vec<u8>,
    /// Per-triangle packed `0xRRGGBBAA` sRGBA colours (from 3MF `<basematerials>`, `<colorgroup>`, or `<texture2dgroup>`), or empty.
    pub triangle_colors_rgba: Vec<u32>,
    /// Per-vertex `[R, G, B, A]` sRGBA8 colours for full-colour / textured 3MF viewport rendering, or empty.
    pub vertex_colors_rgba: Vec<[u8; 4]>,
    /// Per-object filament / colour mixing mode override (`ProjectDefault` by default).
    pub filament_mode: ObjectFilamentMode,
    /// Optional per-object target sRGBA colour (`[R, G, B, A]` in `0.0..=1.0`) for unpainted objects
    /// when using per-object CMYKW optical colour mixing or custom filament tinting.
    pub custom_color_srgba: Option<[f32; 4]>,
}

/// Summary of an asynchronous multi-file import operation.
#[derive(Clone, Debug, Default)]
pub struct LoadedBatchReport {
    pub objects: Vec<LoadedObjectData>,
    /// Multi-plate definitions parsed from the imported `.3mf` archive (if present).
    pub plates: Vec<LoadedPlateInfo>,
    /// Multi-colour / filament slot definitions parsed from the imported `.3mf` archive (if present).
    pub filament_slots: Vec<ThreeMfFilamentSlot>,
    pub restored_config: Option<SlicerConfig>,
    /// File name (e.g. `"cowling_project.3mf"`) when a loaded `.3mf` archive is a Manifold project.
    pub manifold_project_filename: Option<String>,
    pub notices: Vec<String>,
    pub errors: Vec<String>,
}

/// Loads one or more CAD/mesh/project files (`.step`, `.stp`, `.stl`, `.obj`, `.3mf`)
/// through the C++ backend and builds their local meshes and $\le 27$-face convex hulls.
pub fn load_files_into_backend(paths: &[PathBuf]) -> LoadedBatchReport {
    let mut report = LoadedBatchReport::default();

    for path in paths {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();

        match ext.as_str() {
            "step" | "stp" => {
                load_step_file_parts(path, & path.to_string_lossy(), &mut report);
            }
            "stl" => match stl::load_stl_file(path) {
                Ok(obj) => report.objects.push(obj),
                Err(err) => report.errors.push(format!("{}: {err}", path.display())),
            },
            "obj" => {
                obj::load_obj_file_into_report(path, &mut report);
            }
            "3mf" => {
                threemf::load_3mf_archive(path, &mut report);
            }
            other => {
                report.errors.push(format!(
                    "Unsupported file extension '.{other}' for {}",
                    path.display()
                ));
            }
        }
    }

    report
}

/// Decomposes a STEP file via the C++ OCCT backend (`ffi::load_step_assembly`)
/// and constructs `LoadedObjectData` for every solid/shell part in the assembly,
/// preserving the original 3D assembly hierarchy, 3D assembly positions, and STEP material/colour attributes.
pub fn load_step_file_parts(
    step_path: &Path,
    display_source: &str,
    report: &mut LoadedBatchReport,
) -> Vec<LoadedObjectData> {
    let path_str = step_path.to_string_lossy();
    let decomposed = ffi::load_step_assembly(&path_str);

    if decomposed.is_empty() {
        report
            .errors
            .push(format!("OCCT failed to read or tessellate STEP file: {display_source}"));
        return Vec::new();
    }

    if decomposed.len() > 1 {
        report.notices.push(format!(
            "Decomposed STEP assembly '{}' into {} independent parts.",
            step_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(display_source),
            decomposed.len()
        ));
    }

    let mut loaded_parts = Vec::with_capacity(decomposed.len());
    for part in decomposed {
        if let Some(obj) = build_loaded_object_from_part(part, display_source) {
            loaded_parts.push(obj);
        }
    }

    // Center the STEP part (or multi-part STEP assembly as a group) at the bed center `(0, 0)`
    // in scene coordinates while preserving relative XY offsets between sibling parts, and
    // preserve relative 3D Z heights in `assembly_translation` for Orca-style 3D Assembly View.
    if !loaded_parts.is_empty() {
        let mut min_xy = Vec2::splat(f32::INFINITY);
        let mut max_xy = Vec2::splat(f32::NEG_INFINITY);
        let mut min_asm_z = f32::INFINITY;
        for obj in &loaded_parts {
            let t_xy = obj.initial_translation.xy();
            min_xy = min_xy.min(t_xy + obj.local_min.xy());
            max_xy = max_xy.max(t_xy + obj.local_max.xy());
            min_asm_z = min_asm_z.min(obj.assembly_translation.z + obj.local_min.z);
        }
        if min_xy.x.is_finite() && max_xy.x.is_finite() {
            let assembly_center_xy = 0.5 * (min_xy + max_xy);
            let z_base = if min_asm_z.is_finite() { min_asm_z } else { 0.0 };
            for obj in &mut loaded_parts {
                obj.initial_translation.x -= assembly_center_xy.x;
                obj.initial_translation.y -= assembly_center_xy.y;
                obj.assembly_translation.x -= assembly_center_xy.x;
                obj.assembly_translation.y -= assembly_center_xy.y;
                obj.assembly_translation.z = (obj.assembly_translation.z - z_base).max(0.0);
            }
        }
    }

    // Extract distinct STEP materials and colours across all parts/triangles and map them
    // into `report.filament_slots` and per-triangle/per-vertex filament slot assignments.
    let any_step_color_or_mat = loaded_parts.iter().any(|o| {
        !o.triangle_colors_rgba.is_empty()
            || !o.step_material_name.is_empty()
            || o.custom_color_srgba.is_some()
    });
    if any_step_color_or_mat {
        let mut slot_lookup: HashMap<(u32, String), u8> = HashMap::new();
        for existing in &report.filament_slots {
            let rgba = crate::color_optics::pack_rgba8(
                crate::color_optics::srgba_to_rgba8(existing.color_srgba),
            );
            slot_lookup.insert(
                (rgba, existing.filament_type.clone()),
                existing.slot_index as u8,
            );
        }

        let mut get_or_create_step_slot = |rgba: u32, mat_name: &str| -> u8 {
            let normalized_mat = mat_name.trim().to_string();
            let key = (rgba, normalized_mat.clone());
            if let Some(&idx) = slot_lookup.get(&key) {
                return idx;
            }
            if rgba != 0 {
                if let Some((&(_, _), &idx)) =
                    slot_lookup.iter().find(|((c, _), _)| *c == rgba)
                {
                    return idx;
                }
            }
            let slot_idx = report.filament_slots.len();
            let color_srgba = if rgba != 0 {
                rgba8_to_srgba(unpack_rgba8(rgba))
            } else {
                DEFAULT_FILAMENT_SLOT_PALETTE[slot_idx % DEFAULT_FILAMENT_SLOT_PALETTE.len()].1
            };
            let hex_label = if rgba != 0 {
                format_hex_rgba(unpack_rgba8(rgba))
            } else {
                let p = crate::color_optics::srgba_to_rgba8(color_srgba);
                format_hex_rgba(p)
            };
            let mat_label = if normalized_mat.is_empty() {
                "STEP".to_string()
            } else {
                normalized_mat.clone()
            };
            let slot_name = if normalized_mat.is_empty() {
                format!("Slot {} ({})", slot_idx + 1, hex_label)
            } else {
                format!("Slot {} ({} {})", slot_idx + 1, normalized_mat, hex_label)
            };
            let mut slot_entry = ThreeMfFilamentSlot::default_for_slot(slot_idx, slot_idx);
            slot_entry.name = slot_name;
            slot_entry.filament_type = mat_label;
            slot_entry.color_hex = hex_label;
            slot_entry.color_srgba = color_srgba;
            report.filament_slots.push(slot_entry);
            let idx_u8 = (slot_idx.min(254)) as u8;
            slot_lookup.insert(key, idx_u8);
            idx_u8
        };

        for obj in &mut loaded_parts {
            let num_tris = obj.indices.len() / 3;
            if num_tris == 0 {
                continue;
            }
            if !obj.triangle_colors_rgba.is_empty() && obj.triangle_colors_rgba.len() == num_tris {
                let mut tri_slots = Vec::with_capacity(num_tris);
                for &rgba in &obj.triangle_colors_rgba {
                    let s = get_or_create_step_slot(rgba, &obj.step_material_name);
                    tri_slots.push(s);
                }
                let default_slot = tri_slots.first().copied().unwrap_or(0);
                obj.vertex_filament_slots = compute_vertex_filament_slots_from_triangles(
                    obj.vertices.len(),
                    &obj.indices,
                    &tri_slots,
                    default_slot,
                );
                ffi::set_object_triangle_filament_slots(obj.object_id, &tri_slots);
                ffi::set_object_triangle_colors_rgba(obj.object_id, &obj.triangle_colors_rgba);
                obj.triangle_filament_slots = tri_slots;
            } else if !obj.step_material_name.is_empty() {
                let s = get_or_create_step_slot(0, &obj.step_material_name);
                let tri_slots = vec![s; num_tris];
                obj.vertex_filament_slots = vec![s; obj.vertices.len()];
                ffi::set_object_triangle_filament_slots(obj.object_id, &tri_slots);
                obj.triangle_filament_slots = tri_slots;
            }
        }

        if !report.filament_slots.is_empty() {
            report.notices.push(format!(
                "Imported {} STEP material/colour definition(s) from '{}'.",
                report.filament_slots.len(),
                step_path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or(display_source)
            ));
        }
    }

    for obj in &loaded_parts {
        report.objects.push(obj.clone());
    }
    loaded_parts
}

/// Fetches the centered vertices and indices from the C++ backend for `part.object_id`,
/// computes local AABB bounds, builds its $\le 27$-face `SimplifiedConvexHull`, and segments
/// its surface into selectable `SurfacePatch` regions (exact STEP `TopoDS_Face`s or contiguous
/// near-planar mesh regions).
pub fn build_loaded_object_from_part(
    part: DecomposedPartInfo,
    source_file: &str,
) -> Option<LoadedObjectData> {
    if part.object_id == 0 {
        return None;
    }

    let vertices = ffi::get_object_vertices(part.object_id);
    let indices = ffi::get_object_indices(part.object_id);
    if vertices.is_empty() || indices.is_empty() {
        return None;
    }

    let brep_face_ids = if part.has_brep {
        ffi::get_object_triangle_face_ids(part.object_id)
    } else {
        Vec::new()
    };

    let triangle_colors_rgba = if part.has_brep {
        ffi::get_object_triangle_colors_rgba(part.object_id)
    } else {
        Vec::new()
    };

    let num_tris = indices.len() / 3;
    let vertex_colors_rgba = if !triangle_colors_rgba.is_empty()
        && triangle_colors_rgba.len() == num_tris
    {
        let mut v_cols = vec![[220u8, 224u8, 230u8, 255u8]; vertices.len()];
        for (t, &packed) in triangle_colors_rgba.iter().enumerate() {
            if packed != 0 {
                let c = unpack_rgba8(packed);
                let i0 = indices[t * 3] as usize;
                let i1 = indices[t * 3 + 1] as usize;
                let i2 = indices[t * 3 + 2] as usize;
                if i0 < v_cols.len() {
                    v_cols[i0] = c;
                }
                if i1 < v_cols.len() {
                    v_cols[i1] = c;
                }
                if i2 < v_cols.len() {
                    v_cols[i2] = c;
                }
            }
        }
        v_cols
    } else {
        Vec::new()
    };

    let custom_color_srgba = if part.has_color && part.color_rgba != 0 {
        Some(rgba8_to_srgba(unpack_rgba8(part.color_rgba)))
    } else {
        None
    };

    let assembly_path = if part.assembly_path.is_empty() {
        vec![part.name.clone()]
    } else {
        part.assembly_path
    };

    let (local_min, local_max) = compute_vertex_bounds(&vertices);
    let hull = SimplifiedConvexHull::from_vertices(&vertices, MAX_SIMPLIFIED_HULL_FACES);
    let (surface_patches, triangle_to_patch) =
        build_surface_patches(&vertices, &indices, &brep_face_ids);

    Some(LoadedObjectData {
        object_id: part.object_id,
        name: part.name,
        source_file: source_file.to_string(),
        has_brep: part.has_brep,
        assembly_path,
        assembly_translation: Vec3::new(part.origin_x, part.origin_y, part.origin_z),
        step_material_name: part.material_name,
        step_material_density_g_cm3: part.material_density_g_cm3,
        printable: true,
        plate_index: 0,
        // Place at the assembly's XY coordinates with z=0 sitting on the print bed
        initial_translation: Vec3::new(part.origin_x, part.origin_y, 0.0),
        initial_rotation: Quat::IDENTITY,
        base_vertices: vertices.clone(),
        scale_factors: Vec3::ONE,
        scale_rotation: Quat::IDENTITY,
        lock_scale_axes: true,
        vertices,
        indices,
        local_min,
        local_max,
        hull,
        surface_patches,
        triangle_to_patch,
        triangle_filament_slots: Vec::new(),
        vertex_filament_slots: Vec::new(),
        triangle_colors_rgba,
        vertex_colors_rgba,
        filament_mode: ObjectFilamentMode::default(),
        custom_color_srgba,
    })
}

pub fn compute_vertex_bounds(vertices: &[RenderVertex]) -> (Vec3, Vec3) {
    let mut min_pt = Vec3::splat(f32::INFINITY);
    let mut max_pt = Vec3::splat(f32::NEG_INFINITY);
    for v in vertices {
        let p = Vec3::new(v.x, v.y, v.z);
        min_pt = min_pt.min(p);
        max_pt = max_pt.max(p);
    }
    if min_pt.x.is_infinite() {
        (Vec3::ZERO, Vec3::ZERO)
    } else {
        (min_pt, max_pt)
    }
}

/// Scales `base_vertices` along the axes of unit quaternion frame `rotation` by `scale`,
/// keeping the rotated XY centroid and Z minimum stationary and transforming normals by $R^{-1} S^{-1} R$.
pub fn scale_vertices_in_frame(
    base_vertices: &[RenderVertex],
    scale: Vec3,
    rotation: Quat,
) -> Vec<RenderVertex> {
    if base_vertices.is_empty() {
        return Vec::new();
    }
    let rot = if rotation.is_finite() && rotation.length_squared() > 1.0e-8 {
        rotation.normalize()
    } else {
        Quat::IDENTITY
    };
    let inv_rot = rot.inverse();
    let s = Vec3::new(
        if scale.x.is_finite() { scale.x } else { 1.0 },
        if scale.y.is_finite() { scale.y } else { 1.0 },
        if scale.z.is_finite() { scale.z } else { 1.0 },
    )
    .clamp(Vec3::splat(0.001), Vec3::splat(1000.0));

    let mut w_min = Vec3::splat(f32::INFINITY);
    let mut w_max = Vec3::splat(f32::NEG_INFINITY);
    for bv in base_vertices {
        let pw = rot * Vec3::new(bv.x, bv.y, bv.z);
        w_min = w_min.min(pw);
        w_max = w_max.max(pw);
    }
    let pivot = Vec3::new(
        0.5 * (w_min.x + w_max.x),
        0.5 * (w_min.y + w_max.y),
        w_min.z,
    );

    base_vertices
        .iter()
        .map(|bv| {
            let pw = rot * Vec3::new(bv.x, bv.y, bv.z);
            let pw_scaled = pivot + (pw - pivot) * s;
            let pl = inv_rot * pw_scaled;

            let nw = rot * Vec3::new(bv.nx, bv.ny, bv.nz);
            let nw_scaled = Vec3::new(nw.x / s.x, nw.y / s.y, nw.z / s.z);
            let nl = (inv_rot * nw_scaled).normalize_or_zero();
            let nl = if nl.length_squared() > 0.5 {
                nl
            } else {
                Vec3::new(bv.nx, bv.ny, bv.nz)
            };
            RenderVertex {
                x: pl.x,
                y: pl.y,
                z: pl.z,
                nx: nl.x,
                ny: nl.y,
                nz: nl.z,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_object_scaling_uniform_nonuniform_and_bbox_dimensions() {
        let _guard = crate::bridge::TEST_BACKEND_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());

        // Register a 10 x 20 x 30 mm box in the C++ backend
        let positions: [f32; 24] = [
            -5.0, -10.0, 0.0,  5.0, -10.0, 0.0,  5.0, 10.0, 0.0, -5.0, 10.0, 0.0,
            -5.0, -10.0, 30.0, 5.0, -10.0, 30.0, 5.0, 10.0, 30.0, -5.0, 10.0, 30.0,
        ];
        let indices: [u32; 36] = [
            0, 2, 1, 0, 3, 2, // bottom
            4, 5, 6, 4, 6, 7, // top
            0, 1, 5, 0, 5, 4, // front
            1, 2, 6, 1, 6, 5, // right
            2, 3, 7, 2, 7, 6, // back
            3, 0, 4, 3, 4, 7, // left
        ];
        let part = ffi::register_triangle_mesh("scale_test_box", &positions, &indices);
        assert!(part.object_id > 0);
        let obj = build_loaded_object_from_part(part, "scale_test.stl").expect("loaded box");

        let base_ext = obj.local_max - obj.local_min;
        assert!((base_ext - Vec3::new(10.0, 20.0, 30.0)).length() < 1e-4);

        // 1. Uniform proportional scale (150%) -> 15 x 30 x 45 mm
        let s_uniform = Vec3::splat(1.5);
        assert!(ffi::set_object_scale(
            obj.object_id,
            s_uniform.to_array(),
            Quat::IDENTITY.to_array()
        ));
        let v_uniform = scale_vertices_in_frame(&obj.base_vertices, s_uniform, Quat::IDENTITY);
        let (min_u, max_u) = compute_vertex_bounds(&v_uniform);
        assert!(((max_u - min_u) - Vec3::new(15.0, 30.0, 45.0)).length() < 1e-4);

        let cpp_v_u = ffi::get_object_vertices(obj.object_id);
        let (cpp_min_u, cpp_max_u) = compute_vertex_bounds(&cpp_v_u);
        assert!(((cpp_max_u - cpp_min_u) - Vec3::new(15.0, 30.0, 45.0)).length() < 1e-4);

        // 2. Non-uniform scale to specific bounding-box dimensions (25 x 40 x 15 mm) under a 35-deg X rotation
        let rot = Quat::from_rotation_x(35.0_f32.to_radians());
        let mut b_min = Vec3::splat(f32::INFINITY);
        let mut b_max = Vec3::splat(f32::NEG_INFINITY);
        for bv in &obj.base_vertices {
            let pw = rot * Vec3::new(bv.x, bv.y, bv.z);
            b_min = b_min.min(pw);
            b_max = b_max.max(pw);
        }
        let rot_base_ext = b_max - b_min;
        let target_bbox = Vec3::new(25.0, 40.0, 15.0);
        let s_nonuniform = target_bbox / rot_base_ext;

        assert!(ffi::set_object_scale(
            obj.object_id,
            s_nonuniform.to_array(),
            rot.to_array()
        ));
        let v_nonuniform = scale_vertices_in_frame(&obj.base_vertices, s_nonuniform, rot);

        let mut w_min = Vec3::splat(f32::INFINITY);
        let mut w_max = Vec3::splat(f32::NEG_INFINITY);
        for v in &v_nonuniform {
            let pw = rot * Vec3::new(v.x, v.y, v.z);
            w_min = w_min.min(pw);
            w_max = w_max.max(pw);
        }
        assert!(
            ((w_max - w_min) - target_bbox).length() < 1e-3,
            "Expected world bbox {:?}, got {:?}",
            target_bbox,
            w_max - w_min
        );

        // 3. Reset to 100% -> restores 10 x 20 x 30 mm
        assert!(ffi::set_object_scale(
            obj.object_id,
            Vec3::ONE.to_array(),
            Quat::IDENTITY.to_array()
        ));
        let cpp_v_reset = ffi::get_object_vertices(obj.object_id);
        let (r_min, r_max) = compute_vertex_bounds(&cpp_v_reset);
        assert!(((r_max - r_min) - Vec3::new(10.0, 20.0, 30.0)).length() < 1e-4);

        let _ = ffi::remove_cad_object(obj.object_id);
    }

    #[test]
    fn test_step_assembly_hierarchy_and_material_color_import() {
        let _guard = crate::bridge::TEST_BACKEND_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());

        let step_path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("Test Objects/A4T Cowling - Dragon-Rapido UHF [xol-carriage].step");
        if !step_path.exists() {
            return;
        }

        let report = load_files_into_backend(&[step_path]);
        assert!(
            report.errors.is_empty(),
            "Unexpected STEP load errors: {:?}",
            report.errors
        );
        assert!(!report.objects.is_empty(), "Expected at least 1 STEP part");

        let first = &report.objects[0];
        assert!(first.has_brep);
        assert!(
            !first.assembly_path.is_empty(),
            "Expected non-empty STEP assembly path"
        );
        assert!(
            !first.step_material_name.is_empty(),
            "Expected STEP material name to be extracted (got empty)"
        );
        assert!(
            !first.triangle_colors_rgba.is_empty(),
            "Expected STEP face/solid colours to be extracted"
        );
        assert_eq!(first.triangle_colors_rgba.len(), first.indices.len() / 3);
        assert_eq!(first.triangle_filament_slots.len(), first.indices.len() / 3);
        assert!(
            !report.filament_slots.is_empty(),
            "Expected STEP material/colour definitions to populate filament_slots"
        );

        for obj in &report.objects {
            let _ = ffi::remove_cad_object(obj.object_id);
        }
    }
}
