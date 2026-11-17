pub mod obj;
pub mod stl;
pub mod threemf;

use std::path::{Path, PathBuf};
use bevy::prelude::*;
use crate::bridge::ffi::{self, DecomposedPartInfo, RenderVertex};
use crate::config::{SlicerConfig, ThreeMfFilamentSlot};
use crate::hull::{
    build_surface_patches, SimplifiedConvexHull, SurfacePatch, MAX_SIMPLIFIED_HULL_FACES,
};

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
/// and constructs `LoadedObjectData` for every solid/shell part in the assembly.
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
    // in scene coordinates while preserving relative XY offsets between sibling parts.
    if !loaded_parts.is_empty() {
        let mut min_xy = Vec2::splat(f32::INFINITY);
        let mut max_xy = Vec2::splat(f32::NEG_INFINITY);
        for obj in &loaded_parts {
            let t_xy = obj.initial_translation.xy();
            min_xy = min_xy.min(t_xy + obj.local_min.xy());
            max_xy = max_xy.max(t_xy + obj.local_max.xy());
        }
        if min_xy.x.is_finite() && max_xy.x.is_finite() {
            let assembly_center_xy = 0.5 * (min_xy + max_xy);
            for obj in &mut loaded_parts {
                obj.initial_translation.x -= assembly_center_xy.x;
                obj.initial_translation.y -= assembly_center_xy.y;
            }
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

    let (local_min, local_max) = compute_vertex_bounds(&vertices);
    let hull = SimplifiedConvexHull::from_vertices(&vertices, MAX_SIMPLIFIED_HULL_FACES);
    let (surface_patches, triangle_to_patch) =
        build_surface_patches(&vertices, &indices, &brep_face_ids);

    Some(LoadedObjectData {
        object_id: part.object_id,
        name: part.name,
        source_file: source_file.to_string(),
        has_brep: part.has_brep,
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
        triangle_colors_rgba: Vec::new(),
        vertex_colors_rgba: Vec::new(),
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
}
