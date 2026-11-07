//! Builds the virtual DataModel (port of `runtime/engine/project.luau`): Studio snapshots and
//! models (.rbxm / .rbxmx through rbx-dom) and filesystem script trees mounted over them (Rojo
//! layout `<root>/<Service>/...`: init.luau, *.server.luau, *.client.luau, init.meta.json,
//! JSON modules, *.model.json models), offline stand-ins for Studio-only packages, and Studio-only
//! placeholders from the place's hierarchy snapshot, created when a script first looks up or lists
//! children.

use std::{
    path::{Path, PathBuf},
    rc::Rc,
};

use rbx_dom_weak::WeakDom;
use rustc_hash::{FxHashMap, FxHashSet};

use super::{
    ClassId, Engine, InstanceId, K, NameId, Reflection, State,
    dom::{StudioNode, flags},
    instance as core, services,
    value::{Str, Value},
};

pub mod json_model;
#[cfg(feature = "bundles")]
pub mod model_assets;
pub mod plan;

pub use plan::{LazyImport, ModelPlan, PlanProps, PlanPropsIter, Unmodeled, before_method, convert_instances, find_unmade, materialize_lazy, materialize_lazy_subtree, materialize_region};

/// A run's own choice of asset manifest bundles to mount, over rur.json `modelAssets` (`rur-native
/// record --assets-at <Service.Path>` / `--assets <prefix>,...`): where they mount (a playing client
/// finds its delivered bundles elsewhere than an edit-mode story) and which (`*`: every bundle).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ModelAssetsOverride {
    pub mount: Option<String>,
    pub prefixes: Option<Vec<String>>,
    /// What the run is (`ree record` / `ree run`: its harness, place and context): the bundles decoded
    /// ahead are the ones the last run of the same scene imported, not the last run of the mount's
    pub scene: Option<String>,
}

/// Studio hierarchy data and diagnostics of the mounted place.
#[derive(Default)]
pub struct ProjectState {
    #[cfg(feature = "bundles")]
    pub model_assets: model_assets::ModelAssets,
    /// The running client's PlayerGui created in edit mode (init.luau MissingChild)
    pub client_gui: Option<InstanceId>,
    /// Engine.ClientGuiMount (main.luau, rur.json clientGui): mounts what a play-testing client
    /// builds in its PlayerGui (story.rs sets it; lua/globals.rs missing_child runs it once)
    pub client_gui_mount: Option<mlua::Function>,
    /// The PlayerGui was created while a client gui mount was set, and it has not run yet
    pub client_mount_pending: bool,
    studio: Option<Studio>,
    stand_in_sources: FxHashMap<String, Rc<[u8]>>,
    used_placeholders: FxHashSet<InstanceId>,
    pub studio_only_count: u32,
    pub studio_only_used: Vec<String>,
    pub studio_only_gui: Vec<String>,
    pub hierarchy_path: Option<String>,
    /// Mounted files' stamps from the project's listing ([`ScanNode::stamps`])
    pub stamps: FxHashMap<String, crate::files::Stamp>,
    /// Plans imported lazily ([`ModelPlan::instantiate_lazy`])
    pub imports: Vec<LazyImport>,
    /// The lazily imported instances whose plan children are not made yet: (import, node)
    pub lazy: FxHashMap<InstanceId, (u32, u32)>,
    /// Instances made of lazy imports so far
    pub lazy_made: u32,
    /// The directory listings the mount made, kept when a prepared run will check them again
    /// (record/prepare.rs: Some before the mount)
    pub listings: Option<Vec<Listing>>,
}

/// A directory tree as a mount listed it: what a later [`Listing::unchanged`] compares the directory
/// with now.
pub struct Listing(ScanNode);

impl Listing {
    /// Whether the directory listed now is as it was: the same entries, of the same kinds, in the same
    /// order, and the same stamps for every file but the Luau sources not in `read` (a source the run
    /// did not read may change: the run reads it later, as it is then).
    pub fn unchanged(&self, read: &FxHashSet<String>) -> bool {
        same_listing(&self.0, &scan(self.0.path.clone()), read)
    }

    pub fn path(&self) -> &str {
        &self.0.path
    }
}

fn same_listing(old: &ScanNode, new: &ScanNode, read: &FxHashSet<String>) -> bool {
    if old.entries != new.entries || old.stamps.len() != new.stamps.len() || old.dirs.len() != new.dirs.len() {
        return false;
    }
    for (index, entry) in old.entries.iter().enumerate() {
        let path = format!("{}/{entry}", old.path);
        match (old.dirs.get(entry), new.dirs.get(entry)) {
            (Some(old_dir), Some(new_dir)) => {
                if !same_listing(old_dir, new_dir, read) {
                    return false;
                }
            }
            (None, None) => {
                let (before, now) = (old.stamps[index], new.stamps[index]);
                let lower = entry.to_ascii_lowercase();
                let source = lower.ends_with(".luau") || lower.ends_with(".lua");
                if (!source || read.contains(&path)) && (before.is_none() || before != now) {
                    return false;
                }
            }
            _ => return false,
        }
    }
    true
}

/// Keeps `node` when the mount keeps its listings ([`ProjectState::listings`]).
fn keep_listing(engine: &Engine, node: ScanNode) {
    if let Some(listings) = engine.state.borrow_mut().project.listings.as_mut() {
        listings.push(Listing(node));
    }
}

struct Studio {
    game: InstanceId,
    index: std::sync::Arc<StudioIndex>,
}

/// A place's Studio hierarchy as the mount reads it, made once per snapshot per process and shared by
/// its engines (a recording built it anew: 68,477 nodes of a game's place).
pub struct StudioIndex {
    /// The nodes' names, one after another (`name_ends[i]` ends node i + 1's)
    names: Box<[u8]>,
    name_ends: Box<[u32]>,
    classes: Box<[String]>,
    /// Children (1-based node indices) of each 1-based node
    children: FxHashMap<u32, Vec<u32>>,
}

impl StudioIndex {
    fn new(snapshot: &HierarchySnapshot) -> StudioIndex {
        let nodes = &snapshot.nodes;
        let mut names = Vec::new();
        let mut name_ends = Vec::with_capacity(nodes.len());
        let mut classes = Vec::with_capacity(nodes.len());
        let mut children: FxHashMap<u32, Vec<u32>> = FxHashMap::default();
        for (index, node) in nodes.iter().enumerate() {
            names.extend_from_slice(node.name.as_bytes());
            name_ends.push(names.len() as u32);
            classes.push(node.class_name.clone());
            if let Some(parent) = node.parent.filter(|p| *p >= 0) {
                children.entry(parent as u32 + 1).or_default().push(index as u32 + 1);
            }
        }
        StudioIndex { names: names.into(), name_ends: name_ends.into(), classes: classes.into(), children }
    }

    /// Node `node`'s name (1-based)
    fn name(&self, node: u32) -> &[u8] {
        let end = self.name_ends[node as usize - 1] as usize;
        let start = if node > 1 { self.name_ends[node as usize - 2] as usize } else { 0 };
        &self.names[start..end]
    }
}

const SCRIPT_SUFFIXES: &[(&str, &str)] = &[
    (".server.luau", "Script"),
    (".server.lua", "Script"),
    (".client.luau", "LocalScript"),
    (".client.lua", "LocalScript"),
    (".luau", "ModuleScript"),
    (".lua", "ModuleScript"),
];

/// Offline stand-ins (lua/packages/*, compiled in) for packages a place keeps only in Studio.
const PACKAGE_STAND_INS: &[(&str, &str)] = &[("ui-labs", "ui-labs.luau"), ("UILabs", "ui-labs.luau")];

fn stand_in_file(name: &str) -> Option<&'static str> {
    PACKAGE_STAND_INS.iter().find(|(n, _)| *n == name).map(|(_, f)| *f)
}

#[derive(Default, Debug, Clone, Copy)]
pub struct MountCounts {
    pub created: u32,
    pub scripts: u32,
    pub conflicts: u32,
    pub skipped: u32,
}

fn is_a(reflection: &Reflection, class: ClassId, ancestor: &str) -> bool {
    reflection.is_a(class, ancestor)
}

fn find_child(state: &State, parent: InstanceId, name: &str) -> Option<InstanceId> {
    state.dom.find_child(parent, name.as_bytes())
}

// Studio-only instances

fn studio_node_of(state: &mut State, id: InstanceId) -> Option<u32> {
    match state.dom.at(id).studio_node {
        StudioNode::Node(node) => return Some(node),
        StudioNode::None => return None,
        StudioNode::Unknown => {}
    }
    let studio_game = state.project.studio.as_ref()?.game;
    let result = if id == studio_game {
        Some(1)
    } else if !state.dom.at(id).has(flags::SCRIPT_CREATED) {
        match state.dom.at(id).parent {
            Some(parent) => {
                let parent_node = studio_node_of(state, parent);
                let name = state.dom.at(id).name.clone();
                let studio = &state.project.studio.as_ref()?.index;
                parent_node.and_then(|node| studio.children.get(&node)?.iter().copied().find(|&child| studio.name(child) == name.as_bytes()))
            }
            None => None,
        }
    } else {
        None
    };
    state.dom.at_mut(id).studio_node = match result {
        Some(node) => StudioNode::Node(node),
        None => StudioNode::None,
    };
    result
}

/// Hooks.Materialize: adds the Studio-only children of `id` once (and makes a lazily imported
/// instance's children).
pub fn materialize(state: &mut State, reflection: &Reflection, id: InstanceId) {
    if state.dom.at(id).has(flags::LAZY_PENDING) {
        materialize_lazy(state, reflection, id);
    }
    if state.dom.at(id).has(flags::MATERIALIZED) {
        return;
    }
    state.dom.at_mut(id).set_flag(flags::MATERIALIZED, true);
    let Some(game) = state.project.studio.as_ref().map(|s| s.game) else { return };
    if id == game {
        return;
    }
    let Some(node) = studio_node_of(state, id) else { return };
    let Some(children) = state.project.studio.as_ref().and_then(|s| s.index.children.get(&node).cloned()) else {
        return;
    };
    // The k-th Studio child named N corresponds to the k-th existing child named N
    let mut existing: FxHashMap<Str, u32> = FxHashMap::default();
    for &child in &state.dom.at(id).children {
        *existing.entry(state.dom.at(child).name.clone()).or_default() += 1;
    }
    let mut seen: FxHashMap<Str, u32> = FxHashMap::default();
    for child_node in children {
        let (name, class_name) = {
            let studio = &state.project.studio.as_ref().unwrap().index;
            (Str::new(studio.name(child_node)), studio.classes[child_node as usize - 1].clone())
        };
        let count = seen.entry(name.clone()).or_default();
        *count += 1;
        if *count <= existing.get(&name).copied().unwrap_or(0) {
            continue;
        }
        // Scripts that only exist in Studio stay absent, except packages with stand-ins
        let class = reflection.class_id(&class_name);
        let is_script = class.is_some_and(|c| is_a(reflection, c, "LuaSourceContainer"));
        let stand_in = stand_in_file(&name.to_str_lossy()).filter(|_| class_name == "ModuleScript");
        if is_script && stand_in.is_none() {
            continue;
        }
        let create_class = match class {
            Some(c) if !reflection.class(c).service => c,
            _ => reflection.class_id("Folder").expect("Folder"),
        };
        let placeholder = core::create(state, reflection, create_class);
        core::raw_set_silent(state, reflection, placeholder, K::Name as NameId, Value::String(name.clone()));
        let instance = state.dom.at_mut(placeholder);
        instance.studio_node = StudioNode::Node(child_node);
        instance.set_flag(flags::STUDIO_ONLY, true);
        if let Some(file) = stand_in {
            if let Some(source) = state.project.stand_in_sources.get(file).cloned() {
                let instance = state.dom.at_mut(placeholder);
                instance.set_source(Some(source));
                instance.set_stand_in(Some(Rc::from(file)));
            }
        }
        state.dom.attach(placeholder, id);
        state.project.studio_only_count += 1;
    }
}

/// MaterializeDeep: materializes a whole subtree (Clone, GetDescendants).
pub fn materialize_deep(engine: &Engine, id: InstanceId) {
    let mut stack = vec![id];
    while let Some(node) = stack.pop() {
        crate::lua::instance::materialize(engine, node);
        stack.extend_from_slice(&engine.state.borrow().dom.at(node).children);
    }
}

/// Hooks.PlaceholderUsed: a script reached a Studio-only placeholder by name (diagnostics).
pub fn placeholder_used(state: &mut State, reflection: &Reflection, id: InstanceId) {
    if !state.project.used_placeholders.insert(id) {
        return;
    }
    let class = state.dom.at(id).class;
    let full_name = state.dom.full_name(reflection, id);
    let class_name = reflection.class(class).name.to_string();
    if is_a(reflection, class, "GuiBase2d") && state.project.studio_only_gui.len() < 40 {
        state.project.studio_only_gui.push(format!("{full_name} ({class_name})"));
    }
    if let Some(parent) = state.dom.at(id).parent {
        if state.project.used_placeholders.contains(&parent) {
            return;
        }
    }
    if state.project.studio_only_used.len() < 25 {
        state.project.studio_only_used.push(format!("{full_name} ({class_name})"));
    }
}

// Paths

fn normalize(path: &str) -> String {
    path.replace('\\', "/")
}

fn parent_path(path: &str) -> String {
    match path.rfind('/') {
        Some(index) => path[..index].to_string(),
        None => String::new(),
    }
}

fn read_json(path: &str) -> Option<serde_json::Value> {
    crate::files::note(Path::new(path));
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

/// A project's places, at its folder: `{"environments": [{"name": "<Place>", "projectFile":
/// "<root>/default.project.json", "studioPlaceId": <id>}]}`. A root that is the folder of a place's
/// project file belongs to that place ([`find_hierarchy_snapshot`], [`declared_place_id`]).
pub const PLACES_FILE: &str = "studio-places.json";
/// Where a project keeps each place's Studio hierarchy snapshot, `<dir>/<Place>.json` (`{"nodes":
/// [{"name", "className", "parent"}]}`, a node's parent an index into the list, -1 for the DataModel)
pub const SNAPSHOTS_DIR: &str = "studio-snapshots";
/// A project's data roots: `{"<key>": ["<path>", ...]}` at its folder lists the folders that each
/// `<project>/external/<Name>/src` checkout holds for the game ([`data_roots`]); a path may start with
/// the prefix, which is dropped.
pub const DATA_ROOTS_FILE: &str = "data-roots.json";
pub const DATA_ROOTS_KEY: &str = "paths";
const DATA_ROOTS_PREFIX: &str = "";
/// The Studio names of mounted files whose names a file system cannot hold, at a project's folder
/// ([`apply_studio_names`])
pub const STUDIO_NAME_MAP: &str = "studio-name-map.json";

/// A place's Studio hierarchy snapshot (`<project>/<SNAPSHOTS_DIR>/<Place>.json`), parsed once
/// per process and file modification time.
#[derive(serde::Deserialize)]
pub struct HierarchySnapshot {
    pub nodes: Vec<HierarchyNode>,
    /// The mount's index of it, made on first use ([`StudioIndex`])
    #[serde(skip)]
    index: std::sync::OnceLock<std::sync::Arc<StudioIndex>>,
}

#[derive(serde::Deserialize)]
pub struct HierarchyNode {
    #[serde(default)]
    pub name: String,
    #[serde(default, rename = "className")]
    pub class_name: String,
    #[serde(default)]
    pub parent: Option<i64>,
}

pub fn load_hierarchy(path: &str) -> Option<std::sync::Arc<HierarchySnapshot>> {
    use std::sync::{Arc, Mutex, OnceLock};
    type Entry = (String, Option<std::time::SystemTime>, Arc<HierarchySnapshot>);
    static CACHE: OnceLock<Mutex<Vec<Entry>>> = OnceLock::new();
    crate::files::note(Path::new(path));
    let modified = std::fs::metadata(path).and_then(|m| m.modified()).ok();
    let cache = CACHE.get_or_init(Default::default);
    if let Ok(entries) = cache.lock() {
        if let Some((_, _, found)) = entries.iter().find(|(p, m, _)| p == path && *m == modified) {
            return Some(found.clone());
        }
    }
    let bytes = std::fs::read(path).ok()?;
    // (kept for the process: parsed into the long-lived heap)
    let snapshot: HierarchySnapshot = crate::alloc::long_lived(|| serde_json::from_slice(&bytes)).ok()?;
    let snapshot = crate::alloc::long_lived(|| Arc::new(snapshot));
    if let Ok(mut entries) = cache.lock() {
        entries.retain(|(p, _, _)| p != path);
        crate::alloc::long_lived(|| entries.push((path.to_string(), modified, snapshot.clone())));
    }
    Some(snapshot)
}

/// The Studio hierarchy snapshot of the place the roots belong to (`<project>/<PLACES_FILE>`).
pub fn find_hierarchy_snapshot(roots: &[String]) -> Option<String> {
    for root in roots {
        let normalized = normalize(root);
        let project_root = parent_path(&normalized);
        if project_root.is_empty() {
            continue;
        }
        let Some(config) = read_json(&format!("{project_root}/{PLACES_FILE}")) else { continue };
        let Some(environments) = config.get("environments").and_then(|e| e.as_array()) else { continue };
        for environment in environments {
            let project_file = environment.get("projectFile").and_then(|p| p.as_str()).map(normalize).unwrap_or_default();
            let Some(name) = environment.get("name").and_then(|n| n.as_str()) else { continue };
            if format!("{project_root}/{}", parent_path(&project_file)) != normalized {
                continue;
            }
            let committed = format!("{project_root}/{SNAPSHOTS_DIR}/{name}.json");
            crate::files::note(Path::new(&committed));
            if Path::new(&committed).is_file() {
                return Some(committed);
            }
        }
    }
    None
}

/// The place id a project declares for these roots ([`PLACES_FILE`] `studioPlaceId`).
pub fn declared_place_id(roots: &[String]) -> Option<f64> {
    for root in roots {
        let normalized = normalize(root);
        let project_root = parent_path(&normalized);
        if project_root.is_empty() {
            continue;
        }
        let Some(config) = read_json(&format!("{project_root}/{PLACES_FILE}")) else { continue };
        let Some(environments) = config.get("environments").and_then(|e| e.as_array()) else { continue };
        for environment in environments {
            let project_file = environment.get("projectFile").and_then(|p| p.as_str()).map(normalize).unwrap_or_default();
            if format!("{project_root}/{}", parent_path(&project_file)) == normalized {
                if let Some(id) = environment.get("studioPlaceId").and_then(|v| v.as_f64()) {
                    return Some(id);
                }
            }
        }
    }
    None
}

/// rur.json at the project root: the first root folder with a value for `key` wins.
pub fn project_setting(roots: &[String], key: &str) -> Option<serde_json::Value> {
    for root in roots {
        let normalized = normalize(root);
        let trimmed = normalized.trim_end_matches('/');
        let (project_root, folder) = trimmed.rsplit_once('/')?;
        let Some(config) = read_json(&format!("{project_root}/rur.json")) else { continue };
        if let Some(value) = config.get(key).and_then(|values| values.get(folder)) {
            if !value.is_null() {
                return Some(value.clone());
            }
        }
    }
    None
}

// Filesystem scan (same listing order and directory checks as the Luau scan)

#[derive(Clone)]
struct ScanNode {
    path: String,
    entries: Vec<String>,
    /// The entries' stamps from the listing, when a process keeps compiled modules (they check
    /// a module file is unchanged without a query per file)
    stamps: Vec<Option<crate::files::Stamp>>,
    dirs: FxHashMap<String, ScanNode>,
    /// A directory below was listed through a link, whose target a watch on this tree does not see
    linked: bool,
}

fn may_be_directory(entry: &str) -> bool {
    if entry.starts_with('.') {
        return false;
    }
    let lower = entry.to_ascii_lowercase();
    !lower.ends_with(".luau") && !lower.ends_with(".lua")
}

/// Lists a directory tree (directories checked and scanned in parallel; listing order kept).
fn scan(path: String) -> ScanNode {
    use rayon::prelude::*;
    let mut entries = Vec::new();
    let mut kinds = Vec::new();
    let mut stamps = Vec::new();
    let stamped = crate::lua::modules::reusing_compiled_modules();
    if let Ok(read) = std::fs::read_dir(&path) {
        let mut valid = true;
        for entry in read {
            let Ok(entry) = entry else {
                valid = false;
                break;
            };
            match entry.file_name().into_string() {
                Ok(name) => {
                    kinds.push(entry.file_type().ok());
                    stamps.push(if stamped { crate::files::entry_stamp(&entry) } else { None });
                    entries.push(name);
                }
                Err(_) => {
                    valid = false;
                    break;
                }
            }
        }
        if !valid {
            entries.clear();
            kinds.clear();
        }
    }
    let dirs: FxHashMap<String, ScanNode> = entries
        .par_iter()
        .zip(kinds.par_iter())
        .filter(|(entry, _)| may_be_directory(entry))
        .filter_map(|(entry, kind)| {
            let child = format!("{path}/{entry}");
            // Lune's isDir on the joined path (follows links; Windows drops a trailing dot, so a
            // directory named "X." is not a directory to the Luau scan either). The listing's own
            // type says the same for any other entry, without a query per entry.
            let plain = kind.is_some_and(|kind| !kind.is_symlink()) && !entry.ends_with(['.', ' ']);
            let is_dir = match kind {
                Some(kind) if plain => kind.is_dir(),
                _ => std::fs::metadata(&child).is_ok_and(|m| m.is_dir()),
            };
            let via_link = !kind.is_some_and(|kind| !kind.is_symlink());
            is_dir.then(|| {
                let mut node = scan(child);
                node.linked |= via_link;
                (entry.clone(), node)
            })
        })
        .collect();
    let linked = dirs.values().any(|dir| dir.linked);
    ScanNode { path, entries, stamps, dirs, linked }
}

/// [`scan`] of a mounted tree, kept between VMs in a process that keeps compiled modules ([`kept_listing`]).
fn listed(path: String) -> ScanNode {
    #[cfg(windows)]
    if crate::lua::modules::reusing_compiled_modules() {
        return kept_listing::listed(path);
    }
    scan(path)
}

/// Listings kept while Windows reports no change below them and their path names the same directory.
#[cfg(windows)]
mod kept_listing {
    use super::{FxHashMap, ScanNode, scan};
    use std::ffi::c_void;
    use std::os::windows::{ffi::OsStrExt, fs::OpenOptionsExt, io::AsRawHandle};
    use std::sync::{Mutex, OnceLock};

    #[repr(C)]
    #[derive(Default)]
    struct FileInformation {
        attributes: u32,
        created: [u32; 2],
        accessed: [u32; 2],
        written: [u32; 2],
        volume: u32,
        size: [u32; 2],
        links: u32,
        index_high: u32,
        index_low: u32,
    }

    unsafe extern "system" {
        fn FindFirstChangeNotificationW(path: *const u16, subtree: i32, filter: u32) -> *mut c_void;
        fn FindCloseChangeNotification(handle: *mut c_void) -> i32;
        fn WaitForSingleObject(handle: *mut c_void, milliseconds: u32) -> u32;
        fn GetDriveTypeW(root: *const u16) -> u32;
        fn GetFileInformationByHandle(file: *mut c_void, information: *mut FileInformation) -> i32;
    }

    const WAIT_TIMEOUT: u32 = 0x102;
    const DRIVE_FIXED: u32 = 3;
    // File and directory names, attributes, sizes and last-write times anywhere below
    const CHANGES: u32 = 0x1 | 0x2 | 0x4 | 0x8 | 0x10;

    struct Kept {
        watch: *mut c_void,
        id: (u32, u64),
        node: ScanNode,
    }

    // SAFETY: a watch handle is waited on and closed only under the map's lock
    unsafe impl Send for Kept {}

    static KEPT: OnceLock<Mutex<FxHashMap<String, Kept>>> = OnceLock::new();

    fn wide(text: &str) -> Vec<u16> {
        std::ffi::OsStr::new(text).encode_wide().chain([0]).collect()
    }

    /// The volume and file index of the directory `path` names now.
    fn directory_id(path: &str) -> Option<(u32, u64)> {
        // FILE_FLAG_BACKUP_SEMANTICS opens a directory; no access asked, every share allowed
        let file = std::fs::OpenOptions::new().access_mode(0).share_mode(7).custom_flags(0x0200_0000).open(path).ok()?;
        let mut information = FileInformation::default();
        if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut information) } == 0 || information.attributes & 0x10 == 0 {
            return None;
        }
        Some((information.volume, (u64::from(information.index_high) << 32) | u64::from(information.index_low)))
    }

    /// A change notification on `path`'s tree, on a local fixed drive only (a share may not report changes).
    fn watch(path: &str) -> Option<*mut c_void> {
        let absolute = std::path::absolute(path).ok()?;
        let absolute = absolute.to_str()?;
        let drive = absolute.as_bytes();
        if absolute.len() > 240 || drive.len() < 3 || drive[1] != b':' || !matches!(drive[2], b'\\' | b'/') {
            return None;
        }
        if unsafe { GetDriveTypeW(wide(&format!("{}:\\", drive[0] as char)).as_ptr()) } != DRIVE_FIXED {
            return None;
        }
        let handle = unsafe { FindFirstChangeNotificationW(wide(absolute).as_ptr(), 1, CHANGES) };
        (!handle.is_null() && handle as isize != -1).then_some(handle)
    }

    pub(super) fn listed(path: String) -> ScanNode {
        let Some(id) = directory_id(&path) else { return scan(path) };
        let kept = KEPT.get_or_init(Default::default);
        if let Ok(map) = kept.lock() {
            if let Some(entry) = map.get(&path).filter(|entry| entry.id == id && unsafe { WaitForSingleObject(entry.watch, 0) } == WAIT_TIMEOUT) {
                return entry.node.clone();
            }
        }
        // The watch starts before the listing, so a change made while it lists shows next time
        let Some(watch) = watch(&path) else { return scan(path) };
        let node = scan(path.clone());
        if node.linked {
            unsafe { FindCloseChangeNotification(watch) };
            return node;
        }
        let copy = crate::alloc::long_lived(|| node.clone());
        if let Ok(mut map) = kept.lock() {
            if let Some(old) = crate::alloc::long_lived(|| map.insert(path, Kept { watch, id, node: copy })) {
                unsafe { FindCloseChangeNotification(old.watch) };
            }
        } else {
            unsafe { FindCloseChangeNotification(watch) };
        }
        node
    }

    #[test]
    fn a_kept_listing_follows_every_change_below_it() {
        let root = std::env::temp_dir().join(format!("ree-kept-listing-{}", std::process::id()));
        let path = root.to_string_lossy().replace('\\', "/");
        std::fs::create_dir_all(root.join("Folder")).unwrap();
        std::fs::write(root.join("Folder/a.luau"), "return 1").unwrap();
        let names = |node: &ScanNode| node.dirs["Folder"].entries.clone();
        assert_eq!(names(&listed(path.clone())), ["a.luau"]);
        assert_eq!(names(&listed(path.clone())), ["a.luau"]);
        std::fs::write(root.join("Folder/b.luau"), "return 2").unwrap();
        assert_eq!(names(&listed(path.clone())), ["a.luau", "b.luau"]);
        std::fs::remove_file(root.join("Folder/a.luau")).unwrap();
        assert_eq!(names(&listed(path.clone())), ["b.luau"]);
        std::fs::rename(&root, root.with_extension("old")).unwrap();
        std::fs::create_dir_all(root.join("Folder")).unwrap();
        assert!(names(&listed(path.clone())).is_empty());
        std::fs::remove_dir_all(&root).unwrap();
        std::fs::remove_dir_all(root.with_extension("old")).unwrap();
    }
}

fn script_info(file_name: &str) -> Option<(&str, &'static str)> {
    let lower = file_name.to_ascii_lowercase();
    for (suffix, class) in SCRIPT_SUFFIXES {
        if lower.len() > suffix.len() && lower.ends_with(suffix) {
            return Some((&file_name[..file_name.len() - suffix.len()], class));
        }
    }
    None
}

/// EnsureChild: the existing child for a mounted entry, or a new one (scripts replace plain
/// folders of an earlier root, keeping their children).
fn ensure_child(state: &mut State, reflection: &Reflection, parent: InstanceId, name: &str, class_name: &str, mount: &mut MountCounts) -> InstanceId {
    if let Some(existing) = find_child(state, parent, name) {
        let existing_class = state.dom.at(existing).class;
        if &*reflection.class(existing_class).name == class_name {
            return existing;
        }
        let existing_is_script = is_a(reflection, existing_class, "LuaSourceContainer");
        let wants_script = reflection.class_id(class_name).is_some_and(|c| is_a(reflection, c, "LuaSourceContainer"));
        if !existing_is_script && !wants_script {
            return existing;
        }
        let replaces_folder = !existing_is_script && wants_script && &*reflection.class(existing_class).name == "Folder";
        if (existing_is_script && wants_script) || replaces_folder {
            let class = reflection.class_id(class_name).expect("script class");
            let replacement = core::create_named(state, reflection, class, name);
            let children = std::mem::take(&mut state.dom.at_mut(existing).children);
            for child in &children {
                state.dom.at_mut(*child).parent = Some(replacement);
            }
            state.dom.at_mut(replacement).children = children;
            state.dom.replace_child(parent, existing, replacement);
            return replacement;
        }
        if wants_script {
            mount.conflicts += 1;
        }
        return existing;
    }
    let class = reflection.class_id(class_name).expect("mounted class");
    let created = core::create_named(state, reflection, class, name);
    state.dom.attach(created, parent);
    mount.created += 1;
    created
}

fn mount_directory(engine: &Engine, node: &ScanNode, instance: InstanceId, mount: &mut MountCounts, relative: &str) {
    for (index, entry) in node.entries.iter().enumerate() {
        let full_path = format!("{}/{entry}", node.path);
        if entry.starts_with('.') {
            continue;
        }
        if let Some(child_node) = node.dirs.get(entry) {
            let child_relative = if relative.is_empty() { entry.clone() } else { format!("{relative}/{entry}") };
            mount_directory_as(engine, child_node, instance, entry, mount, &child_relative);
        } else {
            let lower = entry.to_ascii_lowercase();
            if lower.starts_with("init.") || lower.ends_with(".meta.json") {
                continue;
            }
            mount_file(engine, &full_path, entry, None, instance, mount);
            if let Some(stamp) = node.stamps.get(index).copied().flatten() {
                engine.state.borrow_mut().project.stamps.insert(full_path, stamp);
            }
        }
    }
}

/// A scanned directory mounted as the child `name` of `parent` (init scripts and init.meta.json
/// className decide its class; a Folder otherwise), then its contents.
fn mount_directory_as(engine: &Engine, node: &ScanNode, parent: InstanceId, name: &str, mount: &mut MountCounts, relative: &str) -> InstanceId {
    let reflection = &engine.reflection;
    let full_path = &node.path;
    let present: FxHashMap<String, &String> = node.entries.iter().map(|name| (name.to_ascii_lowercase(), name)).collect();
    let meta = present.get("init.meta.json").and_then(|name| read_json(&format!("{full_path}/{name}")));
    let meta_class = meta.as_ref().and_then(|m| m.get("className")).and_then(|c| c.as_str()).map(str::to_string);
    let mut init = None;
    for (suffix, class) in SCRIPT_SUFFIXES {
        if let Some(found) = present.get(&format!("init{suffix}")) {
            init = Some((format!("{full_path}/{found}"), *class));
            let listed = node.entries.iter().position(|entry| entry == *found).and_then(|index| node.stamps.get(index).copied().flatten());
            if let Some(stamp) = listed {
                engine.state.borrow_mut().project.stamps.insert(format!("{full_path}/{found}"), stamp);
            }
            break;
        }
    }
    let known = match relative {
        "StarterPlayer/StarterPlayerScripts" => Some("StarterPlayerScripts"),
        "StarterPlayer/StarterCharacterScripts" => Some("StarterCharacterScripts"),
        _ => None,
    };
    let class_name = init
        .as_ref()
        .map(|(_, c)| c.to_string())
        .or(meta_class)
        .or(known.map(str::to_string))
        .unwrap_or_else(|| "Folder".into());
    let child = {
        let mut state = engine.state.borrow_mut();
        let child = ensure_child(&mut state, reflection, parent, name, &class_name, mount);
        if let Some((path, _)) = &init {
            let instance = state.dom.at_mut(child);
            instance.set_source_path(Some(Rc::from(path.as_str())));
            instance.set_source(None);
            mount.scripts += 1;
        }
        child
    };
    mount_directory(engine, node, child, mount, relative);
    child
}

/// A file entry of a mounted directory (`file_name` its name on disk; `name` overrides the
/// instance name): scripts by suffix, JSON models, JSON modules, models. Other files are ignored.
fn mount_file(engine: &Engine, full_path: &str, file_name: &str, name: Option<&str>, instance: InstanceId, mount: &mut MountCounts) {
    let reflection = &engine.reflection;
    let lower = file_name.to_ascii_lowercase();
    if let Some((script_name, class_name)) = script_info(file_name) {
        let mut state = engine.state.borrow_mut();
        let child = ensure_child(&mut state, reflection, instance, name.unwrap_or(script_name), class_name, mount);
        let class = state.dom.at(child).class;
        if is_a(reflection, class, "LuaSourceContainer") {
            let instance = state.dom.at_mut(child);
            instance.set_source_path(Some(Rc::from(full_path)));
            instance.set_source(None);
            mount.scripts += 1;
        }
    } else if lower.ends_with(".model.json") {
        // Rojo's JSON model (project/json_model.rs): one instance, named after the file
        match mounted_plan(engine, full_path) {
            Some(plan) => instantiate_model(engine, &plan, name, instance),
            None => mount.skipped += 1,
        }
    } else if lower.ends_with(".json") {
        // JSON modules (Rojo / JSON-backed data) return the decoded value; read on require
        let module_name = name.unwrap_or(&file_name[..file_name.len() - 5]);
        let mut state = engine.state.borrow_mut();
        let child = ensure_child(&mut state, reflection, instance, module_name, "ModuleScript", mount);
        let instance = state.dom.at_mut(child);
        instance.set_source_path(Some(Rc::from(full_path)));
        instance.set_source(None);
        instance.set_flag(flags::JSON_MODULE, true);
    } else if lower.ends_with(".rbxm") || lower.ends_with(".rbxmx") {
        if let Some(plan) = mounted_plan(engine, full_path) {
            instantiate_model(engine, &plan, name, instance);
        }
    }
}

/// A mounted model file's instances made under `parent`; with `name` (Rojo: a model $path is one
/// instance, named by its project node) a single top instance takes that name.
fn instantiate_model(engine: &Engine, plan: &ModelPlan, name: Option<&str>, parent: InstanceId) {
    let created = plan.instantiate(engine, Some(parent));
    if let (Some(name), [root]) = (name, created.as_slice()) {
        let mut state = engine.state.borrow_mut();
        core::raw_set_silent(&mut state, &engine.reflection, *root, K::Name as NameId, Value::String(Str::from(name)));
    }
}

/// A model file's instances: a Studio model (.rbxm / .rbxmx), or Rojo's JSON model (.model.json),
/// whose top instance is named after the file (one Rojo would reject is reported here). None when
/// it does not decode or holds nothing.
fn decode_model_file(engine: &Engine, path: &str, bytes: &[u8]) -> Option<WeakDom> {
    let file_name = Path::new(path).file_name()?.to_string_lossy();
    if !file_name.to_ascii_lowercase().ends_with(".model.json") {
        return decode_model(bytes).ok();
    }
    match json_model::decode(bytes, &file_name[..file_name.len() - ".model.json".len()]) {
        Ok(dom) => dom,
        Err(message) => {
            engine.warn(format!("{path}: {message}; not mounted"));
            None
        }
    }
}

/// A mounted model file converted: read, decoded and converted, or, in a process that keeps compiled
/// modules (a service), the plan kept for the file while its size and modification time are unchanged
/// (a sim mounts the same bundle file run after run). None when it does not read or decode.
fn mounted_plan(engine: &Engine, path: &str) -> Option<std::sync::Arc<ModelPlan>> {
    type Kept = std::sync::Mutex<FxHashMap<String, (crate::files::Stamp, std::sync::Arc<ModelPlan>)>>;
    static KEPT: std::sync::OnceLock<Kept> = std::sync::OnceLock::new();
    let keep = crate::lua::modules::reusing_compiled_modules();
    crate::files::note(Path::new(path));
    let stamp = crate::files::stamp(Path::new(path));
    if keep {
        let kept = KEPT.get_or_init(Default::default);
        if let Some((kept_stamp, plan)) = kept.lock().ok().and_then(|map| map.get(path).cloned()) {
            if Some(kept_stamp) == stamp {
                return Some(plan);
            }
        }
    }
    let bytes = std::fs::read(path).ok()?;
    let dom = decode_model_file(engine, path, &bytes)?;
    let plan = std::sync::Arc::new(ModelPlan::new(&engine.reflection, &dom, dom.root().children()));
    if let (true, Some(stamp)) = (keep, stamp) {
        let kept = KEPT.get_or_init(|| crate::alloc::long_lived(Kept::default));
        let plan = crate::alloc::long_lived(|| std::sync::Arc::new(ModelPlan::clone_compacted(&plan)));
        if let Ok(mut map) = kept.lock() {
            map.retain(|other, (other_stamp, _)| crate::files::stamp(Path::new(other)) == Some(*other_stamp));
            crate::alloc::long_lived(|| map.insert(path.to_string(), (stamp, plan.clone())));
        }
        return Some(plan);
    }
    Some(plan)
}

/// One explicit mount (`--mount Service/Name=<path>` / `--instance Service/Name=<ClassName>`,
/// cli/rojo.mjs translates Rojo project files into these): `segments` is the instance path below
/// the DataModel, starting with a service.
#[derive(Clone, Debug, PartialEq)]
pub struct Mount {
    pub segments: Vec<String>,
    pub target: MountTarget,
}

#[derive(Clone, Debug, PartialEq)]
pub enum MountTarget {
    /// A directory (its contents become the instance's children, as a Rojo $path) or a file
    Path(String),
    /// An instance of this class (Rojo $className nodes without a $path)
    Class(String),
}

impl Mount {
    /// `Service/Name/...=<value>`; `class` for --instance.
    pub fn parse(text: &str, class: bool) -> Result<Mount, String> {
        let flag = if class { "--instance" } else { "--mount" };
        let (path, value) = text.split_once('=').ok_or_else(|| format!("{flag} needs Service/Name=<{}>", if class { "ClassName" } else { "path" }))?;
        let segments: Vec<String> = path.split(['/', '\\']).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).collect();
        if segments.is_empty() || value.trim().is_empty() {
            return Err(format!("{flag} {text}: an instance path and a value are needed"));
        }
        let target = if class { MountTarget::Class(value.trim().to_string()) } else { MountTarget::Path(value.to_string()) };
        Ok(Mount { segments, target })
    }
}

/// Applies one [`Mount`]: the service, Folders for missing intermediate instances, then the
/// directory / file / class instance at the last segment. A directory mounted at a service
/// merges its contents into the service.
pub fn mount_at(engine: &Engine, spec: &Mount) -> Result<MountCounts, String> {
    let mut mount = MountCounts::default();
    let reflection = &engine.reflection;
    let service_name = &spec.segments[0];
    if !reflection.class_by_name(service_name).is_some_and(|c| c.service) {
        return Err(format!("{}: {service_name} is not a service (paths start with one, e.g. ReplicatedStorage/{})", spec.segments.join("/"), spec.segments.join("/")));
    }
    let game = engine.world().game;
    let service = services::get_service(engine, game, service_name).map_err(|_| format!("{service_name}: not a service"))?;
    let (last, middle) = spec.segments[1..].split_last().map_or((None, &[][..]), |(last, middle)| (Some(last), middle));
    let mut parent = service;
    for segment in middle {
        let mut state = engine.state.borrow_mut();
        parent = ensure_child(&mut state, reflection, parent, segment, "Folder", &mut mount);
    }
    let relative = spec.segments.join("/");
    match &spec.target {
        MountTarget::Class(class_name) => {
            let Some(last) = last else { return Ok(mount) };
            let class = reflection.class_id(class_name).filter(|&c| !reflection.class(c).service);
            let class_name = if class.is_some() { class_name.as_str() } else { "Folder" };
            let mut state = engine.state.borrow_mut();
            ensure_child(&mut state, reflection, parent, last, class_name, &mut mount);
        }
        MountTarget::Path(path) => {
            let path = normalize(path).trim_end_matches('/').to_string();
            crate::files::note(Path::new(&path));
            let metadata = std::fs::metadata(&path).map_err(|e| format!("{}={path}: {e}", spec.segments.join("/")))?;
            if metadata.is_dir() {
                let node = listed(path);
                match last {
                    Some(last) => {
                        mount_directory_as(engine, &node, parent, last, &mut mount, &relative);
                    }
                    None => mount_directory(engine, &node, service, &mut mount, service_name),
                }
                keep_listing(engine, node);
            } else {
                let Some(last) = last else {
                    return mount_model_as_service(engine, &path, service, &mut mount).map(|()| mount);
                };
                let file_name = Path::new(&path).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                mount_file(engine, &path, &file_name, Some(last), parent, &mut mount);
            }
        }
    }
    Ok(mount)
}

/// A model file as a service, as Rojo makes a service from one: the model's top instance stands for the
/// service, so its children become the service's
fn mount_model_as_service(engine: &Engine, path: &str, service: InstanceId, mount: &mut MountCounts) -> Result<(), String> {
    let lower = path.to_ascii_lowercase();
    if !(lower.ends_with(".rbxm") || lower.ends_with(".rbxmx")) {
        return Err(format!("{path}: only a model file can be mounted as a service"));
    }
    let Some(plan) = mounted_plan(engine, path) else {
        mount.skipped += 1;
        return Ok(());
    };
    let created = plan.instantiate(engine, Some(service));
    let mut state = engine.state.borrow_mut();
    for top in created {
        for child in state.dom.children_of(top).to_vec() {
            state.dom.detach(child);
            state.dom.attach(child, service);
        }
        state.dom.detach(top);
    }
    Ok(())
}

/// The contents of directory `path` mounted as the children of `instance`, as a mounted root's
/// directories are (scripts by suffix, `init` scripts making modules with children, Folders, JSON
/// modules, models): a record harness's own modules.
pub fn mount_children(engine: &Engine, path: &str, instance: InstanceId) -> MountCounts {
    let mut mount = MountCounts::default();
    let node = listed(normalize(path).trim_end_matches('/').to_string());
    mount_directory(engine, &node, instance, &mut mount, "");
    keep_listing(engine, node);
    mount
}

/// Mounts `<Root>/<Service>/...` directories onto the matching services.
pub fn mount_layout_root(engine: &Engine, root: &str) -> MountCounts {
    mount_scanned_layout_root(engine, scan_layout_root(root, &engine.reflection))
}

/// Layout root `root`'s service directories, listed: what [`mount_layout_root`] mounts, in its order.
fn scan_layout_root(root: &str, reflection: &Reflection) -> Vec<(String, ScanNode)> {
    let mut services_found = Vec::new();
    crate::files::note_listing(Path::new(root));
    if let Ok(read) = std::fs::read_dir(root) {
        for entry in read.flatten() {
            let Ok(name) = entry.file_name().into_string() else { continue };
            let is_service = reflection.class_by_name(&name).is_some_and(|c| c.service);
            if !name.starts_with('.') && is_service && std::fs::metadata(format!("{root}/{name}")).is_ok_and(|m| m.is_dir()) {
                services_found.push(name);
            }
        }
    }
    services_found.into_iter().map(|name| (name.clone(), listed(format!("{root}/{name}")))).collect()
}

fn mount_scanned_layout_root(engine: &Engine, scanned: Vec<(String, ScanNode)>) -> MountCounts {
    let mut mount = MountCounts::default();
    let game = engine.world().game;
    for (name, node) in scanned {
        let Ok(service) = services::get_service(engine, game, &name) else { continue };
        mount_directory(engine, &node, service, &mut mount, &name);
        keep_listing(engine, node);
    }
    mount
}

/// Layout roots listed on the thread pool while the caller goes on (the mount lists them during
/// Engine.Start, which needs no files): [`Prescan::mount`] mounts each as [`mount_layout_root`] would.
/// A pool of the caller's thread alone lists them when the caller waits ([`listing`]).
pub struct Prescan {
    roots: Vec<(String, std::sync::mpsc::Receiver<Vec<(String, ScanNode)>>)>,
    /// The data roots ([`data_roots`]) and their subtrees listed: (relative path, listing) in order
    data: Option<std::sync::mpsc::Receiver<Vec<(String, ScanNode)>>>,
}

impl Prescan {
    pub fn start(roots: &[String], reflection: &std::sync::Arc<Reflection>) -> Prescan {
        let given = roots.to_vec();
        let layout: Vec<_> = roots
            .iter()
            .map(|root| {
                let (send, receive) = std::sync::mpsc::channel();
                let (path, reflection) = (root.clone(), reflection.clone());
                rayon::spawn(move || drop(send.send(scan_layout_root(&path, &reflection))));
                (root.clone(), receive)
            })
            .collect();
        let (send, receive) = std::sync::mpsc::channel();
        rayon::spawn(move || {
            let listed: Vec<(String, ScanNode)> = data_roots(&given).into_iter().flat_map(|(root, paths)| paths.into_iter().map(move |path| (root.clone(), path))).map(|(root, path)| { let node = listed(format!("{root}/{path}")); (path, node) }).collect();
            drop(send.send(listed));
        });
        Prescan { roots: layout, data: Some(receive) }
    }

    /// The data roots' subtrees mounted from their listings (as mount_subtree mounts each, in
    /// [`data_roots`]' order; listed now if they were not listed ahead).
    pub fn mount_data(&mut self, engine: &Engine, roots: &[String]) {
        let listed = self.data.take().and_then(listing);
        let listed: Vec<(String, ScanNode)> = listed.unwrap_or_else(|| data_roots(roots).into_iter().flat_map(|(root, paths)| paths.into_iter().map(move |path| (root.clone(), path))).map(|(root, path)| { let node = self::listed(format!("{root}/{path}")); (path, node) }).collect());
        for (relative, node) in listed {
            mount_scanned_subtree(engine, &relative, node);
        }
    }

    /// Layout root `root` mounted from its listing (listed now if it was not listed ahead).
    pub fn mount(&mut self, engine: &Engine, root: &str) -> MountCounts {
        let listed = match self.roots.iter().position(|(path, _)| path == root) {
            Some(index) => listing(self.roots.remove(index).1),
            None => None,
        };
        mount_scanned_layout_root(engine, listed.unwrap_or_else(|| scan_layout_root(root, &engine.reflection)))
    }
}

/// What a [`Prescan`] job sent (None if it failed). A caller that is a thread of the pool the job went
/// to runs the jobs it queued there while it waits: a pool built on its caller's thread
/// (`use_current_thread`, rur's browser module, whose page has no other thread) runs a job that
/// thread spawned only when the thread yields to rayon, so blocking on the channel there waited forever.
fn listing<T>(receive: std::sync::mpsc::Receiver<T>) -> Option<T> {
    loop {
        match receive.try_recv() {
            Ok(listed) => return Some(listed),
            Err(std::sync::mpsc::TryRecvError::Disconnected) => return None,
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                if rayon::yield_local() != Some(rayon::Yield::Executed) {
                    return receive.recv().ok();
                }
            }
        }
    }
}

/// [`DATA_ROOTS_FILE`]'s [`DATA_ROOTS_KEY`] paths materialized from `<project>/external/<Name>/src`.
pub fn data_roots(roots: &[String]) -> Vec<(String, Vec<String>)> {
    let mut result = Vec::new();
    let mut seen = FxHashSet::default();
    for root in roots {
        let project_root = parent_path(&normalize(root));
        if project_root.is_empty() || !seen.insert(project_root.clone()) {
            continue;
        }
        let manifest = read_json(&format!("{project_root}/{DATA_ROOTS_FILE}"));
        let Some(paths) = manifest.as_ref().and_then(|m| m.get(DATA_ROOTS_KEY)).and_then(|p| p.as_array()) else {
            continue;
        };
        let external = format!("{project_root}/external");
        crate::files::note_listing(Path::new(&external));
        let Ok(read) = std::fs::read_dir(&external) else { continue };
        for entry in read.flatten() {
            let Ok(name) = entry.file_name().into_string() else { continue };
            let source = format!("{external}/{name}/src");
            let mut found = Vec::new();
            for path in paths {
                let text = match path {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                let clean = text.strip_prefix(DATA_ROOTS_PREFIX).unwrap_or(&text).trim_end_matches('/').to_string();
                crate::files::note(Path::new(&format!("{source}/{clean}")));
                if !clean.is_empty() && Path::new(&format!("{source}/{clean}")).is_dir() {
                    found.push(clean);
                }
            }
            if !found.is_empty() {
                result.push((source, found));
            }
        }
    }
    result
}

/// The Studio names of mounted scripts and folders whose names a file system cannot hold (a colon, a
/// slash, a trailing dot), from a sync tool's `<project>/<STUDIO_NAME_MAP>` (`{"files": {"<root>/<Service>/
/// .../<file>": {"studioPathParts": [...]}}}`): each entry maps a file's path to its Studio path parts,
/// and every mounted instance on that path whose name differs takes its Studio name, as the sync gives
/// it in Studio. Entries of roots that are not mounted, or whose instances are missing, change nothing.
/// Returns how many instances were renamed.
pub fn apply_studio_names(engine: &Engine, roots: &[String]) -> usize {
    let mut renamed = 0;
    let mut seen = FxHashSet::default();
    for root in roots {
        let normalized = normalize(root).trim_end_matches('/').to_string();
        let project_root = parent_path(&normalized);
        let root_name = normalized.rsplit('/').next().unwrap_or_default().to_string();
        if project_root.is_empty() || root_name.is_empty() || !seen.insert(normalized.clone()) {
            continue;
        }
        let Some(map) = read_json(&format!("{project_root}/{STUDIO_NAME_MAP}")) else { continue };
        let Some(files) = map.get("files").and_then(|f| f.as_object()) else { continue };
        for (key, entry) in files {
            let disk: Vec<&str> = key.split('/').collect();
            let Some(studio) = entry.get("studioPathParts").and_then(|p| p.as_array()) else { continue };
            let studio: Vec<&str> = studio.iter().filter_map(|part| part.as_str()).collect();
            if disk.len() != studio.len() || disk.len() < 3 || disk[0] != root_name {
                continue;
            }
            renamed += rename_studio_path(engine, &disk[1..], &studio[1..]);
        }
    }
    renamed
}

/// One name-map entry below a root (`Service/.../<file>` on disk and in Studio): walks the mounted
/// instances by either name and renames those whose Studio name differs.
fn rename_studio_path(engine: &Engine, disk: &[&str], studio: &[&str]) -> usize {
    let file_part = |part: &str| -> String {
        let lower = part.to_ascii_lowercase();
        if lower.starts_with("init.") && script_info(part).is_some() {
            return String::new();
        }
        script_info(part).map(|(name, _)| name.to_string()).unwrap_or_else(|| part.to_string())
    };
    let game = engine.world().game;
    let Some(mut current) = services::find_service(engine, game, disk[0]) else { return 0 };
    let mut renamed = 0;
    let last = disk.len() - 1;
    for index in 1..disk.len() {
        let (disk_name, studio_name) = if index == last { (file_part(disk[index]), file_part(studio[index])) } else { (disk[index].to_string(), studio[index].to_string()) };
        if disk_name.is_empty() {
            break;
        }
        let mut state = engine.state.borrow_mut();
        let Some(child) = find_child(&state, current, &disk_name).or_else(|| find_child(&state, current, &studio_name)) else { return renamed };
        if state.dom.at(child).name.as_bytes() != studio_name.as_bytes() {
            core::raw_set_silent(&mut state, &engine.reflection, child, K::Name as NameId, Value::String(Str::from(studio_name.as_str())));
            renamed += 1;
        }
        current = child;
    }
    renamed
}

/// Mounts `<Root>/<RelativePath>` onto the instance at the same path below the DataModel.
pub fn mount_subtree(engine: &Engine, root: &str, relative: &str) -> MountCounts {
    mount_scanned_subtree(engine, relative, listed(format!("{root}/{relative}")))
}

fn mount_scanned_subtree(engine: &Engine, relative: &str, node: ScanNode) -> MountCounts {
    let mut mount = MountCounts::default();
    let segments: Vec<&str> = relative.split('/').collect();
    let reflection = &engine.reflection;
    if !reflection.class_by_name(segments[0]).is_some_and(|c| c.service) {
        return mount;
    }
    let game = engine.world().game;
    let Ok(mut current) = services::get_service(engine, game, segments[0]) else { return mount };
    for segment in &segments[1..] {
        let mut state = engine.state.borrow_mut();
        current = ensure_child(&mut state, reflection, current, segment, "Folder", &mut mount);
    }
    mount_directory(engine, &node, current, &mut mount, relative);
    keep_listing(engine, node);
    mount
}

fn stand_in_source(file: &str) -> Option<Rc<[u8]>> {
    match file {
        "ui-labs.luau" => Some(Rc::from(&include_bytes!("lua/packages/ui-labs.luau")[..])),
        _ => None,
    }
}

fn mount_stand_in(engine: &Engine, parent: InstanceId, name: &str, file: &str) -> bool {
    let reflection = &engine.reflection;
    let mut state = engine.state.borrow_mut();
    if find_child(&state, parent, name).is_some() {
        return false;
    }
    let Some(source) = stand_in_source(file) else { return false };
    let module = core::create_named(&mut state, reflection, reflection.class_id("ModuleScript").unwrap(), name);
    let instance = state.dom.at_mut(module);
    instance.set_source(Some(source));
    instance.set_stand_in(Some(Rc::from(file)));
    state.dom.attach(module, parent);
    true
}

/// Mounts package stand-ins where the place's Studio hierarchy has them (or, without a snapshot,
/// into ReplicatedStorage.Packages / ReplicatedStorage.*.Packages) unless the roots provide them.
pub fn mount_stand_in_packages(engine: &Engine, roots: &[String]) -> u32 {
    let mut mounted = 0;
    let game = engine.world().game;
    if let Some(snapshot) = find_hierarchy_snapshot(roots) {
        let Some(hierarchy) = load_hierarchy(&snapshot) else { return 0 };
        let nodes = &hierarchy.nodes;
        for node in nodes {
            let class_name = node.class_name.as_str();
            let name = node.name.as_str();
            let Some(file) = stand_in_file(name).filter(|_| class_name == "ModuleScript") else { continue };
            let mut names = Vec::new();
            let mut current = node;
            while let Some(parent) = current.parent.filter(|p| *p >= 0) {
                names.insert(0, current.name.clone());
                current = &nodes[parent as usize];
            }
            let mut target = Some(game);
            for segment in &names[..names.len().saturating_sub(1)] {
                let state = engine.state.borrow();
                target = target.and_then(|t| find_child(&state, t, segment));
            }
            if let Some(target) = target {
                let exists = find_child(&engine.state.borrow(), target, name).is_some();
                if !exists && mount_stand_in(engine, target, name, file) {
                    mounted += 1;
                }
            }
        }
        return mounted;
    }
    let replicated = find_child(&engine.state.borrow(), game, "ReplicatedStorage");
    let Some(replicated) = replicated else { return 0 };
    let candidates: Vec<InstanceId> = {
        let state = engine.state.borrow();
        let mut list: Vec<InstanceId> = find_child(&state, replicated, "Packages").into_iter().collect();
        for &child in &state.dom.at(replicated).children {
            if let Some(packages) = find_child(&state, child, "Packages") {
                list.push(packages);
            }
        }
        list
    };
    for folder in candidates {
        let exists = find_child(&engine.state.borrow(), folder, "ui-labs").is_some();
        if !exists && mount_stand_in(engine, folder, "ui-labs", "ui-labs.luau") {
            mounted += 1;
        }
    }
    mounted
}

/// Loads the Studio hierarchy of the place the roots belong to and serves Studio-only instances
/// from it (Hooks.Materialize). Returns the snapshot path used.
pub fn use_studio_hierarchy(engine: &Engine, roots: &[String]) -> Option<String> {
    let path = find_hierarchy_snapshot(roots)?;
    let hierarchy = load_hierarchy(&path)?;
    // (made once per snapshot per process, in the long-lived heap: crate::alloc)
    let index = hierarchy.index.get_or_init(|| crate::alloc::long_lived(|| std::sync::Arc::new(StudioIndex::new(&hierarchy)))).clone();
    let mut sources = FxHashMap::default();
    for (_, file) in PACKAGE_STAND_INS {
        if let Some(source) = stand_in_source(file) {
            sources.insert(file.to_string(), source);
        }
    }
    let mut state = engine.state.borrow_mut();
    state.project.studio = Some(Studio { game: engine.world().game, index });
    state.project.stand_in_sources = sources;
    state.project.hierarchy_path = Some(path.clone());
    Some(path)
}

/// Place content: the instances a place stores that its hierarchy snapshot lists by name and class
/// only (a Studio-only Model is a default one: scale 1, no pivot, no sizes). `<repo>/data/place-content/
/// <Place>/<parent path>/*.rbxm` holds them as Studio serialized them: the directories below the place
/// are the path of the parent (`ReplicatedStorage/Shared/Rigs`), each file's
/// roots become its children and replace the snapshot's placeholders of the same names, and the
/// snapshot adds nothing below them (it can be older than the place). Returns how many roots were
/// mounted.
pub fn mount_place_content(engine: &Engine, hierarchy: &str) -> usize {
    let Some(place) = Path::new(hierarchy).file_stem().map(|stem| stem.to_string_lossy().into_owned()) else { return 0 };
    let top = repo_path(engine, "data/place-content").join(&place);
    let mut files = Vec::new();
    let mut stack = vec![top.clone()];
    while let Some(dir) = stack.pop() {
        crate::files::note_listing(&dir);
        let Ok(read) = std::fs::read_dir(&dir) else { continue };
        for entry in read.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
            if path.is_dir() {
                stack.push(path);
            } else if name.ends_with(".rbxm") || name.ends_with(".rbxmx") {
                files.push(path);
            }
        }
    }
    files.sort();
    let mut mounted = 0;
    for file in files {
        let segments: Vec<String> = file.parent().and_then(|dir| dir.strip_prefix(&top).ok()).map(|rel| rel.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect()).unwrap_or_default();
        let path = normalize(&file.to_string_lossy());
        let Some(service_name) = segments.first() else {
            engine.warn(format!("place content {path}: files go in the folder of their parent's path"));
            continue;
        };
        let game = engine.world().game;
        let Ok(mut parent) = services::get_service(engine, game, service_name) else {
            engine.warn(format!("place content {path}: {service_name} is not a service"));
            continue;
        };
        let mut missing = None;
        for segment in &segments[1..] {
            crate::lua::instance::materialize(engine, parent);
            match find_child(&engine.state.borrow(), parent, segment) {
                Some(child) => parent = child,
                None => {
                    missing = Some(segment.clone());
                    break;
                }
            }
        }
        if let Some(segment) = missing {
            engine.warn(format!("place content {path}: no {segment} in the place"));
            continue;
        }
        crate::lua::instance::materialize(engine, parent);
        let Some(plan) = mounted_plan(engine, &path) else {
            engine.warn(format!("place content {path}: does not read as a model file"));
            continue;
        };
        // (only the snapshot's placeholders give way, as imported bundles do)
        let names: FxHashSet<&[u8]> = plan.root_names().collect();
        let obsolete: Vec<InstanceId> = {
            let state = engine.state.borrow();
            state.dom.at(parent).children.iter().copied().filter(|&child| state.dom.at(child).has(flags::STUDIO_ONLY) && names.contains(state.dom.at(child).name.as_bytes())).collect()
        };
        for child in obsolete {
            crate::lua::instance::set_parent(engine, child, None).ok();
        }
        for warning in plan.warnings() {
            engine.warn(warning.clone());
        }
        // (and the file is the whole subtree: the snapshot, which can be older than the place, adds nothing below it)
        let roots = plan.instantiate(engine, Some(parent));
        let mut state = engine.state.borrow_mut();
        for &root in &roots {
            state.dom.at_mut(root).studio_node = StudioNode::None;
        }
        mounted += roots.len();
    }
    mounted
}

// Models and snapshots (rbx-dom -> engine instances)

pub fn decode_model(bytes: &[u8]) -> Result<WeakDom, String> {
    if bytes.starts_with(b"<roblox!") {
        // Files from a newer Studio can store a property as a type our reflection database doesn't
        // expect (2026-09: Tags written as a SharedString); rbx_binary rejects the whole file. Retry
        // with that property left out of a copy of the database, so it's skipped (only non-rendering
        // metadata has done this so far).
        let mut first_error = None;
        let mut database: Option<rbx_reflection::ReflectionDatabase<'static>> = None;
        for _ in 0..8 {
            let result = match &database {
                Some(db) => rbx_binary::Deserializer::new().reflection_database(db).deserialize(bytes),
                None => rbx_binary::from_reader(bytes),
            };
            let error = match result {
                Ok(dom) => return Ok(dom),
                Err(error) => error.to_string(),
            };
            let Some((class, property)) = mismatched_property(&error) else { return Err(first_error.unwrap_or(error)) };
            let db = database.get_or_insert_with(|| rbx_reflection_database::get().expect("reflection database").clone());
            if !forget_property(db, &class, &property) {
                return Err(first_error.unwrap_or(error));
            }
            first_error.get_or_insert(error);
        }
        Err(first_error.unwrap_or_default())
    } else {
        rbx_xml::from_reader_default(bytes).map_err(|e| e.to_string())
    }
}

/// (class, property) of an rbx_binary "Type mismatch: Property Class.Name should be ..." error.
fn mismatched_property(error: &str) -> Option<(String, String)> {
    let rest = error.split("Type mismatch: Property ").nth(1)?;
    let (qualified, _) = rest.split_once(" should be ")?;
    let (class, property) = qualified.split_once('.')?;
    Some((class.to_string(), property.to_string()))
}

/// Removes `property` from the class that declares it (walking up from `class`); false if absent.
fn forget_property(db: &mut rbx_reflection::ReflectionDatabase<'static>, class: &str, property: &str) -> bool {
    let mut current = Some(class.to_string());
    while let Some(name) = current {
        let Some(descriptor) = db.classes.get_mut(name.as_str()) else { return false };
        if descriptor.properties.remove(property).is_some() {
            return true;
        }
        current = descriptor.superclass.as_ref().map(|s| s.to_string());
    }
    false
}

/// The enum an rbx-dom Enum property holds (from rbx-dom's property descriptor, as Lune reads it).
fn dom_enum_name(class_name: &str, property: &str) -> Option<String> {
    let database = rbx_reflection_database::get().ok()?;
    let mut current = database.classes.get(class_name);
    while let Some(class) = current {
        if let Some(descriptor) = class.properties.get(property) {
            if let rbx_reflection::DataType::Enum(name) = &descriptor.data_type {
                return Some(name.to_string());
            }
            return None;
        }
        current = class.superclass.as_ref().and_then(|s| database.classes.get(s.as_ref()));
    }
    None
}

/// An rbx-dom Enum property value as the engine's enum item (by rbx-dom's descriptor, as Lune
/// reads it).
fn dom_enum_value(reflection: &Reflection, class_name: &str, property: &str, item: u32) -> Option<Value> {
    let enum_name = dom_enum_name(class_name, property)?;
    let database = rbx_reflection_database::get().ok()?;
    let descriptor = database.enums.get(enum_name.as_str())?;
    let (item_name, _) = descriptor.items.iter().find(|(_, v)| **v == item)?;
    reflection.enum_item(&enum_name, item_name).map(Value::EnumItem)
}

/// ConvertLune: engine instances for the top-level instances of a decoded model (children of
/// `parent` when given), with properties converted up front.
pub fn convert_model(engine: &Engine, dom: &WeakDom, parent: Option<InstanceId>) -> Vec<InstanceId> {
    convert_instances(engine, dom, dom.root().children(), parent)
}

/// Loads a Studio snapshot whose top-level instances become children of `parent` (a service
/// root's children merge into the existing service).
pub fn load_snapshot(engine: &Engine, path: &Path, parent: InstanceId) -> Result<Vec<InstanceId>, String> {
    crate::files::note(path);
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    let dom = decode_model(&bytes)?;
    let mut result = Vec::new();
    for &root in dom.root().children() {
        let source = dom.get_by_ref(root).unwrap();
        let existing = {
            let state = engine.state.borrow();
            find_child(&state, parent, &source.name).filter(|&e| {
                let class = state.dom.at(e).class;
                &*engine.reflection.class(class).name == source.class.as_str() && engine.reflection.class(class).service
            })
        };
        if let Some(existing) = existing {
            result.extend(convert_instances(engine, &dom, source.children(), Some(existing)));
        } else {
            result.extend(convert_instances(engine, &dom, &[root], Some(parent)));
        }
    }
    Ok(result)
}

/// Deserializes a model file into detached instances.
pub fn load_model(engine: &Engine, path: &Path) -> Result<Vec<InstanceId>, String> {
    crate::files::note(path);
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    let dom = decode_model(&bytes)?;
    Ok(convert_model(engine, &dom, None))
}

/// Walks the DataModel for the ModuleScript mounted from `path` (main.luau FindModuleByPath).
pub fn find_module_by_path(engine: &Engine, path: &str) -> Option<InstanceId> {
    let wanted = normalize(path).to_lowercase();
    let state = engine.state.borrow();
    let mut stack = vec![engine.world().game];
    while let Some(node) = stack.pop() {
        let instance = state.dom.at(node);
        if let Some(source) = instance.source_path() {
            if normalize(source).to_lowercase() == wanted {
                return Some(node);
            }
        }
        // Depth-first in child order
        for &child in instance.children.iter().rev() {
            stack.push(child);
        }
    }
    None
}

pub fn repo_path(engine: &Engine, relative: &str) -> PathBuf {
    engine.options.repo.join(relative)
}
