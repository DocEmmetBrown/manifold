use std::collections::HashMap;
use std::fs;
use std::io::BufReader;
use std::path::{Path, PathBuf};

use bevy::math::Vec3;
use bevy::render::mesh::{Indices, VertexAttributeValues};

use crate::bridge::ffi;
use crate::color_optics::{format_hex_rgba, pack_rgba8, rgba8_to_srgba};
use crate::config::{ThreeMfFilamentSlot, DEFAULT_FILAMENT_SLOT_PALETTE};
use crate::io::threemf::{
    compute_vertex_filament_slots_from_triangles, DecodedRgbaImage, TextureTileStyle,
};
use crate::io::{build_loaded_object_from_part, LoadedBatchReport, LoadedObjectData};

/// Parses a Wavefront `.obj` file from disk (using `bevy_obj` and `tobj`) and registers
/// the resulting mesh in the C++ backend.
pub fn load_obj_file(path: &Path) -> Result<LoadedObjectData, String> {
    let mut report = LoadedBatchReport::default();
    load_obj_file_into_report(path, &mut report);
    if let Some(obj) = report.objects.pop() {
        Ok(obj)
    } else if let Some(err) = report.errors.pop() {
        Err(err)
    } else {
        Err(format!("No mesh geometry found in OBJ file {}", path.display()))
    }
}

/// Loads a Wavefront `.obj` file (and optional companion `.mtl` material library and `.png`
/// diffuse textures in the same directory) into `report`, populating both geometry and any
/// multi-material filament slots, vertex colours (`v x y z r g b`), or UV-mapped PNG textures.
pub fn load_obj_file_into_report(path: &Path, report: &mut LoadedBatchReport) {
    let bytes = match fs::read(path) {
        Ok(b) => b,
        Err(e) => {
            report
                .errors
                .push(format!("Failed to read OBJ file {}: {e}", path.display()));
            return;
        }
    };

    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("obj_model");
    let source_label = path.to_string_lossy().into_owned();
    let parent_dir: Option<PathBuf> = path.parent().map(Path::to_path_buf);

    let mtl_dir = parent_dir.clone();
    let png_dir = parent_dir;

    match load_obj_bytes_into_report(
        &bytes,
        stem,
        &source_label,
        |mtl_rel_path| {
            let base = mtl_dir.as_ref()?;
            let candidate = resolve_relative_asset_path(base, mtl_rel_path);
            fs::read(candidate).ok()
        },
        |png_rel_name| {
            let base = png_dir.as_ref()?;
            let candidate = resolve_relative_asset_path(base, Path::new(png_rel_name));
            fs::read(candidate).ok()
        },
        report,
    ) {
        Ok(obj) => report.objects.push(obj),
        Err(err) => report.errors.push(format!("{}: {err}", path.display())),
    }
}

fn resolve_relative_asset_path(base_dir: &Path, rel: &Path) -> PathBuf {
    let direct = base_dir.join(rel);
    if direct.exists() {
        return direct;
    }
    if let Some(fname) = rel.file_name() {
        let flat = base_dir.join(fname);
        if flat.exists() {
            return flat;
        }
    }
    direct
}

/// Parses Wavefront `.obj` bytes via `bevy_obj::load_obj_from_bytes` and `tobj::load_obj_buf`,
/// resolving optional `.mtl` materials, UV-mapped PNG textures (`map_Kd`), and per-vertex RGB
/// colours (`v x y z r g b`), and registers the triangle mesh in the C++ backend.
pub fn load_obj_bytes_into_report<FMtl, FPng>(
    obj_bytes: &[u8],
    name: &str,
    source_label: &str,
    mtl_lookup: FMtl,
    png_lookup: FPng,
    report: &mut LoadedBatchReport,
) -> Result<LoadedObjectData, String>
where
    FMtl: Fn(&Path) -> Option<Vec<u8>>,
    FPng: Fn(&str) -> Option<Vec<u8>>,
{
    // 1. Parse via `tobj::load_obj_buf` with `.mtl` material resolution so we can extract
    //    per-submesh material slots (`usemtl`), diffuse colours (`Kd`), UV PNG textures (`map_Kd`),
    //    and Wavefront per-vertex colours (`v x y z r g b`) in a single pass.
    let mut reader = BufReader::new(obj_bytes);
    let (models, mtl_res) = tobj::load_obj_buf(
        &mut reader,
        &tobj::GPU_LOAD_OPTIONS,
        |mtl_path| match mtl_lookup(mtl_path) {
            Some(mtl_bytes) => {
                let mut mtl_reader = BufReader::new(mtl_bytes.as_slice());
                tobj::load_mtl_buf(&mut mtl_reader)
            }
            None => Err(tobj::LoadError::OpenFileFailed),
        },
    )
    .map_err(|e| format!("OBJ parse error in {source_label}: {e}"))?;

    let materials = match mtl_res {
        Ok(mats) => mats,
        Err(err) => {
            report.notices.push(format!(
                "OBJ '{name}': companion MTL library could not be loaded ({err}); using mesh geometry."
            ));
            Vec::new()
        }
    };

    // If `tobj` produced no models, fall back to `bevy_obj::load_obj_from_bytes` directly.
    if models.is_empty() {
        let bevy_mesh = bevy_obj::load_obj_from_bytes(obj_bytes)
            .map_err(|e| format!("OBJ parse error in {source_label}: {e}"))?;
        return register_from_bevy_obj_mesh(&bevy_mesh, name, source_label);
    }

    let total_verts: usize = models.iter().map(|m| m.mesh.positions.len() / 3).sum();
    let total_indices: usize = models.iter().map(|m| m.mesh.indices.len()).sum();
    if total_verts == 0 || total_indices < 3 {
        let bevy_mesh = bevy_obj::load_obj_from_bytes(obj_bytes)
            .map_err(|e| format!("OBJ parse error in {source_label}: {e}"))?;
        return register_from_bevy_obj_mesh(&bevy_mesh, name, source_label);
    }

    // Decode any referenced `map_Kd` PNG textures once.
    let mut decoded_textures: HashMap<String, Option<DecodedRgbaImage>> = HashMap::new();
    for mat in &materials {
        if let Some(ref tex_name) = mat.diffuse_texture {
            let trimmed = tex_name.trim();
            if !trimmed.is_empty() && !decoded_textures.contains_key(trimmed) {
                let decoded = png_lookup(trimmed).and_then(|b| DecodedRgbaImage::decode_png(&b));
                if decoded.is_none() {
                    report.notices.push(format!(
                        "OBJ '{name}': diffuse texture '{trimmed}' could not be loaded as PNG; falling back to material Kd colour."
                    ));
                }
                decoded_textures.insert(trimmed.to_string(), decoded);
            }
        }
    }

    // Map untextured `.mtl` material indices to 0-based `filament_slot`s in `report.filament_slots`
    // only when materials do not rely on a UV `map_Kd` texture (textured models use continuous
    // `vertex_colors_rgba` / `triangle_colors_rgba` rather than a discrete solid-colour slot).
    let untextured_used_mats: Vec<usize> = models
        .iter()
        .filter_map(|m| m.mesh.material_id)
        .filter(|&mat_id| {
            mat_id < materials.len()
                && materials[mat_id]
                    .diffuse_texture
                    .as_deref()
                    .and_then(|t| decoded_textures.get(t.trim()))
                    .and_then(|opt| opt.as_ref())
                    .is_none()
        })
        .collect();
    let create_discrete_slots = untextured_used_mats.len() >= 2
        || untextured_used_mats.iter().any(|&mat_id| {
            let rgba = material_diffuse_rgba8(&materials[mat_id], mat_id);
            rgba[0] < 250 || rgba[1] < 250 || rgba[2] < 250
        });

    let mut mat_id_to_slot: HashMap<usize, (Option<u8>, [u8; 4])> = HashMap::new();
    for model in &models {
        if let Some(mat_id) = model.mesh.material_id {
            if mat_id < materials.len() && !mat_id_to_slot.contains_key(&mat_id) {
                let mat = &materials[mat_id];
                let rgba8 = material_diffuse_rgba8(mat, mat_id);
                let has_decoded_tex = mat
                    .diffuse_texture
                    .as_deref()
                    .and_then(|t| decoded_textures.get(t.trim()))
                    .and_then(|opt| opt.as_ref())
                    .is_some();
                let slot_opt = if !has_decoded_tex && create_discrete_slots {
                    Some(find_or_insert_obj_filament_slot(
                        &mut report.filament_slots,
                        &mat.name,
                        rgba8,
                    ))
                } else {
                    None
                };
                mat_id_to_slot.insert(mat_id, (slot_opt, rgba8));
            }
        }
    }

    let total_tris = total_indices / 3;
    let mut positions: Vec<f32> = Vec::with_capacity(total_verts * 3);
    let mut indices: Vec<u32> = Vec::with_capacity(total_tris * 3);
    let mut tri_slots: Vec<u8> = Vec::with_capacity(total_tris);
    let mut tri_colors_rgba: Vec<u32> = Vec::with_capacity(total_tris);
    let mut vert_colors_rgba: Vec<[u8; 4]> = Vec::with_capacity(total_verts);

    let mut any_slot_assigned = false;
    let mut any_color_assigned = false;

    for model in &models {
        let mesh = &model.mesh;
        let v_count = mesh.positions.len() / 3;
        let t_count = mesh.indices.len() / 3;
        if v_count == 0 || t_count == 0 {
            continue;
        }

        let vertex_offset = (positions.len() / 3) as u32;
        positions.extend_from_slice(&mesh.positions[..v_count * 3]);

        let (model_slot, mat_rgba8, mat_tex) = match mesh.material_id {
            Some(mat_id) if mat_id < materials.len() => {
                let (slot_opt, rgba8) = mat_id_to_slot
                    .get(&mat_id)
                    .copied()
                    .unwrap_or((None, [220, 220, 220, 255]));
                let tex = materials[mat_id]
                    .diffuse_texture
                    .as_deref()
                    .and_then(|t| decoded_textures.get(t.trim()))
                    .and_then(|opt| opt.as_ref());
                if slot_opt.is_some() {
                    any_slot_assigned = true;
                }
                (slot_opt.unwrap_or(0), Some(rgba8), tex)
            }
            _ => (0u8, None, None),
        };

        let has_uvs = mesh.texcoords.len() >= v_count * 2;
        let has_vcolors = mesh.vertex_color.len() >= v_count * 3;
        // Detect whether `v x y z r g b` is authored in `0..=255` or normalized `0.0..=1.0`.
        let vcolor_scale = if has_vcolors
            && mesh.vertex_color[..v_count * 3]
                .iter()
                .any(|&c| c > 1.001)
        {
            1.0_f32
        } else {
            255.0_f32
        };

        let local_vert_start = vert_colors_rgba.len();
        for v_idx in 0..v_count {
            let v_rgba = if let (Some(tex), true) = (mat_tex, has_uvs) {
                any_color_assigned = true;
                let u = mesh.texcoords[v_idx * 2];
                let v = mesh.texcoords[v_idx * 2 + 1];
                let sampled = tex.sample_bilinear_srgba8(
                    u,
                    v,
                    TextureTileStyle::Wrap,
                    TextureTileStyle::Wrap,
                );
                if let Some(kd) = mat_rgba8 {
                    modulate_rgba8(sampled, kd)
                } else {
                    sampled
                }
            } else if has_vcolors {
                any_color_assigned = true;
                let r = (mesh.vertex_color[v_idx * 3] * vcolor_scale)
                    .round()
                    .clamp(0.0, 255.0) as u8;
                let g = (mesh.vertex_color[v_idx * 3 + 1] * vcolor_scale)
                    .round()
                    .clamp(0.0, 255.0) as u8;
                let b = (mesh.vertex_color[v_idx * 3 + 2] * vcolor_scale)
                    .round()
                    .clamp(0.0, 255.0) as u8;
                [r, g, b, 255]
            } else if let Some(kd) = mat_rgba8 {
                any_color_assigned = true;
                kd
            } else {
                [220, 220, 220, 255]
            };
            vert_colors_rgba.push(v_rgba);
        }

        for tri in mesh.indices[..t_count * 3].chunks_exact(3) {
            let i0 = (tri[0] as usize).min(v_count - 1);
            let i1 = (tri[1] as usize).min(v_count - 1);
            let i2 = (tri[2] as usize).min(v_count - 1);

            indices.push(vertex_offset + i0 as u32);
            indices.push(vertex_offset + i1 as u32);
            indices.push(vertex_offset + i2 as u32);
            tri_slots.push(model_slot);

            let tri_rgba = if let (Some(tex), true) = (mat_tex, has_uvs) {
                let u0 = mesh.texcoords[i0 * 2];
                let v0 = mesh.texcoords[i0 * 2 + 1];
                let u1 = mesh.texcoords[i1 * 2];
                let v1 = mesh.texcoords[i1 * 2 + 1];
                let u2 = mesh.texcoords[i2 * 2];
                let v2 = mesh.texcoords[i2 * 2 + 1];
                let um = (u0 + u1 + u2) / 3.0;
                let vm = (v0 + v1 + v2) / 3.0;
                let c0 = vert_colors_rgba[local_vert_start + i0];
                let c1 = vert_colors_rgba[local_vert_start + i1];
                let c2 = vert_colors_rgba[local_vert_start + i2];
                let mut cm = tex.sample_bilinear_srgba8(
                    um,
                    vm,
                    TextureTileStyle::Wrap,
                    TextureTileStyle::Wrap,
                );
                if let Some(kd) = mat_rgba8 {
                    cm = modulate_rgba8(cm, kd);
                }
                average_four_rgba8(c0, c1, c2, cm)
            } else {
                let c0 = vert_colors_rgba[local_vert_start + i0];
                let c1 = vert_colors_rgba[local_vert_start + i1];
                let c2 = vert_colors_rgba[local_vert_start + i2];
                average_three_rgba8(c0, c1, c2)
            };
            tri_colors_rgba.push(pack_rgba8(tri_rgba));
        }
    }

    let part_info = ffi::register_triangle_mesh(name, &positions, &indices);
    let mut obj = build_loaded_object_from_part(part_info, source_label)
        .ok_or_else(|| format!("Backend failed to register OBJ mesh '{name}'"))?;
    // Place newly imported standalone OBJ meshes at the bed center `(0, 0, 0)` on the build plate
    obj.initial_translation = Vec3::ZERO;

    if any_color_assigned && tri_colors_rgba.len() == obj.indices.len() / 3 {
        ffi::set_object_triangle_colors_rgba(obj.object_id, &tri_colors_rgba);
        obj.triangle_colors_rgba = tri_colors_rgba;
        if vert_colors_rgba.len() == obj.vertices.len() {
            obj.vertex_colors_rgba = vert_colors_rgba;
        }
    }

    if any_slot_assigned && tri_slots.len() == obj.indices.len() / 3 {
        ffi::set_object_triangle_filament_slots(obj.object_id, &tri_slots);
        obj.vertex_filament_slots = compute_vertex_filament_slots_from_triangles(
            obj.vertices.len(),
            &obj.indices,
            &tri_slots,
            0,
        );
        obj.triangle_filament_slots = tri_slots;
    }

    Ok(obj)
}

fn register_from_bevy_obj_mesh(
    bevy_mesh: &bevy::render::mesh::Mesh,
    name: &str,
    source_label: &str,
) -> Result<LoadedObjectData, String> {
    let Some(VertexAttributeValues::Float32x3(pos_attr)) =
        bevy_mesh.attribute(bevy::render::mesh::Mesh::ATTRIBUTE_POSITION)
    else {
        return Err(format!("OBJ mesh '{name}' contains no vertex positions"));
    };
    let indices: Vec<u32> = match bevy_mesh.indices() {
        Some(Indices::U32(idx)) => idx.clone(),
        Some(Indices::U16(idx)) => idx.iter().map(|&i| i as u32).collect(),
        None => (0..pos_attr.len() as u32).collect(),
    };
    if pos_attr.is_empty() || indices.len() < 3 {
        return Err(format!("OBJ mesh '{name}' contains no triangles"));
    }
    let mut positions = Vec::with_capacity(pos_attr.len() * 3);
    for p in pos_attr {
        positions.push(p[0]);
        positions.push(p[1]);
        positions.push(p[2]);
    }
    let part_info = ffi::register_triangle_mesh(name, &positions, &indices);
    let mut obj = build_loaded_object_from_part(part_info, source_label)
        .ok_or_else(|| format!("Backend failed to register OBJ mesh '{name}'"))?;
    obj.initial_translation = Vec3::ZERO;
    Ok(obj)
}

fn material_diffuse_rgba8(mat: &tobj::Material, fallback_idx: usize) -> [u8; 4] {
    let alpha = mat
        .dissolve
        .map(|d| (d.clamp(0.0, 1.0) * 255.0).round() as u8)
        .unwrap_or(255);
    if let Some(kd) = mat.diffuse {
        let scale = if kd.iter().any(|&c| c > 1.001) {
            1.0_f32
        } else {
            255.0_f32
        };
        [
            (kd[0] * scale).round().clamp(0.0, 255.0) as u8,
            (kd[1] * scale).round().clamp(0.0, 255.0) as u8,
            (kd[2] * scale).round().clamp(0.0, 255.0) as u8,
            alpha,
        ]
    } else {
        let (_, srgba) =
            DEFAULT_FILAMENT_SLOT_PALETTE[fallback_idx % DEFAULT_FILAMENT_SLOT_PALETTE.len()];
        [
            (srgba[0] * 255.0).round() as u8,
            (srgba[1] * 255.0).round() as u8,
            (srgba[2] * 255.0).round() as u8,
            alpha,
        ]
    }
}

fn find_or_insert_obj_filament_slot(
    slots: &mut Vec<ThreeMfFilamentSlot>,
    mat_name: &str,
    rgba8: [u8; 4],
) -> u8 {
    let hex = format_hex_rgba([rgba8[0], rgba8[1], rgba8[2], 255]);
    if let Some(existing) = slots.iter().find(|s| {
        s.color_hex.eq_ignore_ascii_case(&hex)
            && (mat_name.trim().is_empty() || s.name.contains(mat_name.trim()))
    }) {
        return existing.slot_index.min(254) as u8;
    }
    let next_idx = slots.len().min(254);
    let clean_name = mat_name.trim();
    let label = if clean_name.is_empty() {
        format!("Filament #{} (PLA {})", next_idx + 1, hex)
    } else {
        format!("Filament #{} ({})", next_idx + 1, clean_name)
    };
    slots.push(ThreeMfFilamentSlot {
        slot_index: next_idx as u32,
        name: label,
        filament_type: "PLA".to_string(),
        color_hex: hex,
        color_srgba: rgba8_to_srgba([rgba8[0], rgba8[1], rgba8[2], 255]),
        tool_index: next_idx,
    });
    next_idx as u8
}

#[inline]
fn modulate_rgba8(tex: [u8; 4], kd: [u8; 4]) -> [u8; 4] {
    // Only tint the texture if Kd is noticeably non-white (< 250 on any RGB channel)
    if kd[0] >= 250 && kd[1] >= 250 && kd[2] >= 250 {
        return tex;
    }
    [
        ((tex[0] as u16 * kd[0] as u16 + 127) / 255) as u8,
        ((tex[1] as u16 * kd[1] as u16 + 127) / 255) as u8,
        ((tex[2] as u16 * kd[2] as u16 + 127) / 255) as u8,
        ((tex[3] as u16 * kd[3] as u16 + 127) / 255) as u8,
    ]
}

#[inline]
fn average_three_rgba8(a: [u8; 4], b: [u8; 4], c: [u8; 4]) -> [u8; 4] {
    [
        ((a[0] as u16 + b[0] as u16 + c[0] as u16 + 1) / 3) as u8,
        ((a[1] as u16 + b[1] as u16 + c[1] as u16 + 1) / 3) as u8,
        ((a[2] as u16 + b[2] as u16 + c[2] as u16 + 1) / 3) as u8,
        ((a[3] as u16 + b[3] as u16 + c[3] as u16 + 1) / 3) as u8,
    ]
}

#[inline]
fn average_four_rgba8(a: [u8; 4], b: [u8; 4], c: [u8; 4], d: [u8; 4]) -> [u8; 4] {
    [
        ((a[0] as u16 + b[0] as u16 + c[0] as u16 + d[0] as u16 + 2) / 4) as u8,
        ((a[1] as u16 + b[1] as u16 + c[1] as u16 + d[1] as u16 + 2) / 4) as u8,
        ((a[2] as u16 + b[2] as u16 + c[2] as u16 + d[2] as u16 + 2) / 4) as u8,
        ((a[3] as u16 + b[3] as u16 + c[3] as u16 + d[3] as u16 + 2) / 4) as u8,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::color_optics::unpack_rgba8;

    #[test]
    fn test_load_obj_geometry_mtl_materials_and_vertex_colors() {
        let obj_text = r#"# Two-material cube with vertex normals and UVs
mtllib cube.mtl
v -5.0 -5.0 0.0
v  5.0 -5.0 0.0
v  5.0  5.0 0.0
v -5.0  5.0 0.0
v -5.0 -5.0 10.0
v  5.0 -5.0 10.0
v  5.0  5.0 10.0
v -5.0  5.0 10.0
vt 0.0 0.0
vt 1.0 0.0
vt 1.0 1.0
vt 0.0 1.0
g BottomHalf
usemtl CyanMat
f 1/1 3/3 2/2
f 1/1 4/4 3/3
f 1/1 2/2 6/3
f 1/1 6/3 5/4
f 2/1 3/2 7/3
f 2/1 7/3 6/4
g TopHalf
usemtl YellowMat
f 5/1 6/2 7/3
f 5/1 7/3 8/4
f 3/1 4/2 8/3
f 3/1 8/3 7/4
f 4/1 1/2 5/3
f 4/1 5/3 8/4
"#;

        let mtl_text = r#"newmtl CyanMat
Kd 0.0 0.525 0.839
d 1.0

newmtl YellowMat
Kd 0.988 0.890 0.0
d 1.0
"#;

        let mut report = LoadedBatchReport::default();
        let obj = load_obj_bytes_into_report(
            obj_text.as_bytes(),
            "two_color_cube",
            "two_color_cube.obj",
            |p| {
                if p.to_string_lossy().ends_with("cube.mtl") {
                    Some(mtl_text.as_bytes().to_vec())
                } else {
                    None
                }
            },
            |_| None,
            &mut report,
        )
        .expect("Failed to load two-material OBJ");

        assert!(obj.object_id > 0);
        assert_eq!(obj.indices.len() / 3, 12, "Expected 12 triangles in cube");
        assert_eq!(
            report.filament_slots.len(),
            2,
            "Expected 2 filament slots from cube.mtl"
        );
        assert_eq!(obj.triangle_filament_slots.len(), 12);
        assert_eq!(obj.triangle_colors_rgba.len(), 12);
        assert_eq!(obj.triangle_filament_slots[0], 0);
        assert_eq!(obj.triangle_filament_slots[6], 1);

        let c_cyan = unpack_rgba8(obj.triangle_colors_rgba[0]);
        let c_yellow = unpack_rgba8(obj.triangle_colors_rgba[6]);
        assert!(c_cyan[2] > 200 && c_cyan[0] < 10, "Expected cyan RGB, got {c_cyan:?}");
        assert!(
            c_yellow[0] > 240 && c_yellow[1] > 210 && c_yellow[2] < 15,
            "Expected yellow RGB, got {c_yellow:?}"
        );

        // Also verify Wavefront per-vertex colour (`v x y z r g b`) parsing without an MTL file
        let vcolor_obj = r#"# Tetrahedron with per-vertex RGB
v 0.0 0.0 0.0 1.0 0.0 0.0
v 10.0 0.0 0.0 0.0 1.0 0.0
v 5.0 8.66 0.0 0.0 0.0 1.0
v 5.0 2.88 8.16 1.0 1.0 0.0
f 1 3 2
f 1 2 4
f 2 3 4
f 3 1 4
"#;
        let mut vc_report = LoadedBatchReport::default();
        let vc_obj = load_obj_bytes_into_report(
            vcolor_obj.as_bytes(),
            "vcolor_tet",
            "vcolor_tet.obj",
            |_| None,
            |_| None,
            &mut vc_report,
        )
        .expect("Failed to load vertex-colored OBJ");
        assert_eq!(vc_obj.indices.len() / 3, 4);
        assert_eq!(vc_obj.vertex_colors_rgba.len(), 4);
        assert!(vc_obj.vertex_colors_rgba.contains(&[255, 0, 0, 255]));
        assert!(vc_obj.vertex_colors_rgba.contains(&[0, 255, 0, 255]));
        assert!(vc_obj.vertex_colors_rgba.contains(&[0, 0, 255, 255]));
        assert!(vc_obj.vertex_colors_rgba.contains(&[255, 255, 0, 255]));

        // Verify 8K PNG texture (`Elephant_SPII_1.png`, > 64 MiB uncompressed) + MTL with `Kd 1 1 1`
        // decodes cleanly and produces textured `default_cad_vertex_colors` instead of uniform white.
        let elephant_png_path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("Test Objects/elephant-figurine/Elephant_SPII_1.png");
        let elephant_mtl_path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("Test Objects/elephant-figurine/Elephant_SPII.mtl");
        if elephant_png_path.exists() && elephant_mtl_path.exists() {
            let png_bytes = fs::read(&elephant_png_path).expect("read Elephant_SPII_1.png");
            let mtl_bytes = fs::read(&elephant_mtl_path).expect("read Elephant_SPII.mtl");
            let textured_obj = r#"mtllib Elephant_SPII.mtl
o Elephant_Patch
usemtl material_0
v 0.0 0.0 0.0
v 10.0 0.0 0.0
v 10.0 10.0 0.0
v 0.0 10.0 0.0
vt 0.25 0.25
vt 0.50 0.25
vt 0.50 0.50
vt 0.25 0.50
f 1/1 2/2 3/3
f 1/1 3/3 4/4
"#;
            let mut tex_report = LoadedBatchReport::default();
            let tex_loaded = load_obj_bytes_into_report(
                textured_obj.as_bytes(),
                "elephant_patch",
                "Elephant_SPII.obj",
                |_| Some(mtl_bytes.clone()),
                |_| Some(png_bytes.clone()),
                &mut tex_report,
            )
            .expect("Failed to load textured OBJ with 8K PNG");

            assert!(
                tex_loaded.vertex_filament_slots.is_empty(),
                "Textured OBJ with Kd 1 1 1 should not create a dummy white filament slot"
            );
            assert_eq!(tex_loaded.vertex_colors_rgba.len(), tex_loaded.vertices.len());
            assert_eq!(
                tex_loaded.triangle_colors_rgba.len(),
                tex_loaded.indices.len() / 3
            );
            assert!(
                tex_loaded
                    .vertex_colors_rgba
                    .iter()
                    .any(|&c| c[0] < 245 || c[1] < 245 || c[2] < 245),
                "Expected non-white sampled texels from Elephant_SPII_1.png, got {:?}",
                tex_loaded.vertex_colors_rgba
            );
            let cfg = crate::config::SlicerConfig::default();
            let cad_colors = crate::field_vis::default_cad_vertex_colors(
                tex_loaded.vertices.len(),
                &tex_loaded.vertex_filament_slots,
                &tex_loaded.vertex_colors_rgba,
                &cfg,
            );
            assert!(
                cad_colors
                    .iter()
                    .any(|&c| c[0] < 0.95 || c[1] < 0.95 || c[2] < 0.95),
                "Expected default_cad_vertex_colors to display sampled texture rather than uniform white"
            );
        }
    }
}
