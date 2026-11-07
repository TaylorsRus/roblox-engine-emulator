//! The 3D world: what scripts see of parts, attachments, effects and the Workspace beyond stored
//! properties. Part and attachment CFrame views (Position, Orientation, Rotation, Axis, World*),
//! Workspace.Terrain, BulkMoveTo, the spatial queries (query.rs: Raycast, Blockcast, Spherecast,
//! Shapecast, GetPartBoundsInBox / InRadius, GetPartsInPart, FindPartOnRay*), the Camera's
//! projections (camera.rs: WorldTo*Point, *PointToRay), and the calls that
//! leave nothing in a property (ParticleEmitter:Emit / Clear, Trail:Clear, Sound:Play / Stop,
//! SoundService:PlayLocalSound), reported to observers as [`WorldEvent`]s (engine::record keeps
//! them). Part of the engine core: no layout, text, export, input or session dependency.
//!
//! Rigid assemblies (joints.rs): parts joined by Welds, Motor6Ds, WeldConstraints and the other rigid
//! joints move together when one of them moves, as Roblox's kinematics move them.
//!
//! Entry point: [`install`] (behaviours, from Engine.Start after the derived views).
//!
//! Fidelity (none of this is Studio-measured yet): the views follow Roblox's documented
//! definitions (Position = CFrame.Position, Orientation = CFrame:ToOrientation() in degrees,
//! Rotation = CFrame:ToEulerAnglesXYZ() in degrees, an attachment's World* = its parent part's
//! CFrame times its own); their change events fire after the CFrame's.

pub mod camera;
pub mod frame;
pub mod gravity;
pub mod joints;
pub mod query;

#[cfg(test)]
mod tests;

use std::rc::Rc;

use glam::{Affine3A, Vec3};
use mlua::prelude::*;
use rbx_dom_weak::types::Variant;

use super::{
    Engine, InstanceId, NameId, State,
    derived::{self, ReadFn, ViewEvents, WriteFn},
    instance as core,
    lua::{Args, Flow, convert, none, raise},
    services::{GetterFn, MethodFn, SetterFn},
    value::Value,
};

/// What the world did that no property records.
#[derive(Clone, Debug, PartialEq)]
pub enum WorldEvent {
    /// `ParticleEmitter:Emit(count)`, from where the emitter was at the call (`from`, when observers
    /// are installed): Roblox spawns the particles in the call (not measured here), so a script that
    /// moves an attachment between calls in one frame leaves a burst at each place
    Emit { emitter: InstanceId, count: f64, from: Option<Emission> },
    /// `ParticleEmitter:Clear()`, `Trail:Clear()`
    Clear { instance: InstanceId },
    /// `Sound:Play()` / `:Resume()`, `AudioPlayer:Play()`, or `SoundService:PlayLocalSound(sound)` (`local`)
    SoundPlayed { sound: InstanceId, local: bool },
    /// `Sound:Stop()` / `:Pause()`
    SoundStopped { sound: InstanceId },
}

/// Where an emit's particles leave from: the world CFrame of the emitter's part or attachment at the
/// call (a bone's where its animation puts it, as the recorder draws attachments), and a part's Size
/// then (a part's emitter spawns over it).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Emission {
    pub cframe: Affine3A,
    pub part_size: Option<Vec3>,
}

/// Where `emitter` emits from now; None when it is on neither a part nor an attachment (ror holds
/// such an emitter at the world's origin, as Studio draws one).
pub fn emission(state: &State, engine: &Engine, emitter: InstanceId) -> Option<Emission> {
    let parent = state.dom.parent_of(emitter)?;
    let class = state.dom.at(parent).class;
    if engine.reflection.is_a(class, "BasePart") {
        let part_size = match state.dom.raw_get(parent, crate::K::Size as NameId) {
            Some(Value::Vector3(size)) => Some(Vec3::new(size.x, size.y, size.z)),
            _ => None,
        };
        return Some(Emission { cframe: world_cframe(state, engine, parent), part_size });
    }
    if engine.reflection.is_a(class, "Bone") {
        return Some(Emission { cframe: transformed_world_cframe(state, engine, parent), part_size: None });
    }
    if engine.reflection.is_a(class, "Attachment") {
        return Some(Emission { cframe: world_cframe(state, engine, parent), part_size: None });
    }
    None
}

/// Whether anything observes the world's events (a recorder): what only they read is worked out then.
fn observed(engine: &Engine) -> bool {
    !engine.hooks.borrow().world_events.is_empty()
}

/// Reports `event` to every observer (Hooks.world_events).
pub fn report(engine: &Engine, event: WorldEvent) {
    let hooks = engine.hooks.borrow();
    if hooks.world_events.is_empty() {
        return;
    }
    let mut state = engine.state.borrow_mut();
    for observer in &hooks.world_events {
        observer(&mut state, &event);
    }
}

fn name(engine: &Engine, text: &str) -> Option<NameId> {
    engine.reflection.name_id(text)
}

/// A Vector3 argument or value (the native vector, or a Lune Vector3).
pub fn vector_arg(value: &LuaValue) -> Option<Vec3> {
    match convert::from_lua(value) {
        Value::Vector3(v) => Some(Vec3::new(v.x, v.y, v.z)),
        _ => None,
    }
}

/// A CFrame argument.
pub fn cframe_arg(value: &LuaValue) -> Option<Affine3A> {
    cframe_of(&convert::from_lua(value))
}

/// A CFrame value as a transform (None for anything else).
pub fn cframe_of(value: &Value) -> Option<Affine3A> {
    match value {
        Value::Dom(variant) => match &**variant {
            Variant::CFrame(cframe) => Some(frame::to_affine(cframe)),
            _ => None,
        },
        _ => None,
    }
}

/// The interned name "CFrame" (not one of reflection.rs's pre-interned names).
pub fn cframe_name(engine: &Engine) -> NameId {
    engine.reflection.name_id("CFrame").expect("the reflection names CFrame")
}

/// An instance's world CFrame as Roblox places it: a part's CFrame, an attachment's (a bone's too)
/// parent's frame times its own, anything else the identity.
pub fn world_cframe(state: &State, engine: &Engine, id: InstanceId) -> Affine3A {
    let reflection = &engine.reflection;
    let class = state.dom.at(id).class;
    if reflection.is_a(class, "Attachment") {
        let own = frame::affine_of(state.dom.raw_get(id, cframe_name(engine)));
        return hung_from(state, engine, id) * own;
    }
    if reflection.is_a(class, "BasePart") {
        return frame::affine_of(state.dom.raw_get(id, cframe_name(engine)));
    }
    Affine3A::IDENTITY
}

/// The frame an attachment hangs from, measured in Studio (2026-10-17): its nearest ancestor that is
/// an attachment, a part or a model, past Folders, Configurations, Accessories and the like; a model's
/// is its pivot (one without parts or a stored pivot: the identity), a WorldModel's or the
/// Workspace's the identity, and none at all leaves its own CFrame as its world one. An effect kept its beams' attachments in a Folder
/// under a ball part: ree hung them from the origin, 2,900 studs off Studio's.
fn hung_from(state: &State, engine: &Engine, id: InstanceId) -> Affine3A {
    let reflection = &engine.reflection;
    let mut at = state.dom.parent_of(id);
    while let Some(ancestor) = at {
        let class = state.dom.at(ancestor).class;
        if is_part_or_attachment(state, engine, ancestor) {
            return parent_frame(state, engine, ancestor);
        }
        if reflection.is_a(class, "Model") && !reflection.is_a(class, "WorldRoot") {
            return model_pivot(state, engine, ancestor);
        }
        if reflection.is_a(class, "PVInstance") {
            return Affine3A::IDENTITY;
        }
        at = state.dom.parent_of(ancestor);
    }
    Affine3A::IDENTITY
}

/// Model:GetPivot as lua/services.luau has it: the PrimaryPart's pivot, else the stored
/// WorldPivotData, else the centre of its parts' world-axis bounds (the identity without parts).
fn model_pivot(state: &State, engine: &Engine, model: InstanceId) -> Affine3A {
    let reflection = &engine.reflection;
    if let Some(Value::Instance(primary)) = name(engine, "PrimaryPart").and_then(|primary| state.dom.raw_get(model, primary)) {
        let offset = name(engine, "PivotOffset").map(|offset| frame::affine_of(state.dom.raw_get(*primary, offset))).unwrap_or(Affine3A::IDENTITY);
        return frame::affine_of(state.dom.raw_get(*primary, cframe_name(engine))) * offset;
    }
    if let Some(stored) = name(engine, "WorldPivotData").and_then(|data| state.dom.at(model).stored(data))
        && matches!(stored, Value::Dom(variant) if matches!(**variant, Variant::CFrame(_) | Variant::OptionalCFrame(Some(_))))
    {
        return frame::affine_of(Some(stored));
    }
    let (Some(size_name), cframe) = (name(engine, "Size"), cframe_name(engine)) else { return Affine3A::IDENTITY };
    let mut bounds: Option<(Vec3, Vec3)> = None;
    let mut stack = vec![model];
    while let Some(node) = stack.pop() {
        if reflection.is_a(state.dom.at(node).class, "BasePart") {
            let transform = frame::affine_of(state.dom.raw_get(node, cframe));
            let half = match state.dom.raw_get(node, size_name) {
                Some(Value::Vector3(size)) => Vec3::new(size.x, size.y, size.z) / 2.0,
                _ => Vec3::ZERO,
            };
            for corner in 0..8 {
                let sign = Vec3::new(if corner & 1 == 0 { -1.0 } else { 1.0 }, if corner & 2 == 0 { -1.0 } else { 1.0 }, if corner & 4 == 0 { -1.0 } else { 1.0 });
                let point = transform.transform_point3(half * sign);
                bounds = Some(bounds.map_or((point, point), |(low, high)| (low.min(point), high.max(point))));
            }
        }
        stack.extend(state.dom.children_of(node).iter().copied());
    }
    bounds.map_or(Affine3A::IDENTITY, |(low, high)| Affine3A::from_translation((low + high) / 2.0))
}

/// The frame an attachment or bone hangs from: a parent bone where its animation puts it
/// (TransformedWorldCFrame), any other part or attachment its world CFrame. Measured in Studio: a
/// child bone's WorldCFrame carries its parent bone's Transform (not its own), and an Attachment
/// under a bone follows the bone's TransformedWorldCFrame (an animated creature rig: the
/// attachments on its animated bones were 74 studs off Studio's, at their rest pose).
fn parent_frame(state: &State, engine: &Engine, parent: InstanceId) -> Affine3A {
    if engine.reflection.is_a(state.dom.at(parent).class, "Bone") {
        transformed_world_cframe(state, engine, parent)
    } else {
        world_cframe(state, engine, parent)
    }
}

/// A Bone's world CFrame with the animation applied (Bone.TransformedWorldCFrame): its parent's
/// (a parent bone's own TransformedWorldCFrame, so a chain carries its parents' animation) times
/// CFrame * Transform. WorldCFrame stays the un-transformed reference (Bone's documentation).
pub fn transformed_world_cframe(state: &State, engine: &Engine, id: InstanceId) -> Affine3A {
    let local = transformed_cframe(state, engine, id);
    match state.dom.parent_of(id) {
        Some(parent) if engine.reflection.is_a(state.dom.at(parent).class, "Bone") => transformed_world_cframe(state, engine, parent) * local,
        _ => hung_from(state, engine, id) * local,
    }
}

/// Bone.TransformedCFrame: CFrame * Transform, relative to its parent.
fn transformed_cframe(state: &State, engine: &Engine, id: InstanceId) -> Affine3A {
    let own = frame::affine_of(state.dom.raw_get(id, cframe_name(engine)));
    let transform = name(engine, "Transform").map(|transform| frame::affine_of(state.dom.raw_get(id, transform))).unwrap_or(Affine3A::IDENTITY);
    own * transform
}

/// A Bone's read-only animated member: `read` of the CFrame `of` gives.
fn bone_member(engine: &Rc<Engine>, member: &str, of: fn(&State, &Engine, InstanceId) -> Affine3A) {
    let (Some(class), Some(name)) = (engine.reflection.class_id("Bone"), name(engine, member)) else { return };
    let getter: GetterFn = Rc::new(move |lua, engine, id| {
        let value = frame::cframe_value(&of(&engine.state.borrow(), engine, id));
        Ok(convert::to_lua(lua, engine, &value)?)
    });
    engine.behaviors.borrow_mut().add_getter(class, name, getter);
}

fn is_part_or_attachment(state: &State, engine: &Engine, id: InstanceId) -> bool {
    let class = state.dom.at(id).class;
    engine.reflection.is_a(class, "BasePart") || engine.reflection.is_a(class, "Attachment")
}

/// Sets a CFrame property through RawSet (its change events and observers run).
fn set_cframe(engine: &Engine, id: InstanceId, transform: &Affine3A) {
    let hooks = engine.hooks.borrow();
    let mut state = engine.state.borrow_mut();
    core::raw_set(&mut state, &engine.reflection, &hooks, id, cframe_name(engine), frame::cframe_value(transform));
}

/// A view of a CFrame property: `read` turns the transform into the view's value, `write` the
/// assigned value and the current transform into the new one. `alone`: writing it moves a part
/// without its assembly (BasePart Position / Orientation / Rotation, joints.rs).
fn cframe_view(engine: &Rc<Engine>, class_name: &str, view: &str, read: fn(&Affine3A) -> Value, write: fn(&Affine3A, &Value) -> Option<Affine3A>, alone: bool) {
    let cframe = cframe_name(engine);
    let read: ReadFn = Rc::new(move |value| read(&frame::affine_of(value)));
    let write: WriteFn = Rc::new(move |state, reflection, hooks, id, value| {
        let current = frame::affine_of(state.dom.raw_get(id, cframe));
        let Some(next) = write(&current, &value) else { return };
        if alone {
            joints::move_alone(state, reflection, hooks, id, cframe, &next);
        } else {
            core::raw_set(state, reflection, hooks, id, cframe, frame::cframe_value(&next));
        }
    });
    derived::add_view(engine, class_name, view, "CFrame", ViewEvents::After, read, write);
}

fn read_position(t: &Affine3A) -> Value {
    frame::vector_value(frame::translation(t))
}

fn write_position(t: &Affine3A, value: &Value) -> Option<Affine3A> {
    Some(frame::with_translation(t, frame::vector_of(value)?))
}

fn read_orientation(t: &Affine3A) -> Value {
    frame::vector_value(frame::orientation(&t.matrix3.into()))
}

fn write_orientation(t: &Affine3A, value: &Value) -> Option<Affine3A> {
    Some(Affine3A::from_mat3_translation(frame::from_orientation(frame::vector_of(value)?), frame::translation(t)))
}

fn read_rotation(t: &Affine3A) -> Value {
    frame::vector_value(frame::rotation_xyz(&t.matrix3.into()))
}

fn write_rotation(t: &Affine3A, value: &Value) -> Option<Affine3A> {
    Some(Affine3A::from_mat3_translation(frame::from_rotation_xyz(frame::vector_of(value)?), frame::translation(t)))
}

fn read_axis(t: &Affine3A) -> Value {
    frame::vector_value(Vec3::from(t.matrix3.x_axis))
}

fn write_axis(t: &Affine3A, value: &Value) -> Option<Affine3A> {
    Some(frame::from_axes(frame::translation(t), frame::vector_of(value)?, Vec3::from(t.matrix3.y_axis)))
}

fn read_secondary_axis(t: &Affine3A) -> Value {
    frame::vector_value(Vec3::from(t.matrix3.y_axis))
}

fn write_secondary_axis(t: &Affine3A, value: &Value) -> Option<Affine3A> {
    Some(frame::from_axes(frame::translation(t), Vec3::from(t.matrix3.x_axis), frame::vector_of(value)?))
}

/// An attachment's world-space member: `read` of its world CFrame; `write` (when settable) the
/// new world CFrame from the assigned value, stored relative to its parent.
fn world_member(engine: &Rc<Engine>, member: &str, read: fn(&Affine3A) -> Value, write: Option<fn(&Affine3A, &Value) -> Option<Affine3A>>) {
    let (Some(class), Some(name)) = (engine.reflection.class_id("Attachment"), name(engine, member)) else { return };
    let getter: GetterFn = Rc::new(move |lua, engine, id| {
        let value = read(&world_cframe(&engine.state.borrow(), engine, id));
        Ok(convert::to_lua(lua, engine, &value)?)
    });
    let mut behaviors = engine.behaviors.borrow_mut();
    behaviors.add_getter(class, name, getter);
    let Some(write) = write else { return };
    let setter: SetterFn = Rc::new(move |_, engine, id, value| {
        let assigned = convert::from_lua(&value);
        let (world, parent) = {
            let state = engine.state.borrow();
            (world_cframe(&state, engine, id), hung_from(&state, engine, id))
        };
        let Some(next) = write(&world, &assigned) else {
            return raise(format!("Unable to assign property {}", engine.reflection.name(name)));
        };
        set_cframe(engine, id, &(parent.inverse() * next));
        Ok(())
    });
    behaviors.add_setter(class, name, setter);
}

fn write_cframe(_: &Affine3A, value: &Value) -> Option<Affine3A> {
    cframe_of(value)
}

fn read_cframe(t: &Affine3A) -> Value {
    frame::cframe_value(t)
}

fn method(f: impl Fn(&Lua, &Rc<Engine>, InstanceId, &Args) -> Result<LuaMultiValue, Flow> + 'static) -> MethodFn {
    Rc::new(f)
}

/// Installs the world's behaviours (after Derived.Install, which owns the view table).
pub fn install(engine: &Rc<Engine>) {
    // Parts: Position, Orientation and Rotation are views of CFrame (writing one moves the part
    // without its assembly)
    cframe_view(engine, "BasePart", "Position", read_position, write_position, true);
    cframe_view(engine, "BasePart", "Orientation", read_orientation, write_orientation, true);
    cframe_view(engine, "BasePart", "Rotation", read_rotation, write_rotation, true);
    // Attachments: the same of their own CFrame, plus the axes
    cframe_view(engine, "Attachment", "Position", read_position, write_position, false);
    cframe_view(engine, "Attachment", "Orientation", read_orientation, write_orientation, false);
    cframe_view(engine, "Attachment", "Rotation", read_rotation, write_rotation, false);
    cframe_view(engine, "Attachment", "Axis", read_axis, write_axis, false);
    cframe_view(engine, "Attachment", "SecondaryAxis", read_secondary_axis, write_secondary_axis, false);
    world_member(engine, "WorldCFrame", read_cframe, Some(write_cframe));
    world_member(engine, "WorldPosition", read_position, Some(write_position));
    world_member(engine, "WorldOrientation", read_orientation, Some(write_orientation));
    world_member(engine, "WorldRotation", read_rotation, None);
    world_member(engine, "WorldAxis", read_axis, Some(write_axis));
    world_member(engine, "WorldSecondaryAxis", read_secondary_axis, Some(write_secondary_axis));
    bone_member(engine, "TransformedCFrame", transformed_cframe);
    bone_member(engine, "TransformedWorldCFrame", transformed_world_cframe);

    let reflection = engine.reflection.clone();
    let mut behaviors = engine.behaviors.borrow_mut();

    // ParticleEmitter:Emit(count = 16) / Clear(), Trail:Clear(): nothing is simulated, they are reported
    if let (Some(emitter), Some(emit), Some(clear)) = (reflection.class_id("ParticleEmitter"), name(engine, "Emit"), name(engine, "Clear")) {
        behaviors.add_method(emitter, emit, method(|_, engine, id, args| {
            let count = match args.get(1) {
                LuaValue::Nil => 16.0,
                _ => args.number(1).unwrap_or(16.0),
            };
            let from = if observed(engine) { emission(&engine.state.borrow(), engine, id) } else { None };
            report(engine, WorldEvent::Emit { emitter: id, count, from });
            Ok(none())
        }));
        behaviors.add_method(emitter, clear, method(|_, engine, id, _| {
            report(engine, WorldEvent::Clear { instance: id });
            Ok(none())
        }));
    }
    if let (Some(trail), Some(clear)) = (reflection.class_id("Trail"), name(engine, "Clear")) {
        behaviors.add_method(trail, clear, method(|_, engine, id, _| {
            report(engine, WorldEvent::Clear { instance: id });
            Ok(none())
        }));
    }

    // Sound: Play / Resume set Playing and report the play, Stop / Pause clear it (services' Sound
    // behaviour, now observable)
    if let (Some(sound), Some(playing)) = (reflection.class_id("Sound"), name(engine, "Playing")) {
        for (member, on) in [("Play", true), ("Resume", true), ("Stop", false), ("Pause", false)] {
            let Some(member) = name(engine, member) else { continue };
            behaviors.add_method(sound, member, method(move |_, engine, id, _| {
                super::lua::instance::set_property(engine, id, playing, Value::Bool(on));
                report(engine, if on { WorldEvent::SoundPlayed { sound: id, local: false } } else { WorldEvent::SoundStopped { sound: id } });
                Ok(none())
            }));
        }
    }

    // AudioPlayer (the audio API's source): Play / Stop set IsPlaying and report as Sound's do
    if let (Some(player), Some(playing)) = (reflection.class_id("AudioPlayer"), name(engine, "IsPlaying")) {
        for (member, on) in [("Play", true), ("Stop", false)] {
            let Some(member) = name(engine, member) else { continue };
            behaviors.add_method(player, member, method(move |_, engine, id, _| {
                super::lua::instance::set_property(engine, id, playing, Value::Bool(on));
                report(engine, if on { WorldEvent::SoundPlayed { sound: id, local: false } } else { WorldEvent::SoundStopped { sound: id } });
                Ok(none())
            }));
        }
    }

    if let (Some(service), Some(member)) = (reflection.class_id("SoundService"), name(engine, "PlayLocalSound")) {
        behaviors.add_method(service, member, method(|_, engine, _, args| {
            if let Some(sound) = args.instance(1) {
                report(engine, WorldEvent::SoundPlayed { sound, local: true });
            }
            Ok(none())
        }));
    }

    // Workspace.Terrain: its Terrain child, made when first asked for
    if let (Some(workspace), Some(member)) = (reflection.class_id("Workspace"), name(engine, "Terrain")) {
        behaviors.add_getter(workspace, member, Rc::new(move |lua, engine, id| match terrain(engine, id) {
            Some(found) => Ok(super::lua::instance_value(lua, engine, found)?),
            None => Ok(LuaValue::Nil),
        }));
    }

    // Workspace:BulkMoveTo(parts, cframes, mode): each part's CFrame, with its events
    if let (Some(world_root), Some(bulk)) = (reflection.class_id("WorldRoot"), name(engine, "BulkMoveTo")) {
        behaviors.add_method(world_root, bulk, method(|_, engine, _, args| {
            let (LuaValue::Table(parts), LuaValue::Table(cframes)) = (args.get(1), args.get(2)) else {
                return raise("BulkMoveTo: parts and cframes must be tables");
            };
            // (the default mode fires every event; Studio: 1.03 us a part, FireCFrameChanged 0.42)
            let each = match convert::from_lua(args.get(3)) {
                Value::EnumItem(item) if &*engine.reflection.enum_item_info(item).name == "FireCFrameChanged" => crate::cost::BULK_MOVED,
                _ => crate::cost::BULK_MOVED_ALL_EVENTS,
            };
            for (index, part) in parts.sequence_values::<LuaValue>().enumerate() {
                let Some(id) = part.ok().as_ref().and_then(super::lua::instance_id) else { continue };
                let target = cframes.raw_get::<LuaValue>(index + 1).ok().as_ref().and_then(cframe_arg);
                if let Some(target) = target {
                    set_cframe(engine, id, &target);
                    engine.state.borrow().sched.charge(each);
                }
            }
            Ok(none())
        }));
    }
    drop(behaviors);
    query::install(engine);
    camera::install(engine);
    joints::install(engine);
    gravity::install(engine);
    engine.clear_member_cache();
}

/// The Workspace's Terrain (Studio's Workspace always holds one; scripts adorn and parent to it, and
/// it draws nothing and holds no voxels here), made the first time `Workspace.Terrain` is read: an
/// engine that made it at start would number every later instance differently.
pub fn terrain(engine: &Engine, workspace: InstanceId) -> Option<InstanceId> {
    let class = engine.reflection.class_id("Terrain")?;
    if let Some(found) = engine.state.borrow().dom.find_child_of_class(workspace, class) {
        return Some(found);
    }
    let hooks = engine.hooks.borrow();
    let mut state = engine.state.borrow_mut();
    let id = core::create_named(&mut state, &engine.reflection, class, "Terrain");
    core::set_parent(&mut state, &engine.reflection, &hooks, id, Some(workspace)).ok()?;
    state.dom.at_mut(id).set_flag(super::dom::flags::PARENT_LOCKED, true);
    Some(id)
}
