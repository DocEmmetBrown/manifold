use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{Cursor, Read, Write};
use std::path::Path;

use bevy::prelude::*;
use quick_xml::events::Event;
use quick_xml::reader::Reader;
use serde::{Deserialize, Serialize};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

use crate::bridge::ffi::{self, RenderVertex};
use crate::color_optics;
use crate::config::{ObjectFilamentMode, SlicerConfig, ThreeMfFilamentSlot};
use crate::io::{
    build_loaded_object_from_part, load_step_file_parts, obj, stl, LoadedBatchReport,
    LoadedObjectData, LoadedPlateInfo,
};

const MANIFOLD_CONFIG_ENTRY: &str = "Metadata/manifold2_config.toml";
const MANIFOLD_SCENE_ENTRY: &str = "Metadata/manifold2_scene.toml";
const MODEL_SETTINGS_CONFIG_ENTRY: &str = "Metadata/model_settings.config";
const PROJECT_SETTINGS_CONFIG_ENTRY: &str = "Metadata/project_settings.config";
const SLIC3R_PE_CONFIG_ENTRY: &str = "Metadata/Slic3r_PE.config";
const SLIC3R_PE_MODEL_CONFIG_ENTRY: &str = "Metadata/Slic3r_PE_model.config";

/// Saved metadata for objects exported by Manifold2 so that embedded STEP B-Rep
/// shapes inside `/Metadata/` reconnect with their exact world transforms and plate assignments upon reload.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct SavedSceneManifest {
    #[serde(default)]
    plates: Vec<String>,
    objects: Vec<SavedObjectEntry>,
}

fn default_true_bool() -> bool {
    true
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct SavedObjectEntry {
    name: String,
    embedded_step_entry: Option<String>,
    translation: [f32; 3],
    rotation_xyzw: [f32; 4],
    #[serde(default)]
    plate_index: usize,
    #[serde(default = "default_true_bool")]
    printable: bool,
    #[serde(default)]
    filament_mode: ObjectFilamentMode,
    #[serde(default)]
    custom_color_srgba: Option<[f32; 4]>,
}

/// Snapshot of an active build-plate object passed to the 3MF exporter.
pub struct ExportableObject {
    pub object_id: u64,
    pub name: String,
    pub has_brep: bool,
    pub printable: bool,
    pub plate_index: usize,
    pub transform: Transform,
    pub vertices: Vec<RenderVertex>,
    pub indices: Vec<u32>,
    pub filament_mode: ObjectFilamentMode,
    pub custom_color_srgba: Option<[f32; 4]>,
}

/// Parsed `<plate>` block from `Metadata/model_settings.config` (OrcaSlicer / Bambu Studio / PrusaSlicer).
#[derive(Clone, Debug, Default)]
struct ParsedConfigPlate {
    plater_id: u32,
    plater_name: String,
    /// Pairs of `(object_id, instance_id)` assigned to this build plate.
    instances: Vec<(String, u32)>,
}

/// Parsed `<volume firstid="..." lastid="...">` extruder assignment from `Slic3r_PE_model.config`.
#[derive(Clone, Debug)]
struct ParsedVolumeExtruderRange {
    first_id: usize,
    last_id: usize,
    /// 1-based extruder index (`0` = inherit object default).
    extruder_one_based: u8,
}

/// Parsed metadata from `Metadata/model_settings.config` or `Metadata/Slic3r_PE_model.config`.
#[derive(Clone, Debug, Default)]
struct ParsedModelSettingsConfig {
    /// Human-readable object names keyed by `<object id="...">`.
    object_names: HashMap<String, String>,
    /// 1-based extruder index assigned at `<object id="...">` or `<part id="...">` level.
    object_extruders: HashMap<String, u8>,
    /// Per-object `printable` flag parsed from `<object id="..."><metadata key="printable" .../>`.
    object_printable: HashMap<String, bool>,
    /// Per-instance `printable` flag parsed from `<model_instance><metadata key="printable" .../>`.
    instance_printable: HashMap<(String, u32), bool>,
    /// Per-object `<volume firstid="..." lastid="...">` extruder assignments.
    volume_extruders: HashMap<String, Vec<ParsedVolumeExtruderRange>>,
    /// Multi-plate definitions ordered by `plater_id`.
    plates: Vec<ParsedConfigPlate>,
}

impl ParsedModelSettingsConfig {
    fn merge_from(&mut self, other: Self) {
        for (k, v) in other.object_names {
            self.object_names.entry(k).or_insert(v);
        }
        for (k, v) in other.object_extruders {
            if v > 0 {
                self.object_extruders.insert(k, v);
            }
        }
        for (k, v) in other.object_printable {
            self.object_printable.insert(k, v);
        }
        for (k, v) in other.instance_printable {
            self.instance_printable.insert(k, v);
        }
        for (k, v) in other.volume_extruders {
            if !v.is_empty() {
                self.volume_extruders.insert(k, v);
            }
        }
        if self.plates.is_empty() && !other.plates.is_empty() {
            self.plates = other.plates;
        }
    }
}

/// Compact per-triangle property reference (`pid`, `p1`, `p2`, `p3`) from 3MF Core / Materials.
#[derive(Clone, Copy, Debug, Default)]
struct TrianglePropRef {
    /// 1-based index into `ParsedXmlMesh::prop_pids` (`0` = inherit `<object pid="...">`).
    pid_idx_one_based: u16,
    has_p1: bool,
    p1: u32,
    p2: u32,
    p3: u32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum TextureTileStyle {
    #[default]
    Wrap,
    Clamp,
    Mirror,
}

impl TextureTileStyle {
    fn from_attr(val: &str) -> Self {
        if val.eq_ignore_ascii_case("clamp") || val.eq_ignore_ascii_case("none") {
            Self::Clamp
        } else if val.eq_ignore_ascii_case("mirror") {
            Self::Mirror
        } else {
            Self::Wrap
        }
    }

    fn apply(self, t: f32) -> f32 {
        if !t.is_finite() {
            return 0.0;
        }
        match self {
            Self::Wrap => t.rem_euclid(1.0),
            Self::Clamp => t.clamp(0.0, 1.0),
            Self::Mirror => {
                let period = t.rem_euclid(2.0);
                if period <= 1.0 {
                    period
                } else {
                    2.0 - period
                }
            }
        }
    }
}

#[derive(Clone, Debug)]
struct ParsedBaseMaterial {
    name: String,
    rgba8: [u8; 4],
}

#[derive(Clone, Debug)]
struct ParsedTexture2d {
    path: String,
    tile_u: TextureTileStyle,
    tile_v: TextureTileStyle,
}

#[derive(Clone, Debug, Default)]
struct ParsedTexture2dGroup {
    tex_id: String,
    coords: Vec<[f32; 2]>,
}

/// Raw `<mesh>` parsed from a `.model` XML inside a `.3mf` archive.
#[derive(Clone, Debug, Default)]
struct ParsedXmlMesh {
    positions: Vec<f32>,
    indices: Vec<u32>,
    /// Per-triangle 1-based MMU extruder state (`0` = unpainted / use volume or object default,
    /// `s >= 1` = 1-based extruder `s`, corresponding to 0-based `filament_slot = s - 1`).
    triangle_mmu_states: Vec<u8>,
    /// Interned `pid` strings referenced by triangles in this mesh.
    prop_pids: Vec<String>,
    /// Populated only when at least one `<triangle>` specifies `pid`/`p1`/`p2`/`p3`.
    triangle_prop_refs: Vec<TrianglePropRef>,
}

/// `<component>` reference inside an `<object><components>` block.
#[derive(Clone, Debug)]
struct ParsedComponentRef {
    object_id: String,
    model_path: Option<String>,
    transform: Mat4,
}

/// `<object>` resource inside a `.model` XML file.
#[derive(Clone, Debug, Default)]
struct ParsedXmlObject {
    id: String,
    name: Option<String>,
    pid: Option<String>,
    pindex: Option<u32>,
    mesh: Option<ParsedXmlMesh>,
    components: Vec<ParsedComponentRef>,
}

/// `<item>` entry inside `<build>`.
#[derive(Clone, Debug)]
struct ParsedBuildItem {
    object_id: String,
    model_path: Option<String>,
    transform: Mat4,
    printable: Option<bool>,
}

/// All `<object>` resources, color/material/texture groups, and `<build>` items parsed from a `.model` XML stream.
#[derive(Clone, Debug, Default)]
struct ParsedModelFile {
    objects: HashMap<String, ParsedXmlObject>,
    build_items: Vec<ParsedBuildItem>,
    base_materials: HashMap<String, Vec<ParsedBaseMaterial>>,
    color_groups: HashMap<String, Vec<[u8; 4]>>,
    textures_2d: HashMap<String, ParsedTexture2d>,
    texture_2d_groups: HashMap<String, ParsedTexture2dGroup>,
}

/// Loads a `.3mf` OPC/ZIP archive, supporting:
/// - Standard 3MF `/3D/3dmodel.model` and Production Extension sub-models (`/3D/Objects/*.model`)
/// - Embedded `.step` / `.stp` CAD files
/// - Embedded `.stl` mesh files
/// - Saved Manifold2 configuration (`/Metadata/manifold2_config.toml`)
/// - Graceful ignoring + reporting of unsupported files from third-party slicers
pub fn load_3mf_archive(path: &Path, report: &mut LoadedBatchReport) {
    let file = match File::open(path) {
        Ok(f) => f,
        Err(e) => {
            report
                .errors
                .push(format!("Failed to open 3MF archive {}: {e}", path.display()));
            return;
        }
    };

    let mut archive = match ZipArchive::new(file) {
        Ok(a) => a,
        Err(e) => {
            report
                .errors
                .push(format!("Invalid 3MF ZIP archive {}: {e}", path.display()));
            return;
        }
    };

    let archive_label = path.to_string_lossy().into_owned();
    let objects_start_idx = report.objects.len();
    let mut model_files: HashMap<String, ParsedModelFile> = HashMap::new();
    let mut embedded_step_entries: HashMap<String, Vec<u8>> = HashMap::new();
    let mut embedded_stl_entries: Vec<(String, Vec<u8>)> = Vec::new();
    let mut embedded_obj_entries: Vec<(String, Vec<u8>)> = Vec::new();
    let mut embedded_mtl_entries: HashMap<String, Vec<u8>> = HashMap::new();
    let mut embedded_png_entries: HashMap<String, Vec<u8>> = HashMap::new();
    let mut scene_manifest: Option<SavedSceneManifest> = None;
    let mut model_settings: Option<ParsedModelSettingsConfig> = None;
    let mut project_settings_bed_size: Option<(f32, f32)> = None;
    let mut imported_filament_slots: Vec<ThreeMfFilamentSlot> = Vec::new();
    let mut is_manifold_project = false;
    let mut skipped_third_party_entries: Vec<String> = Vec::new();

    for i in 0..archive.len() {
        let mut entry = match archive.by_index(i) {
            Ok(e) => e,
            Err(_) => continue,
        };
        if entry.is_dir() {
            continue;
        }

        let raw_name = entry.name().trim_start_matches('/').to_string();
        let lower_name = raw_name.to_ascii_lowercase();

        if raw_name == MANIFOLD_CONFIG_ENTRY {
            let mut text = String::new();
            if entry.read_to_string(&mut text).is_ok() {
                match SlicerConfig::from_toml_str(&text) {
                    Ok(cfg) => {
                        is_manifold_project = true;
                        report.restored_config = Some(cfg);
                        report.notices.push(format!(
                            "Restored Manifold2 slicer & bed configuration from '{}'.",
                            path.file_name()
                                .and_then(|n| n.to_str())
                                .unwrap_or(&archive_label)
                        ));
                    }
                    Err(err) => {
                        report
                            .notices
                            .push(format!("Ignored malformed {MANIFOLD_CONFIG_ENTRY}: {err}"));
                    }
                }
            }
            continue;
        }

        if raw_name == MANIFOLD_SCENE_ENTRY {
            let mut text = String::new();
            if entry.read_to_string(&mut text).is_ok() {
                if let Ok(manifest) = toml::from_str::<SavedSceneManifest>(&text) {
                    is_manifold_project = true;
                    scene_manifest = Some(manifest);
                }
            }
            continue;
        }

        if lower_name.eq_ignore_ascii_case(MODEL_SETTINGS_CONFIG_ENTRY)
            || lower_name.eq_ignore_ascii_case(SLIC3R_PE_MODEL_CONFIG_ENTRY)
        {
            let mut xml_bytes = Vec::new();
            if entry.read_to_end(&mut xml_bytes).is_ok() {
                if let Ok(parsed_settings) = parse_model_settings_config_xml(&xml_bytes) {
                    if let Some(ref mut existing) = model_settings {
                        existing.merge_from(parsed_settings);
                    } else {
                        model_settings = Some(parsed_settings);
                    }
                }
            }
            continue;
        }

        if lower_name.eq_ignore_ascii_case(SLIC3R_PE_CONFIG_ENTRY) {
            let mut text = String::new();
            if entry.read_to_string(&mut text).is_ok() {
                let parsed_slots = parse_slic3r_pe_config_filament_slots(&text);
                if !parsed_slots.is_empty() {
                    imported_filament_slots = parsed_slots;
                }
            }
            continue;
        }

        if lower_name.eq_ignore_ascii_case(PROJECT_SETTINGS_CONFIG_ENTRY) {
            let mut text = String::new();
            if entry.read_to_string(&mut text).is_ok() {
                project_settings_bed_size = parse_project_settings_bed_size(&text);
                if imported_filament_slots.is_empty() {
                    imported_filament_slots = parse_project_settings_filament_slots(&text);
                }
            }
            continue;
        }

        // Standard OPC relationship / content-type plumbing
        if lower_name == "[content_types].xml" || lower_name.ends_with(".rels") {
            continue;
        }

        if lower_name.ends_with(".model") {
            let mut xml_bytes = Vec::new();
            if entry.read_to_end(&mut xml_bytes).is_ok() {
                match parse_3mf_model_xml(&xml_bytes) {
                    Ok(parsed) => {
                        model_files.insert(raw_name, parsed);
                    }
                    Err(err) => {
                        report
                            .errors
                            .push(format!("Failed to parse 3MF XML '{raw_name}': {err}"));
                    }
                }
            }
            continue;
        }

        if lower_name.ends_with(".step") || lower_name.ends_with(".stp") {
            let mut bytes = Vec::new();
            if entry.read_to_end(&mut bytes).is_ok() {
                embedded_step_entries.insert(raw_name, bytes);
            }
            continue;
        }

        if lower_name.ends_with(".stl") {
            let mut bytes = Vec::new();
            if entry.read_to_end(&mut bytes).is_ok() {
                embedded_stl_entries.push((raw_name, bytes));
            }
            continue;
        }

        if lower_name.ends_with(".obj") {
            let mut bytes = Vec::new();
            if entry.read_to_end(&mut bytes).is_ok() {
                embedded_obj_entries.push((raw_name, bytes));
            }
            continue;
        }

        if lower_name.ends_with(".mtl") {
            let mut bytes = Vec::new();
            if entry.read_to_end(&mut bytes).is_ok() {
                embedded_mtl_entries.insert(raw_name, bytes);
            }
            continue;
        }

        if lower_name.ends_with(".png") {
            let mut bytes = Vec::new();
            if entry.read_to_end(&mut bytes).is_ok() {
                embedded_png_entries.insert(raw_name, bytes);
            }
            continue;
        }

        // Any other file inside the 3MF (e.g., PrusaSlicer/OrcaSlicer/BambuStudio
        // .config, .gcode, thumbnails, custom profile JSONs) is safely ignored.
        skipped_third_party_entries.push(raw_name);
    }

    if is_manifold_project {
        if let Some(fname) = path.file_name().and_then(|n| n.to_str()) {
            report.manifold_project_filename = Some(fname.to_string());
        }
    }

    // Populate multi-plate list from Manifold's `SavedSceneManifest` first, or from
    // `Metadata/model_settings.config` (OrcaSlicer / Bambu Studio) when present.
    let mut instance_to_plate: HashMap<(String, u32), usize> = HashMap::new();
    let mut object_to_plate: HashMap<String, usize> = HashMap::new();

    if let Some(ref manifest) = scene_manifest {
        if !manifest.plates.is_empty() && report.plates.is_empty() {
            report.plates = manifest
                .plates
                .iter()
                .map(|name| LoadedPlateInfo { name: name.clone() })
                .collect();
        }
    }

    if let Some(ref ms) = model_settings {
        for (plate_idx, plate_cfg) in ms.plates.iter().enumerate() {
            let plate_name = if plate_cfg.plater_name.trim().is_empty() {
                format!("Plate {}", plate_idx + 1)
            } else {
                plate_cfg.plater_name.trim().to_string()
            };
            if !is_manifold_project && report.plates.len() <= plate_idx {
                report.plates.push(LoadedPlateInfo { name: plate_name });
            }
            for (obj_id, inst_id) in &plate_cfg.instances {
                instance_to_plate.insert((obj_id.clone(), *inst_id), plate_idx);
                object_to_plate.entry(obj_id.clone()).or_insert(plate_idx);
            }
        }
        if !is_manifold_project && ms.plates.len() > 1 {
            report.notices.push(format!(
                "Imported {} build plates from '{}'.",
                ms.plates.len(),
                path.file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or(&archive_label)
            ));
        }
    }

    if !skipped_third_party_entries.is_empty() {
        let preview: Vec<&str> = skipped_third_party_entries
            .iter()
            .take(4)
            .map(String::as_str)
            .collect();
        let suffix = if skipped_third_party_entries.len() > 4 {
            format!(" (+{} more)", skipped_third_party_entries.len() - 4)
        } else {
            String::new()
        };
        report.notices.push(format!(
            "Ignored {} unsupported/third-party 3MF entry(s): {}{}",
            skipped_third_party_entries.len(),
            preview.join(", "),
            suffix
        ));
    }

    // 1. If this 3MF has embedded STEP files, load them through OCCT!
    //    Track which embedded STEP files were matched to a SavedSceneManifest entry
    //    so we don't duplicate mesh fallbacks that were written for compatibility.
    let mut manifest_step_loaded = false;
    if let Some(ref manifest) = scene_manifest {
        for saved_obj in &manifest.objects {
            if let Some(ref step_entry) = saved_obj.embedded_step_entry {
                let normalized = step_entry.trim_start_matches('/');
                if let Some(step_bytes) = embedded_step_entries.remove(normalized) {
                    if let Some(mut loaded_objs) = load_embedded_step_bytes(
                        normalized,
                        &step_bytes,
                        &archive_label,
                        report,
                    ) {
                        for obj in &mut loaded_objs {
                            obj.name = saved_obj.name.clone();
                            obj.plate_index = saved_obj.plate_index;
                            obj.printable = saved_obj.printable;
                            obj.filament_mode = saved_obj.filament_mode;
                            obj.custom_color_srgba = saved_obj.custom_color_srgba;
                            obj.initial_translation = Vec3::from_array(saved_obj.translation);
                            obj.initial_rotation =
                                Quat::from_array(saved_obj.rotation_xyzw).normalize();
                        }
                        if let Some(last) = report.objects.last_mut() {
                            if loaded_objs.len() == 1 {
                                last.name = saved_obj.name.clone();
                                last.plate_index = saved_obj.plate_index;
                                last.printable = saved_obj.printable;
                                last.filament_mode = saved_obj.filament_mode;
                                last.custom_color_srgba = saved_obj.custom_color_srgba;
                                last.initial_translation = Vec3::from_array(saved_obj.translation);
                                last.initial_rotation =
                                    Quat::from_array(saved_obj.rotation_xyzw).normalize();
                            }
                        }
                        manifest_step_loaded = true;
                    }
                }
            }
        }
    }

    // Load any remaining standalone embedded STEP files inside the 3MF
    for (entry_name, step_bytes) in &embedded_step_entries {
        let _ = load_embedded_step_bytes(entry_name, step_bytes, &archive_label, report);
    }

    // 2. Load any embedded STL or OBJ files inside the 3MF
    for (entry_name, stl_bytes) in embedded_stl_entries {
        let mut cursor = Cursor::new(stl_bytes);
        let stem = Path::new(&entry_name)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("embedded_stl");
        match stl::load_stl_from_reader(&mut cursor, stem, &format!("{archive_label}::{entry_name}"))
        {
            Ok(obj) => report.objects.push(obj),
            Err(err) => report.errors.push(err),
        }
    }

    for (entry_name, obj_bytes) in embedded_obj_entries {
        let stem = Path::new(&entry_name)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("embedded_obj");
        let label = format!("{archive_label}::{entry_name}");
        match obj::load_obj_bytes_into_report(
            &obj_bytes,
            stem,
            &label,
            |mtl_path| {
                let mtl_str = mtl_path.to_string_lossy();
                let mtl_fname = mtl_path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                embedded_mtl_entries
                    .get(mtl_str.as_ref())
                    .or_else(|| {
                        embedded_mtl_entries
                            .iter()
                            .find(|(k, _)| {
                                k.eq_ignore_ascii_case(&mtl_str)
                                    || (!mtl_fname.is_empty()
                                        && Path::new(k)
                                            .file_name()
                                            .and_then(|n| n.to_str())
                                            .map_or(false, |n| n.eq_ignore_ascii_case(mtl_fname)))
                            })
                            .map(|(_, v)| v)
                    })
                    .cloned()
            },
            |png_name| {
                let png_fname = Path::new(png_name)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("");
                embedded_png_entries
                    .get(png_name)
                    .or_else(|| {
                        embedded_png_entries
                            .iter()
                            .find(|(k, _)| {
                                k.eq_ignore_ascii_case(png_name)
                                    || (!png_fname.is_empty()
                                        && Path::new(k)
                                            .file_name()
                                            .and_then(|n| n.to_str())
                                            .map_or(false, |n| n.eq_ignore_ascii_case(png_fname)))
                            })
                            .map(|(_, v)| v)
                    })
                    .cloned()
            },
            report,
        ) {
            Ok(obj) => report.objects.push(obj),
            Err(err) => report.errors.push(err),
        }
    }

    // 3. Instantiate `<build><item>` objects from `/3D/3dmodel.model` (or fallback root model)
    //    If all objects in a Manifold2 3MF were already restored from lossless STEP B-Reps,
    //    only load the non-STEP objects from `<build>`.
    if manifest_step_loaded
        && scene_manifest
            .as_ref()
            .map_or(false, |m| m.objects.iter().all(|o| o.embedded_step_entry.is_some()))
    {
        remap_restored_config_hint_object_ids(report, objects_start_idx);
        return;
    }

    let root_model_key = model_files
        .keys()
        .find(|k| k.eq_ignore_ascii_case("3d/3dmodel.model"))
        .cloned()
        .or_else(|| model_files.keys().next().cloned());

    let Some(root_key) = root_model_key else {
        remap_restored_config_hint_object_ids(report, objects_start_idx);
        return;
    };

    let Some(root_model) = model_files.get(&root_key).cloned() else {
        remap_restored_config_hint_object_ids(report, objects_start_idx);
        return;
    };

    let file_stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("3mf_model");

    let mut tex_cache = TextureCache::new(&embedded_png_entries);

    if root_model.build_items.is_empty() {
        // Fallback if `<build>` was omitted: instantiate every mesh object directly
        for obj in root_model.objects.values() {
            if let Some(ref mesh) = obj.mesh {
                let name = obj
                    .name
                    .clone()
                    .or_else(|| {
                        model_settings
                            .as_ref()
                            .and_then(|ms| ms.object_names.get(&obj.id).cloned())
                    })
                    .unwrap_or_else(|| format!("{file_stem}_obj_{}", obj.id));
                let plate_idx = object_to_plate.get(&obj.id).copied().unwrap_or(0);
                let obj_extruder = model_settings
                    .as_ref()
                    .and_then(|ms| ms.object_extruders.get(&obj.id).copied())
                    .unwrap_or(1);
                let vol_ranges = model_settings
                    .as_ref()
                    .and_then(|ms| ms.volume_extruders.get(&obj.id).map(Vec::as_slice))
                    .unwrap_or(&[]);
                let prev_obj_len = report.objects.len();
                instantiate_3mf_mesh(
                    &name,
                    &root_model,
                    obj,
                    mesh,
                    Mat4::IDENTITY,
                    plate_idx,
                    obj_extruder,
                    vol_ranges,
                    &archive_label,
                    &mut tex_cache,
                    &mut imported_filament_slots,
                    report,
                );
                let obj_printable = model_settings
                    .as_ref()
                    .and_then(|ms| ms.object_printable.get(&obj.id).copied())
                    .unwrap_or(true);
                for loaded in &mut report.objects[prev_obj_len..] {
                    loaded.printable = obj_printable;
                }
            }
        }
    } else {
        let mut visited = HashSet::new();
        let mut instance_counts: HashMap<String, u32> = HashMap::new();
        for (idx, item) in root_model.build_items.iter().enumerate() {
            // If this item was already restored from a lossless embedded STEP file via `scene_manifest`, skip its mesh fallback
            if manifest_step_loaded {
                if let Some(saved_obj) = scene_manifest.as_ref().and_then(|m| m.objects.get(idx)) {
                    if saved_obj.embedded_step_entry.is_some() {
                        continue;
                    }
                }
            }

            let inst_id = *instance_counts
                .entry(item.object_id.clone())
                .and_modify(|c| *c += 1)
                .or_insert(0);

            let plate_idx = if let Some(saved_obj) =
                scene_manifest.as_ref().and_then(|m| m.objects.get(idx))
            {
                saved_obj.plate_index
            } else {
                instance_to_plate
                    .get(&(item.object_id.clone(), inst_id))
                    .copied()
                    .or_else(|| object_to_plate.get(&item.object_id).copied())
                    .unwrap_or(0)
            };

            let preferred_name = scene_manifest
                .as_ref()
                .and_then(|m| m.objects.get(idx))
                .map(|o| o.name.as_str())
                .or_else(|| {
                    model_settings
                        .as_ref()
                        .and_then(|ms| ms.object_names.get(&item.object_id).map(String::as_str))
                });

            let target_model = item
                .model_path
                .as_deref()
                .map(|p| p.trim_start_matches('/').to_string())
                .unwrap_or_else(|| root_key.clone());

            let default_fallback = preferred_name
                .map(str::to_string)
                .unwrap_or_else(|| format!("{file_stem}_{}", idx + 1));

            visited.clear();
            let root_extruder = model_settings
                .as_ref()
                .and_then(|ms| ms.object_extruders.get(&item.object_id).copied())
                .unwrap_or(1);
            let prev_obj_len = report.objects.len();
            instantiate_3mf_object_recursive(
                &model_files,
                &target_model,
                &item.object_id,
                item.transform,
                plate_idx,
                root_extruder,
                model_settings.as_ref(),
                &default_fallback,
                &archive_label,
                &mut tex_cache,
                &mut imported_filament_slots,
                &mut visited,
                report,
            );
            let item_printable = scene_manifest
                .as_ref()
                .and_then(|m| m.objects.get(idx))
                .map(|o| o.printable)
                .or(item.printable)
                .or_else(|| {
                    model_settings.as_ref().and_then(|ms| {
                        ms.instance_printable
                            .get(&(item.object_id.clone(), inst_id))
                            .copied()
                    })
                })
                .or_else(|| {
                    model_settings
                        .as_ref()
                        .and_then(|ms| ms.object_printable.get(&item.object_id).copied())
                })
                .unwrap_or(true);
            let saved_filament_mode = scene_manifest
                .as_ref()
                .and_then(|m| m.objects.get(idx))
                .map(|o| o.filament_mode)
                .unwrap_or_default();
            let saved_custom_color = scene_manifest
                .as_ref()
                .and_then(|m| m.objects.get(idx))
                .and_then(|o| o.custom_color_srgba);
            for loaded in &mut report.objects[prev_obj_len..] {
                loaded.printable = item_printable;
                loaded.filament_mode = saved_filament_mode;
                loaded.custom_color_srgba = saved_custom_color;
            }
        }
    }

    // Populate `report.filament_slots` when the 3MF defines filament slots or uses multi-filament assignments
    let mut max_used_slot: Option<usize> = None;
    let mut distinct_slots: HashSet<u8> = HashSet::new();
    for obj in &report.objects[objects_start_idx..] {
        for &slot in &obj.triangle_filament_slots {
            distinct_slots.insert(slot);
            let s = slot as usize;
            max_used_slot = Some(max_used_slot.map_or(s, |m| m.max(s)));
        }
    }
    if !imported_filament_slots.is_empty() || distinct_slots.len() > 1 || max_used_slot.unwrap_or(0) > 0 {
        let needed_len = max_used_slot
            .map_or(0, |m| m + 1)
            .max(imported_filament_slots.len());
        while imported_filament_slots.len() < needed_len {
            let idx = imported_filament_slots.len();
            imported_filament_slots.push(ThreeMfFilamentSlot::default_for_slot(idx, idx));
        }
        if !is_manifold_project && imported_filament_slots.len() > 1 {
            report.notices.push(format!(
                "Imported {} 3MF filament/colour slot(s) from '{}'.",
                imported_filament_slots.len(),
                path.file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or(&archive_label)
            ));
        }
        report.filament_slots = imported_filament_slots;
    }

    // 4. For third-party 3MF archives (OrcaSlicer / Bambu Studio / PrusaSlicer), convert global
    //    multi-plate platter XY coordinates into plate-local scene coordinates centered at `(0, 0)`.
    if !is_manifold_project && report.objects.len() > objects_start_idx {
        let max_plate_idx = report.objects[objects_start_idx..]
            .iter()
            .map(|o| o.plate_index)
            .max()
            .unwrap_or(0);
        let num_plates = (max_plate_idx + 1).max(report.plates.len()).max(1);
        let has_third_party_platter =
            model_settings.is_some() || project_settings_bed_size.is_some() || num_plates > 1;

        if has_third_party_platter {
            for p_idx in 0..num_plates {
                let mut min_xy = Vec2::splat(f32::INFINITY);
                let mut max_xy = Vec2::splat(f32::NEG_INFINITY);
                let mut count_on_plate = 0usize;

                for obj in &report.objects[objects_start_idx..] {
                    if obj.plate_index == p_idx {
                        let t_xy = obj.initial_translation.xy();
                        min_xy = min_xy.min(t_xy + obj.local_min.xy());
                        max_xy = max_xy.max(t_xy + obj.local_max.xy());
                        count_on_plate += 1;
                    }
                }
                if count_on_plate == 0 || !min_xy.x.is_finite() || !max_xy.x.is_finite() {
                    continue;
                }

                let bbox_center_xy = 0.5 * (min_xy + max_xy);
                let plate_origin_xy = if let Some((w, d)) = project_settings_bed_size {
                    if w > 10.0 && d > 10.0 {
                        // OrcaSlicer / Bambu Studio places plates on a 2D grid with stride `1.2 * W` along `+X`
                        // and `1.2 * D` along `-Y`, with Plate (0, 0) centered at `(0.5 * W, 0.5 * D)`.
                        let col = ((bbox_center_xy.x - 0.5 * w) / (1.2 * w)).round();
                        let row = ((0.5 * d - bbox_center_xy.y) / (1.2 * d)).round();
                        let grid_center = Vec2::new((0.5 + 1.2 * col) * w, (0.5 - 1.2 * row) * d);
                        if (bbox_center_xy.x - grid_center.x).abs() <= 0.65 * w
                            && (bbox_center_xy.y - grid_center.y).abs() <= 0.65 * d
                        {
                            grid_center
                        } else {
                            bbox_center_xy
                        }
                    } else {
                        bbox_center_xy
                    }
                } else {
                    bbox_center_xy
                };

                for obj in &mut report.objects[objects_start_idx..] {
                    if obj.plate_index == p_idx {
                        obj.initial_translation.x -= plate_origin_xy.x;
                        obj.initial_translation.y -= plate_origin_xy.y;
                    }
                }
            }

            if num_plates > 1 {
                report.objects[objects_start_idx..].sort_by_key(|o| o.plate_index);
            }
        }
    }

    remap_restored_config_hint_object_ids(report, objects_start_idx);
}

/// Remaps `object_id` references inside a restored 3MF `SlicerConfig` (`surface_hints` and
/// `point_hints`) to the runtime `object_id`s assigned to the objects loaded from this archive.
fn remap_restored_config_hint_object_ids(report: &mut LoadedBatchReport, objects_start_idx: usize) {
    let loaded_ids: Vec<u64> = report.objects[objects_start_idx..]
        .iter()
        .map(|o| o.object_id)
        .collect();
    if loaded_ids.is_empty() {
        return;
    }
    let Some(ref mut cfg) = report.restored_config else {
        return;
    };

    // Collect distinct saved object IDs referenced by hints in order of appearance
    let mut saved_ids: Vec<u64> = Vec::new();
    for h in &cfg.eikonal.surface_hints {
        if !saved_ids.contains(&h.object_id) {
            saved_ids.push(h.object_id);
        }
    }
    for h in &cfg.eikonal.point_hints {
        if !saved_ids.contains(&h.object_id) {
            saved_ids.push(h.object_id);
        }
    }

    // If any saved hint object_id does not match a loaded object_id, remap saved IDs
    // positionally onto `loaded_ids` (defaulting to `loaded_ids[0]` for single-object projects).
    if saved_ids.iter().any(|id| !loaded_ids.contains(id)) {
        for h in &mut cfg.eikonal.surface_hints {
            if !loaded_ids.contains(&h.object_id) {
                let ord = saved_ids.iter().position(|&id| id == h.object_id).unwrap_or(0);
                h.object_id = loaded_ids.get(ord).copied().unwrap_or(loaded_ids[0]);
            }
        }
        for h in &mut cfg.eikonal.point_hints {
            if !loaded_ids.contains(&h.object_id) {
                let ord = saved_ids.iter().position(|&id| id == h.object_id).unwrap_or(0);
                h.object_id = loaded_ids.get(ord).copied().unwrap_or(loaded_ids[0]);
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn instantiate_3mf_object_recursive(
    model_files: &HashMap<String, ParsedModelFile>,
    model_key: &str,
    object_id: &str,
    accumulated_transform: Mat4,
    plate_index: usize,
    inherited_extruder_one_based: u8,
    model_settings: Option<&ParsedModelSettingsConfig>,
    fallback_name: &str,
    archive_label: &str,
    tex_cache: &mut TextureCache<'_>,
    imported_filament_slots: &mut Vec<ThreeMfFilamentSlot>,
    visited: &mut HashSet<(String, String)>,
    report: &mut LoadedBatchReport,
) {
    let visit_key = (model_key.to_string(), object_id.to_string());
    if !visited.insert(visit_key) {
        return;
    }

    // Case-insensitive lookup for model file path inside ZIP
    let resolved_model = model_files.get(model_key).or_else(|| {
        model_files
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(model_key))
            .map(|(_, v)| v)
    });

    let Some(model_file) = resolved_model else {
        report.notices.push(format!(
            "Skipped missing 3MF sub-model reference '{model_key}' (object {object_id})"
        ));
        return;
    };

    let Some(xml_obj) = model_file.objects.get(object_id) else {
        return;
    };

    let obj_name = xml_obj
        .name
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or(fallback_name);

    let effective_extruder = model_settings
        .and_then(|ms| ms.object_extruders.get(object_id).copied())
        .filter(|&e| e > 0)
        .unwrap_or(inherited_extruder_one_based.max(1));
    let vol_ranges = model_settings
        .and_then(|ms| ms.volume_extruders.get(object_id).map(Vec::as_slice))
        .unwrap_or(&[]);

    if let Some(ref mesh) = xml_obj.mesh {
        instantiate_3mf_mesh(
            obj_name,
            model_file,
            xml_obj,
            mesh,
            accumulated_transform,
            plate_index,
            effective_extruder,
            vol_ranges,
            archive_label,
            tex_cache,
            imported_filament_slots,
            report,
        );
    }

    let is_single_wrapper = xml_obj.mesh.is_none() && xml_obj.components.len() == 1;
    for (comp_idx, comp) in xml_obj.components.iter().enumerate() {
        let next_model = comp
            .model_path
            .as_deref()
            .map(|p| p.trim_start_matches('/').to_string())
            .unwrap_or_else(|| model_key.to_string());
        let next_transform = accumulated_transform * comp.transform;
        let child_fallback = if is_single_wrapper {
            obj_name.to_string()
        } else {
            format!("{obj_name}_part_{}", comp_idx + 1)
        };
        instantiate_3mf_object_recursive(
            model_files,
            &next_model,
            &comp.object_id,
            next_transform,
            plate_index,
            effective_extruder,
            model_settings,
            &child_fallback,
            archive_label,
            tex_cache,
            imported_filament_slots,
            visited,
            report,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn instantiate_3mf_mesh(
    name: &str,
    model_file: &ParsedModelFile,
    xml_obj: &ParsedXmlObject,
    mesh: &ParsedXmlMesh,
    world_transform: Mat4,
    plate_index: usize,
    default_extruder_one_based: u8,
    volume_ranges: &[ParsedVolumeExtruderRange],
    archive_label: &str,
    tex_cache: &mut TextureCache<'_>,
    imported_filament_slots: &mut Vec<ThreeMfFilamentSlot>,
    report: &mut LoadedBatchReport,
) {
    if mesh.positions.len() < 9 || mesh.indices.len() < 3 {
        return;
    }

    // Apply the 3MF `<item>` / `<component>` transform to the vertices before registering
    // in the C++ backend so the backend computes accurate world-scale normals and bounds
    // while recentering the local mesh at (origin_x, origin_y, z_min).
    let num_verts = mesh.positions.len() / 3;
    let mut transformed_positions = Vec::with_capacity(mesh.positions.len());
    for i in 0..num_verts {
        let local_p = Vec3::new(
            mesh.positions[i * 3],
            mesh.positions[i * 3 + 1],
            mesh.positions[i * 3 + 2],
        );
        let world_p = world_transform.transform_point3(local_p);
        transformed_positions.push(world_p.x);
        transformed_positions.push(world_p.y);
        transformed_positions.push(world_p.z);
    }

    let part_info = ffi::register_triangle_mesh(name, &transformed_positions, &mesh.indices);
    if let Some(mut obj) = build_loaded_object_from_part(part_info, archive_label) {
        obj.plate_index = plate_index;

        let num_tris = mesh.indices.len() / 3;
        let base_slot = default_extruder_one_based.saturating_sub(1);
        let has_mmu_paint = mesh.triangle_mmu_states.iter().any(|&s| s > 0);
        let has_volume_extruder = volume_ranges.iter().any(|r| r.extruder_one_based > 0);
        let has_slicer_slots =
            has_mmu_paint || has_volume_extruder || base_slot > 0 || !imported_filament_slots.is_empty();

        let (resolved_colors, basemat_tri_slots) = resolve_3mf_mesh_surface_colors(
            model_file,
            xml_obj,
            mesh,
            obj.vertices.len(),
            &obj.indices,
            tex_cache,
            imported_filament_slots,
            has_slicer_slots,
        );

        if let Some((tri_colors_rgba, vert_colors_rgba)) = resolved_colors {
            ffi::set_object_triangle_colors_rgba(obj.object_id, &tri_colors_rgba);
            obj.triangle_colors_rgba = tri_colors_rgba;
            obj.vertex_colors_rgba = vert_colors_rgba;
        }

        if has_mmu_paint || has_volume_extruder || base_slot > 0 {
            let mut tri_slots = vec![base_slot; num_tris];
            for range in volume_ranges {
                if range.extruder_one_based > 0 && range.first_id < num_tris {
                    let v_slot = range.extruder_one_based - 1;
                    let end_excl = (range.last_id + 1).min(num_tris);
                    for s in &mut tri_slots[range.first_id..end_excl] {
                        *s = v_slot;
                    }
                }
            }
            for (t_idx, &mmu_state) in mesh.triangle_mmu_states.iter().enumerate().take(num_tris) {
                if mmu_state > 0 {
                    tri_slots[t_idx] = mmu_state - 1;
                }
            }

            ffi::set_object_triangle_filament_slots(obj.object_id, &tri_slots);
            obj.vertex_filament_slots = compute_vertex_filament_slots_from_triangles(
                obj.vertices.len(),
                &obj.indices,
                &tri_slots,
                base_slot,
            );
            obj.triangle_filament_slots = tri_slots;
        } else if let Some(tri_slots) = basemat_tri_slots {
            ffi::set_object_triangle_filament_slots(obj.object_id, &tri_slots);
            obj.vertex_filament_slots = compute_vertex_filament_slots_from_triangles(
                obj.vertices.len(),
                &obj.indices,
                &tri_slots,
                0,
            );
            obj.triangle_filament_slots = tri_slots;
        } else if !obj.triangle_colors_rgba.is_empty() {
            // Automatically quantize UV-textured (`<m:texture2d>`) or `<m:colorgroup>` triangles into
            // perceptual Oklab filament slots (`imported_filament_slots` + C++ `filamentSlotGrid`) so
            // textured 3MF objects print in multi-colour through the entire 3D shell, while keeping
            // `obj.vertex_filament_slots` empty so the 3D CAD mesh displays the full continuous texture.
            if let Some(quantized_slots) =
                crate::color_optics::quantize_texture_triangles_to_filament_slots(
                    &obj.triangle_colors_rgba,
                    imported_filament_slots,
                    6,
                )
            {
                if quantized_slots.len() == obj.indices.len() / 3 {
                    ffi::set_object_triangle_filament_slots(obj.object_id, &quantized_slots);
                    obj.triangle_filament_slots = quantized_slots;
                }
            }
        }

        report.objects.push(obj);
    }
}

pub(crate) struct DecodedRgbaImage {
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) pixels: Vec<u8>,
}

impl DecodedRgbaImage {
    pub(crate) fn decode_png(bytes: &[u8]) -> Option<Self> {
        let mut decoder = png::Decoder::new_with_limits(
            Cursor::new(bytes),
            png::Limits {
                bytes: 1024 * 1024 * 1024,
            },
        );
        decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
        let mut reader = decoder.read_info().ok()?;
        let buf_len = reader.output_buffer_size()?;
        let mut raw = vec![0u8; buf_len];
        let info = reader.next_frame(&mut raw).ok()?;
        let width = info.width;
        let height = info.height;
        if width == 0 || height == 0 {
            return None;
        }
        let used = &raw[..info.buffer_size()];
        let num_px = (width as usize) * (height as usize);
        let pixels = match info.color_type {
            png::ColorType::Rgba => {
                if used.len() < num_px * 4 {
                    return None;
                }
                used[..num_px * 4].to_vec()
            }
            png::ColorType::Rgb => {
                if used.len() < num_px * 3 {
                    return None;
                }
                let mut out = Vec::with_capacity(num_px * 4);
                for rgb in used[..num_px * 3].chunks_exact(3) {
                    out.extend_from_slice(&[rgb[0], rgb[1], rgb[2], 255]);
                }
                out
            }
            png::ColorType::GrayscaleAlpha => {
                if used.len() < num_px * 2 {
                    return None;
                }
                let mut out = Vec::with_capacity(num_px * 4);
                for ga in used[..num_px * 2].chunks_exact(2) {
                    out.extend_from_slice(&[ga[0], ga[0], ga[0], ga[1]]);
                }
                out
            }
            png::ColorType::Grayscale => {
                if used.len() < num_px {
                    return None;
                }
                let mut out = Vec::with_capacity(num_px * 4);
                for &g in &used[..num_px] {
                    out.extend_from_slice(&[g, g, g, 255]);
                }
                out
            }
            png::ColorType::Indexed => return None,
        };
        Some(Self {
            width,
            height,
            pixels,
        })
    }

    fn texel(&self, x: u32, y: u32) -> [u8; 4] {
        let cx = x.min(self.width.saturating_sub(1)) as usize;
        let cy = y.min(self.height.saturating_sub(1)) as usize;
        let off = (cy * (self.width as usize) + cx) * 4;
        [
            self.pixels[off],
            self.pixels[off + 1],
            self.pixels[off + 2],
            self.pixels[off + 3],
        ]
    }

    pub(crate) fn sample_bilinear_srgba8(
        &self,
        u: f32,
        v: f32,
        tile_u: TextureTileStyle,
        tile_v: TextureTileStyle,
    ) -> [u8; 4] {
        let uw = tile_u.apply(u);
        // 3MF texture V=0 is the bottom row of the image; PNG row 0 is the top row.
        let vw = 1.0 - tile_v.apply(v);

        let fx = uw * (self.width.saturating_sub(1) as f32);
        let fy = vw * (self.height.saturating_sub(1) as f32);
        let x0 = (fx.floor() as u32).min(self.width.saturating_sub(1));
        let y0 = (fy.floor() as u32).min(self.height.saturating_sub(1));
        let x1 = (x0 + 1).min(self.width.saturating_sub(1));
        let y1 = (y0 + 1).min(self.height.saturating_sub(1));
        let tx = (fx - (x0 as f32)).clamp(0.0, 1.0);
        let ty = (fy - (y0 as f32)).clamp(0.0, 1.0);

        let c00 = self.texel(x0, y0);
        let c10 = self.texel(x1, y0);
        let c01 = self.texel(x0, y1);
        let c11 = self.texel(x1, y1);

        let mut out = [0u8; 4];
        for ch in 0..4 {
            let top = (c00[ch] as f32) * (1.0 - tx) + (c10[ch] as f32) * tx;
            let bot = (c01[ch] as f32) * (1.0 - tx) + (c11[ch] as f32) * tx;
            let val = top * (1.0 - ty) + bot * ty;
            out[ch] = val.round().clamp(0.0, 255.0) as u8;
        }
        out
    }
}

struct TextureCache<'a> {
    raw_pngs: &'a HashMap<String, Vec<u8>>,
    decoded: HashMap<String, Option<DecodedRgbaImage>>,
}

impl<'a> TextureCache<'a> {
    fn new(raw_pngs: &'a HashMap<String, Vec<u8>>) -> Self {
        Self {
            raw_pngs,
            decoded: HashMap::new(),
        }
    }

    fn get(&mut self, raw_path: &str) -> Option<&DecodedRgbaImage> {
        let key = raw_path.trim_start_matches('/').to_string();
        if !self.decoded.contains_key(&key) {
            let bytes = self.raw_pngs.get(&key).or_else(|| {
                self.raw_pngs
                    .iter()
                    .find(|(k, _)| k.eq_ignore_ascii_case(&key))
                    .map(|(_, v)| v)
            });
            let img = bytes.and_then(|b| DecodedRgbaImage::decode_png(b));
            self.decoded.insert(key.clone(), img);
        }
        self.decoded.get(&key).and_then(|opt| opt.as_ref())
    }
}

#[allow(clippy::too_many_arguments)]
fn resolve_3mf_mesh_surface_colors(
    model_file: &ParsedModelFile,
    xml_obj: &ParsedXmlObject,
    mesh: &ParsedXmlMesh,
    num_vertices: usize,
    indices: &[u32],
    tex_cache: &mut TextureCache<'_>,
    imported_filament_slots: &mut Vec<ThreeMfFilamentSlot>,
    has_slicer_slots: bool,
) -> (Option<(Vec<u32>, Vec<[u8; 4]>)>, Option<Vec<u8>>) {
    let num_tris = indices.len() / 3;
    if num_tris == 0 || num_vertices == 0 {
        return (None, None);
    }
    if mesh.triangle_prop_refs.is_empty() && xml_obj.pid.is_none() {
        return (None, None);
    }
    if model_file.base_materials.is_empty()
        && model_file.color_groups.is_empty()
        && model_file.texture_2d_groups.is_empty()
    {
        return (None, None);
    }

    let obj_pid = xml_obj.pid.as_deref();
    let obj_pindex = xml_obj.pindex.unwrap_or(0);

    let mut tri_rgba = vec![0u32; num_tris];
    let mut vert_accum = vec![[0u32; 5]; num_vertices]; // [r_sum, g_sum, b_sum, a_sum, count]
    let mut any_color = false;

    let mut single_basemat_pid: Option<&str> = None;
    let mut basemat_mixed_pids = false;
    let mut tri_basemat_slots = Vec::new();

    for (t_idx, tri) in indices.chunks_exact(3).enumerate() {
        let prop = mesh
            .triangle_prop_refs
            .get(t_idx)
            .copied()
            .unwrap_or_default();
        let pid_opt = if prop.pid_idx_one_based > 0 {
            mesh.prop_pids
                .get((prop.pid_idx_one_based - 1) as usize)
                .map(String::as_str)
        } else {
            obj_pid
        };
        let Some(pid) = pid_opt else {
            continue;
        };

        let (p1, p2, p3) = if prop.has_p1 {
            (prop.p1 as usize, prop.p2 as usize, prop.p3 as usize)
        } else {
            let idx = obj_pindex as usize;
            (idx, idx, idx)
        };

        if let Some(mats) = model_file.base_materials.get(pid) {
            if let Some(m1) = mats.get(p1) {
                let c0 = m1.rgba8;
                let c1 = mats.get(p2).map(|m| m.rgba8).unwrap_or(c0);
                let c2 = mats.get(p3).map(|m| m.rgba8).unwrap_or(c0);
                let avg = average_three_rgba8(c0, c1, c2);
                tri_rgba[t_idx] =
                    color_optics::pack_rgba8([avg[0], avg[1], avg[2], avg[3].max(1)]);
                accumulate_tri_vertex_colors(&mut vert_accum, tri, [c0, c1, c2]);
                any_color = true;

                if !has_slicer_slots && mats.len() <= 16 {
                    match single_basemat_pid {
                        None => {
                            single_basemat_pid = Some(pid);
                            tri_basemat_slots.resize(num_tris, 0);
                        }
                        Some(prev) if prev != pid => {
                            basemat_mixed_pids = true;
                        }
                        _ => {}
                    }
                    if !basemat_mixed_pids && p1 < 16 {
                        tri_basemat_slots[t_idx] = p1 as u8;
                    }
                }
            }
        } else if let Some(colors) = model_file.color_groups.get(pid) {
            if let Some(&c0) = colors.get(p1) {
                let c1 = colors.get(p2).copied().unwrap_or(c0);
                let c2 = colors.get(p3).copied().unwrap_or(c0);
                let avg = average_three_rgba8(c0, c1, c2);
                tri_rgba[t_idx] =
                    color_optics::pack_rgba8([avg[0], avg[1], avg[2], avg[3].max(1)]);
                accumulate_tri_vertex_colors(&mut vert_accum, tri, [c0, c1, c2]);
                any_color = true;
            }
        } else if let Some(tex_grp) = model_file.texture_2d_groups.get(pid) {
            if let Some(tex_meta) = model_file.textures_2d.get(&tex_grp.tex_id) {
                if let Some(img) = tex_cache.get(&tex_meta.path) {
                    if let Some(&uv0) = tex_grp.coords.get(p1) {
                        let uv1 = tex_grp.coords.get(p2).copied().unwrap_or(uv0);
                        let uv2 = tex_grp.coords.get(p3).copied().unwrap_or(uv0);
                        let c0 = img.sample_bilinear_srgba8(
                            uv0[0],
                            uv0[1],
                            tex_meta.tile_u,
                            tex_meta.tile_v,
                        );
                        let c1 = img.sample_bilinear_srgba8(
                            uv1[0],
                            uv1[1],
                            tex_meta.tile_u,
                            tex_meta.tile_v,
                        );
                        let c2 = img.sample_bilinear_srgba8(
                            uv2[0],
                            uv2[1],
                            tex_meta.tile_u,
                            tex_meta.tile_v,
                        );
                        let uc = (uv0[0] + uv1[0] + uv2[0]) / 3.0;
                        let vc = (uv0[1] + uv1[1] + uv2[1]) / 3.0;
                        let center =
                            img.sample_bilinear_srgba8(uc, vc, tex_meta.tile_u, tex_meta.tile_v);
                        tri_rgba[t_idx] = color_optics::pack_rgba8([
                            center[0],
                            center[1],
                            center[2],
                            center[3].max(1),
                        ]);
                        accumulate_tri_vertex_colors(&mut vert_accum, tri, [c0, c1, c2]);
                        any_color = true;
                    }
                }
            }
        }
    }

    if !any_color {
        return (None, None);
    }

    let default_rgba = [214u8, 217, 224, 255];
    let mut vert_rgba = vec![default_rgba; num_vertices];
    for (v_idx, acc) in vert_accum.iter().enumerate() {
        if acc[4] > 0 {
            let cnt = acc[4];
            vert_rgba[v_idx] = [
                ((acc[0] + cnt / 2) / cnt).min(255) as u8,
                ((acc[1] + cnt / 2) / cnt).min(255) as u8,
                ((acc[2] + cnt / 2) / cnt).min(255) as u8,
                ((acc[3] + cnt / 2) / cnt).clamp(1, 255) as u8,
            ];
        }
    }

    let basemat_slots_out = if !has_slicer_slots && !basemat_mixed_pids {
        if let Some(pid) = single_basemat_pid {
            if let Some(mats) = model_file.base_materials.get(pid) {
                if imported_filament_slots.is_empty() {
                    for (idx, mat) in mats.iter().enumerate() {
                        let mut slot = ThreeMfFilamentSlot::default_for_slot(idx, idx);
                        let srgba = [
                            mat.rgba8[0] as f32 / 255.0,
                            mat.rgba8[1] as f32 / 255.0,
                            mat.rgba8[2] as f32 / 255.0,
                            mat.rgba8[3] as f32 / 255.0,
                        ];
                        slot.color_srgba = srgba;
                        slot.color_hex = ThreeMfFilamentSlot::format_hex_color(srgba);
                        slot.name = if mat.name.trim().is_empty() {
                            format!("Material {} ({})", idx + 1, slot.color_hex)
                        } else {
                            format!("{} ({})", mat.name.trim(), slot.color_hex)
                        };
                        imported_filament_slots.push(slot);
                    }
                }
                Some(tri_basemat_slots)
            } else {
                None
            }
        } else {
            None
        }
    } else {
        None
    };

    (Some((tri_rgba, vert_rgba)), basemat_slots_out)
}

fn average_three_rgba8(a: [u8; 4], b: [u8; 4], c: [u8; 4]) -> [u8; 4] {
    [
        ((a[0] as u16 + b[0] as u16 + c[0] as u16 + 1) / 3) as u8,
        ((a[1] as u16 + b[1] as u16 + c[1] as u16 + 1) / 3) as u8,
        ((a[2] as u16 + b[2] as u16 + c[2] as u16 + 1) / 3) as u8,
        ((a[3] as u16 + b[3] as u16 + c[3] as u16 + 1) / 3) as u8,
    ]
}

fn accumulate_tri_vertex_colors(
    vert_accum: &mut [[u32; 5]],
    tri: &[u32],
    corners: [[u8; 4]; 3],
) {
    for (corner_idx, &v_u32) in tri.iter().enumerate().take(3) {
        let v = v_u32 as usize;
        if let Some(acc) = vert_accum.get_mut(v) {
            let c = corners[corner_idx];
            acc[0] += c[0] as u32;
            acc[1] += c[1] as u32;
            acc[2] += c[2] as u32;
            acc[3] += c[3] as u32;
            acc[4] += 1;
        }
    }
}

/// Computes a per-vertex filament slot index from per-triangle filament slots by selecting
/// each vertex's dominant incident triangle slot.
pub fn compute_vertex_filament_slots_from_triangles(
    num_vertices: usize,
    indices: &[u32],
    triangle_slots: &[u8],
    default_slot: u8,
) -> Vec<u8> {
    if num_vertices == 0 || triangle_slots.is_empty() {
        return Vec::new();
    }
    let mut counts = vec![[0u16; 8]; num_vertices];
    let mut overflow_slot = vec![default_slot; num_vertices];
    for (t_idx, tri) in indices.chunks_exact(3).enumerate() {
        let slot = triangle_slots.get(t_idx).copied().unwrap_or(default_slot);
        for &v_u32 in tri {
            let v = v_u32 as usize;
            if v < num_vertices {
                if (slot as usize) < 8 {
                    counts[v][slot as usize] = counts[v][slot as usize].saturating_add(1);
                } else {
                    overflow_slot[v] = slot;
                }
            }
        }
    }
    let mut out = vec![default_slot; num_vertices];
    for v in 0..num_vertices {
        let mut best_s = overflow_slot[v];
        let mut best_c = 0u16;
        for (s, &c) in counts[v].iter().enumerate() {
            if c > best_c {
                best_c = c;
                best_s = s as u8;
            }
        }
        out[v] = best_s;
    }
    out
}

fn load_embedded_step_bytes(
    entry_name: &str,
    step_bytes: &[u8],
    archive_label: &str,
    report: &mut LoadedBatchReport,
) -> Option<Vec<LoadedObjectData>> {
    let safe_filename = entry_name.replace(['/', '\\', ':'], "_");
    let temp_path = std::env::temp_dir().join(format!(
        "manifold2_3mf_{}_{}",
        std::process::id(),
        safe_filename
    ));
    if std::fs::write(&temp_path, step_bytes).is_err() {
        return None;
    }

    let loaded = load_step_file_parts(
        &temp_path,
        &format!("{archive_label}::{entry_name}"),
        report,
    );
    let _ = std::fs::remove_file(&temp_path);
    Some(loaded)
}

/// Saves the current multi-plate build arrangement, meshes, embedded STEP B-Rep geometry
/// (when available), and `SlicerConfig` into a standards-compliant `.3mf` archive.
pub fn save_project_to_3mf(
    out_path: &Path,
    plate_names: &[String],
    objects: &[ExportableObject],
    config: &SlicerConfig,
) -> Result<(), String> {
    let file =
        File::create(out_path).map_err(|e| format!("Failed to create {}: {e}", out_path.display()))?;
    let mut zip = ZipWriter::new(file);
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);

    // 1. Write `[Content_Types].xml`
    let content_types_xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
  <Default Extension="model" ContentType="application/vnd.ms-package.3dmanufacturing-3dmodel+xml"/>
  <Default Extension="config" ContentType="text/xml"/>
  <Default Extension="toml" ContentType="application/toml"/>
  <Default Extension="step" ContentType="application/step"/>
</Types>"#;
    zip.start_file("[Content_Types].xml", options)
        .map_err(|e| e.to_string())?;
    zip.write_all(content_types_xml.as_bytes())
        .map_err(|e| e.to_string())?;

    // 2. Write `_rels/.rels`
    let rels_xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Target="/3D/3dmodel.model" Id="rel0" Type="http://schemas.microsoft.com/3dmanufacturing/2013/01/3dmodel"/>
</Relationships>"#;
    zip.start_file("_rels/.rels", options)
        .map_err(|e| e.to_string())?;
    zip.write_all(rels_xml.as_bytes())
        .map_err(|e| e.to_string())?;

    // 3. Write `3D/3dmodel.model` containing all object meshes and their 3x4 `<item transform="..."/>`
    let mut model_xml = String::with_capacity(65536);
    model_xml.push_str(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<model unit="millimeter" xml:lang="en-US" xmlns="http://schemas.microsoft.com/3dmanufacturing/core/2015/02">
  <metadata name="Application">Manifold2 Non-Planar Slicer</metadata>
  <resources>
"#,
    );

    for (idx, obj) in objects.iter().enumerate() {
        let xml_id = idx + 1;
        let escaped_name = escape_xml_attr(&obj.name);
        model_xml.push_str(&format!(
            "    <object id=\"{xml_id}\" name=\"{escaped_name}\" type=\"model\">\n      <mesh>\n        <vertices>\n"
        ));
        for v in &obj.vertices {
            model_xml.push_str(&format!(
                "          <vertex x=\"{:.6}\" y=\"{:.6}\" z=\"{:.6}\"/>\n",
                v.x, v.y, v.z
            ));
        }
        model_xml.push_str("        </vertices>\n        <triangles>\n");
        for tri in obj.indices.chunks_exact(3) {
            model_xml.push_str(&format!(
                "          <triangle v1=\"{}\" v2=\"{}\" v3=\"{}\"/>\n",
                tri[0], tri[1], tri[2]
            ));
        }
        model_xml.push_str("        </triangles>\n      </mesh>\n    </object>\n");
    }

    model_xml.push_str("  </resources>\n  <build>\n");
    for (idx, obj) in objects.iter().enumerate() {
        let xml_id = idx + 1;
        let transform_str = format_3mf_transform(&obj.transform);
        let printable_str = if obj.printable { "1" } else { "0" };
        model_xml.push_str(&format!(
            "    <item objectid=\"{xml_id}\" transform=\"{transform_str}\" printable=\"{printable_str}\"/>\n"
        ));
    }
    model_xml.push_str("  </build>\n</model>\n");

    zip.start_file("3D/3dmodel.model", options)
        .map_err(|e| e.to_string())?;
    zip.write_all(model_xml.as_bytes())
        .map_err(|e| e.to_string())?;

    // 4. Write `Metadata/model_settings.config` for multi-plate cross-slicer compatibility
    let max_obj_plate = objects.iter().map(|o| o.plate_index).max().unwrap_or(0);
    let total_plates = plate_names.len().max(max_obj_plate + 1).max(1);
    let mut model_settings_xml = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<config>\n");
    for (idx, obj) in objects.iter().enumerate() {
        let xml_id = idx + 1;
        let escaped_name = escape_xml_attr(&obj.name);
        let printable_str = if obj.printable { "1" } else { "0" };
        model_settings_xml.push_str(&format!(
            "  <object id=\"{xml_id}\">\n    <metadata key=\"name\" value=\"{escaped_name}\"/>\n    <metadata key=\"printable\" value=\"{printable_str}\"/>\n  </object>\n"
        ));
    }
    for p_idx in 0..total_plates {
        let plater_id = p_idx + 1;
        let p_name = plate_names
            .get(p_idx)
            .cloned()
            .unwrap_or_else(|| format!("Plate {plater_id}"));
        let escaped_pname = escape_xml_attr(&p_name);
        model_settings_xml.push_str(&format!(
            "  <plate>\n    <metadata key=\"plater_id\" value=\"{plater_id}\"/>\n    <metadata key=\"plater_name\" value=\"{escaped_pname}\"/>\n"
        ));
        for (idx, obj) in objects.iter().enumerate() {
            if obj.plate_index == p_idx {
                let xml_id = idx + 1;
                let printable_str = if obj.printable { "1" } else { "0" };
                model_settings_xml.push_str(&format!(
                    "    <model_instance>\n      <metadata key=\"object_id\" value=\"{xml_id}\"/>\n      <metadata key=\"instance_id\" value=\"0\"/>\n      <metadata key=\"printable\" value=\"{printable_str}\"/>\n    </model_instance>\n"
                ));
            }
        }
        model_settings_xml.push_str("  </plate>\n");
    }
    model_settings_xml.push_str("</config>\n");
    zip.start_file(MODEL_SETTINGS_CONFIG_ENTRY, options)
        .map_err(|e| e.to_string())?;
    zip.write_all(model_settings_xml.as_bytes())
        .map_err(|e| e.to_string())?;

    // 5. Embed `/Metadata/manifold2_config.toml`
    let config_toml = config
        .to_toml_string()
        .map_err(|e| format!("Failed to serialize SlicerConfig: {e}"))?;
    zip.start_file(MANIFOLD_CONFIG_ENTRY, options)
        .map_err(|e| e.to_string())?;
    zip.write_all(config_toml.as_bytes())
        .map_err(|e| e.to_string())?;

    // 6. Export any B-Rep STEP shapes into `/Metadata/cad_<id>.step` and write `manifold2_scene.toml`
    let mut scene_manifest = SavedSceneManifest {
        plates: (0..total_plates)
            .map(|p_idx| {
                plate_names
                    .get(p_idx)
                    .cloned()
                    .unwrap_or_else(|| format!("Plate {}", p_idx + 1))
            })
            .collect(),
        objects: Vec::with_capacity(objects.len()),
    };
    for obj in objects {
        let mut embedded_step_entry = None;
        if obj.has_brep {
            let temp_step = std::env::temp_dir().join(format!(
                "manifold2_export_{}_{}.step",
                std::process::id(),
                obj.object_id
            ));
            let temp_str = temp_step.to_string_lossy();
            if ffi::export_object_step_to_path(obj.object_id, &temp_str) {
                if let Ok(step_bytes) = std::fs::read(&temp_step) {
                    let entry_path = format!("Metadata/cad_{}.step", obj.object_id);
                    if zip.start_file(&entry_path, options).is_ok()
                        && zip.write_all(&step_bytes).is_ok()
                    {
                        embedded_step_entry = Some(entry_path);
                    }
                }
            }
            let _ = std::fs::remove_file(&temp_step);
        }

        scene_manifest.objects.push(SavedObjectEntry {
            name: obj.name.clone(),
            embedded_step_entry,
            translation: obj.transform.translation.to_array(),
            rotation_xyzw: obj.transform.rotation.to_array(),
            plate_index: obj.plate_index,
            printable: obj.printable,
            filament_mode: obj.filament_mode,
            custom_color_srgba: obj.custom_color_srgba,
        });
    }

    if let Ok(scene_toml) = toml::to_string_pretty(&scene_manifest) {
        zip.start_file(MANIFOLD_SCENE_ENTRY, options)
            .map_err(|e| e.to_string())?;
        zip.write_all(scene_toml.as_bytes())
            .map_err(|e| e.to_string())?;
    }

    zip.finish().map_err(|e| e.to_string())?;
    Ok(())
}

/// Packages a sliced G-code string (`plate_1.gcode`) and its metadata into an in-memory
/// Bambu Lab `.gcode.3mf` OPC/ZIP archive containing:
/// - `[Content_Types].xml` & `_rels/.rels`
/// - `3D/3dmodel.model`
/// - `Metadata/plate_1.gcode`
/// - `Metadata/plate_1.gcode.md5` (RFC 1321 MD5 hex digest required by Bambu Lab firmware)
/// - `Metadata/slice_info.config` (XML manifest with filament types, colors, weight, and prediction time)
/// - `Metadata/plate_1.png` (optional 300x300 PNG thumbnail extracted from the G-code header)
pub fn build_bambu_gcode_3mf_bytes(
    gcode_text: &str,
    config: Option<&SlicerConfig>,
) -> Result<Vec<u8>, String> {
    let mut cursor = Cursor::new(Vec::with_capacity(gcode_text.len() / 2 + 16384));
    {
        let mut zip = ZipWriter::new(&mut cursor);
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);

        let content_types_xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
  <Default Extension="model" ContentType="application/vnd.ms-package.3dmanufacturing-3dmodel+xml"/>
  <Default Extension="png" ContentType="image/png"/>
  <Default Extension="gcode" ContentType="text/x.gcode"/>
  <Default Extension="config" ContentType="text/xml"/>
  <Default Extension="md5" ContentType="text/plain"/>
</Types>"#;
        zip.start_file("[Content_Types].xml", options)
            .map_err(|e| e.to_string())?;
        zip.write_all(content_types_xml.as_bytes())
            .map_err(|e| e.to_string())?;

        let rels_xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Target="/3D/3dmodel.model" Id="rel-1" Type="http://schemas.microsoft.com/3dmanufacturing/2013/01/3dmodel"/>
  <Relationship Target="/Metadata/plate_1.png" Id="rel-2" Type="http://schemas.openxmlformats.org/package/2006/relationships/metadata/thumbnail"/>
</Relationships>"#;
        zip.start_file("_rels/.rels", options)
            .map_err(|e| e.to_string())?;
        zip.write_all(rels_xml.as_bytes())
            .map_err(|e| e.to_string())?;

        let model_xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<model unit="millimeter" xml:lang="en-US" xmlns="http://schemas.microsoft.com/3dmanufacturing/core/2015/02" xmlns:BambuStudio="http://schemas.bambulab.com/package/2021">
  <metadata name="Application">BambuStudio-01.09.05.51-Manifold2</metadata>
  <metadata name="BambuStudio:3mfVersion">1</metadata>
  <resources/>
  <build/>
</model>"#;
        zip.start_file("3D/3dmodel.model", options)
            .map_err(|e| e.to_string())?;
        zip.write_all(model_xml.as_bytes())
            .map_err(|e| e.to_string())?;

        // Write `Metadata/plate_1.gcode` and `Metadata/plate_1.gcode.md5`
        let gcode_bytes = gcode_text.as_bytes();
        zip.start_file("Metadata/plate_1.gcode", options)
            .map_err(|e| e.to_string())?;
        zip.write_all(gcode_bytes).map_err(|e| e.to_string())?;

        let md5_digest = crate::moonraker::md5_hex(gcode_bytes);
        zip.start_file("Metadata/plate_1.gcode.md5", options)
            .map_err(|e| e.to_string())?;
        zip.write_all(md5_digest.as_bytes())
            .map_err(|e| e.to_string())?;

        // Extract optional PNG thumbnail from G-code header comments (`; thumbnail begin WxH LEN`)
        if let Some(png_bytes) = extract_largest_png_thumbnail_from_gcode(gcode_text) {
            zip.start_file("Metadata/plate_1.png", options)
                .map_err(|e| e.to_string())?;
            zip.write_all(&png_bytes).map_err(|e| e.to_string())?;
        }

        // Write `Metadata/slice_info.config`
        let slice_info_xml = build_bambu_slice_info_config_xml(gcode_text, config);
        zip.start_file("Metadata/slice_info.config", options)
            .map_err(|e| e.to_string())?;
        zip.write_all(slice_info_xml.as_bytes())
            .map_err(|e| e.to_string())?;

        zip.finish().map_err(|e| e.to_string())?;
    }
    Ok(cursor.into_inner())
}

fn build_bambu_slice_info_config_xml(gcode_text: &str, config: Option<&SlicerConfig>) -> String {
    let mut est_time_s: u64 = 0;
    let mut weight_g: f32 = 0.0;
    let mut length_m: f32 = 0.0;
    let mut nozzle_diam: f32 = config
        .and_then(|c| c.active_tool())
        .map(|t| t.nozzle_diameter_mm)
        .unwrap_or(0.40);

    // Parse footer metadata comments from `gcode_text` when available
    for line in gcode_text.lines().rev().take(64) {
        let trimmed = line.trim().trim_start_matches(';').trim();
        if let Some(val) = trimmed.strip_prefix("estimated printing time (normal mode) =") {
            est_time_s = parse_hms_duration_to_seconds(val.trim());
        } else if let Some(val) = trimmed.strip_prefix("total filament used [g] =") {
            weight_g = val.trim().parse::<f32>().unwrap_or(weight_g);
        } else if let Some(val) = trimmed.strip_prefix("filament used [mm] =") {
            if let Ok(mm) = val.trim().parse::<f32>() {
                length_m = mm / 1000.0;
            }
        } else if let Some(val) = trimmed.strip_prefix("nozzle_diameter =") {
            nozzle_diam = val.trim().parse::<f32>().unwrap_or(nozzle_diam);
        }
    }

    let mut filaments_xml = String::new();
    if let Some(cfg) = config {
        let count = cfg.tools.len().max(1);
        let per_tool_m = if count > 0 { length_m / (count as f32) } else { length_m };
        let per_tool_g = if count > 0 { weight_g / (count as f32) } else { weight_g };
        for (idx, tool) in cfg.tools.iter().enumerate() {
            let tray_id = idx + 1;
            let f_type = escape_xml_attr(if tool.filament_type.trim().is_empty() {
                "PLA"
            } else {
                tool.filament_type.trim()
            });
            let f_color = escape_xml_attr(if tool.filament_color_hex.trim().is_empty() {
                "#2298FA"
            } else {
                tool.filament_color_hex.trim()
            });
            filaments_xml.push_str(&format!(
                "    <filament id=\"{tray_id}\" type=\"{f_type}\" color=\"{f_color}\" used_m=\"{per_tool_m:.2}\" used_g=\"{per_tool_g:.2}\"/>\n"
            ));
        }
    } else {
        filaments_xml.push_str(&format!(
            "    <filament id=\"1\" type=\"PLA\" color=\"#2298FA\" used_m=\"{length_m:.2}\" used_g=\"{weight_g:.2}\"/>\n"
        ));
    }

    format!(
        concat!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n",
            "<config>\n",
            "  <header>\n",
            "    <header_item key=\"X-BBL-Client-Type\" value=\"slicer\"/>\n",
            "    <header_item key=\"X-BBL-Client-Version\" value=\"01.09.05.51-Manifold2\"/>\n",
            "  </header>\n",
            "  <plate>\n",
            "    <metadata key=\"index\" value=\"1\"/>\n",
            "    <metadata key=\"prediction\" value=\"{est_time_s}\"/>\n",
            "    <metadata key=\"weight\" value=\"{weight_g:.2}\"/>\n",
            "    <metadata key=\"outside\" value=\"false\"/>\n",
            "    <metadata key=\"support_used\" value=\"false\"/>\n",
            "    <metadata key=\"nozzle_diameters\" value=\"{nozzle_diam:.2}\"/>\n",
            "{filaments_xml}",
            "  </plate>\n",
            "</config>\n"
        ),
        est_time_s = est_time_s,
        weight_g = weight_g,
        nozzle_diam = nozzle_diam,
        filaments_xml = filaments_xml
    )
}

fn parse_hms_duration_to_seconds(s: &str) -> u64 {
    let mut total: u64 = 0;
    for tok in s.split_whitespace() {
        if let Some(h) = tok.strip_suffix('h') {
            total += h.parse::<u64>().unwrap_or(0) * 3600;
        } else if let Some(m) = tok.strip_suffix('m') {
            total += m.parse::<u64>().unwrap_or(0) * 60;
        } else if let Some(sec) = tok.strip_suffix('s') {
            total += sec.parse::<u64>().unwrap_or(0);
        }
    }
    total
}

fn extract_largest_png_thumbnail_from_gcode(gcode_text: &str) -> Option<Vec<u8>> {
    let mut best_b64 = String::new();
    let mut in_thumb = false;
    let mut cur_b64 = String::new();

    for line in gcode_text.lines().take(1200) {
        let trimmed = line.trim();
        if trimmed.starts_with("; thumbnail begin ") {
            in_thumb = true;
            cur_b64.clear();
        } else if trimmed.starts_with("; thumbnail end") {
            if in_thumb && cur_b64.len() > best_b64.len() {
                best_b64 = cur_b64.clone();
            }
            in_thumb = false;
        } else if in_thumb {
            let chunk = trimmed.trim_start_matches(';').trim();
            cur_b64.push_str(chunk);
        }
    }

    if best_b64.is_empty() {
        None
    } else {
        decode_base64_simple(&best_b64)
    }
}

fn decode_base64_simple(input: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    let mut buf = 0u32;
    let mut bits = 0u8;
    for b in input.bytes() {
        let val = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => break,
            _ if b.is_ascii_whitespace() => continue,
            _ => return None,
        };
        buf = (buf << 6) | (val as u32);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((buf >> bits) & 0xFF) as u8);
        }
    }
    Some(out)
}

/// Parses a 3MF `.model` XML buffer using `quick-xml`, extracting `<object>` meshes,
/// `<basematerials>`, `<m:colorgroup>`, `<m:texture2d>`, `<m:texture2dgroup>`,
/// `<components>`, and `<build><item>` entries while ignoring unknown slicer tags.
fn parse_3mf_model_xml(xml_bytes: &[u8]) -> Result<ParsedModelFile, String> {
    let mut reader = Reader::from_reader(xml_bytes);
    reader.config_mut().trim_text(true);

    let mut buf = Vec::new();
    let mut model_file = ParsedModelFile::default();
    let mut current_object: Option<ParsedXmlObject> = None;
    let mut current_mesh: Option<ParsedXmlMesh> = None;
    let mut current_base_materials: Option<(String, Vec<ParsedBaseMaterial>)> = None;
    let mut current_color_group: Option<(String, Vec<[u8; 4]>)> = None;
    let mut current_tex2d_group: Option<(String, ParsedTexture2dGroup)> = None;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                let qname = e.name();
                let local = local_tag_name(qname.as_ref());
                match local {
                    b"object" => {
                        let mut id = String::new();
                        let mut name = None;
                        let mut pid = None;
                        let mut pindex = None;
                        for attr in e.attributes().flatten() {
                            match local_tag_name(attr.key.as_ref()) {
                                b"id" => {
                                    id = String::from_utf8_lossy(&attr.value).into_owned();
                                }
                                b"name" => {
                                    name = Some(String::from_utf8_lossy(&attr.value).into_owned());
                                }
                                b"pid" => {
                                    pid = Some(String::from_utf8_lossy(&attr.value).into_owned());
                                }
                                b"pindex" => {
                                    pindex = std::str::from_utf8(&attr.value)
                                        .ok()
                                        .and_then(|s| s.trim().parse::<u32>().ok());
                                }
                                _ => {}
                            }
                        }
                        current_object = Some(ParsedXmlObject {
                            id,
                            name,
                            pid,
                            pindex,
                            mesh: None,
                            components: Vec::new(),
                        });
                    }
                    b"basematerials" => {
                        if let Some(id) = extract_xml_id_attr(e) {
                            current_base_materials = Some((id, Vec::new()));
                        }
                    }
                    b"base" => {
                        if let Some((_, ref mut list)) = current_base_materials {
                            parse_base_material_element(e, list);
                        }
                    }
                    b"colorgroup" => {
                        if let Some(id) = extract_xml_id_attr(e) {
                            current_color_group = Some((id, Vec::new()));
                        }
                    }
                    b"color" => {
                        if let Some((_, ref mut list)) = current_color_group {
                            parse_color_element(e, list);
                        }
                    }
                    b"texture2d" => {
                        parse_texture2d_element(e, &mut model_file.textures_2d);
                    }
                    b"texture2dgroup" => {
                        if let Some((id, grp)) = parse_texture2dgroup_start(e) {
                            current_tex2d_group = Some((id, grp));
                        }
                    }
                    b"tex2coord" => {
                        if let Some((_, ref mut grp)) = current_tex2d_group {
                            parse_tex2coord_element(e, grp);
                        }
                    }
                    b"mesh" => {
                        current_mesh = Some(ParsedXmlMesh::default());
                    }
                    b"vertex" => {
                        if let Some(ref mut mesh) = current_mesh {
                            parse_vertex_element(e, mesh);
                        }
                    }
                    b"triangle" => {
                        if let Some(ref mut mesh) = current_mesh {
                            parse_triangle_element(e, mesh);
                        }
                    }
                    b"component" => {
                        if let Some(ref mut obj) = current_object {
                            if let Some(comp) = parse_component_element(e) {
                                obj.components.push(comp);
                            }
                        }
                    }
                    b"item" => {
                        if let Some(item) = parse_build_item_element(e) {
                            model_file.build_items.push(item);
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::Empty(ref e)) => {
                let qname = e.name();
                let local = local_tag_name(qname.as_ref());
                match local {
                    b"base" => {
                        if let Some((_, ref mut list)) = current_base_materials {
                            parse_base_material_element(e, list);
                        }
                    }
                    b"color" => {
                        if let Some((_, ref mut list)) = current_color_group {
                            parse_color_element(e, list);
                        }
                    }
                    b"texture2d" => {
                        parse_texture2d_element(e, &mut model_file.textures_2d);
                    }
                    b"tex2coord" => {
                        if let Some((_, ref mut grp)) = current_tex2d_group {
                            parse_tex2coord_element(e, grp);
                        }
                    }
                    b"vertex" => {
                        if let Some(ref mut mesh) = current_mesh {
                            parse_vertex_element(e, mesh);
                        }
                    }
                    b"triangle" => {
                        if let Some(ref mut mesh) = current_mesh {
                            parse_triangle_element(e, mesh);
                        }
                    }
                    b"component" => {
                        if let Some(ref mut obj) = current_object {
                            if let Some(comp) = parse_component_element(e) {
                                obj.components.push(comp);
                            }
                        }
                    }
                    b"item" => {
                        if let Some(item) = parse_build_item_element(e) {
                            model_file.build_items.push(item);
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::End(ref e)) => {
                let qname = e.name();
                let local = local_tag_name(qname.as_ref());
                match local {
                    b"basematerials" => {
                        if let Some((id, list)) = current_base_materials.take() {
                            if !id.is_empty() && !list.is_empty() {
                                model_file.base_materials.insert(id, list);
                            }
                        }
                    }
                    b"colorgroup" => {
                        if let Some((id, list)) = current_color_group.take() {
                            if !id.is_empty() && !list.is_empty() {
                                model_file.color_groups.insert(id, list);
                            }
                        }
                    }
                    b"texture2dgroup" => {
                        if let Some((id, grp)) = current_tex2d_group.take() {
                            if !id.is_empty() && !grp.coords.is_empty() {
                                model_file.texture_2d_groups.insert(id, grp);
                            }
                        }
                    }
                    b"mesh" => {
                        if let (Some(ref mut obj), Some(mesh)) =
                            (&mut current_object, current_mesh.take())
                        {
                            obj.mesh = Some(mesh);
                        }
                    }
                    b"object" => {
                        if let Some(obj) = current_object.take() {
                            if !obj.id.is_empty() {
                                model_file.objects.insert(obj.id.clone(), obj);
                            }
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(format!("XML error at position {}: {e}", reader.error_position())),
            _ => {}
        }
        buf.clear();
    }

    Ok(model_file)
}

fn extract_xml_id_attr(e: &quick_xml::events::BytesStart<'_>) -> Option<String> {
    for attr in e.attributes().flatten() {
        if local_tag_name(attr.key.as_ref()) == b"id" {
            let id = String::from_utf8_lossy(&attr.value).trim().to_string();
            if !id.is_empty() {
                return Some(id);
            }
        }
    }
    None
}

fn parse_base_material_element(
    e: &quick_xml::events::BytesStart<'_>,
    out: &mut Vec<ParsedBaseMaterial>,
) {
    let mut name = String::new();
    let mut rgba8 = [214u8, 217, 224, 255];
    for attr in e.attributes().flatten() {
        let val = String::from_utf8_lossy(&attr.value);
        match local_tag_name(attr.key.as_ref()) {
            b"name" => name = val.into_owned(),
            b"displaycolor" => {
                if let Some(parsed) = color_optics::parse_color_encoding_rgba8(&val) {
                    rgba8 = parsed;
                }
            }
            _ => {}
        }
    }
    out.push(ParsedBaseMaterial { name, rgba8 });
}

fn parse_color_element(e: &quick_xml::events::BytesStart<'_>, out: &mut Vec<[u8; 4]>) {
    let mut rgba8 = [214u8, 217, 224, 255];
    for attr in e.attributes().flatten() {
        if local_tag_name(attr.key.as_ref()) == b"color" {
            let val = String::from_utf8_lossy(&attr.value);
            if let Some(parsed) = color_optics::parse_color_encoding_rgba8(&val) {
                rgba8 = parsed;
            }
        }
    }
    out.push(rgba8);
}

fn parse_texture2d_element(
    e: &quick_xml::events::BytesStart<'_>,
    out: &mut HashMap<String, ParsedTexture2d>,
) {
    let mut id = String::new();
    let mut path = String::new();
    let mut tile_u = TextureTileStyle::Wrap;
    let mut tile_v = TextureTileStyle::Wrap;
    for attr in e.attributes().flatten() {
        let val = String::from_utf8_lossy(&attr.value);
        match local_tag_name(attr.key.as_ref()) {
            b"id" => id = val.trim().to_string(),
            b"path" => path = val.trim().trim_start_matches('/').to_string(),
            b"tilestyleu" => tile_u = TextureTileStyle::from_attr(val.trim()),
            b"tilestylev" => tile_v = TextureTileStyle::from_attr(val.trim()),
            _ => {}
        }
    }
    if !id.is_empty() && !path.is_empty() {
        out.insert(
            id,
            ParsedTexture2d {
                path,
                tile_u,
                tile_v,
            },
        );
    }
}

fn parse_texture2dgroup_start(
    e: &quick_xml::events::BytesStart<'_>,
) -> Option<(String, ParsedTexture2dGroup)> {
    let mut id = String::new();
    let mut tex_id = String::new();
    for attr in e.attributes().flatten() {
        let val = String::from_utf8_lossy(&attr.value);
        match local_tag_name(attr.key.as_ref()) {
            b"id" => id = val.trim().to_string(),
            b"texid" => tex_id = val.trim().to_string(),
            _ => {}
        }
    }
    if id.is_empty() || tex_id.is_empty() {
        None
    } else {
        Some((
            id,
            ParsedTexture2dGroup {
                tex_id,
                coords: Vec::new(),
            },
        ))
    }
}

fn parse_tex2coord_element(
    e: &quick_xml::events::BytesStart<'_>,
    grp: &mut ParsedTexture2dGroup,
) {
    let mut u = 0.0_f32;
    let mut v = 0.0_f32;
    for attr in e.attributes().flatten() {
        let val = std::str::from_utf8(&attr.value).unwrap_or("0");
        match local_tag_name(attr.key.as_ref()) {
            b"u" => u = val.trim().parse().unwrap_or(0.0),
            b"v" => v = val.trim().parse().unwrap_or(0.0),
            _ => {}
        }
    }
    grp.coords.push([u, v]);
}

fn local_tag_name(raw: &[u8]) -> &[u8] {
    if let Some(pos) = raw.iter().rposition(|&b| b == b':') {
        &raw[pos + 1..]
    } else {
        raw
    }
}

fn parse_vertex_element(e: &quick_xml::events::BytesStart<'_>, mesh: &mut ParsedXmlMesh) {
    let mut x = 0.0_f32;
    let mut y = 0.0_f32;
    let mut z = 0.0_f32;
    for attr in e.attributes().flatten() {
        let val_str = std::str::from_utf8(&attr.value).unwrap_or("0");
        match local_tag_name(attr.key.as_ref()) {
            b"x" => x = val_str.parse().unwrap_or(0.0),
            b"y" => y = val_str.parse().unwrap_or(0.0),
            b"z" => z = val_str.parse().unwrap_or(0.0),
            _ => {}
        }
    }
    mesh.positions.push(x);
    mesh.positions.push(y);
    mesh.positions.push(z);
}

fn intern_mesh_prop_pid(mesh: &mut ParsedXmlMesh, pid: &str) -> u16 {
    if let Some(pos) = mesh.prop_pids.iter().position(|s| s == pid) {
        return (pos as u16).saturating_add(1);
    }
    if mesh.prop_pids.len() >= (u16::MAX as usize) - 1 {
        return 0;
    }
    mesh.prop_pids.push(pid.to_string());
    mesh.prop_pids.len() as u16
}

fn parse_triangle_element(e: &quick_xml::events::BytesStart<'_>, mesh: &mut ParsedXmlMesh) {
    let prev_tri_count = mesh.indices.len() / 3;
    let mut v1 = 0_u32;
    let mut v2 = 0_u32;
    let mut v3 = 0_u32;
    let mut mmu_seg_bytes: Option<Vec<u8>> = None;
    let mut tri_pid: Option<String> = None;
    let mut p1_opt: Option<u32> = None;
    let mut p2_opt: Option<u32> = None;
    let mut p3_opt: Option<u32> = None;

    for attr in e.attributes().flatten() {
        let val_str = std::str::from_utf8(&attr.value).unwrap_or("0");
        match local_tag_name(attr.key.as_ref()) {
            b"v1" => v1 = val_str.parse().unwrap_or(0),
            b"v2" => v2 = val_str.parse().unwrap_or(0),
            b"v3" => v3 = val_str.parse().unwrap_or(0),
            b"mmu_segmentation" | b"paint_color" => {
                let trimmed = val_str.trim().as_bytes();
                if !trimmed.is_empty() {
                    mmu_seg_bytes = Some(trimmed.to_vec());
                }
            }
            b"pid" => {
                let s = val_str.trim();
                if !s.is_empty() {
                    tri_pid = Some(s.to_string());
                }
            }
            b"p1" => p1_opt = val_str.trim().parse::<u32>().ok(),
            b"p2" => p2_opt = val_str.trim().parse::<u32>().ok(),
            b"p3" => p3_opt = val_str.trim().parse::<u32>().ok(),
            _ => {}
        }
    }

    if let Some(ref hex_bytes) = mmu_seg_bytes {
        let mut cursor = hex_bytes.len();
        emit_mmu_segmented_triangle(mesh, [v1, v2, v3], hex_bytes, &mut cursor, 0);
    } else {
        mesh.indices.push(v1);
        mesh.indices.push(v2);
        mesh.indices.push(v3);
        mesh.triangle_mmu_states.push(0);
    }

    let has_prop = tri_pid.is_some() || p1_opt.is_some() || p2_opt.is_some() || p3_opt.is_some();
    if has_prop || !mesh.triangle_prop_refs.is_empty() {
        if mesh.triangle_prop_refs.len() < prev_tri_count {
            mesh.triangle_prop_refs
                .resize(prev_tri_count, TrianglePropRef::default());
        }
        let pid_idx_one_based = tri_pid
            .as_deref()
            .map(|pid| intern_mesh_prop_pid(mesh, pid))
            .unwrap_or(0);
        let prop_ref = if let Some(p1) = p1_opt {
            TrianglePropRef {
                pid_idx_one_based,
                has_p1: true,
                p1,
                p2: p2_opt.unwrap_or(p1),
                p3: p3_opt.unwrap_or(p1),
            }
        } else {
            TrianglePropRef {
                pid_idx_one_based,
                has_p1: false,
                p1: 0,
                p2: 0,
                p3: 0,
            }
        };
        let new_tri_count = mesh.indices.len() / 3;
        mesh.triangle_prop_refs.resize(new_tri_count, prop_ref);
    }
}

fn read_next_mmu_nibble(hex_bytes: &[u8], cursor: &mut usize) -> u8 {
    while *cursor > 0 {
        *cursor -= 1;
        let b = hex_bytes[*cursor];
        let val = match b {
            b'0'..=b'9' => Some(b - b'0'),
            b'a'..=b'f' => Some(b - b'a' + 10),
            b'A'..=b'F' => Some(b - b'A' + 10),
            _ => None,
        };
        if let Some(v) = val {
            return v;
        }
    }
    0
}

fn add_midpoint_vertex(mesh: &mut ParsedXmlMesh, va: u32, vb: u32) -> u32 {
    let num_verts = mesh.positions.len() / 3;
    let ia = (va as usize).min(num_verts.saturating_sub(1)) * 3;
    let ib = (vb as usize).min(num_verts.saturating_sub(1)) * 3;
    let mx = 0.5 * (mesh.positions[ia] + mesh.positions[ib]);
    let my = 0.5 * (mesh.positions[ia + 1] + mesh.positions[ib + 1]);
    let mz = 0.5 * (mesh.positions[ia + 2] + mesh.positions[ib + 2]);
    let idx = num_verts as u32;
    mesh.positions.push(mx);
    mesh.positions.push(my);
    mesh.positions.push(mz);
    idx
}

/// Decodes a PrusaSlicer / Bambu Studio / OrcaSlicer `slic3rpe:mmu_segmentation` (`paint_color`)
/// hex nibble stream (read right-to-left) for triangle `v = [v0, v1, v2]`.
/// For unsplit leaf triangles (`split_sides == 0`), emits `(v0, v1, v2)` directly with zero overhead.
/// For subdivided painted triangles (`split_sides in 1..=3`), recursively splits edges up to depth 4.
fn emit_mmu_segmented_triangle(
    mesh: &mut ParsedXmlMesh,
    v: [u32; 3],
    hex_bytes: &[u8],
    cursor: &mut usize,
    depth: u8,
) -> u8 {
    let code = read_next_mmu_nibble(hex_bytes, cursor);
    let split_sides = code & 0b11;
    let state_or_side = code >> 2;

    if split_sides == 0 {
        let state = if state_or_side == 3 {
            read_next_mmu_nibble(hex_bytes, cursor).saturating_add(3)
        } else {
            state_or_side
        };
        if depth <= 4 {
            mesh.indices.push(v[0]);
            mesh.indices.push(v[1]);
            mesh.indices.push(v[2]);
            mesh.triangle_mmu_states.push(state);
        }
        return state;
    }

    let s = (state_or_side as usize) % 3;
    let i0 = s;
    let i1 = (s + 1) % 3;
    let i2 = (s + 2) % 3;

    if depth >= 4 {
        // Consume child subtree without allocating further sub-triangles, then emit parent with majority state
        let mut child_states = [0u8; 4];
        let num_children = (split_sides as usize) + 1;
        for c in 0..num_children {
            child_states[c] = emit_mmu_segmented_triangle(mesh, v, hex_bytes, cursor, depth + 1);
        }
        let chosen = child_states[..num_children]
            .iter()
            .copied()
            .find(|&st| st > 0)
            .unwrap_or(child_states[0]);
        if depth == 4 {
            mesh.indices.push(v[0]);
            mesh.indices.push(v[1]);
            mesh.indices.push(v[2]);
            mesh.triangle_mmu_states.push(chosen);
        }
        return chosen;
    }

    match split_sides {
        1 => {
            let m = add_midpoint_vertex(mesh, v[i0], v[i1]);
            let s0 = emit_mmu_segmented_triangle(mesh, [v[i2], v[i0], m], hex_bytes, cursor, depth + 1);
            let s1 = emit_mmu_segmented_triangle(mesh, [v[i1], v[i2], m], hex_bytes, cursor, depth + 1);
            if s0 > 0 { s0 } else { s1 }
        }
        2 => {
            let m1 = add_midpoint_vertex(mesh, v[i1], v[i2]);
            let m2 = add_midpoint_vertex(mesh, v[i2], v[i0]);
            let s0 = emit_mmu_segmented_triangle(mesh, [v[i2], m2, m1], hex_bytes, cursor, depth + 1);
            let s1 = emit_mmu_segmented_triangle(mesh, [v[i0], v[i1], m1], hex_bytes, cursor, depth + 1);
            let s2 = emit_mmu_segmented_triangle(mesh, [m1, m2, v[i0]], hex_bytes, cursor, depth + 1);
            if s0 > 0 { s0 } else if s1 > 0 { s1 } else { s2 }
        }
        _ => {
            let m0 = add_midpoint_vertex(mesh, v[0], v[1]);
            let m1 = add_midpoint_vertex(mesh, v[1], v[2]);
            let m2 = add_midpoint_vertex(mesh, v[2], v[0]);
            let s0 = emit_mmu_segmented_triangle(mesh, [v[0], m0, m2], hex_bytes, cursor, depth + 1);
            let s1 = emit_mmu_segmented_triangle(mesh, [m0, v[1], m1], hex_bytes, cursor, depth + 1);
            let s2 = emit_mmu_segmented_triangle(mesh, [m1, v[2], m2], hex_bytes, cursor, depth + 1);
            let s3 = emit_mmu_segmented_triangle(mesh, [m0, m1, m2], hex_bytes, cursor, depth + 1);
            if s0 > 0 { s0 } else if s1 > 0 { s1 } else if s2 > 0 { s2 } else { s3 }
        }
    }
}

fn parse_component_element(e: &quick_xml::events::BytesStart<'_>) -> Option<ParsedComponentRef> {
    let mut object_id = String::new();
    let mut model_path = None;
    let mut transform = Mat4::IDENTITY;

    for attr in e.attributes().flatten() {
        let val_str = String::from_utf8_lossy(&attr.value);
        match local_tag_name(attr.key.as_ref()) {
            b"objectid" => object_id = val_str.into_owned(),
            b"path" => model_path = Some(val_str.into_owned()),
            b"transform" => transform = parse_3mf_transform(&val_str),
            _ => {}
        }
    }

    if object_id.is_empty() {
        None
    } else {
        Some(ParsedComponentRef {
            object_id,
            model_path,
            transform,
        })
    }
}

fn parse_3mf_bool_attr(s: &str) -> Option<bool> {
    let trimmed = s.trim();
    if trimmed.eq_ignore_ascii_case("0")
        || trimmed.eq_ignore_ascii_case("false")
        || trimmed.eq_ignore_ascii_case("no")
    {
        Some(false)
    } else if trimmed.eq_ignore_ascii_case("1")
        || trimmed.eq_ignore_ascii_case("true")
        || trimmed.eq_ignore_ascii_case("yes")
    {
        Some(true)
    } else {
        None
    }
}

fn parse_build_item_element(e: &quick_xml::events::BytesStart<'_>) -> Option<ParsedBuildItem> {
    let mut object_id = String::new();
    let mut model_path = None;
    let mut transform = Mat4::IDENTITY;
    let mut printable = None;

    for attr in e.attributes().flatten() {
        let val_str = String::from_utf8_lossy(&attr.value);
        match local_tag_name(attr.key.as_ref()) {
            b"objectid" => object_id = val_str.into_owned(),
            b"path" => model_path = Some(val_str.into_owned()),
            b"transform" => transform = parse_3mf_transform(&val_str),
            b"printable" => printable = parse_3mf_bool_attr(&val_str),
            _ => {}
        }
    }

    if object_id.is_empty() {
        None
    } else {
        Some(ParsedBuildItem {
            object_id,
            model_path,
            transform,
            printable,
        })
    }
}

/// Parses a 3MF 12-float affine transformation string (`m00 m01 m02 m10 m11 m12 m20 m21 m22 tx ty tz`)
/// into a column-major `Mat4`.
pub fn parse_3mf_transform(s: &str) -> Mat4 {
    let vals: Vec<f32> = s
        .split_whitespace()
        .filter_map(|tok| tok.parse::<f32>().ok())
        .collect();
    if vals.len() != 12 {
        return Mat4::IDENTITY;
    }

    // In 3MF Core Specification, a row vector [x y z 1] is multiplied on the right
    // by the 4x3 matrix:
    //   [ m00 m01 m02 0 ]
    //   [ m10 m11 m12 0 ]
    //   [ m20 m21 m22 0 ]
    //   [ tx  ty  tz  1 ]
    // Which corresponds to column vectors in glam/Bevy `Mat4::from_cols`:
    Mat4::from_cols(
        Vec4::new(vals[0], vals[1], vals[2], 0.0),
        Vec4::new(vals[3], vals[4], vals[5], 0.0),
        Vec4::new(vals[6], vals[7], vals[8], 0.0),
        Vec4::new(vals[9], vals[10], vals[11], 1.0),
    )
}

/// Formats a Bevy `Transform` into the 3MF 12-float affine matrix string.
pub fn format_3mf_transform(transform: &Transform) -> String {
    let mat = transform.compute_matrix();
    let c0 = mat.col(0);
    let c1 = mat.col(1);
    let c2 = mat.col(2);
    let c3 = mat.col(3);
    format!(
        "{:.6} {:.6} {:.6} {:.6} {:.6} {:.6} {:.6} {:.6} {:.6} {:.6} {:.6} {:.6}",
        c0.x, c0.y, c0.z, c1.x, c1.y, c1.z, c2.x, c2.y, c2.z, c3.x, c3.y, c3.z
    )
}

fn escape_xml_attr(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Parses `Metadata/model_settings.config` or `Metadata/Slic3r_PE_model.config`
/// (OrcaSlicer / Bambu Studio / PrusaSlicer XML) to extract `<object id="...">` names,
/// per-object/part/volume `extruder` assignments, and `<plate>` assignments.
fn parse_model_settings_config_xml(xml_bytes: &[u8]) -> Result<ParsedModelSettingsConfig, String> {
    let mut reader = Reader::from_reader(xml_bytes);
    reader.config_mut().trim_text(true);

    let mut buf = Vec::new();
    let mut result = ParsedModelSettingsConfig::default();
    let mut current_object_id: Option<String> = None;
    let mut current_part_id: Option<String> = None;
    let mut current_volume_range: Option<ParsedVolumeExtruderRange> = None;
    let mut current_plate: Option<ParsedConfigPlate> = None;
    let mut current_instance: Option<(Option<String>, Option<u32>, Option<bool>)> = None;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                let qname = e.name();
                let local = local_tag_name(qname.as_ref());
                match local {
                    b"object" => {
                        for attr in e.attributes().flatten() {
                            if local_tag_name(attr.key.as_ref()) == b"id" {
                                current_object_id =
                                    Some(String::from_utf8_lossy(&attr.value).into_owned());
                            }
                        }
                        current_part_id = None;
                        current_volume_range = None;
                    }
                    b"part" => {
                        let mut part_id = None;
                        for attr in e.attributes().flatten() {
                            if local_tag_name(attr.key.as_ref()) == b"id" {
                                part_id = Some(String::from_utf8_lossy(&attr.value).into_owned());
                            }
                        }
                        current_part_id = part_id.or_else(|| Some(String::new()));
                    }
                    b"volume" => {
                        let mut first_id = 0usize;
                        let mut last_id = 0usize;
                        for attr in e.attributes().flatten() {
                            let val_str = std::str::from_utf8(&attr.value).unwrap_or("0");
                            match local_tag_name(attr.key.as_ref()) {
                                b"firstid" => first_id = val_str.parse().unwrap_or(0),
                                b"lastid" => last_id = val_str.parse().unwrap_or(0),
                                _ => {}
                            }
                        }
                        current_volume_range = Some(ParsedVolumeExtruderRange {
                            first_id,
                            last_id,
                            extruder_one_based: 0,
                        });
                    }
                    b"plate" => {
                        current_plate = Some(ParsedConfigPlate::default());
                    }
                    b"model_instance" => {
                        current_instance = Some((None, None, None));
                    }
                    b"metadata" => {
                        handle_model_settings_metadata(
                            e,
                            &current_object_id,
                            &current_part_id,
                            &mut current_volume_range,
                            &mut current_plate,
                            &mut current_instance,
                            &mut result,
                        );
                    }
                    _ => {}
                }
            }
            Ok(Event::Empty(ref e)) => {
                let qname = e.name();
                let local = local_tag_name(qname.as_ref());
                if local == b"metadata" {
                    handle_model_settings_metadata(
                        e,
                        &current_object_id,
                        &current_part_id,
                        &mut current_volume_range,
                        &mut current_plate,
                        &mut current_instance,
                        &mut result,
                    );
                }
            }
            Ok(Event::End(ref e)) => {
                let qname = e.name();
                let local = local_tag_name(qname.as_ref());
                match local {
                    b"object" => {
                        current_object_id = None;
                        current_part_id = None;
                        current_volume_range = None;
                    }
                    b"part" => {
                        current_part_id = None;
                    }
                    b"volume" => {
                        if let (Some(ref obj_id), Some(vol_range)) =
                            (&current_object_id, current_volume_range.take())
                        {
                            if vol_range.extruder_one_based > 0 {
                                result
                                    .volume_extruders
                                    .entry(obj_id.clone())
                                    .or_default()
                                    .push(vol_range);
                            }
                        }
                    }
                    b"model_instance" => {
                        if let (Some(ref mut plate), Some((Some(obj_id), inst_opt, pr_opt))) =
                            (&mut current_plate, current_instance.take())
                        {
                            let inst_id = inst_opt.unwrap_or(0);
                            if let Some(pr) = pr_opt {
                                result.instance_printable.insert((obj_id.clone(), inst_id), pr);
                            }
                            plate.instances.push((obj_id, inst_id));
                        }
                    }
                    b"plate" => {
                        if let Some(plate) = current_plate.take() {
                            result.plates.push(plate);
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => {
                return Err(format!(
                    "XML error in model_settings.config at {}: {e}",
                    reader.error_position()
                ))
            }
            _ => {}
        }
        buf.clear();
    }

    result.plates.sort_by_key(|p| {
        if p.plater_id == 0 {
            u32::MAX
        } else {
            p.plater_id
        }
    });
    Ok(result)
}

fn handle_model_settings_metadata(
    e: &quick_xml::events::BytesStart<'_>,
    current_object_id: &Option<String>,
    current_part_id: &Option<String>,
    current_volume_range: &mut Option<ParsedVolumeExtruderRange>,
    current_plate: &mut Option<ParsedConfigPlate>,
    current_instance: &mut Option<(Option<String>, Option<u32>, Option<bool>)>,
    result: &mut ParsedModelSettingsConfig,
) {
    let mut key = String::new();
    let mut value = String::new();
    for attr in e.attributes().flatten() {
        match local_tag_name(attr.key.as_ref()) {
            b"key" => key = String::from_utf8_lossy(&attr.value).into_owned(),
            b"value" => value = String::from_utf8_lossy(&attr.value).into_owned(),
            _ => {}
        }
    }

    if let Some((ref mut obj_id_slot, ref mut inst_id_slot, ref mut pr_slot)) = current_instance {
        match key.as_str() {
            "object_id" => *obj_id_slot = Some(value),
            "instance_id" => *inst_id_slot = value.parse::<u32>().ok(),
            "printable" => *pr_slot = parse_3mf_bool_attr(&value),
            _ => {}
        }
    } else if let Some(ref mut plate) = current_plate {
        match key.as_str() {
            "plater_id" => {
                if let Ok(id) = value.parse::<u32>() {
                    plate.plater_id = id;
                }
            }
            "plater_name" => {
                plate.plater_name = value;
            }
            _ => {}
        }
    } else if let Some(ref mut vol_range) = current_volume_range {
        if key == "extruder" {
            if let Ok(ext) = value.trim().parse::<u8>() {
                vol_range.extruder_one_based = ext;
            }
        } else if key == "name" && !value.trim().is_empty() {
            if let Some(ref obj_id) = current_object_id {
                result.object_names.entry(obj_id.clone()).or_insert(value);
            }
        }
    } else if let Some(ref obj_id) = current_object_id {
        let in_part = current_part_id.is_some();
        if key == "name" && !value.trim().is_empty() {
            if !in_part || !result.object_names.contains_key(obj_id) {
                result.object_names.insert(obj_id.clone(), value);
            }
        } else if key == "printable" {
            if let Some(pr) = parse_3mf_bool_attr(&value) {
                if !in_part || !result.object_printable.contains_key(obj_id) {
                    result.object_printable.insert(obj_id.clone(), pr);
                }
            }
        } else if key == "extruder" {
            if let Ok(ext) = value.trim().parse::<u8>() {
                if ext > 0 {
                    if let Some(ref part_id) = current_part_id {
                        if !part_id.is_empty() {
                            result.object_extruders.insert(part_id.clone(), ext);
                        }
                        result.object_extruders.entry(obj_id.clone()).or_insert(ext);
                    } else {
                        result.object_extruders.insert(obj_id.clone(), ext);
                    }
                }
            }
        }
    }
}

/// Parses `Metadata/Slic3r_PE.config` (PrusaSlicer INI-style key-value config) to extract
/// `extruder_colour`, `filament_colour`, and `filament_type` definitions into `Vec<ThreeMfFilamentSlot>`.
fn parse_slic3r_pe_config_filament_slots(text: &str) -> Vec<ThreeMfFilamentSlot> {
    let mut extruder_colours: Vec<String> = Vec::new();
    let mut filament_colours: Vec<String> = Vec::new();
    let mut filament_types: Vec<String> = Vec::new();

    for raw_line in text.lines() {
        let line = raw_line.trim().trim_start_matches(';').trim();
        let Some((key, val)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let val = val.trim();
        match key {
            "extruder_colour" => {
                extruder_colours = val.split(';').map(|s| s.trim().to_string()).collect();
            }
            "filament_colour" => {
                filament_colours = val.split(';').map(|s| s.trim().to_string()).collect();
            }
            "filament_type" => {
                filament_types = val.split(';').map(|s| s.trim().to_string()).collect();
            }
            _ => {}
        }
    }

    build_filament_slots_from_lists(&extruder_colours, &filament_colours, &filament_types)
}

/// Parses `"filament_colour"` and `"filament_type"` arrays from `Metadata/project_settings.config`
/// (OrcaSlicer / Bambu Studio JSON config).
fn parse_project_settings_filament_slots(json_text: &str) -> Vec<ThreeMfFilamentSlot> {
    let filament_colours = extract_json_string_array(json_text, "\"filament_colour\"");
    let extruder_colours = extract_json_string_array(json_text, "\"extruder_colour\"");
    let filament_types = extract_json_string_array(json_text, "\"filament_type\"");
    build_filament_slots_from_lists(&extruder_colours, &filament_colours, &filament_types)
}

fn extract_json_string_array(json_text: &str, key_quoted: &str) -> Vec<String> {
    let Some(key_idx) = json_text.find(key_quoted) else {
        return Vec::new();
    };
    let after_key = &json_text[key_idx + key_quoted.len()..];
    let Some(open_bracket) = after_key.find('[') else {
        return Vec::new();
    };
    let after_open = &after_key[open_bracket + 1..];
    let Some(close_bracket) = after_open.find(']') else {
        return Vec::new();
    };
    let array_body = &after_open[..close_bracket];
    array_body
        .split(',')
        .map(|tok| tok.trim().trim_matches('"').trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn build_filament_slots_from_lists(
    extruder_colours: &[String],
    filament_colours: &[String],
    filament_types: &[String],
) -> Vec<ThreeMfFilamentSlot> {
    let count = extruder_colours
        .len()
        .max(filament_colours.len())
        .max(filament_types.len());
    if count == 0 {
        return Vec::new();
    }
    let mut slots = Vec::with_capacity(count);
    for idx in 0..count {
        let mut slot = ThreeMfFilamentSlot::default_for_slot(idx, idx);
        let hex_candidate = extruder_colours
            .get(idx)
            .filter(|s| !s.is_empty())
            .or_else(|| filament_colours.get(idx).filter(|s| !s.is_empty()));
        if let Some(hex) = hex_candidate {
            if let Some(rgba) = ThreeMfFilamentSlot::parse_hex_color(hex) {
                slot.color_srgba = rgba;
                slot.color_hex = ThreeMfFilamentSlot::format_hex_color(rgba);
            }
        }
        if let Some(ftype) = filament_types.get(idx).filter(|s| !s.is_empty()) {
            slot.filament_type = ftype.clone();
        }
        slot.name = format!(
            "Filament {} ({} {})",
            idx + 1,
            slot.filament_type,
            slot.color_hex
        );
        slots.push(slot);
    }
    slots
}

/// Extracts `(width_x, depth_y)` in mm from `"printable_area"` inside `Metadata/project_settings.config`
/// (e.g. `"printable_area": ["0x0", "350x0", "350x350", "0x350"]`).
fn parse_project_settings_bed_size(json_text: &str) -> Option<(f32, f32)> {
    let key_idx = json_text.find("\"printable_area\"")?;
    let after_key = &json_text[key_idx + "\"printable_area\"".len()..];
    let open_bracket = after_key.find('[')?;
    let after_open = &after_key[open_bracket + 1..];
    let close_bracket = after_open.find(']')?;
    let array_body = &after_open[..close_bracket];

    let mut min_x = f32::INFINITY;
    let mut max_x = f32::NEG_INFINITY;
    let mut min_y = f32::INFINITY;
    let mut max_y = f32::NEG_INFINITY;
    let mut count = 0usize;

    for token in array_body.split(',') {
        let trimmed = token.trim().trim_matches('"').trim();
        let Some((x_str, y_str)) = trimmed.split_once(['x', 'X']) else {
            continue;
        };
        if let (Ok(x), Ok(y)) = (x_str.trim().parse::<f32>(), y_str.trim().parse::<f32>()) {
            min_x = min_x.min(x);
            max_x = max_x.max(x);
            min_y = min_y.min(y);
            max_y = max_y.max(y);
            count += 1;
        }
    }

    if count >= 2 {
        let w = max_x - min_x;
        let d = max_y - min_y;
        if w > 10.0 && d > 10.0 {
            return Some((w, d));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_3mf_transform_roundtrip() {
        let t = Transform {
            translation: Vec3::new(42.5, -18.0, 0.0),
            rotation: Quat::from_rotation_z(0.75) * Quat::from_rotation_x(0.3),
            scale: Vec3::ONE,
        };
        let encoded = format_3mf_transform(&t);
        let decoded = parse_3mf_transform(&encoded);
        let expected = t.compute_matrix();

        for col in 0..4 {
            let diff = (decoded.col(col) - expected.col(col)).length();
            assert!(diff < 1e-4, "Column {col} mismatch: {diff}");
        }
    }

    #[test]
    fn test_parse_3mf_model_with_unknown_third_party_tags() {
        let sample_xml = br#"<?xml version="1.0" encoding="UTF-8"?>
<model unit="millimeter" xmlns="http://schemas.microsoft.com/3dmanufacturing/core/2015/02" xmlns:slic3rpe="http://schemas.slic3r.org/3mf/2017/06">
  <metadata name="slic3rpe:Version3mf">1</metadata>
  <resources>
    <object id="1" name="TestTetra" type="model">
      <slic3rpe:custom_tag foo="bar"/>
      <mesh>
        <vertices>
          <vertex x="0" y="0" z="0"/>
          <vertex x="10" y="0" z="0"/>
          <vertex x="0" y="10" z="0"/>
          <vertex x="0" y="0" z="10"/>
        </vertices>
        <triangles>
          <triangle v1="0" v2="2" v3="1"/>
          <triangle v1="0" v2="1" v3="3"/>
          <triangle v1="0" v2="3" v3="2"/>
          <triangle v1="1" v2="2" v3="3"/>
        </triangles>
      </mesh>
    </object>
  </resources>
  <build>
    <item objectid="1" transform="1 0 0 0 1 0 0 0 1 15 -20 0"/>
  </build>
</model>"#;

        let parsed = parse_3mf_model_xml(sample_xml).expect("Should parse 3MF XML cleanly");
        assert_eq!(parsed.objects.len(), 1);
        assert_eq!(parsed.build_items.len(), 1);
        let obj = parsed.objects.get("1").unwrap();
        assert_eq!(obj.name.as_deref(), Some("TestTetra"));
        let mesh = obj.mesh.as_ref().unwrap();
        assert_eq!(mesh.positions.len(), 12);
        assert_eq!(mesh.indices.len(), 12);
    }

    #[test]
    fn test_manifold_3mf_project_filename_recalled_on_load() {
        let temp_3mf = std::env::temp_dir().join(format!(
            "custom_manifold_project_{}.3mf",
            std::process::id()
        ));
        let expected_filename = temp_3mf
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap()
            .to_string();

        let plate_names = vec!["Plate 1".to_string(), "Plate 2 (Rear)".to_string()];
        let export_objs = vec![
            ExportableObject {
                object_id: 1,
                name: "TetraA".to_string(),
                has_brep: false,
                printable: true,
                plate_index: 0,
                transform: Transform::IDENTITY,
                vertices: vec![
                    RenderVertex { x: 0.0, y: 0.0, z: 0.0, nx: 0.0, ny: 0.0, nz: -1.0 },
                    RenderVertex { x: 10.0, y: 0.0, z: 0.0, nx: 0.0, ny: 0.0, nz: -1.0 },
                    RenderVertex { x: 0.0, y: 10.0, z: 0.0, nx: 0.0, ny: 0.0, nz: -1.0 },
                    RenderVertex { x: 0.0, y: 0.0, z: 10.0, nx: 0.0, ny: 0.0, nz: 1.0 },
                ],
                indices: vec![0, 2, 1, 0, 1, 3, 0, 3, 2, 1, 2, 3],
                filament_mode: ObjectFilamentMode::default(),
                custom_color_srgba: None,
            },
            ExportableObject {
                object_id: 2,
                name: "TetraB".to_string(),
                has_brep: false,
                printable: false,
                plate_index: 1,
                transform: Transform::from_xyz(15.0, -10.0, 0.0),
                vertices: vec![
                    RenderVertex { x: 0.0, y: 0.0, z: 0.0, nx: 0.0, ny: 0.0, nz: -1.0 },
                    RenderVertex { x: 10.0, y: 0.0, z: 0.0, nx: 0.0, ny: 0.0, nz: -1.0 },
                    RenderVertex { x: 0.0, y: 10.0, z: 0.0, nx: 0.0, ny: 0.0, nz: -1.0 },
                    RenderVertex { x: 0.0, y: 0.0, z: 10.0, nx: 0.0, ny: 0.0, nz: 1.0 },
                ],
                indices: vec![0, 2, 1, 0, 1, 3, 0, 3, 2, 1, 2, 3],
                filament_mode: ObjectFilamentMode::OpticalFrontLit,
                custom_color_srgba: Some([0.2, 0.7, 0.9, 1.0]),
            },
        ];
        let cfg = SlicerConfig::default();
        save_project_to_3mf(&temp_3mf, &plate_names, &export_objs, &cfg)
            .expect("Should write Manifold 3MF");

        let mut report = LoadedBatchReport::default();
        load_3mf_archive(&temp_3mf, &mut report);
        let _ = std::fs::remove_file(&temp_3mf);

        assert_eq!(
            report.manifold_project_filename.as_deref(),
            Some(expected_filename.as_str())
        );
        assert!(report.restored_config.is_some());
        assert_eq!(report.plates.len(), 2);
        assert_eq!(report.plates[0].name, "Plate 1");
        assert_eq!(report.plates[1].name, "Plate 2 (Rear)");
        assert_eq!(report.objects.len(), 2);
        assert_eq!(report.objects[0].name, "TetraA");
        assert_eq!(report.objects[0].plate_index, 0);
        assert!(report.objects[0].printable);
        assert_eq!(report.objects[0].filament_mode, ObjectFilamentMode::ProjectDefault);
        assert_eq!(report.objects[0].custom_color_srgba, None);
        assert_eq!(report.objects[1].name, "TetraB");
        assert_eq!(report.objects[1].plate_index, 1);
        assert!(!report.objects[1].printable);
        assert_eq!(report.objects[1].filament_mode, ObjectFilamentMode::OpticalFrontLit);
        assert_eq!(report.objects[1].custom_color_srgba, Some([0.2, 0.7, 0.9, 1.0]));
        for obj in &report.objects {
            let _ = ffi::remove_cad_object(obj.object_id);
        }
    }

    #[test]
    fn test_import_orca_multi_plate_3mf_iron_heart() {
        let sample_path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../Test Objects/Iron heart MOD (deepergrain).3mf");
        if !sample_path.exists() {
            return;
        }

        let mut report = LoadedBatchReport::default();
        load_3mf_archive(&sample_path, &mut report);

        assert!(
            report.errors.is_empty(),
            "Expected no errors importing Iron heart MOD 3mf, got: {:?}",
            report.errors
        );
        assert_eq!(report.plates.len(), 3, "Expected 3 build plates");
        assert_eq!(report.plates[0].name, "Plate 1");
        assert_eq!(report.plates[1].name, "Plate 2");
        assert_eq!(report.plates[2].name, "Plate 3");

        assert_eq!(report.objects.len(), 3, "Expected 3 objects (1 per plate)");
        assert_eq!(report.objects[0].name, "Front shroud 80mm (2)");
        assert_eq!(report.objects[0].plate_index, 0);
        assert!(
            report.objects[0].initial_translation.xy().length() < 1.0,
            "Plate 1 object should be centered near (0, 0), got {:?}",
            report.objects[0].initial_translation
        );

        assert_eq!(report.objects[1].name, "Rear Shroud 80mm (2)");
        assert_eq!(report.objects[1].plate_index, 1);
        assert!(
            report.objects[1].initial_translation.xy().length() < 1.0,
            "Plate 2 object should be centered near (0, 0), got {:?}",
            report.objects[1].initial_translation
        );

        assert_eq!(report.objects[2].name, "Burly Z cable chain mount");
        assert_eq!(report.objects[2].plate_index, 2);
        assert!(
            report.objects[2].initial_translation.xy().length() < 1.0,
            "Plate 3 object should be centered near (0, 0), got {:?}",
            report.objects[2].initial_translation
        );

        for obj in &report.objects {
            let _ = ffi::remove_cad_object(obj.object_id);
        }
    }

    #[test]
    fn test_import_prusa_multicolor_3mf_lizard() {
        let sample_path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../Test Objects/kogi3d_colorful_lizard_prusa.3mf");
        if !sample_path.exists() {
            return;
        }

        let mut report = LoadedBatchReport::default();
        load_3mf_archive(&sample_path, &mut report);

        assert!(
            report.errors.is_empty(),
            "Expected no errors importing kogi3d_colorful_lizard_prusa.3mf, got: {:?}",
            report.errors
        );
        assert_eq!(report.filament_slots.len(), 4, "Expected 4 filament slots");
        assert_eq!(report.filament_slots[0].color_hex, "#25CC0F");
        assert_eq!(report.filament_slots[1].color_hex, "#A958CE");
        assert_eq!(report.filament_slots[2].color_hex, "#000000");
        assert_eq!(report.filament_slots[3].color_hex, "#F5F5F2");

        assert_eq!(report.objects.len(), 1);
        let obj = &report.objects[0];
        assert_eq!(obj.name, "KOGI3D_lizard_purple");
        assert_eq!(obj.triangle_filament_slots.len(), 1_003_422);
        assert_eq!(obj.vertex_filament_slots.len(), obj.vertices.len());

        let mut counts = [0usize; 4];
        for &slot in &obj.triangle_filament_slots {
            assert!((slot as usize) < 4);
            counts[slot as usize] += 1;
        }
        assert_eq!(counts[0], 80_526);
        assert_eq!(counts[1], 231_755);
        assert_eq!(counts[2], 369_889);
        assert_eq!(counts[3], 321_252);

        let backend_slots = ffi::get_object_triangle_filament_slots(obj.object_id);
        assert_eq!(backend_slots.len(), 1_003_422);

        for obj in &report.objects {
            let _ = ffi::remove_cad_object(obj.object_id);
        }
    }

    #[test]
    fn test_import_3mf_colorgroup_and_png_texture2d() {
        let temp_3mf = std::env::temp_dir().join(format!(
            "manifold2_textured_3mf_test_{}.3mf",
            std::process::id()
        ));
        let file = File::create(&temp_3mf).expect("create temp 3mf");
        let mut zip = ZipWriter::new(file);
        let opts = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);

        // Encode a 2x2 RGBA PNG:
        // Top-left (u=0, v=1): Pure Cyan (0, 174, 239)
        // Top-right (u=1, v=1): Pure Magenta (236, 0, 140)
        // Bottom-left (u=0, v=0): Pure Yellow (255, 242, 0)
        // Bottom-right (u=1, v=0): Pure White (255, 255, 255)
        let mut png_bytes = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut png_bytes, 2, 2);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().expect("png header");
            let px: [u8; 16] = [
                0, 174, 239, 255, 236, 0, 140, 255, // top row (v=1)
                255, 242, 0, 255, 255, 255, 255, 255, // bottom row (v=0)
            ];
            writer.write_image_data(&px).expect("png data");
        }
        zip.start_file("3D/Textures/palette.png", opts).unwrap();
        zip.write_all(&png_bytes).unwrap();

        let model_xml = r##"<?xml version="1.0" encoding="UTF-8"?>
<model unit="millimeter" xmlns="http://schemas.microsoft.com/3dmanufacturing/core/2015/02" xmlns:m="http://schemas.microsoft.com/3dmanufacturing/material/2015/02">
  <resources>
    <m:colorgroup id="10">
      <m:color color="#FF4020"/>
      <m:color color="#20A0FF"/>
    </m:colorgroup>
    <m:texture2d id="20" path="/3D/Textures/palette.png" contenttype="image/png" tilestyleu="clamp" tilestylev="clamp"/>
    <m:texture2dgroup id="21" texid="20">
      <m:tex2coord u="0.0" v="1.0"/>
      <m:tex2coord u="1.0" v="1.0"/>
      <m:tex2coord u="0.0" v="0.0"/>
    </m:texture2dgroup>
    <object id="1" name="TexturedTetra" type="model">
      <mesh>
        <vertices>
          <vertex x="0" y="0" z="0"/>
          <vertex x="12" y="0" z="0"/>
          <vertex x="0" y="12" z="0"/>
          <vertex x="0" y="0" z="12"/>
        </vertices>
        <triangles>
          <triangle v1="0" v2="2" v3="1" pid="10" p1="0"/>
          <triangle v1="0" v2="1" v3="3" pid="10" p1="1"/>
          <triangle v1="0" v2="3" v3="2" pid="21" p1="0" p2="0" p3="0"/>
          <triangle v1="1" v2="2" v3="3" pid="21" p1="2" p2="2" p3="2"/>
        </triangles>
      </mesh>
    </object>
  </resources>
  <build>
    <item objectid="1"/>
  </build>
</model>"##;
        zip.start_file("3D/3dmodel.model", opts).unwrap();
        zip.write_all(model_xml.as_bytes()).unwrap();
        zip.finish().unwrap();

        let mut report = LoadedBatchReport::default();
        load_3mf_archive(&temp_3mf, &mut report);
        let _ = std::fs::remove_file(&temp_3mf);

        assert!(report.errors.is_empty(), "errors: {:?}", report.errors);
        assert_eq!(report.objects.len(), 1);
        let obj = &report.objects[0];
        assert_eq!(obj.triangle_colors_rgba.len(), 4);
        assert_eq!(obj.vertex_colors_rgba.len(), 4);

        // Triangle 0: #FF4020 from colorgroup
        assert_eq!(
            color_optics::unpack_rgba8(obj.triangle_colors_rgba[0]),
            [255, 64, 32, 255]
        );
        // Triangle 1: #20A0FF from colorgroup
        assert_eq!(
            color_optics::unpack_rgba8(obj.triangle_colors_rgba[1]),
            [32, 160, 255, 255]
        );
        // Triangle 2: UV (0, 1) -> top-left of PNG = Cyan (0, 174, 239)
        assert_eq!(
            color_optics::unpack_rgba8(obj.triangle_colors_rgba[2]),
            [0, 174, 239, 255]
        );
        // Triangle 3: UV (0, 0) -> bottom-left of PNG = Yellow (255, 242, 0)
        assert_eq!(
            color_optics::unpack_rgba8(obj.triangle_colors_rgba[3]),
            [255, 242, 0, 255]
        );

        let backend_colors = ffi::get_object_triangle_colors_rgba(obj.object_id);
        assert_eq!(backend_colors, obj.triangle_colors_rgba);

        let _ = ffi::remove_cad_object(obj.object_id);
    }
}
