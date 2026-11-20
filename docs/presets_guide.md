# Manifold Slicer — Factory Presets & Non-Planar Tool Clearance Guide

Manifold Slicer organizes configuration into **four independent preset categories**:
1. **Printer** (`~/.config/manifold2/presets/printers/*.toml`) — Build volume, coordinate origin, multi-axis kinematics (`ThreeAxis`, `FiveAxisTiltingBedAB`, `FiveAxisBC`), firmware G-code dialect (`Klipper`, `Prusa`, `RepRapFirmware`, `BambuLab`), motion limits, start/end G-code macros, and network printing protocol (`Moonraker`, `PrusaLink`, `BambuLan`).
2. **Tool** (`~/.config/manifold2/presets/tools/*.toml`) — Nozzle orifice diameter, rotational capability, maximum volumetric flow capacity ($Q_{\max}$), retraction/wipe/Z-hop, toolchange timing, and **local-frame 3D collision clearance envelope** (`PiecewiseXZ`, `ParametricCone`, or `RectilinearCone`).
3. **Filament** (`~/.config/manifold2/presets/filaments/*.toml`) — Polymer type, density, nozzle & bed temperatures, extrusion multiplier, part/bridge/wave-overhang cooling fan schedules, dual-band non-Newtonian Pressure Advance ($k_{\text{low}}, k_{\text{high}}$), viscoelastic die-swell ratios, and Kubelka–Munk optical Transmission Distance (`TD` in mm).
4. **Process** (`~/.config/manifold2/presets/processes/*.toml`) — Volumetric Eikonal solver parameters, variable layer height `(target_mm, min_mm, max_mm, first_layer_mm, top_layer_mm)`, 3D shell thickness, wall count, brick-layer staggering, infill pattern/density, bridging & wave overhangs, non-planar tree/grid supports, scarf seams, feature speeds, and optical colour mixing.

You can browse, inspect, diff, and apply any preset from the GUI (**Configuration & Presets** tab) or via the CLI:
```bash
manifold-frontend profile list
manifold-frontend profile show "0.4mm Voron A4T (+8.5mm MZE / UHF)" --category tool
manifold-frontend slice --printer "Voron Trident 5-Axis Tilting Bed (300³ A/B)" \
                        --tool "0.4mm Voron A4T (+8.5mm MZE / UHF)" \
                        --filament "Generic ASA" \
                        part.step -o part.gcode
```

---

## 1. Printer Presets

### 1.1 Voron & Standard Klipper Printers (`Klipper` Dialect / `Moonraker` Network Protocol)

| Printer Preset | Build Volume ($X \times Y \times Z$) | Kinematics Mode | G-code Origin | Notes |
| :--- | :--- | :--- | :--- | :--- |
| **`256³ CoreXY (Standard Klipper)`** | $256 \times 256 \times 256\text{ mm}$ | `ThreeAxis` | Front-Left `(0, 0)` | Default baseline CoreXY profile (`SET_PRESSURE_ADVANCE`, `SET_VELOCITY_LIMIT`, `EXCLUDE_OBJECT_*`). |
| **`Voron 0`** | $120 \times 120 \times 120\text{ mm}$ | `ThreeAxis` | Front-Left `(0, 0)` | Compact CoreXY (`120³ mm`). |
| **`Voron Trident 250`** | $250 \times 250 \times 250\text{ mm}$ | `ThreeAxis` | Front-Left `(0, 0)` | 3-leadscrew bed CoreXY (`250³ mm`). |
| **`Voron Trident 300`** | $300 \times 300 \times 250\text{ mm}$ | `ThreeAxis` | Front-Left `(0, 0)` | 3-leadscrew bed CoreXY (`300×300×250 mm`). |
| **`Voron Trident 350`** | $350 \times 350 \times 250\text{ mm}$ | `ThreeAxis` | Front-Left `(0, 0)` | 3-leadscrew bed CoreXY (`350×350×250 mm`). |
| **`Voron 2.4 250`** | $250 \times 250 \times 220\text{ mm}$ | `ThreeAxis` | Front-Left `(0, 0)` | Flying-gantry CoreXY (`250×250×220 mm`). |
| **`Voron 2.4 300`** | $300 \times 300 \times 280\text{ mm}$ | `ThreeAxis` | Front-Left `(0, 0)` | Flying-gantry CoreXY (`300×300×280 mm`). |
| **`Voron 2.4 350`** | $350 \times 350 \times 330\text{ mm}$ | `ThreeAxis` | Front-Left `(0, 0)` | Flying-gantry CoreXY (`350×350×330 mm`). |

### 1.2 Prusa CORE One & Bondtech INDX Printers (`Prusa` Dialect / `PrusaLink` Network Protocol)

All Prusa presets use `GcodeDialect::Prusa` (`M572 S...` pressure advance, `M204 P... T...` acceleration, `M486` object cancellation, `M555` + `G29` print-area mesh leveling) and `PrinterNetworkProtocol::PrusaLink` (HTTP Digest Auth with default username `maker`).

| Printer Preset | Build Volume ($X \times Y \times Z$) | Tools | Notes |
| :--- | :--- | :---: | :--- |
| **`Prusa CORE One`** | $250 \times 220 \times 270\text{ mm}$ | `1` | Single Nextruder enclosed CoreXY (`M862.3 P "COREONE"`). |
| **`Prusa CORE One L`** | $300 \times 300 \times 330\text{ mm}$ | `1` | Large-format enclosed CoreXY (`M862.3 P "COREONEL"`). |
| **`Prusa CORE One INDX (4-Tool)`** | $248 \times 205 \times 270\text{ mm}$ | `4` (`T0..T3`) | Bondtech INDX 4-head wireless induction toolchanger on CORE One. |
| **`Prusa CORE One INDX (8-Tool)`** | $248 \times 205 \times 270\text{ mm}$ | `8` (`T0..T7`) | Bondtech INDX 8-head wireless induction toolchanger on CORE One. |
| **`Prusa CORE One L INDX (4-Tool)`** | $298 \times 275 \times 330\text{ mm}$ | `4` (`T0..T3`) | Bondtech INDX 4-head wireless induction toolchanger on CORE One L. |
| **`Prusa CORE One L INDX (8-Tool)`** | $298 \times 275 \times 330\text{ mm}$ | `8` (`T0..T7`) | Bondtech INDX 8-head wireless induction toolchanger on CORE One L. |

### 1.3 Bambu Lab Printers (`BambuLab` Dialect / `BambuLan` FTPS + MQTT-TLS Protocol)

All Bambu Lab presets use `GcodeDialect::BambuLab` (`M900 K...` pressure advance, `M204 P... T...` acceleration, `M106 P1 S...` part cooling fan, `M620`/`M621` AMS filament swaps, `M624`/`M625` skip-object markers), automatically package exports and uploads into signed-MD5 `.gcode.3mf` archives (`Metadata/plate_1.gcode`, `plate_1.gcode.md5`, `slice_info.config`, `plate_1.png`), and upload directly over **Bambu LAN Mode** (implicit FTPS on port `990` + MQTT-TLS on port `8883` as user `bblp` with the printer's 8-character Access Code and Serial Number).

| Printer Preset | Build Volume ($X \times Y \times Z$) | Tools | Notes |
| :--- | :--- | :---: | :--- |
| **`Bambu Lab X1 Carbon`** | $256 \times 256 \times 256\text{ mm}$ | `1` | Single-spool X1C / X1E CoreXY. |
| **`Bambu Lab X1 Carbon (AMS 4-Slot)`** | $256 \times 256 \times 256\text{ mm}$ | `4` (`T0..T3`) | X1C / X1E with 4-slot AMS (`M620 S{id}A` / `M621 S{id}A`). |
| **`Bambu Lab P1S / P1P`** | $256 \times 256 \times 256\text{ mm}$ | `1` | Single-spool P1S / P1P CoreXY. |
| **`Bambu Lab P1S (AMS 4-Slot)`** | $256 \times 256 \times 256\text{ mm}$ | `4` (`T0..T3`) | P1S / P1P with 4-slot AMS. |
| **`Bambu Lab A1`** | $256 \times 256 \times 256\text{ mm}$ | `1` | Single-spool A1 bed-slinger. |
| **`Bambu Lab A1 (AMS Lite 4-Slot)`** | $256 \times 256 \times 256\text{ mm}$ | `4` (`T0..T3`) | A1 with 4-slot AMS Lite. |
| **`Bambu Lab A1 Mini`** | $180 \times 180 \times 180\text{ mm}$ | `1` | Compact A1 Mini (`180³ mm`). |

### 1.4 Multi-Axis (5-Axis & 6-Axis) Printers

| Printer Preset | Build Volume ($X \times Y \times Z$) | Kinematics Mode | Dialect | Notes |
| :--- | :--- | :--- | :--- | :--- |
| **`5-Axis Tilting Build Plate (250×210)`** | $250 \times 210 \times 220\text{ mm}$ | `FiveAxisTiltingBedAB` (`X Y Z A B`) | `Klipper` | Tilting build plate (`A` roll around $X$, `B` pitch around $Y$, $\pm 45^\circ$). Gravity and fixed-gantry tool axes dynamically track `(A, B)` in bed coordinates. |
| **`Voron Trident 5-Axis Tilting Bed (300³ A/B)`** | $300 \times 300 \times 250\text{ mm}$ | `FiveAxisTiltingBedAB` (`X Y Z A B`) | `Klipper` | Articulated-bed Voron Trident variant (`X Y Z A B`, bed-center origin, $\pm 45^\circ$ `A`/`B` range). Gravity and tool axes in bed coordinates dynamically couple to `(A, B)`. |
| **`5-Axis Circular Bed (RRF B/C)`** | $\varnothing 240 \times 220\text{ mm}$ | `FiveAxisBC` (`X Y Z B C`) | `RepRapFirmware` | Circular rotary-tilt build plate (`B` polar tilt $0^\circ..60^\circ$, continuous $360^\circ$ `C` rotation, RRF `M568`/`M116` tool management). |
| **`6-Axis Robot Arm Cell (350³)`** | $350 \times 350 \times 350\text{ mm}$ | `ThreeAxis` / 6-Axis | `Klipper` | 6-axis robotic cell workspace with bed-center origin and conservative acceleration limits. |

---

## 2. Toolhead Presets & Local-Frame XZ Clearance Envelopes

Every toolhead in Manifold Slicer defines its collision envelope in the **local orthonormal `NozzleFrame`** $(\hat{\mathbf{x}}_{\text{nozzle}}, \hat{\mathbf{y}}_{\text{nozzle}}, \hat{\mathbf{z}}_{\text{nozzle}})$:
- $Z \ge 0$ (`z_height_mm`) is measured **upward along `nozzle_axis` ($\hat{\mathbf{z}}_{\text{nozzle}}$)** from the nozzle tip ($Z = 0$).
- $X \ge 0$ (`x_radius_mm`) is the **radial half-width perpendicular to `nozzle_axis`** at height $Z$.

This profile governs:
1. **Maximum Non-Planar Layer Slope ($\alpha_{\max}$)**: So the brass nozzle cone, silicone sock, part-cooling ducts, and fan shroud never gouge surrounding non-planar layers.
2. **Conformal SDF Surface Skin Collision Standoff**: Stands conformal skin layers off from rising walls (such as Benchy bulwarks or cabin roofs).
3. **5-Axis Bed-Safe Tilt Angle ($\gamma_{\max}(h_{\text{bed}})$)**: On 5-axis tilting-bed printers (`FiveAxisTiltingBedAB` and `FiveAxisBC`), limits bed tilt near $h_{\text{bed}} = 0$ so the wide upper cooling shroud at $(r_m, z_m)$ never contacts the build plate:
   $$h_{\text{bed}} + z_m \cos\gamma - r_m \sin\gamma \ge 0 \implies \gamma \le \arctan\!\left(\frac{z_m}{r_m}\right) + \arcsin\!\left(\frac{h_{\text{bed}}}{\sqrt{r_m^2 + z_m^2}}\right)$$

---

### 2.1 Voron Toolhead Presets (Standard vs. `+8.5mm` Melt Zone Extender / UHF)

All Voron toolhead profiles were measured directly from the official CAD/STL repositories (`VoronDesign/Voron-Stealthburner`, `VoronDesign/voron-downdraft`, `Armchair-Heavy-Industries/Xol-Toolhead`, and `Armchair-Heavy-Industries/A4T`).

> **Important Note on Melt Zone Extenders (MZE)**:
> A standard V6-thread Melt Zone Extender (such as the Trianglelab ZS-MZE or MZE HF Plus) adds **`+8.4 mm to +8.5 mm`** of axial drop between the heater block and the nozzle tip:
> - **Standard Cowling + Free-Hanging MZE (`(+8.5mm MZE / UHF)` presets)**: If you add an MZE to a standard-length cowling/duct set without extending the lower plastic fan ducts downward, the nozzle tip sits **`~10.0 mm`** below the cooling duct tips (`1.5 mm + 8.5 mm`) and **`~17.5 mm`** below the wide lower cowling body. This dramatically increases non-planar slope clearance and allows full $\pm 22^\circ\text{–}30^\circ$ 5-axis bed tilt much closer to the build plate!
> - **UHF Cowling / UHF Ducts (`(Standard)` clearance + higher flow)**: If you print the dedicated UHF lower ducts (e.g. `SLiMXolPAP Ducts - UHF.stl` or `A4T Cowling - Dragon-Rapido UHF.stl`), the plastic ducts drop down by `8.5 mm` to match the extended nozzle tip (`1.4–1.5 mm` tip protrusion), so the external clearance envelope matches the `(Standard)` profile while supporting UHF volumetric flow (`35–38 mm³/s`).

| Tool Preset Name | Orifice | $Q_{\max}$ | Retract | Piecewise XZ Clearance Profile `(radius_x_mm, height_z_mm)` |
| :--- | :---: | :---: | :---: | :--- |
| **`0.4mm Voron Stealthburner (Standard)`** | `0.40 mm` | `24 mm³/s` | `0.75 mm @ 40 mm/s` | `(0.5, 0.0) → (4.0, 1.5) → (10.5, 2.0) → (24.0, 6.0) → (32.6, 12.0) → (35.5, 45.0)`<br>*(1.5 mm official nozzle protrusion; 65.1 mm main cowling width; 71.1 mm rear carriage width)* |
| **`0.4mm Voron Stealthburner (+8.5mm MZE / UHF)`** | `0.40 mm` | `35 mm³/s` | `0.90 mm @ 45 mm/s` | `(0.5, 0.0) → (3.5, 2.0) → (6.0, 8.5) → (10.5, 10.5) → (24.0, 14.5) → (32.6, 20.5) → (35.5, 53.5)` |
| **`0.4mm Voron DownDraft (Standard / Low-Mount)`** | `0.40 mm` | `26 mm³/s` | `0.75 mm @ 45 mm/s` | `(0.5, 0.0) → (4.0, 1.5) → (9.5, 2.2) → (22.0, 6.5) → (30.5, 10.5) → (34.0, 43.0)`<br>*(61.0 mm Dual-4010 shroud width; 68.0 mm outer Dual-5015 width)* |
| **`0.4mm Voron DownDraft (+8.5mm MZE / UHF)`** | `0.40 mm` | `38 mm³/s` | `0.90 mm @ 45 mm/s` | `(0.5, 0.0) → (3.5, 2.0) → (6.0, 8.5) → (9.5, 10.7) → (22.0, 15.0) → (30.5, 19.0) → (34.0, 51.5)` |
| **`0.4mm Voron Xol (Standard)`** | `0.40 mm` | `28 mm³/s` | `0.80 mm @ 45 mm/s` | `(0.5, 0.0) → (4.0, 1.5) → (9.0, 5.5) → (35.8, 9.0) → (39.3, 44.0)`<br>*(71.7 mm `SLiMXolPAP` duct width; 78.6 mm upper 4010 fan mount width)* |
| **`0.4mm Voron Xol (+8.5mm MZE / UHF)`** | `0.40 mm` | `38 mm³/s` | `0.90 mm @ 45 mm/s` | `(0.5, 0.0) → (3.5, 2.0) → (6.0, 8.5) → (9.0, 14.0) → (35.8, 17.5) → (39.3, 52.5)` |
| **`0.4mm Voron A4T (Standard)`** | `0.40 mm` | `28 mm³/s` | `0.80 mm @ 45 mm/s` | `(0.5, 0.0) → (4.0, 1.4) → (9.0, 6.0) → (32.0, 9.0) → (44.3, 40.0)`<br>*(1.4 mm tip clearance; 64.0 mm lower duct width at $z=9\text{ mm}$; 88.6 mm outer cowling width)* |
| **`0.4mm Voron A4T (+8.5mm MZE / UHF)`** | `0.40 mm` | `38 mm³/s` | `0.90 mm @ 45 mm/s` | `(0.5, 0.0) → (3.5, 2.0) → (6.0, 8.5) → (9.0, 14.5) → (32.0, 17.5) → (44.3, 48.5)` |

---

### 2.2 Prusa Nextruder & Bondtech INDX Toolhead Presets

| Tool Preset Name | Orifice | $Q_{\max}$ | Retract | Toolchange | Piecewise XZ Clearance Profile `(radius_x_mm, height_z_mm)` |
| :--- | :---: | :---: | :---: | :---: | :--- |
| **`0.4mm Prusa Nextruder Standard`** | `0.40 mm` | `16 mm³/s` | `0.70 mm @ 35 mm/s` | — | `(0.5, 0.0) → (3.5, 1.5) → (11.0, 3.0) → (24.0, 9.0) → (34.0, 18.0) → (38.0, 45.0)` |
| **`0.4mm Prusa Nextruder High-Flow`** | `0.40 mm` | `26 mm³/s` | `0.70 mm @ 35 mm/s` | — | `(0.5, 0.0) → (3.5, 1.5) → (11.0, 3.0) → (24.0, 9.0) → (34.0, 18.0) → (38.0, 45.0)`<br>*(Prusa CORE One / MK4S CHT High-Flow Nextruder with 360° turbine cooling shroud)* |
| **`0.6mm Prusa Nextruder High-Flow`** | `0.60 mm` | `32 mm³/s` | `0.80 mm @ 35 mm/s` | — | `(0.75, 0.0) → (3.5, 1.5) → (11.0, 3.0) → (24.0, 9.0) → (34.0, 18.0) → (38.0, 45.0)` |
| **`0.4mm Bondtech INDX Toolchanger`** | `0.40 mm` | `24 mm³/s` | `0.60 mm @ 40 mm/s` | `12.0 s` | `(0.5, 0.0) → (3.0, 2.0) → (8.5, 5.0) → (17.5, 11.0) → (30.0, 22.0) → (35.0, 45.0)`<br>*(Slim 35 mm pitch passive induction toolhead + Smart Toolhead carrier)* |
| **`0.6mm Bondtech INDX Toolchanger`** | `0.60 mm` | `30 mm³/s` | `0.70 mm @ 40 mm/s` | `12.0 s` | `(0.75, 0.0) → (3.0, 2.0) → (8.5, 5.0) → (17.5, 11.0) → (30.0, 22.0) → (35.0, 45.0)` |

---

### 2.3 Bambu Lab & Generic 5/6-Axis Conical Toolhead Presets

| Tool Preset Name | Orifice | $Q_{\max}$ | Rotational | Piecewise XZ Clearance Profile `(radius_x_mm, height_z_mm)` |
| :--- | :---: | :---: | :---: | :--- |
| **`0.4mm Bambu Lab X1/P1 Hardened Steel`** | `0.40 mm` | `22 mm³/s` | `false` (`85s` AMS swap) | `(0.55, 0.0) → (3.2, 2.2) → (14.5, 4.5) → (28.0, 14.0) → (42.0, 45.0)` |
| **`0.4mm Bambu Lab A1 Quick-Swap`** | `0.40 mm` | `22 mm³/s` | `false` (`45s` AMS Lite) | `(0.55, 0.0) → (3.0, 2.5) → (13.0, 5.0) → (26.0, 15.0) → (38.0, 45.0)` |
| **`0.4mm Standard 5/6-Axis Conical`** | `0.40 mm` | `22 mm³/s` | `true` ($\le 45^\circ$) | `(0.50, 0.0) → (2.5, 2.0) → (9.0, 6.0) → (22.0, 18.0) → (28.0, 45.0)` |
| **`0.6mm High-Flow`** | `0.60 mm` | `28 mm³/s` | `true` ($\le 45^\circ$) | `(0.75, 0.0) → (2.8, 2.0) → (9.5, 6.0) → (22.0, 18.0) → (28.0, 45.0)` |
| **`0.25mm Precision Extended-Tip`** | `0.25 mm` | `8 mm³/s` | `true` ($\le 60^\circ$) | `(0.35, 0.0) → (1.8, 3.0) → (6.5, 9.0) → (18.0, 22.0) → (26.0, 48.0)` |
| **`0.4mm Rectilinear Shroud Cone (XY)`** | `0.40 mm` | `22 mm³/s` | `true` ($\le 45^\circ$) | `RectilinearCone`: tip $0.60 \times 0.60\text{ mm}$, half-angles $\theta_X = 38^\circ, \theta_Y = 24^\circ, H = 45\text{ mm}$ |

---

## 3. Filament Presets

### 3.1 Standard Engineering & Functional Filaments

| Filament Preset | Type | Nozzle Temp | Bed Temp | Density | Fan (Min / Nom / Max / Bridge / Wave) | Dual-Band PA ($k_{\text{low}} / k_{\text{high}}$) |
| :--- | :---: | :---: | :---: | :---: | :---: | :---: |
| **`Generic PLA`** | `PLA` | $215^\circ\text{C}$ | $60^\circ\text{C}$ | $1.24\text{ g/cm}^3$ | `20% / 100% / 100% / 100% / 100%` | `0.042 s / 0.024 s` |
| **`High-Flow PLA+`** | `PLA` | $225^\circ\text{C}$ | $60^\circ\text{C}$ | $1.24\text{ g/cm}^3$ | `20% / 100% / 100% / 100% / 100%` | `0.036 s / 0.020 s` |
| **`Generic PETG`** | `PETG` | $240^\circ\text{C}$ | $75^\circ\text{C}$ | $1.27\text{ g/cm}^3$ | `20% / 40% / 80% / 80% / 70%` | `0.055 s / 0.032 s` |
| **`Generic ABS`** | `ABS` | $255^\circ\text{C}$ | $100^\circ\text{C}$ | $1.04\text{ g/cm}^3$ | `10% / 20% / 50% / 50% / 45%` | `0.038 s / 0.022 s` |
| **`Generic ASA`** | `ASA` | $260^\circ\text{C}$ | $105^\circ\text{C}$ | $1.07\text{ g/cm}^3$ | `10% / 20% / 50% / 50% / 45%` | `0.042 s / 0.024 s` |
| **`TPU 95A`** | `TPU` | $225^\circ\text{C}$ | $50^\circ\text{C}$ | $1.21\text{ g/cm}^3$ | `25% / 50% / 70% / 100% / 100%` | `0.120 s / 0.085 s` |

### 3.2 Calibrated Kubelka–Munk Optical Mixing Palette (Bambu PLA CMYKW)

| Filament Preset | Hex Color | Transmission Distance (`TD`) | Purpose |
| :--- | :---: | :---: | :--- |
| **`Bambu PLA Basic Cyan`** | `#0086D6` | `3.5 mm` | Subtractive primary Cyan for front-lit & lithophane CMYKW mixing. |
| **`Bambu PLA Basic Magenta`** | `#EC008C` | `3.2 mm` | Subtractive primary Magenta. |
| **`Bambu PLA Basic Yellow`** | `#FCE300` | `6.0 mm` | High-translucency primary Yellow. |
| **`Bambu PLA Basic Jade White`** | `#FFFFFF` | `5.0 mm` | Scattering diffuser & high-albedo core backing slab. |
| **`Bambu PLA Basic Black`** | `#161616` | `0.6 mm` | High-extinction key/shading channel. |

---

## 4. Process Presets

| Process Preset | Order Field Mode | Layer Height `(Target / Min..Max)` | Shell & Infill | Key Characteristics |
| :--- | :--- | :---: | :--- | :--- |
| **`0.20mm Standard Non-Planar`** | `ConstrainedNonPlanar` | `0.20 mm` (`0.08..0.32 mm`) | `1.20 mm` shell, 2 inner walls, `20%` Gyroid | Balanced non-planar Eikonal slicing with overhang surface orientation bias and conformal skins. |
| **`0.12mm Fine Conformal`** | `ConstrainedNonPlanar` | `0.12 mm` (`0.05..0.20 mm`) | `1.20 mm` shell, 2 inner walls, `20%` Gyroid | High-detail surface finish (`0.05 mm` min apex thickness, slower `50 mm/s` outer walls). |
| **`0.28mm Structural Draft`** | `ConstrainedNonPlanar` | `0.28 mm` (`0.12..0.40 mm`) | `2.00 mm` shell, 3 inner walls, `30%` Cubic | Fast, high-strength structural profile with thick 3D shells and space-diagonal Cubic truss infill. |
| **`0.20mm Standard Planar (Height-Only)`** | `HeightOnlyPlanar` | `0.20 mm` (`0.08..0.32 mm`) | `1.20 mm` shell, 2 inner walls, `20%` Gyroid | Linear height-only order field along `nozzle_axis` for traditional planar slicing with full Manifold rheology & G-code planning. |
| **`0.20mm Non-Planar Tree Support (Bed Only)`** | `ConstrainedNonPlanar` | `0.20 mm` (`0.08..0.32 mm`) | `1.20 mm` shell + `NonPlanarTree` supports | Organic 3D tree supports grown strictly from the build plate around part obstacles. |
| **`0.16mm CMYKW Full-Colour Optical Mixing`** | `ConstrainedNonPlanar` | `0.16 mm` (`0.08..0.24 mm`) | `1.60 mm` shell, 3 optical shell slabs | Enables 3-slab Kubelka–Munk Redheffer optical colour mixing with Oklab spatial dithering. |
